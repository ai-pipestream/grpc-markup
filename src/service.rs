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
use crate::parse::{self, Cancel, ParseConfig, ParseError, REGISTRY};
use crate::proto::v1 as pb;
use crate::proto::v1::markup_parse_service_server::{MarkupParseService, MarkupParseServiceServer};
use crate::sniff::Format;

/// Default document byte cap when a request asks for 0: 256 MiB.
pub const DEFAULT_MAX_DOCUMENT_MIB: u32 = 256;

/// Hard ceiling on the byte cap, whatever a request asks for: 1 GiB.
pub const CEILING_MAX_DOCUMENT_MIB: u32 = 1024;

/// Default number of parses admitted at once.
pub const DEFAULT_MAX_CONCURRENT_PARSES: usize = 64;

/// Default budget for document bytes buffered across every parse at once:
/// 2 GiB.
///
/// The byte cap bounds one request and the concurrency limit bounds how
/// many run, but their product (64 parses at the 1 GiB ceiling) is far more
/// memory than a pod has. Every buffered byte draws from this one budget
/// while it is held, so a handful of large uploads is refused rather than
/// letting the pod be OOM-killed with all its in-flight work.
pub const DEFAULT_MEMORY_BUDGET_MIB: u32 = 2048;

/// Default for how long an upload may go without a request frame before it
/// is abandoned.
pub const DEFAULT_UPLOAD_IDLE: Duration = Duration::from_secs(30);

/// Default for how long a whole upload may take.
pub const DEFAULT_UPLOAD_TOTAL: Duration = Duration::from_secs(600);

/// Default wall-clock budget for one parse once its bytes are in.
pub const DEFAULT_MAX_PARSE: Duration = Duration::from_secs(300);

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
    /// One permit per buffered document byte, shared by every parse.
    memory: Arc<tokio::sync::Semaphore>,
    upload_idle: Duration,
    upload_total: Duration,
    max_parse: Duration,
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
            memory: Arc::new(tokio::sync::Semaphore::new(budget_permits(
                DEFAULT_MEMORY_BUDGET_MIB,
            ))),
            upload_idle: DEFAULT_UPLOAD_IDLE,
            upload_total: DEFAULT_UPLOAD_TOTAL,
            max_parse: DEFAULT_MAX_PARSE,
            metrics,
        }
    }

    /// Override the budget for document bytes buffered across every parse
    /// at once.
    #[must_use]
    pub fn with_memory_budget_mib(mut self, mib_value: u32) -> Self {
        self.memory = Arc::new(tokio::sync::Semaphore::new(budget_permits(mib_value)));
        self
    }

    /// Override how long an upload may go without a request frame.
    #[must_use]
    pub const fn with_upload_idle_timeout(mut self, idle: Duration) -> Self {
        self.upload_idle = idle;
        self
    }

    /// Override how long a whole upload may take.
    #[must_use]
    pub const fn with_upload_total_timeout(mut self, total: Duration) -> Self {
        self.upload_total = total;
        self
    }

    /// Override the wall-clock budget for one parse. A client deadline
    /// shorter than this wins.
    #[must_use]
    pub const fn with_max_parse_time(mut self, max: Duration) -> Self {
        self.max_parse = max;
        self
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
                .min(self.ceiling_max_document_bytes)
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
    ///
    /// Every buffered byte is drawn from the shared memory budget as it
    /// arrives; the returned permit holds them until the parse is done.
    async fn receive(
        &self,
        requests: &mut Streaming<pb::ParseMarkupRequest>,
        limit_bytes: u64,
        upload_deadline: tokio::time::Instant,
    ) -> Result<(Vec<u8>, Option<tokio::sync::OwnedSemaphorePermit>), Status> {
        let mut bytes: Vec<u8> = Vec::new();
        let mut held: Option<tokio::sync::OwnedSemaphorePermit> = None;
        while let Some(message) = self.next_frame(requests, upload_deadline).await? {
            match message.payload {
                Some(pb::parse_markup_request::Payload::Chunk(chunk)) => {
                    if bytes.len() as u64 + chunk.len() as u64 > limit_bytes {
                        self.metrics.parses_capped.fetch_add(1, Ordering::Relaxed);
                        return Err(Status::resource_exhausted(format!(
                            "the document exceeds the {limit_bytes} byte cap for this request; \
                             raise max_document_mib if it is genuinely this large"
                        )));
                    }
                    if !chunk.is_empty() {
                        let permit = u32::try_from(chunk.len())
                            .ok()
                            .and_then(|n| Arc::clone(&self.memory).try_acquire_many_owned(n).ok())
                            .ok_or_else(|| {
                                self.metrics.parses_refused.fetch_add(1, Ordering::Relaxed);
                                Status::resource_exhausted(
                                    "the server's memory budget for buffered documents is in \
                                     use; retry shortly or raise GRPC_MARKUP_MEMORY_BUDGET_MIB",
                                )
                            })?;
                        match held.as_mut() {
                            Some(held) => held.merge(permit),
                            None => held = Some(permit),
                        }
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
        Ok((bytes, held))
    }

    /// The next request frame, or `DEADLINE_EXCEEDED` when none arrives
    /// within the idle timeout or before the upload's overall deadline.
    ///
    /// A parse slot is held from the moment a call is admitted, so without
    /// these a client that opens streams and then trickles or stops could
    /// hold every slot indefinitely.
    async fn next_frame(
        &self,
        requests: &mut Streaming<pb::ParseMarkupRequest>,
        upload_deadline: tokio::time::Instant,
    ) -> Result<Option<pb::ParseMarkupRequest>, Status> {
        let idle_deadline = tokio::time::Instant::now() + self.upload_idle;
        let deadline = idle_deadline.min(upload_deadline);
        match tokio::time::timeout_at(deadline, requests.message()).await {
            Ok(result) => result,
            Err(_) => {
                let why = if deadline == upload_deadline {
                    format!(
                        "the upload took longer than {} s and was abandoned",
                        self.upload_total.as_secs()
                    )
                } else {
                    format!(
                        "no request frame arrived for {} ms; the upload was abandoned",
                        self.upload_idle.as_millis()
                    )
                };
                Err(Status::deadline_exceeded(why))
            }
        }
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
        let upload_deadline = tokio::time::Instant::now() + self.upload_total;
        let client_timeout = grpc_timeout(request.metadata());
        let mut requests = request.into_inner();

        let options = match self.next_frame(&mut requests, upload_deadline).await {
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
            // Unset means on: the field exists to turn spans off, not to
            // opt into them.
            emit_inline_spans: options.emit_inline_spans.unwrap_or(true),
        };
        let limit = self.resolve_cap(options.max_document_mib);

        self.metrics.parses_started.fetch_add(1, Ordering::Relaxed);
        let (bytes, memory) = match self.receive(&mut requests, limit, upload_deadline).await {
            Ok(received) => received,
            Err(status) => {
                self.metrics.parses_failed.fetch_add(1, Ordering::Relaxed);
                return Err(status);
            }
        };

        let (event_tx, event_rx) = mpsc::channel(EVENT_CHANNEL_BOUND);
        // The parse stops when the client hangs up or its time is up: the
        // server's own budget, or the client's deadline when that is
        // sooner, counted from when the call arrived.
        let budget_deadline = Instant::now() + self.max_parse;
        let deadline = client_timeout
            .and_then(|timeout| started.checked_add(timeout))
            .map_or(budget_deadline, |client| client.min(budget_deadline));
        let probe = event_tx.clone();
        let cancel = Cancel::new(Some(deadline), Some(Arc::new(move || probe.is_closed())));
        let panic_tx = event_tx.clone();
        let metrics = Arc::clone(&self.metrics);
        let handle = tokio::runtime::Handle::current();
        tokio::spawn(async move {
            // The permit lives as long as the parse, not as long as this
            // async wrapper, so it is moved into the blocking closure.
            let joined = tokio::task::spawn_blocking(move || {
                let _permit = permit;
                let _memory = memory;
                run_parse(
                    &handle, &bytes, &config, &cancel, started, &event_tx, &metrics,
                );
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
            default_max_document_mib: to_mib(self.resolve_cap(0)),
            ceiling_max_document_mib: to_mib(self.ceiling_max_document_bytes),
            max_concurrent_parses: u32::try_from(self.max_concurrent_parses).unwrap_or(u32::MAX),
            ui: Some(pb::UiInfo {
                title: "Markup".to_owned(),
                path: "/ui/markup".to_owned(),
                description: "Markdown, HTML, AsciiDoc, LaTeX, WebVTT, BoxNote, Docling JSON to a \
                              shared block vocabulary"
                    .to_owned(),
            }),
        }))
    }
}

/// The blocking half of one parse: drive the fold, deliver every event
/// with backpressure, and turn a failure into the fleet's status taxonomy.
fn run_parse(
    handle: &tokio::runtime::Handle,
    bytes: &[u8],
    config: &ParseConfig,
    cancel: &Cancel,
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
    match parse::run_with(bytes, config, cancel, started, &mut emit) {
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
        ParseError::DeadlineExceeded(_) => Status::deadline_exceeded(message),
    }
}

/// The client's deadline from its `grpc-timeout` header, when it sent one
/// the gRPC spec can read: at most eight digits and one unit letter.
fn grpc_timeout(metadata: &tonic::metadata::MetadataMap) -> Option<Duration> {
    let value = metadata.get("grpc-timeout")?.to_str().ok()?;
    let (digits, unit) = value.split_at(value.len().checked_sub(1)?);
    if digits.is_empty() || digits.len() > 8 || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let amount: u64 = digits.parse().ok()?;
    Some(match unit {
        "H" => Duration::from_secs(amount * 3600),
        "M" => Duration::from_secs(amount * 60),
        "S" => Duration::from_secs(amount),
        "m" => Duration::from_millis(amount),
        "u" => Duration::from_micros(amount),
        "n" => Duration::from_nanos(amount),
        _ => return None,
    })
}

/// A memory budget in mebibytes as semaphore permits, one per byte,
/// clamped to what a tokio semaphore can hold.
fn budget_permits(mib_value: u32) -> usize {
    usize::try_from(mib(mib_value))
        .unwrap_or(usize::MAX)
        .min(tokio::sync::Semaphore::MAX_PERMITS)
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
    fn a_default_above_the_ceiling_is_clamped_to_it() {
        let service = MarkupGrpc::new()
            .with_default_max_document_mib(4096)
            .with_ceiling_max_document_mib(64);
        assert_eq!(service.resolve_cap(0), mib(64));
    }

    #[test]
    fn grpc_timeout_headers_parse_per_the_spec() {
        let header = |value: &str| {
            let mut metadata = tonic::metadata::MetadataMap::new();
            metadata.insert("grpc-timeout", value.parse().unwrap());
            grpc_timeout(&metadata)
        };
        assert_eq!(header("5S"), Some(Duration::from_secs(5)));
        assert_eq!(header("250m"), Some(Duration::from_millis(250)));
        assert_eq!(header("2H"), Some(Duration::from_secs(7200)));
        assert_eq!(header("123456789S"), None, "at most eight digits");
        assert_eq!(header("5x"), None);
        assert_eq!(header("S"), None);
        assert_eq!(grpc_timeout(&tonic::metadata::MetadataMap::new()), None);
    }

    #[test]
    fn every_parse_error_maps_to_its_documented_code() {
        use tonic::Code;
        let cases = [
            (ParseError::Invalid("x".into()), Code::InvalidArgument),
            (ParseError::Unsupported("x".into()), Code::Unimplemented),
            (ParseError::Internal("x".into()), Code::Internal),
            (ParseError::ConsumerGone, Code::Cancelled),
            (
                ParseError::DeadlineExceeded("x".into()),
                Code::DeadlineExceeded,
            ),
        ];
        for (error, want) in cases {
            assert_eq!(status_for(&error).code(), want, "{error}");
        }
    }
}
