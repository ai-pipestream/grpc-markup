# AGENTS.md — grpc-markup

**Status: all seven formats in the contract are implemented.** `MARKDOWN`,
`VTT`, `BOXNOTE`, `DOCLING_JSON`, `HTML`, `ASCIIDOC` and `LATEX` all parse.
`UNIMPLEMENTED` remains the response for any format outside the registry
(there is none today) and for failed sniffs. See `README.md` for what the
server does today and `docs/design.md` for the mappings.

## Read this first, in order

1. This file
2. `proto/ai/pipestream/markup/v1/` — the contract. It is fixed; the
   comments are promises the implementation keeps, not documentation of it.
3. `docs/design.md` — format mappings, projection losses, sniff precedence
4. `README.md` — build, run, wire API, environment

Sibling repos set the conventions this one follows: `grpc-xml` (the same
bidi options+chunks/events protocol, main.rs env knobs, service shape,
tests layout) and `grpc-epub` (the document-fold doctrine: dense local
refs, reciprocal parent/children, `CollectorSource` on every item,
format-prefixed `meta.custom_fields`, an `integrity_errors` checker every
fold test asserts).

## This service

gRPC collector for lightweight text markup, projecting into the gRParse
Document data plane.

- **Language:** Rust (tonic + pulldown-cmark + serde_json). No pandoc, no
  headless browser, no Python.
- **Stack:** One process, format enum on the request, sniff if unspecified.
  Nothing fetched, nothing written to disk, embedded HTML flattened never
  parsed.
- **Stream:** MarkupInfo first, content events in document order, the
  optional Document immediately before ParseStatus, ParseStatus last.
- **Extension point:** one fold module per format plus one entry in
  `parse::REGISTRY`. Do not add per-format branches anywhere else;
  `GetServiceInfo.formats`, dispatch and the `UNIMPLEMENTED` refusals all
  derive from the registry.

## Rules that outrank convenience

- The contract is fixed. A change to `proto/` is a fleet decision, not a
  local one, and the vendored `document.proto` is never edited here.
- Error taxonomy: `INVALID_ARGUMENT` for input the format's own rules
  cannot repair, `UNIMPLEMENTED` for formats outside the build and failed
  sniffs, `RESOURCE_EXHAUSTED` for the byte cap, the admission limit and
  the memory budget, `DEADLINE_EXCEEDED` for stalled uploads and parses
  past their deadline, `INTERNAL` for our own faults. The mapping lives in `service::status_for`
  and nowhere else.
- Warnings aggregate per (code, message) and never fail a parse.
- The sniff never guesses between two candidates. An explicit request wins
  outright; signatures beat heuristics; ambiguity fails closed.
- Fold tests assert `document_fold::integrity_errors` is empty. A fragment
  that fails it corrupts a merged document the fold never sees.
- Fixtures are string literals in the tests. No corpus in git, no fixture
  files on disk, no network in tests.

## Definition of done

ParseMarkup stream over a real socket, seven format folds with golden
fixtures, framing/cap/admission tests, sniff evidence tests, Document fold
integrity per format, buf-lint-clean proto, health+reflection, read-only
image. `cargo fmt --check`, `cargo clippy --all-targets` (no warnings) and
`cargo test` all green.

## Workspace

Checkout path: `/work/main/grpc-services/grpc-markup`.
Commit messages: plain prose, no em dashes (a hook rejects them). Do not
push without being asked.
