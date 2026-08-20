# grpc-markup

gRPC collector for lightweight text markup — Markdown, HTML, AsciiDoc,
LaTeX, WebVTT, BoxNote and serialized Docling JSON — projecting into the
gRParse Document data plane.

One Rust process maps every format onto one shared block vocabulary: a
heading is the same `TextBlock` whether it was `# Title` or a BoxNote
heading node, a table streams as `TableStart` / `TableRow` / `TableEnd`
whichever source wrote it, and an optional fold turns the whole parse into
one `ai.pipestream.document.v1.Document` a coordinator can merge with
another collector's parse of the same work. LaTeX is in the contract but
not in this build; asking for it is `UNIMPLEMENTED` with the format named.
It is not PipeStream core.

## Build and run

```bash
cargo build --release          # build.rs compiles proto/ with tonic-prost-build
cargo test                     # unit + integration; no network, no fixtures on disk
cargo clippy --all-targets
cargo fmt --check
buf lint

./target/release/grpc-markup   # listens on 0.0.0.0:50065
```

Container, with the tests gating the image and a read-only root filesystem:

```bash
docker build -t grpc-markup .
docker run --rm --read-only -p 50065:50065 grpc-markup
```

Poke at it with reflection, no local protos required:

```bash
grpcurl -plaintext localhost:50065 list
grpcurl -plaintext localhost:50065 ai.pipestream.markup.v1.MarkupParseService/GetServiceInfo
grpcurl -plaintext localhost:50065 grpc.health.v1.Health/Check
```

`protoc` is required to build (the build script invokes it through
tonic-prost-build). `buf` is only needed to lint the contract.

## Wire API

Package `ai.pipestream.markup.v1`, service `MarkupParseService`, contract in
[`proto/ai/pipestream/markup/v1`](proto/ai/pipestream/markup/v1). Every
message, field, enum value and RPC carries a documentation comment; `buf
lint` runs `STANDARD` + `COMMENTS` with comment ignores disallowed.

```text
rpc ParseMarkup(stream ParseMarkupRequest) returns (stream ParseMarkupResponse);
rpc GetServiceInfo(GetServiceInfoRequest) returns (GetServiceInfoResponse);
```

`GetServiceInfo` also carries a `ui` block (`title`, `path`, `description`)
advertising this service's tab in the shared demo shell.

**Request.** The first frame sets `options`; every frame after it carries a
`chunk` of document bytes, concatenated in stream order. `options` twice, or
a frame with neither field, is `INVALID_ARGUMENT`.

| Option | Meaning |
|---|---|
| `format` | `MARKDOWN` / `HTML` / `ASCIIDOC` / `LATEX` / `VTT` / `BOXNOTE` / `DOCLING_JSON`, or unset to sniff |
| `max_document_mib` | Per-request byte cap; 0 takes the server default, over the ceiling clamps |
| `emit_document` | Also fold the parse into one `ai.pipestream.document.v1.Document`, sent just before the trailer (see below) |

**Response.** Exactly one `info` first, content events in document order,
exactly one `status` last. Stream indexes are dense across every indexed
event kind.

| Event | Carries |
|---|---|
| `info` | `MarkupInfo`: resolved format and the evidence for it, declared title, document metadata as a typed `MetaValue` tree holding the shape the source declared (front matter as its own tree, header lines as scalars) |
| `text_block` | One unit of text: title, section header, paragraph, list item, code, formula, caption, quote, footnote. `role` is an open refinement (`task-item`, `html`, a footnote label) |
| `table_start` / `table_row` / `table_end` | A table, streamed a row at a time, header rows and cells flagged, spans carried |
| `image` | One image reference — `src`, `alt`, `title` as written. Never dereferenced |
| `cue` | One WebVTT cue: identifier, timings in milliseconds, speaker, flattened text |
| `document` | The whole parse folded into one `Document`. Only when `emit_document` was set, exactly once, immediately before `status` |
| `status` | `ParseStatus`: format, counts, aggregated warnings, bytes consumed, elapsed |

Every item carries a `CollectorSource` (`collector = "markup"`, `model` =
the fold: `markdown`, `vtt`, `boxnote`, `docling-json`) and, where the
format is line-oriented, a 1-based `source_line`.

**Errors.** A failed parse ends the stream with a status and no `status`
event:

| Condition | Code |
|---|---|
| Over the byte cap (refused on the crossing chunk, mid-upload), or past the concurrency limit | `RESOURCE_EXHAUSTED` |
| Input the format's own error rules cannot repair: a VTT without its header, bytes that are not the JSON a JSON format requires, a legacy `atext` BoxNote | `INVALID_ARGUMENT` |
| A format outside the build (none today; the registry, not the enum, decides), or a sniff that found nothing | `UNIMPLEMENTED` |
| A parser fault | `INTERNAL` |

Warnings never fail a parse. They aggregate per (code, message) on the
trailer, so a document with ten thousand instances of one finding produces
one warning with a count.

## Web demo

[`demos/node-client`](demos/node-client) is a dependency-light node:http
bridge (only `@grpc/grpc-js` and `@grpc/proto-loader`) plus a live viewer
page: pick a format or let the server sniff it, upload or paste markup, and
watch the shared block vocabulary stream back over Server-Sent Events on the
same response that carries the upload up.

```bash
cd demos/node-client
npm install
npm start                              # http://127.0.0.1:8088
UI_BASE=/ui/markup npm start           # mounted the way the demo shell does
```

The bridge honours `MARKUP_ADDR` (default `127.0.0.1:50065`), `PORT` (default
`8088`) and `UI_BASE`. Samples for every mapped format live in
[`demos/sample-data`](demos/sample-data); they mirror the golden fixtures the
test suite pins as string literals.

## Format coverage

| Format | Sniffed by | Mapped |
|---|---|---|
| Markdown | Heuristic only (ATX headings, fences, links, front matter) | headings (first H1 = title), paragraphs, nested lists with markers as written, task lists, fenced/indented code with language, blockquotes, pipe tables, images, footnotes, display math, YAML front matter to metadata; raw HTML flattened to text with a warning |
| WebVTT | `WEBVTT` signature | header title and `Key: value` metadata, cues with millisecond timings, voice spans to `speaker`, styling and karaoke tags stripped, NOTE/STYLE/REGION skipped, malformed cues skipped with a warning |
| BoxNote | JSON with a ProseMirror `doc` tree (or legacy `atext`, refused) | headings, styled paragraphs, nested/check lists, code, quotes, spanned tables, images, safe-scheme links |
| Docling JSON | JSON whose `schema_name` names a DoclingDocument | field-by-field re-ingestion of the core arrays; typed events are a projection of the body in reading order; unconsumed keys warn per key path |
| HTML | `<!DOCTYPE html` or `<html` signature | whole documents and body-only fragments (html5ever); head title and `meta.*` metadata, h1-h6 ladder (first h1 = title), paragraphs, blockquotes, ul/ol/dl nesting, spanned tables with captions, `pre` code with `language-*` classes, figures and images, whole-block links; script/style/head never become text; tree-builder repairs warn as malformed structure |
| AsciiDoc | Heuristic only (`= ` title, `== ` sections, `:name: value` entries, `----` fences) | attribute entries to metadata, `= `/`== ` title and section ladder (level is marker length minus one), multiline paragraphs, admonitions as `role: "admonition"`, `*`/`.` lists nested by marker depth with markers as written, `|===` tables with the first row as header and cell specifiers stripped, `[source,lang]` + `----` listings as code, `image::` macros with `.Caption` lines |
| LaTeX | `\documentclass` or `\begin{document}` | preamble skipped with `\title`/`\author`/`\date` surfaced, section ladder (part/chapter/section 1 through subparagraph 5), inline commands flattened (`\cite`/`\ref` to `[key]`), itemize/enumerate/description nesting, tabular with `\multicolumn`/`\multirow` spans and rules dropped, verbatim/lstlisting/tikz as code, equation family and `$$`/`\[..\]` as formulas, `\includegraphics` by reference, quotes, theorems, bibliographies; unknown environments descend with one aggregated warning |

Sniffing order is the one [`docs/design.md`](docs/design.md) fixes: an
explicit request wins outright, then unambiguous signatures, and only then a
Markdown/AsciiDoc structure heuristic. The server never guesses between two
candidates it cannot separate — bytes that match both fail closed.

## The Document projection (opt-in)

Set `emit_document` and the server folds its own event stream into one
`ai.pipestream.document.v1.Document` and sends it as a `document` event
immediately before the trailer. The typed events still go out first,
unchanged and in order. With the option off, the fold is never constructed.

The fold is [`src/document_fold.rs`](src/document_fold.rs) — a standalone,
directly-testable module. The schema is vendored byte-identical from the
gRParse repository into
[`proto/ai/pipestream/document/v1`](proto/ai/pipestream/document/v1) and is
never edited here.

| Wire event | Document |
|---|---|
| `info` | `name` (the declared title), `origin.mimetype` per format, metadata on the body meta under the fold's prefix (`markdown.author`, …); the `MetaValue` tree converts recursively — scalar to string, list to `ListValue`, map to `Struct` |
| `text_block` | A `BaseTextItem` variant chosen by label — `TitleItem`, `SectionHeaderItem`, `ListItem`, `CodeItem`, `FormulaItem`, else `TextItem`. Quotes fold as `TEXT` with the quotation recorded in custom fields; a declared code language maps onto `CodeLanguageLabel` when it matches an enum name, else `code_language_raw` |
| `text_block` list items | Grouped under `GROUP_LABEL_LIST` / `GROUP_LABEL_ORDERED_LIST` groups, one per contiguous list, nested lists as nested groups |
| `table_*` | One `TableItem`: both `grid` and flat `table_cells`, offsets honoring spans, a leading caption created as a `CAPTION` item and referenced |
| `image` | A `PictureItem` whose `ImageRef.uri` is the `src` verbatim — no bytes, no size, no dpi |
| `cue` | A `TextItem` whose provenance is a `TrackSource` (timings in seconds, identifier, voice) alongside the collector stamp |
| `status` | Nothing; it describes the stream, not the document |

For `DOCLING_JSON` the `document` event is not a re-fold: the ingested
document is carried over item for item, with this collector's
`CollectorSource` appended to each item's sources (attribution is additive;
sources never overwrite each other).

Structure is a heading-as-parent ladder: a section header of level N is parented
to the nearest open header of a lower level (`#/body` when there is none)
and the content after it hangs off that header. Refs are dense and local
(`#/texts/0`, `#/tables/1`) with `parent` and `children` reciprocal, which
is what lets the coordinator merge the fragment additively;
`document_fold::integrity_errors` is that contract as a check, and every
fold test asserts it is empty. Markup has no pages and no boxes, so no item
carries `prov` — except cues, whose track timing is real provenance, and
ingested Docling JSON payloads, whose provenance passes through untouched.

## Security and bounds

- **Diskless.** Document bytes are buffered in memory — these parsers need
  the whole text, unlike a pull parser — and nothing is ever written. The
  image runs `--read-only` with no tmpfs.
- **Nothing is fetched.** Image references, links and URIs stay the strings
  the source wrote. Embedded HTML inside Markdown is still flattened to its
  text; standalone HTML parses without fetching a single subresource.
- **Bounded.** The byte cap is enforced on the chunk that crosses it, while
  the client is still uploading; concurrent parses are capped and refused
  rather than queued; the response channel is small, so a client that stops
  reading stops the parse.
- **Bounded trailer.** Warnings aggregate per (code, message) and the table
  of distinct kinds is capped, so a hostile document cannot grow the trailer
  without bound.

## Environment

| Variable | Default | Meaning |
|---|---|---|
| `GRPC_MARKUP_ADDR` | `0.0.0.0:50065` | Listen address |
| `GRPC_MARKUP_WORKERS` | CPU count | Tokio worker threads |
| `GRPC_MARKUP_BLOCKING_THREADS` | 512 | Blocking pool that runs the parsers |
| `GRPC_MARKUP_MAX_CONCURRENT_PARSES` | 64 | Parses admitted at once; past it, `RESOURCE_EXHAUSTED` |
| `GRPC_MARKUP_MAX_DOCUMENT_MIB` | 256 | Byte cap when a request asks for 0 |
| `GRPC_MARKUP_MAX_DOCUMENT_MIB_CEILING` | 1024 | Hard cap a request cannot exceed |
| `GRPC_MARKUP_METRICS_INTERVAL_SECS` | 60 | Seconds between metrics lines; 0 disables |
| `GRPC_MARKUP_WINDOW_BYTES` | 16 MiB | HTTP/2 initial stream and connection window |

Metrics are a line on stdout on that interval — parses started, ok, failed,
refused, capped, bytes in, events out, and a per-format count. The counters
live in [`src/metrics.rs`](src/metrics.rs) if a Prometheus endpoint is ever
wanted.

## Layout

```text
proto/ai/pipestream/markup/v1/    the contract; buf lint STANDARD + COMMENTS
proto/ai/pipestream/document/v1/  the Document plane, vendored from gRParse
src/sniff.rs                      format resolution and its evidence
src/parse.rs                      the shared driver: emitter, envelope, fold registry
src/markdown.rs                   the Markdown fold (pulldown-cmark)
src/html.rs                       the HTML fold (html5ever via scraper)
src/asciidoc.rs                   the AsciiDoc fold (line-oriented)
src/latex.rs                      the LaTeX fold (hand-rolled tolerant reader)
src/vtt.rs                        the WebVTT fold (hand-written cue parser)
src/boxnote.rs                    the BoxNote fold (doc-tree walk)
src/docling_json.rs               the Docling JSON fold (field-by-field ingestion)
src/document_fold.rs              the opt-in fold from events to one Document
src/service.rs                    tonic wiring, framing, byte cap, admission control
src/metrics.rs                    counters and the interval line
demos/node-client/                the web viewer: node:http SSE bridge plus page
demos/sample-data/                one sample per format, mirroring the test literals
tests/markdown.rs                 golden mappings for the Markdown fold
tests/html.rs                     golden mappings for the HTML fold
tests/asciidoc.rs                 golden mappings for the AsciiDoc fold
tests/latex.rs                    golden mappings for the LaTeX fold
tests/vtt.rs                      golden mappings for the WebVTT fold
tests/json_formats.rs             golden mappings for BoxNote and Docling JSON
tests/document.rs                 the Document fold per format, placement, integrity
tests/server.rs                   framing, caps, admission, sniffing, refusals, info
```

Adding a format is one fold module plus one entry in `parse::REGISTRY`;
`GetServiceInfo.formats`, the dispatch and the `UNIMPLEMENTED` refusals all
follow from the registry.

## Docs

- [AGENTS.md](AGENTS.md) — read order, definition of done, git
- [Design](docs/design.md) — format mappings, what the projection drops, sniff precedence
