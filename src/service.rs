// SPDX-License-Identifier: Apache-2.0

//! The `ai.pipestream.markup.v1.MarkupParseService` implementation.
//!
//! The request framing — `options` first, then chunks — is validated here;
//! the document bytes are buffered in memory with the byte cap enforced on
//! the chunk that crosses it, while the client is still uploading, and the
//! parse then runs on the blocking pool with its events flowing back
//! through a small bounded channel. The buffer is unavoidable: unlike a
//! pull parser, every fold this build maps (a CommonMark parser, a JSON
//! reader, a cue-block scanner) wants the whole text. The cap is what makes
//! the buffer safe, and the concurrency limit is what makes the caps
//! additive across clients.
//!
//! Nothing is ever written to disk, and the response channel is small: a
//! client that stops reading stops the parse rather than filling the
//! server's heap with events it is not collecting.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status, Streaming};

use crate::VERSION;
use crate::metrics::Metrics;
use crate::parse::{self, ParseConfig, ParseError, REGISTRY};
use crate::proto::v1 as pb;
use crate::proto::v1::markup_parse_service_server::{MarkupParseService, MarkupParseServiceServer};
use crate::sniff::Format;

/// Default document byte cap when a request asks for 0: 256 MiB.
pub const DEFAULT_MAX_DOCUMENT_MIB: u32 = 256;

/// Hard ceiling on the byte cap, whatever a request asks for: 1 GiB.
pub const CEILING_MAX_DOCUMENT_MIB: u32 = 1024;

/// Default number of parses admitted at once.
pub const DEFAULT_MAX_CONCURRENT_PARSES: usize = 64;

/// Bound of the event channel from the parser back to the client.
const EVENT_CHANNEL_BOUND: usize = 32;

/// How long the parser may wait on a client that is not draining before
/// the parse is abandoned.
///
/// Without it a client that opens streams and never reads them pins one
/// blocking thread each, and enough of those take the whole pool. A
/// consumer that has taken nothing in this long is not slow, it is gone.
const CONSUMER_STALL: Duration = Duration::from_secs(30);

/// gRPC implementation of `ai.pipestream.markup.v1.MarkupParseService`.
pub struct MarkupGrpc {
    default_max_document_bytes: u64,
    ceiling_max_document_bytes: u64,
    max_concurrent_parses: usize,
    parse_slots: Arc<tokio::sync::Semaphore>,
    metrics: Arc<Metrics>,
}

impl MarkupGrpc {
    /// A service with the fleet defaults.
    #[must_use]
    pub fn new() -> Self {
        Self::with_metrics(Metrics::new())
    }

    /// A service reporting into an existing counter set, so the binary can
    /// print the same counters the service updates.
    #[must_use]
    pub fn with_metrics(metrics: Arc<Metrics>) -> Self {
        Self {
            default_max_document_bytes: mib(DEFAULT_MAX_DOCUMENT_MIB),
            ceiling_max_document_bytes: mib(CEILING_MAX_DOCUMENT_MIB),
            max_concurrent_parses: DEFAULT_MAX_CONCURRENT_PARSES,
            parse_slots: Arc::new(tokio::sync::Semaphore::new(DEFAULT_MAX_CONCURRENT_PARSES)),
            metrics,
        }
    }

    /// Override the cap applied when a request asks for 0.
    #[must_use]
    pub fn with_default_max_document_mib(mut self, mib_value: u32) -> Self {
        self.default_max_document_bytes = mib(mib_value);
        self
    }

    /// Override the hard ceiling a request cannot exceed.
    #[must_use]
    pub fn with_ceiling_max_document_mib(mut self, mib_value: u32) -> Self {
        self.ceiling_max_document_bytes = mib(mib_value);
        self
    }

    /// Override how many parses may run at once.
    #[must_use]
    pub fn with_max_concurrent_parses(mut self, max: usize) -> Self {
        self.max_concurrent_parses = max;
        self.parse_slots = Arc::new(tokio::sync::Semaphore::new(max));
        self
    }

    /// Wrap the service in its generated tonic server.
    #[must_use]
    pub fn into_service(self) -> MarkupParseServiceServer<Self> {
        MarkupParseServiceServer::new(self)
    }

    /// The counters this service updates.
    #[must_use]
    pub fn metrics(&self) -> Arc<Metrics> {
        Arc::clone(&self.metrics)
    }

    /// Resolve the byte cap for one request: 0 means the default, and
    /// anything above the ceiling is clamped rather than refused, because a
    /// client asking for more memory than the server has is asking, not
    /// attacking.
    fn resolve_cap(&self, requested_mib: u32) -> u64 {
        if requested_mib == 0 {
            self.default_max_document_bytes
        } else {
            mib(requested_mib).min(self.ceiling_max_document_bytes)
        }
    }

    /// Take a parse slot, or refuse. The contract queues nothing: a request
    /// past the limit is told so immediately.
    fn admit(&self) -> Result<tokio::sync::OwnedSemaphorePermit, Status> {
        Arc::clone(&self.parse_slots)
            .try_acquire_owned()
            .map_err(|_| {
                self.metrics.parses_refused.fetch_add(1, Ordering::Relaxed);
                Status::resource_exhausted(
                    "too many concurrent parses; retry shortly or raise \
                     GRPC_MARKUP_MAX_CONCURRENT_PARSES",
                )
            })
    }

    /// Drain the request stream into one buffer, enforcing the byte cap on
    /// the chunk that crosses it — while the client is still uploading —
    /// and the framing rules on every frame.
    async fn receive(
        &self,
        requests: &mut Streaming<pb::ParseMarkupRequest>,
        limit_bytes: u64,
    ) -> Result<Vec<u8>, Status> {
        let mut bytes: Vec<u8> = Vec::new();
        while let Some(message) = requests.message().await? {
            match message.payload {
                Some(pb::parse_markup_request::Payload::Chunk(chunk)) => {
                    if bytes.len() as u64 + chunk.len() as u64 > limit_bytes {
                        self.metrics.parses_capped.fetch_add(1, Ordering::Relaxed);
                        return Err(Status::resource_exhausted(format!(
                            "the document exceeds the {limit_bytes} byte cap for this request; \
                             raise max_document_mib if it is genuinely this large"
                        )));
                    }
                    bytes.extend_from_slice(&chunk);
                }
                Some(pb::parse_markup_request::Payload::Options(_)) => {
                    return Err(Status::invalid_argument(
                        "`options` may only be set on the first request message",
                    ));
                }
                None => {
                    return Err(Status::invalid_argument(
                        "every ParseMarkup request message must set `options` or `chunk`",
                    ));
                }
            }
        }
        Ok(bytes)
    }
}

impl Default for MarkupGrpc {
    fn default() -> Self {
        Self::new()
    }
}

#[tonic::async_trait]
impl MarkupParseService for MarkupGrpc {
    type ParseMarkupStream = ReceiverStream<Result<pb::ParseMarkupResponse, Status>>;

    async fn parse_markup(
        &self,
        request: Request<Streaming<pb::ParseMarkupRequest>>,
    ) -> Result<Response<Self::ParseMarkupStream>, Status> {
        let permit = self.admit()?;
        let started = Instant::now();
        let mut requests = request.into_inner();

        let options = match requests.message().await {
            Ok(Some(message)) => match message.payload {
                Some(pb::parse_markup_request::Payload::Options(options)) => options,
                Some(pb::parse_markup_request::Payload::Chunk(_)) | None => {
                    return Err(Status::invalid_argument(
                        "the first ParseMarkup request message must set `options`",
                    ));
                }
            },
            Ok(None) => {
                return Err(Status::invalid_argument(
                    "empty ParseMarkup request stream; the first message must set `options`",
                ));
            }
            Err(status) => return Err(status),
        };

        let format = pb::MarkupFormat::try_from(options.format)
            .map_err(|_| Status::invalid_argument(format!("unknown format {}", options.format)))?;
        let config = ParseConfig {
            format: Format::from_proto(format),
            emit_document: options.emit_document,
        };
        let limit = self.resolve_cap(options.max_document_mib);

        self.metrics.parses_started.fetch_add(1, Ordering::Relaxed);
        let bytes = match self.receive(&mut requests, limit).await {
            Ok(bytes) => bytes,
            Err(status) => {
                self.metrics.parses_failed.fetch_add(1, Ordering::Relaxed);
                return Err(status);
            }
        };

        let (event_tx, event_rx) = mpsc::channel(EVENT_CHANNEL_BOUND);
        let panic_tx = event_tx.clone();
        let metrics = Arc::clone(&self.metrics);
        let handle = tokio::runtime::Handle::current();
        tokio::spawn(async move {
            // The permit lives as long as the parse, not as long as this
            // async wrapper, so it is moved into the blocking closure.
            let joined = tokio::task::spawn_blocking(move || {
                let _permit = permit;
                run_parse(&handle, &bytes, &config, started, &event_tx, &metrics);
            })
            .await;
            match joined {
                Ok(()) => {}
                Err(e) if e.is_panic() => {
                    // A panic in a fold is this server's fault, not the
                    // document's, so it is INTERNAL and not INVALID_ARGUMENT.
                    let _ = panic_tx
                        .send(Err(Status::internal("the markup parser task panicked")))
                        .await;
                }
                Err(_) => {
                    let _ = panic_tx
                        .send(Err(Status::cancelled(
                            "the markup parser task was cancelled",
                        )))
                        .await;
                }
            }
        });

        Ok(Response::new(ReceiverStream::new(event_rx)))
    }

    async fn get_service_info(
        &self,
        _request: Request<pb::GetServiceInfoRequest>,
    ) -> Result<Response<pb::GetServiceInfoResponse>, Status> {
        Ok(Response::new(pb::GetServiceInfoResponse {
            service: "grpc-markup".to_owned(),
            version: VERSION.to_owned(),
            formats: REGISTRY
                .iter()
                .map(|(format, _)| format.to_proto() as i32)
                .collect(),
            default_max_document_mib: to_mib(self.default_max_document_bytes),
            ceiling_max_document_mib: to_mib(self.ceiling_max_document_bytes),
            max_concurrent_parses: u32::try_from(self.max_concurrent_parses).unwrap_or(u32::MAX),
        }))
    }
}

/// The blocking half of one parse: drive the fold, deliver every event
/// with backpressure, and turn a failure into the fleet's status taxonomy.
fn run_parse(
    handle: &tokio::runtime::Handle,
    bytes: &[u8],
    config: &ParseConfig,
    started: Instant,
    event_tx: &mpsc::Sender<Result<pb::ParseMarkupResponse, Status>>,
    metrics: &Metrics,
) {
    let mut events = 0u64;
    let mut emit = |event: pb::ParseMarkupResponse| {
        let sent = deliver(handle, event_tx, event);
        if sent {
            events += 1;
        }
        sent
    };
    match parse::run(bytes, config, started, &mut emit) {
        Ok(format) => metrics.record_success(format, bytes.len() as u64, events),
        Err(ParseError::ConsumerGone) => {
            metrics.parses_failed.fetch_add(1, Ordering::Relaxed);
        }
        Err(error) => {
            metrics.parses_failed.fetch_add(1, Ordering::Relaxed);
            let failure = status_for(&error);
            let _ = handle.block_on(async {
                tokio::time::timeout(CONSUMER_STALL, event_tx.send(Err(failure))).await
            });
        }
    }
}

/// Send one event to the client.
///
/// A bounded send with a deadline: real backpressure for a client that is
/// merely slow, and an exit for one that has stopped reading without
/// closing the stream. Returns false when the consumer is gone.
fn deliver(
    handle: &tokio::runtime::Handle,
    event_tx: &mpsc::Sender<Result<pb::ParseMarkupResponse, Status>>,
    event: pb::ParseMarkupResponse,
) -> bool {
    handle.block_on(async {
        tokio::time::timeout(CONSUMER_STALL, event_tx.send(Ok(event)))
            .await
            .is_ok_and(|r| r.is_ok())
    })
}

/// The fleet's error taxonomy, in one place.
fn status_for(error: &ParseError) -> Status {
    let message = error.to_string();
    match error {
        ParseError::Invalid(_) => Status::invalid_argument(message),
        ParseError::Unsupported(_) => Status::unimplemented(message),
        ParseError::Internal(_) => Status::internal(message),
        ParseError::ConsumerGone => Status::cancelled(message),
    }
}

/// Mebibytes as bytes.
fn mib(value: u32) -> u64 {
    u64::from(value) * 1024 * 1024
}

/// Bytes as whole mebibytes, for reporting.
fn to_mib(value: u64) -> u32 {
    u32::try_from(value / (1024 * 1024)).unwrap_or(u32::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_selects_the_default_and_large_requests_clamp() {
        let service = MarkupGrpc::new()
            .with_default_max_document_mib(8)
            .with_ceiling_max_document_mib(64);
        assert_eq!(service.resolve_cap(0), mib(8));
        assert_eq!(service.resolve_cap(16), mib(16));
        assert_eq!(service.resolve_cap(4096), mib(64));
    }

    #[test]
    fn every_parse_error_maps_to_its_documented_code() {
        use tonic::Code;
        let cases = [
            (ParseError::Invalid("x".into()), Code::InvalidArgument),
            (ParseError::Unsupported("x".into()), Code::Unimplemented),
            (ParseError::Internal("x".into()), Code::Internal),
            (ParseError::ConsumerGone, Code::Cancelled),
        ];
        for (error, want) in cases {
            assert_eq!(status_for(&error).code(), want, "{error}");
        }
    }
}
