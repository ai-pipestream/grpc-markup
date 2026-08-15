# grpc-markup design

## 1. Goals

- One block vocabulary for lightweight text markup: Markdown, HTML,
  AsciiDoc, LaTeX, WebVTT, BoxNote and serialized Docling JSON — every
  format the contract names.
- One process, one port, format selected on the request, sniffed when
  unspecified.
- Typed events in document order as each fold produces them, and an
  identical `Document` projection gRParse can merge with another
  collector's parse of the same work.
- Feature parity in intent with docling's `md_backend`, `webvtt_backend`
  and `boxnote_backend`, minus the inline formatting model this wire
  deliberately flattens away.

## 2. Non-goals (this build)

- Parsing HTML embedded in another format: raw HTML inside Markdown is
  still flattened to its text with `WARNING_CODE_EMBEDDED_HTML_FLATTENED`.
  Standalone HTML has its own fold.
- Rendering HTML: no browser, no CSS, no visibility computation, no form
  extraction — the parts of docling's HTML backend that need a rendered
  page are out of scope by construction.
- Fetching anything: images, links and URIs stay the strings the source
  wrote.
- Rendering, syntax highlighting, or language detection beyond what the
  source declares.
- Inline formatting (bold, italic) on the typed wire: blocks are flattened
  text. The Docling JSON fold preserves `formatting` on ingested items
  because there it is data, not interpretation.

## 3. The fold registry

`parse::REGISTRY` is the single list of formats this build maps: one
`(Format, FoldFn)` pair per format. A fold is a plain function from the
whole document text to calls on the `Emitter`, which owns everything the
contract promises about the stream — exactly one `info` first, dense
indexes across every indexed event kind, warnings aggregated per
(code, message) with a bounded table, the optional `document` immediately
before `status`, `status` last exactly once.

Adding a format is one module plus one registry entry.
`GetServiceInfo.formats`, dispatch, per-format metrics slots and the
`UNIMPLEMENTED` refusals all derive from the registry; nothing else in the
tree branches on format.

## 4. Format mappings

### 4.1 Markdown (`pulldown-cmark`, CommonMark + tables, footnotes, task lists, strikethrough, math)

| Source construct | Wire |
|---|---|
| YAML front matter between leading `---` fences | `MarkupInfo.metadata` as the tree the source declared: scalars verbatim, sequences as `MetaList`, nested mappings as `MetaMap`, keys the mapping's own; `title:` also to `MarkupInfo.title` when it is a non-empty scalar. Unparseable front matter: one `WARNING_CODE_METADATA`, raw text not emitted as content |
| First H1 | `BLOCK_LABEL_TITLE` |
| Every other heading | `BLOCK_LABEL_SECTION_HEADER` with the Markdown level |
| Paragraph | `BLOCK_LABEL_PARAGRAPH`, inline markup flattened, whitespace collapsed; `hyperlink` when the whole paragraph is one link |
| List item | `BLOCK_LABEL_LIST_ITEM` with `enumerated`, the marker as written (read back from the source bytes), `list_depth`; task-list checkbox in `role` (`task-item` / `task-item-checked`) |
| Fenced / indented code | `BLOCK_LABEL_CODE`, text verbatim, info-string first word as `language` |
| Blockquote | `BLOCK_LABEL_QUOTE`, one block per paragraph, nesting flattened |
| Pipe table | `TableStart` / `TableRow` (header row and cells flagged) / `TableEnd` |
| Image | `ImageBlock` with `src`, `alt`, `title` as written |
| Footnote definition | `BLOCK_LABEL_FOOTNOTE`, `role` = the label; references stay in the referring text as `[label]` |
| Display math | `BLOCK_LABEL_FORMULA`, verbatim |
| HTML block / inline HTML | flattened to text (`role: "html"` for blocks) + `WARNING_CODE_EMBEDDED_HTML_FLATTENED` |
| Thematic break | nothing + `WARNING_CODE_UNMAPPED_CONSTRUCT` |

`source_line` on every block comes from the offset iterator, mapped to
1-based lines of the original file, front matter included.

### 4.2 HTML (`scraper` / html5ever)

Ports the mapping decisions of docling's `html_backend.py` — which elements
become items — without its rendering, bounding-box and form machinery.

| Source construct | Wire |
|---|---|
| `<title>`, `<meta name content>` | `MarkupInfo.title`, scalar `MarkupInfo.metadata` values under `meta.*` keys |
| `h1`..`h6` | first `h1` is `BLOCK_LABEL_TITLE` (the Markdown fold's rule); every other heading `BLOCK_LABEL_SECTION_HEADER` at its own level |
| `p`, loose inline text between blocks | `BLOCK_LABEL_PARAGRAPH`, inline markup flattened, whitespace collapsed; a block that is entirely one `<a>` carries `hyperlink` |
| `address`, `summary` | paragraphs with the element name as `role` |
| `blockquote` | `BLOCK_LABEL_QUOTE` per paragraph, nesting flattened |
| `ul` / `ol` / `li` | `BLOCK_LABEL_LIST_ITEM` with nesting depth; `enumerated` for `ol`; a marker is written only when the source numbered the list (`ol start`), as docling writes it |
| `dl` / `dt` / `dd` | list items with the proto's `definition-term` / `definition-description` roles, descriptions one level under their term |
| `table` (+`thead`/`tbody`/`tfoot`, `caption`, `colspan`/`rowspan`) | streamed table with the caption on `TableStart`, header cells and rows flagged, spans advancing the column index over an occupancy grid |
| `pre` (+`language-*` class) | `BLOCK_LABEL_CODE`, text verbatim |
| `img`, `figure`/`figcaption` | `ImageBlock` (`src`/`alt`/`title` verbatim), the figcaption as a following `CAPTION` block |
| `details` | descended; its `summary` keeps its role |
| `script`, `style`, `head`, `template`, form controls, media embeds | never text |
| `hr` | nothing + `WARNING_CODE_UNMAPPED_CONSTRUCT` |
| any element without a mapping | text flattened into the enclosing block + one aggregated `WARNING_CODE_UNMAPPED_CONSTRUCT` per element name |

`WARNING_CODE_MALFORMED_STRUCTURE` comes only from html5ever's own
tree-builder error list — repairs the parser actually made — with the
missing-doctype complaint excluded, because a body-only fragment is a legal
input. Deliberately not ported from docling: browser rendering, bounding
boxes, form/key-value extraction, checkbox recognition, rich table cells
(nested structure inside a cell flattens to its text), the `<inline-formula>`
JATS-ism, and image fetching.

### 4.3 AsciiDoc (line-oriented, docling parity)

A parity fold: it mirrors docling's `asciidoc_backend.py` — a line
machine, not an AsciiDoc implementation — plus the three constructs the
fleet needs that docling skips (listing blocks, attribute entries,
admonitions).

| Source construct | Wire |
|---|---|
| `:name: value` attribute entries | `MarkupInfo.metadata`, gathered wherever they appear |
| `= Title` | `BLOCK_LABEL_TITLE` (first; later ones level-1 headers) |
| `== `..`====== ` | `BLOCK_LABEL_SECTION_HEADER` at marker length minus one — docling's own level rule |
| consecutive non-empty lines | one `BLOCK_LABEL_PARAGRAPH`, joined with spaces, flushed at a blank line |
| `NOTE:` / `TIP:` / `IMPORTANT:` / `WARNING:` / `CAUTION:` paragraphs | `role: "admonition"`, the label stripped |
| `*`/`**`/`.`/`..`/`-`/`1.` items | `BLOCK_LABEL_LIST_ITEM`, nesting by marker repetition, `enumerated` for dot and numbered markers, marker verbatim |
| `|===` blocks | streamed table; each `|` line is a row, cells split on `|`, docling's cell-specifier grammar (`2+`, `^.^h`) stripped, spans **not** expanded, first row flagged as header; a preceding `.Caption` lands on `TableStart` |
| `[source,lang]` + `----` fences | `BLOCK_LABEL_CODE`, verbatim, with the declared language; an unclosed fence keeps its content and warns `MALFORMED_STRUCTURE` |
| `image::path[alt, k=v]` | `ImageBlock` with the path verbatim (docling's `file:` URI mangling is deliberately not ported — this service never dereferences); a pending `.Caption` follows as a `CAPTION` block |

Deliberately skipped, matching docling's thin coverage where the format is
richer: block quotes (`____`), sidebars (`****`), literal blocks (`....`),
description lists, cell spans and per-cell styles in tables (specifiers are
stripped, not honored), includes and conditional directives, inline
formatting, and comment lines (`//` lines join paragraphs as text, exactly
as they do in docling). Two small divergences from docling, both kinder to
the author: nesting follows marker depth (`**`) rather than leading
indentation, and single-cell `|` rows inside `|===` are kept (docling's
two-pipe regex drops them).

### 4.4 LaTeX (hand-rolled tolerant reader)

pylatexenc has no Rust counterpart, so the node model — text runs, groups,
macros with signature-driven arguments, environments, math — is built
here, and the mapping ports what docling's `latex` backend maps.

| Source construct | Wire |
|---|---|
| preamble | skipped; `\title` surfaces as `MarkupInfo.title` and a `BLOCK_LABEL_TITLE` block, `\author`/`\date` as paragraphs with their macro name as `role` |
| `\section` family | `BLOCK_LABEL_SECTION_HEADER` at docling's levels (part/chapter/section 1, subsection 2, subsubsection 3, paragraph 4, subparagraph 5) |
| text runs | paragraphs split at blank lines; `\textbf` and friends flatten, `\cite`/`\ref` become `[key]`, `\href{u}{t}` becomes `[t](u)`, `~` a space, `\%` and friends their character; `%` comments stripped |
| `itemize`/`enumerate`/`description` | nested `BLOCK_LABEL_LIST_ITEM`s, `enumerated` for enumerate, description terms as `term: ` lead-ins |
| `tabular` | table events: `&` splits cells, `\\` ends rows, rules dropped, `\multicolumn` and `\multirow` as spans |
| `verbatim`/`lstlisting`/`minted` | `BLOCK_LABEL_CODE`, verbatim, `[language=x]` as the declared language; `tikzpicture` as code declared `tikz` (no renderer here — docling's own no-engine fallback) |
| equation family, `$$..$$`, `\[..\]` | `BLOCK_LABEL_FORMULA` with `\label{...}` removed; `$..$` stays inline |
| `\includegraphics` | `ImageBlock`, path verbatim; `\caption` a `CAPTION` block; `\footnote`/`\marginpar` `FOOTNOTE` blocks |
| `quote`/`quotation`/`verse` | `BLOCK_LABEL_QUOTE` paragraphs |
| `abstract`, theorems, `proof`, `thebibliography` | an `Abstract` header with `role: "abstract"`, a `Theorem.`-style lead paragraph with the environment as `role`, a `References` header plus one `[key]`-led list item per `\bibitem` |
| unknown environments | descended into, one aggregated `WARNING_CODE_UNMAPPED_CONSTRUCT` per environment name |
| `\input`/`\include` | never read (diskless); one aggregated warning names the refusal |

Zero-argument `\newcommand` macros expand one level, as docling expands
them; parameterized custom macros are deliberately not expanded (docling
substitutes `#N` textually, which this fold declines to guess at). Also
deliberate: theorem lead-ins are plain text with a `role`, not docling's
`**Theorem.**` markdown-in-a-string; `\bibliography{file}` is consumed
silently rather than leaking its filename as text; accent macros drop the
accent and keep the base letter; no `source_line` (the reader is not
line-oriented and inventing lines would be guessing).

### 4.5 WebVTT (hand-written parser)

Mirrors `webvtt_backend.py` in effect: the `WEBVTT` header is mandatory
(`INVALID_ARGUMENT` without it, empty input included), header title and
`Key: value` lines land on `MarkupInfo`, `NOTE` / `STYLE` / `REGION` blocks
are skipped, and each cue becomes one `Cue` event: identifier, timings in
milliseconds (strict `mm:ss.ttt` grammar, optional hours), first `<v>`
annotation as `speaker`, payload with voice / styling / class / karaoke
timestamp tags stripped and lines joined with `\n`. A malformed cue — no
timings line, unparseable timestamps, end before start — is skipped with
one aggregated `WARNING_CODE_MALFORMED_STRUCTURE`, which is WebVTT's own
error recovery.

### 4.6 BoxNote (ProseMirror `doc` tree)

Mirrors `boxnote_backend.py` node for node: headings (first level-1 is the
title), paragraphs from styled runs (a single run keeps its safe-scheme
hyperlink, a mixed span flattens), bullet / ordered / check lists (checkbox
in `role`; BoxNote writes no markers so `marker` stays absent), code
blocks, blockquotes, spanned tables (`rowspan` / `colspan` advance the
column index over the occupied grid), images from the node's own attributes
(`boxSharedLink`, then `src`, then `fileName`), and wrappers
(`call_out_box`) recursed rather than dropped. Legacy `atext` notes are
refused as `INVALID_ARGUMENT`, exactly as the docling backend refuses them.
An unknown node type with nothing inside warns `UNMAPPED_CONSTRUCT` naming
it.

### 4.7 Docling JSON (field-by-field ingestion)

The vendored `document.proto` mirrors docling-core's `docling_document` v2
schema, so ingestion is a mapping, not an interpretation: `schema_name`,
`version`, `name`, `origin`, `body`, `furniture`, `groups`, `texts`
(variant chosen by the docling `label`), `tables` (cells with span
offsets, both `grid` and `table_cells`), `pictures` (with `ImageRef`),
`pages`, plus `prov` (page, bbox, charspan), `formatting`, `hyperlink` and
the whole `self_ref` / `parent` / `children` wiring. Identity is strict —
not JSON, or `schema_name` not naming a DoclingDocument, is
`INVALID_ARGUMENT` — while unknown labels and layers degrade to defaults
with a warning.

Every key the mapping does not consume produces one aggregated
`WARNING_CODE_UNMAPPED_CONSTRUCT` per key path, array indexes normalized
(`$.pictures[].annotations`), with a count.

The typed events for this format are a projection: the body is walked in
reading order and its items re-emitted as `TextBlock` / table / `ImageBlock`
events (list depth from the group nesting, table captions resolved onto
`TableStart`). The `document` event is the point of the format: the
ingested document carried over item for item, with this collector's
`CollectorSource` appended to each item's sources.

## 5. The Document projection, and what it drops

The fold (`src/document_fold.rs`) consumes this service's own emitted
events — grpc-epub's pattern — and produces a merge-safe fragment: dense
local refs, reciprocal `parent` / `children`, `CollectorSource{collector:
"markup", model: <fold>, version}` on every item, format-native facts in
`meta.custom_fields` under the fold's prefix. `integrity_errors` is the
merge contract as a check; every fold test asserts it is empty.

Structure: docling's heading-as-parent ladder, plus one
`GROUP_LABEL_LIST` / `GROUP_LABEL_ORDERED_LIST` group per contiguous list
with nested lists as nested groups.

Dropped or demoted per format, deliberately:

- **All formats:** `ParseStatus` (describes the stream, not the document);
  warnings; `evidence`. A block's `role` and `source_line` survive only as
  custom fields.
- **HTML:** everything docling derives from a rendered page (provenance
  boxes, visibility, forms) never existed here; nested structure inside a
  table cell survives only as the cell's flattened text; `hr` and unmapped
  elements survive only as warnings.
- **AsciiDoc:** table cell specifiers vanish (they are neither spans nor
  styles downstream); admonition kinds collapse into one `admonition` role;
  attribute entries live only on the body meta.
- **LaTeX:** everything TeX (macro definitions, spacing, counters, page
  furniture) dissolves; labels vanish once their formula is cleaned; tikz
  survives only as its source; description-term structure survives only as
  a `term: ` prefix.
- **Markdown:** the distinction between quote and plain text is demoted to
  a custom field (`DocItemLabel` has no quote); an unknown code language
  survives only as `code_language_raw`; image `alt` / `title` become custom
  fields, not caption items; thematic breaks and the raw text of HTML tags
  are gone entirely (warned on the trailer).
- **VTT:** cue order is document order but timings live only in the
  `TrackSource`; skipped NOTE/STYLE/REGION content never reaches the
  document.
- **BoxNote:** checkbox state is a custom-field `role`, not a
  `CHECKBOX_SELECTED` item; inline formatting is flattened away.
- **Docling JSON:** nothing is re-folded — the ingested document passes
  through with stamps appended. What the *mapping* dropped (unconsumed
  keys: annotations, key-value items, form items) is exactly what the
  per-key-path warnings name.

## 6. Sniffing

Precedence, fixed:

1. **Explicit request wins outright.** The sniff never runs.
2. **Signatures:** the `WEBVTT` header (optional BOM already stripped); a
   JSON object whose `schema_name` mentions docling (`DOCLING_JSON`) or
   with a ProseMirror `doc` tree / legacy `atext` key (`BOXNOTE`);
   `<!DOCTYPE html` or `<html` (`HTML`); `\documentclass` or
   `\begin{document}` (`LATEX`). Every signature format is mapped by this
   build; if a registry entry were ever removed, the signature would still
   match and the refusal would name what was sniffed.
3. **Heuristics, Markdown and AsciiDoc only:** ATX headings, fences, inline
   links or front matter say Markdown; a `= ` document title, `== `
   sections, `:name: value` attribute entries or `----` fences say
   AsciiDoc. Both at once, or neither, is `UNIMPLEMENTED` — the server
   never guesses between candidates it cannot separate, and plain prose is
   not blessed as Markdown by default.

Bytes that merely start like JSON but do not parse are not a signature;
they fall through to the heuristics and, in practice, to the refusal.

`MarkupInfo.evidence` reports which rung matched (`REQUESTED`,
`SIGNATURE`, `HEURISTIC`), so a surprising mapping can be explained without
re-running the sniff by hand.

## 7. Tests

- Unit tests per module: sniff signatures and heuristics, VTT timestamp
  grammar and tag stripping, BoxNote run collapsing, emitter aggregation
  and bounds, fold integrity and structure.
- Integration tests over a real socket (`tests/`), fixtures inline:
  per-format golden event sequences and envelope assertions
  (`info` first, `status` last, dense indexes), warning aggregation, the
  Document fold per format with `integrity_errors` asserted, sniff
  resolution and evidence values, framing violations, the byte cap tripping
  mid-upload, admission refusal, the `UNIMPLEMENTED` refusals, and
  `GetServiceInfo` coupled to the actual parse behaviour.
- Empty input is per-format: Markdown parses to an empty document, VTT and
  the JSON formats are `INVALID_ARGUMENT`, an empty sniff is
  `UNIMPLEMENTED`.
