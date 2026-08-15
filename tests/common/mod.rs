// SPDX-License-Identifier: Apache-2.0

//! Shared harness and selectors for the integration tests.
//!
//! Every fixture is a string literal authored inline in the test that reads
//! it. Nothing is read from disk, nothing is fetched, and no corpus is
//! committed: the tests are the specification of what these formats look
//! like as far as this repository is concerned, so they have to be readable
//! next to the assertions that consume them.
//!
//! The harness runs a real tonic server on an ephemeral port and drives it
//! with the generated client, because the properties under test — that the
//! envelope is ordered on the wire, that a cap trips while the client is
//! still uploading, that a refusal is a status and not an event — are
//! properties of the wire and cannot be observed by calling the folds
//! directly.

#![allow(dead_code)]

use std::time::Duration;

use grpc_markup::proto::v1 as pb;
use grpc_markup::proto::v1::markup_parse_service_client::MarkupParseServiceClient;
use grpc_markup::service::MarkupGrpc;
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio_stream::wrappers::{ReceiverStream, TcpListenerStream};
use tonic::transport::Channel;

/// How long a test waits on the server before declaring it stalled.
pub const CALL_TIMEOUT: Duration = Duration::from_secs(10);

/// Start a server on an ephemeral localhost port and connect a client.
pub async fn client() -> MarkupParseServiceClient<Channel> {
    client_with(MarkupGrpc::new()).await
}

/// Start a server built from a specific service configuration.
pub async fn client_with(service: MarkupGrpc) -> MarkupParseServiceClient<Channel> {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().unwrap();
    let service = service.into_service();
    tokio::spawn(async move {
        tonic::transport::Server::builder()
            .add_service(service)
            .serve_with_incoming(TcpListenerStream::new(listener))
            .await
            .expect("server");
    });
    // The listener is bound before the task starts, so connecting cannot
    // race the serve call.
    MarkupParseServiceClient::connect(format!("http://{addr}"))
        .await
        .expect("connect")
}

/// Options asking for one explicit format, server defaults otherwise.
pub fn options(format: pb::MarkupFormat) -> pb::ParseOptions {
    pb::ParseOptions {
        format: format as i32,
        ..pb::ParseOptions::default()
    }
}

/// Options asking the server to sniff the format.
pub fn sniff_options() -> pb::ParseOptions {
    pb::ParseOptions::default()
}

/// Build the request frames for a whole document in one go.
pub fn frames(
    document: &str,
    options: pb::ParseOptions,
    chunk_size: usize,
) -> Vec<pb::ParseMarkupRequest> {
    let mut frames = vec![pb::ParseMarkupRequest {
        payload: Some(pb::parse_markup_request::Payload::Options(options)),
    }];
    frames.extend(
        document
            .as_bytes()
            .chunks(chunk_size)
            .map(|chunk| pb::ParseMarkupRequest {
                payload: Some(pb::parse_markup_request::Payload::Chunk(chunk.to_vec())),
            }),
    );
    frames
}

/// Parse a document end to end and return every event, or the status that
/// ended the stream — whether it ended the call or arrived mid-stream.
pub async fn parse(
    client: &MarkupParseServiceClient<Channel>,
    document: &str,
    options: pb::ParseOptions,
) -> Result<Vec<pb::ParseMarkupResponse>, tonic::Status> {
    let mut client = client.clone();
    let requests = frames(document, options, 8 * 1024);
    let mut stream = client
        .parse_markup(tokio_stream::iter(requests))
        .await?
        .into_inner();
    let mut events = Vec::new();
    while let Some(event) = stream.message().await? {
        events.push(event);
    }
    Ok(events)
}

/// Parse a document that is expected to succeed, asserting the stream
/// envelope: exactly one `info` first, exactly one `status` last.
pub async fn parse_ok(
    client: &MarkupParseServiceClient<Channel>,
    document: &str,
    options: pb::ParseOptions,
) -> Vec<pb::ParseMarkupResponse> {
    let events = parse(client, document, options)
        .await
        .expect("parse succeeds");
    assert!(
        events.len() >= 2,
        "a successful parse is at least info + status"
    );
    assert!(
        matches!(
            events.first().and_then(|e| e.event.as_ref()),
            Some(pb::parse_markup_response::Event::Info(_))
        ),
        "the first event must be MarkupInfo"
    );
    assert!(
        matches!(
            events.last().and_then(|e| e.event.as_ref()),
            Some(pb::parse_markup_response::Event::Status(_))
        ),
        "the last event must be ParseStatus"
    );
    let infos = events
        .iter()
        .filter(|e| matches!(e.event, Some(pb::parse_markup_response::Event::Info(_))))
        .count();
    let statuses = events
        .iter()
        .filter(|e| matches!(e.event, Some(pb::parse_markup_response::Event::Status(_))))
        .count();
    assert_eq!(infos, 1, "MarkupInfo is sent exactly once");
    assert_eq!(statuses, 1, "ParseStatus is sent exactly once");
    events
}

/// An upload the test feeds by hand, so it can hold bytes back and observe
/// how the server reacts while the stream is still open.
pub struct LiveUpload {
    /// Sink for further request frames. Dropping it ends the upload.
    pub requests: mpsc::Sender<pb::ParseMarkupRequest>,
    /// The in-flight call, resolving to the response stream or the status
    /// that refused it.
    pub call: tokio::task::JoinHandle<
        Result<tonic::Response<tonic::Streaming<pb::ParseMarkupResponse>>, tonic::Status>,
    >,
}

impl LiveUpload {
    /// Open a call and send only the options frame.
    pub async fn start(
        client: &MarkupParseServiceClient<Channel>,
        options: pb::ParseOptions,
    ) -> Self {
        let mut client = client.clone();
        let (tx, rx) = mpsc::channel(4);
        tx.send(pb::ParseMarkupRequest {
            payload: Some(pb::parse_markup_request::Payload::Options(options)),
        })
        .await
        .expect("send options");
        let call = tokio::spawn(async move { client.parse_markup(ReceiverStream::new(rx)).await });
        Self { requests: tx, call }
    }

    /// Send one chunk of document bytes; false when the server has already
    /// hung up.
    pub async fn send(&self, chunk: &[u8]) -> bool {
        self.requests
            .send(pb::ParseMarkupRequest {
                payload: Some(pb::parse_markup_request::Payload::Chunk(chunk.to_vec())),
            })
            .await
            .is_ok()
    }
}

// ---------------------------------------------------------------- selectors

/// The kind of one event, for order assertions.
pub fn kind(event: &pb::ParseMarkupResponse) -> &'static str {
    match event.event.as_ref() {
        Some(pb::parse_markup_response::Event::Info(_)) => "info",
        Some(pb::parse_markup_response::Event::TextBlock(_)) => "text_block",
        Some(pb::parse_markup_response::Event::TableStart(_)) => "table_start",
        Some(pb::parse_markup_response::Event::TableRow(_)) => "table_row",
        Some(pb::parse_markup_response::Event::TableEnd(_)) => "table_end",
        Some(pb::parse_markup_response::Event::Image(_)) => "image",
        Some(pb::parse_markup_response::Event::Cue(_)) => "cue",
        Some(pb::parse_markup_response::Event::Document(_)) => "document",
        Some(pb::parse_markup_response::Event::Status(_)) => "status",
        None => "empty",
    }
}

/// Every `TextBlock` in a stream.
pub fn text_blocks(events: &[pb::ParseMarkupResponse]) -> Vec<&pb::TextBlock> {
    events
        .iter()
        .filter_map(|e| match e.event.as_ref() {
            Some(pb::parse_markup_response::Event::TextBlock(block)) => Some(block),
            _ => None,
        })
        .collect()
}

/// Every `TextBlock` carrying a given label.
pub fn blocks_labelled(
    events: &[pb::ParseMarkupResponse],
    label: pb::BlockLabel,
) -> Vec<&pb::TextBlock> {
    text_blocks(events)
        .into_iter()
        .filter(|block| block.label == label as i32)
        .collect()
}

/// The texts of a set of blocks, for golden comparison.
pub fn texts(blocks: &[&pb::TextBlock]) -> Vec<String> {
    blocks.iter().map(|b| b.text.clone()).collect()
}

/// Every `Cue` in a stream.
pub fn cues(events: &[pb::ParseMarkupResponse]) -> Vec<&pb::Cue> {
    events
        .iter()
        .filter_map(|e| match e.event.as_ref() {
            Some(pb::parse_markup_response::Event::Cue(cue)) => Some(cue),
            _ => None,
        })
        .collect()
}

/// Every `ImageBlock` in a stream.
pub fn images(events: &[pb::ParseMarkupResponse]) -> Vec<&pb::ImageBlock> {
    events
        .iter()
        .filter_map(|e| match e.event.as_ref() {
            Some(pb::parse_markup_response::Event::Image(image)) => Some(image),
            _ => None,
        })
        .collect()
}

/// Every `TableRow` in a stream.
pub fn table_rows(events: &[pb::ParseMarkupResponse]) -> Vec<&pb::TableRow> {
    events
        .iter()
        .filter_map(|e| match e.event.as_ref() {
            Some(pb::parse_markup_response::Event::TableRow(row)) => Some(row),
            _ => None,
        })
        .collect()
}

/// The `MarkupInfo` header of a stream.
pub fn info(events: &[pb::ParseMarkupResponse]) -> &pb::MarkupInfo {
    events
        .iter()
        .find_map(|e| match e.event.as_ref() {
            Some(pb::parse_markup_response::Event::Info(info)) => Some(info),
            _ => None,
        })
        .expect("stream has a MarkupInfo")
}

/// The scalar text of a metadata value; panics on a list or a map.
pub fn meta_scalar(value: &pb::MetaValue) -> &str {
    match value.kind.as_ref() {
        Some(pb::meta_value::Kind::Value(text)) => text,
        other => panic!("expected a scalar metadata value, got {other:?}"),
    }
}

/// The items of a metadata list; panics on a scalar or a map.
pub fn meta_list(value: &pb::MetaValue) -> &[pb::MetaValue] {
    match value.kind.as_ref() {
        Some(pb::meta_value::Kind::List(list)) => &list.items,
        other => panic!("expected a metadata list, got {other:?}"),
    }
}

/// The entries of a nested metadata map; panics on a scalar or a list.
pub fn meta_map(value: &pb::MetaValue) -> &std::collections::HashMap<String, pb::MetaValue> {
    match value.kind.as_ref() {
        Some(pb::meta_value::Kind::Map(map)) => &map.entries,
        other => panic!("expected a metadata map, got {other:?}"),
    }
}

/// The `ParseStatus` trailer of a stream.
pub fn status(events: &[pb::ParseMarkupResponse]) -> &pb::ParseStatus {
    events
        .iter()
        .find_map(|e| match e.event.as_ref() {
            Some(pb::parse_markup_response::Event::Status(status)) => Some(status),
            _ => None,
        })
        .expect("stream has a ParseStatus")
}

/// The `Document` event of a stream, when one was emitted.
pub fn document(
    events: &[pb::ParseMarkupResponse],
) -> Option<&grpc_markup::document::v1::Document> {
    events.iter().find_map(|e| match e.event.as_ref() {
        Some(pb::parse_markup_response::Event::Document(document)) => Some(document),
        _ => None,
    })
}

/// The warning of a given code, when the trailer carries one.
pub fn warning(
    events: &[pb::ParseMarkupResponse],
    code: pb::WarningCode,
) -> Option<&pb::ParseWarning> {
    status(events)
        .warnings
        .iter()
        .find(|w| w.code == code as i32)
}

/// The stream indexes of every indexed event, in stream order.
pub fn indexes(events: &[pb::ParseMarkupResponse]) -> Vec<u64> {
    events
        .iter()
        .filter_map(|e| match e.event.as_ref() {
            Some(pb::parse_markup_response::Event::TextBlock(b)) => Some(b.index),
            Some(pb::parse_markup_response::Event::TableStart(t)) => Some(t.index),
            Some(pb::parse_markup_response::Event::Image(i)) => Some(i.index),
            Some(pb::parse_markup_response::Event::Cue(c)) => Some(c.index),
            _ => None,
        })
        .collect()
}

// ----------------------------------------------------------------- fixtures

/// A current-format Box Note exercising headings, styled runs, nested and
/// check lists, code, a quote, a spanned table, an image, a whole-paragraph
/// link, and one unknown node type.
pub const BOXNOTE: &str = r#"{
  "title": "Meeting Notes",
  "doc": {
    "type": "doc",
    "content": [
      {"type": "heading", "attrs": {"level": 1},
       "content": [{"type": "text", "text": "Meeting Notes"}]},
      {"type": "paragraph",
       "content": [{"type": "text", "text": "Agreed to "},
                   {"type": "text", "text": "ship", "marks": [{"type": "strong"}]}]},
      {"type": "heading", "attrs": {"level": 2},
       "content": [{"type": "text", "text": "Decisions"}]},
      {"type": "bullet_list", "content": [
        {"type": "list_item", "content": [
          {"type": "paragraph", "content": [{"type": "text", "text": "first"}]}]},
        {"type": "list_item", "content": [
          {"type": "paragraph", "content": [{"type": "text", "text": "second"}]},
          {"type": "ordered_list", "content": [
            {"type": "list_item", "content": [
              {"type": "paragraph", "content": [{"type": "text", "text": "nested"}]}]}]}]}]},
      {"type": "check_list", "content": [
        {"type": "check_list_item", "attrs": {"checked": true}, "content": [
          {"type": "paragraph", "content": [{"type": "text", "text": "done thing"}]}]}]},
      {"type": "code_block", "attrs": {"language": "sql"},
       "content": [{"type": "text", "text": "SELECT 1;"}]},
      {"type": "blockquote", "content": [
        {"type": "paragraph", "content": [{"type": "text", "text": "a quote"}]}]},
      {"type": "table", "content": [
        {"type": "table_row", "content": [
          {"type": "table_header", "content": [
            {"type": "paragraph", "content": [{"type": "text", "text": "H1"}]}]},
          {"type": "table_header", "attrs": {"colspan": 2}, "content": [
            {"type": "paragraph", "content": [{"type": "text", "text": "H2"}]}]}]},
        {"type": "table_row", "content": [
          {"type": "table_cell", "content": [
            {"type": "paragraph", "content": [{"type": "text", "text": "a"}]}]},
          {"type": "table_cell", "content": [
            {"type": "paragraph", "content": [{"type": "text", "text": "b"}]}]},
          {"type": "table_cell", "content": [
            {"type": "paragraph", "content": [{"type": "text", "text": "c"}]}]}]}]},
      {"type": "image", "attrs": {"alt": "diagram", "fileName": "arch.png",
                                  "boxSharedLink": "https://app.box.com/s/abc"}},
      {"type": "paragraph", "content": [
        {"type": "text", "text": "See docs",
         "marks": [{"type": "link", "attrs": {"href": "https://example.com/docs"}}]}]},
      {"type": "mystery_widget"}
    ]
  }
}"#;

/// A serialized `DoclingDocument` with a title, a section, a list group, a
/// table, a picture, a code item, real page and provenance data, and one
/// key the mapping does not consume.
pub const DOCLING: &str = r##"{
  "schema_name": "DoclingDocument",
  "version": "1.3.0",
  "name": "Quarterly Review",
  "origin": {"mimetype": "application/pdf", "binary_hash": 42, "filename": "q.pdf"},
  "furniture": {"self_ref": "#/furniture", "children": [], "content_layer": "furniture",
                "name": "_root_", "label": "unspecified"},
  "body": {"self_ref": "#/body",
           "children": [{"$ref": "#/texts/0"}, {"$ref": "#/texts/1"}, {"$ref": "#/groups/0"},
                        {"$ref": "#/tables/0"}, {"$ref": "#/pictures/0"}, {"$ref": "#/texts/4"}],
           "content_layer": "body", "name": "_root_", "label": "unspecified"},
  "groups": [{"self_ref": "#/groups/0", "parent": {"$ref": "#/body"},
              "children": [{"$ref": "#/texts/2"}, {"$ref": "#/texts/3"}],
              "content_layer": "body", "name": "list", "label": "list"}],
  "texts": [
    {"self_ref": "#/texts/0", "parent": {"$ref": "#/body"}, "children": [],
     "content_layer": "body", "label": "title",
     "prov": [{"page_no": 1,
               "bbox": {"l": 10.0, "t": 20.0, "r": 100.0, "b": 30.0, "coord_origin": "TOPLEFT"},
               "charspan": [0, 16]}],
     "orig": "Quarterly Review", "text": "Quarterly Review"},
    {"self_ref": "#/texts/1", "parent": {"$ref": "#/body"}, "children": [],
     "content_layer": "body", "label": "section_header", "level": 1,
     "prov": [], "orig": "Summary", "text": "Summary"},
    {"self_ref": "#/texts/2", "parent": {"$ref": "#/groups/0"}, "children": [],
     "content_layer": "body", "label": "list_item", "enumerated": false, "marker": "-",
     "prov": [], "orig": "Revenue up", "text": "Revenue up"},
    {"self_ref": "#/texts/3", "parent": {"$ref": "#/groups/0"}, "children": [],
     "content_layer": "body", "label": "list_item", "enumerated": false, "marker": "-",
     "prov": [], "orig": "Costs flat", "text": "Costs flat"},
    {"self_ref": "#/texts/4", "parent": {"$ref": "#/body"}, "children": [],
     "content_layer": "body", "label": "code", "code_language": "python",
     "captions": [], "references": [], "footnotes": [],
     "prov": [], "orig": "print(1)", "text": "print(1)"}
  ],
  "tables": [
    {"self_ref": "#/tables/0", "parent": {"$ref": "#/body"}, "children": [],
     "content_layer": "body", "label": "table", "prov": [],
     "captions": [], "references": [], "footnotes": [],
     "data": {"num_rows": 2, "num_cols": 2,
       "table_cells": [],
       "grid": [
         [{"row_span": 1, "col_span": 1, "start_row_offset_idx": 0, "end_row_offset_idx": 1,
           "start_col_offset_idx": 0, "end_col_offset_idx": 1, "text": "Region",
           "column_header": true, "row_header": false, "row_section": false},
          {"row_span": 1, "col_span": 1, "start_row_offset_idx": 0, "end_row_offset_idx": 1,
           "start_col_offset_idx": 1, "end_col_offset_idx": 2, "text": "Delta",
           "column_header": true, "row_header": false, "row_section": false}],
         [{"row_span": 1, "col_span": 1, "start_row_offset_idx": 1, "end_row_offset_idx": 2,
           "start_col_offset_idx": 0, "end_col_offset_idx": 1, "text": "NA",
           "column_header": false, "row_header": false, "row_section": false},
          {"row_span": 1, "col_span": 1, "start_row_offset_idx": 1, "end_row_offset_idx": 2,
           "start_col_offset_idx": 1, "end_col_offset_idx": 2, "text": "+12%",
           "column_header": false, "row_header": false, "row_section": false}]]}}
  ],
  "pictures": [
    {"self_ref": "#/pictures/0", "parent": {"$ref": "#/body"}, "children": [],
     "content_layer": "body", "label": "picture", "prov": [],
     "captions": [], "references": [], "footnotes": [],
     "image": {"mimetype": "image/png", "dpi": 72,
               "size": {"width": 64.0, "height": 48.0}, "uri": "img/chart.png"},
     "annotations": [{"kind": "description", "text": "a chart", "provenance": "model"}]}
  ],
  "pages": {"1": {"size": {"width": 612.0, "height": 792.0}, "page_no": 1}}
}"##;
