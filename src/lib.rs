// SPDX-License-Identifier: Apache-2.0

//! A gRPC collector that maps all seven contract formats — Markdown, HTML,
//! AsciiDoc, LaTeX, WebVTT, BoxNote and serialized Docling JSON — into
//! the gRParse
//! Document plane, streaming typed events as each fold produces them.
//!
//! Design rules, in the order they constrain everything else:
//!
//! - **One block vocabulary.** Every format lands on the same events
//!   ([`TextBlock`](proto::v1::TextBlock), tables, images, cues), so a
//!   downstream reader handles a heading the same way whether it was
//!   `# Title` or a BoxNote heading node. The per-format knowledge lives in
//!   one fold module each, registered in [`parse::REGISTRY`]; adding a format
//!   is one module plus one registry entry.
//! - **Diskless.** Document bytes arrive over the request stream, are
//!   buffered in memory up to the byte cap — these parsers need the whole
//!   text, unlike a pull parser — and nothing is ever written.
//! - **The parser resolves nothing.** Image references, links and URIs stay
//!   the strings the source wrote; nothing is fetched or dereferenced.
//!
//! The modules follow the data: [`sniff`] decides which format the bytes
//! are, [`parse`] is the shared driver that owns event ordering, warning
//! aggregation and the fold registry, the format modules ([`markdown`],
//! [`html`], [`asciidoc`], [`latex`], [`vtt`], [`boxnote`], [`docling_json`])
//! each map one input family, [`toml`] reads the TOML dialect of front
//! matter onto the same metadata tree the YAML one lands on, [`spans`] is
//! the shared inline flattener that
//! records where the formatted, linked and referring runs landed, and
//! [`service`] wires it all to tonic. [`document_fold`] is the optional
//! second consumer of the same event stream: it folds a parse into one
//! `ai.pipestream.document.v1.Document` when a caller asks for one.

pub mod asciidoc;
pub mod boxnote;
pub mod docling_json;
pub mod document_fold;
pub mod html;
pub mod latex;
pub mod markdown;
pub mod metrics;
pub mod parse;
pub mod service;
pub mod sniff;
pub mod spans;
pub mod toml;
pub mod vtt;

/// Generated protobuf messages and gRPC stubs for `ai.pipestream.markup.v1`.
///
/// Produced from `proto/` by `build.rs` on every build, so they cannot drift
/// from the schema that `buf lint` gates. The stubs carry no documentation of
/// their own; the commented, linted source of truth is the `.proto` files.
#[allow(missing_docs, clippy::all, clippy::pedantic, clippy::nursery)]
pub mod proto {
    /// Encoded `FileDescriptorSet` for the package, served by gRPC
    /// reflection.
    pub const FILE_DESCRIPTOR_SET: &[u8] = tonic::include_file_descriptor_set!("descriptor");

    /// Messages, client and server for the `ai.pipestream.markup.v1` package.
    pub mod v1 {
        tonic::include_proto!("ai.pipestream.markup.v1");
    }
}

/// Generated protobuf messages for `ai.pipestream.document.v1`, the Document
/// plane this collector projects into.
///
/// The schema is vendored byte-identical from the gRParse repository and is
/// never edited here; [`document_fold`] and [`docling_json`] are the only
/// things in this crate that build one.
///
/// The module sits at the crate root rather than under [`proto`] because
/// prost resolves a cross-package reference by walking up from the referring
/// package's module: the `document` variant generated into
/// `proto::v1::parse_markup_response` names
/// `super::super::super::document::v1`, which is `crate::document::v1`.
/// Moving this module changes nothing but the compile error.
#[allow(missing_docs, clippy::all, clippy::pedantic, clippy::nursery)]
pub mod document {
    /// Messages for the `ai.pipestream.document.v1` package.
    pub mod v1 {
        tonic::include_proto!("ai.pipestream.document.v1");
    }
}

pub use service::MarkupGrpc;

/// Version of this server, reported by `GetServiceInfo` and attached to
/// every item's `CollectorSource`.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Value of `CollectorSource.collector` on every item this service produces.
pub const COLLECTOR: &str = "markup";
