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
- Full structural coverage of each format — every construct the mappings
  in section 4 name — plus the inline formatting, link and reference
  structure each parser computes while flattening, carried as `InlineSpan`
  runs over the flattened text.

## 2. Non-goals (this build)

- Parsing HTML embedded in another format: raw HTML inside Markdown is
  still flattened to its text with `WARNING_CODE_EMBEDDED_HTML_FLATTENED`.
  Standalone HTML has its own fold.
- Rendering HTML: no browser, no CSS, no visibility computation, no form
  extraction — anything that needs a rendered page is out of scope by
  construction.
- Fetching anything: images, links and URIs stay the strings the source
  wrote.
- Rendering, syntax highlighting, or language detection beyond what the
  source declares.
- Rich block structure inside a table cell or an image alt: nested lists
  and nested tables inside a cell flatten to the cell's text. The cell's
  own emphasis and links survive as runs; its structure does not.
- Resolving what a reference points at *on the typed wire*: a span carries
  the key, and the Document projection is where a key becomes a `FineRef`
  into a real item.

## 2a. Inline runs

`TextBlock.spans`, `TableCell.spans` and `Cue.spans` carry one `InlineSpan`
per run of the flattened text that says something the flat string cannot: a
rendering signal (`SpanStyle`), a link (`hyperlink` plus `link_title`), an
anchor the run declares (`anchor_id`), a key it points at (`reference` plus
`reference_kind`), the source notation flattening removed (`raw`), a run's
own typography (`font_family`, `font_size_pt`, `color`), its own language
tag (`language`), the named character style the source put on it
(`style_name`), and the interlinear reading printed over it
(`annotation`).

Three rules make the ranges usable:

- **Code points, not bytes.** `IntRange` is half-open and counts Unicode
  scalar values into the same event's `text`, so a consumer slices the
  string it was handed. `src/spans.rs` builds the buffer one `char` at a
  time and counts as it builds, which is why a multi-byte document cannot
  produce a range that is right in bytes and wrong in scalars.
- **A run starts where its first visible character lands.** Marks open
  before the text arrives; the start offset is fixed at the moment a
  character is appended, so a mark that opens while a collapsed space is
  pending does not swallow the space. A run that covers nothing leaves no
  span.
- **Spans come back in start order**, outermost first where two runs start
  together. They may nest and overlap.

`TextBlock.hyperlink` keeps its old meaning — "this whole block is one
link" — because that is a question about the block. Every other URL is a
span, so a paragraph with three links yields three URLs where it used to
yield none.

`ParseOptions.emit_inline_spans` gates the field and is unset-means-on:
every fold computes the structure while flattening either way, so the
option exists to turn the runs off for a consumer that wants flat text.

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
| Front matter between leading `---` (YAML) or `+++` (TOML) fences | `MarkupInfo.metadata` as the tree the source declared: scalars verbatim, sequences as `MetaList`, nested mappings as `MetaMap`, keys the mapping's own; `title` also to `MarkupInfo.title` when it is a non-empty scalar. The fence picks the dialect, and both land on the same tree. Unparseable front matter: one `WARNING_CODE_METADATA` naming the dialect, raw text not emitted as content |
| First H1 | `BLOCK_LABEL_TITLE` |
| Every other heading | `BLOCK_LABEL_SECTION_HEADER` with the Markdown level |
| Paragraph | `BLOCK_LABEL_PARAGRAPH`, inline markup flattened, whitespace collapsed; `hyperlink` when the whole paragraph is one link, and one `InlineSpan` per formatted, linked or referring run |
| Emphasis, strong, strikethrough, sub/superscript | span styles over the run they cover |
| Hard line break (`\\` or two trailing spaces) | a `\n` in the block's text; a soft wrap stays a space |
| Inline code, inline math | span styles; math also keeps its source notation in `raw` |
| Inline link | a span with `hyperlink` and the link title, whether or not the block is one link |
| Heading attribute `{#id}` | `TextBlock.anchor_id` |
| Definition list | list items with the `definition-term` / `definition-description` roles, the description one depth under its term |
| GFM alert (`> [!WARNING]`) | `BLOCK_LABEL_QUOTE` with `role: "admonition"` and the `admonition` kind |
| List item | `BLOCK_LABEL_LIST_ITEM` with `enumerated`, the marker as written (read back from the source bytes), `list_depth`; a task-list checkbox as the typed `task_state` |
| Fenced / indented code | `BLOCK_LABEL_CODE`, text verbatim, info-string first word as `language` |
| Blockquote | `BLOCK_LABEL_QUOTE`, one block per paragraph, with `quote_depth` saying how deep it sat; every block inside the quotation carries the depth, list items included |
| Pipe table | `TableStart` (delimiter-row alignments as `columns`) / `TableRow` (header row and cells flagged, cell runs carried) / `TableEnd` |
| Image | `ImageBlock` with `src`, `alt`, `title` as written |
| Footnote definition | `BLOCK_LABEL_FOOTNOTE`, `role` and `anchor_id` = the label; a reference is a superscript span over the label carrying the key |
| Display math | `BLOCK_LABEL_FORMULA`, verbatim |
| HTML block / inline HTML | flattened to text (`role: "html"` for blocks) + `WARNING_CODE_EMBEDDED_HTML_FLATTENED`; a block also keeps its own markup verbatim in `raw`, so the flattening is reported *and* recoverable |
| Thematic break | nothing + `WARNING_CODE_UNMAPPED_CONSTRUCT` |

`source_line` on every block comes from the offset iterator, mapped to
1-based lines of the original file, front matter included.

### 4.2 HTML (`scraper` / html5ever)

The mapping decides which elements become items and which flatten into the
enclosing text; rendering, bounding boxes and form machinery are out of
scope by construction.

| Source construct | Wire |
|---|---|
| `<title>`, `<meta name content>` or `<meta property content>` | `MarkupInfo.title`, scalar `MarkupInfo.metadata` values under `meta.*` keys |
| `h1`..`h6` | first `h1` is `BLOCK_LABEL_TITLE` (the Markdown fold's rule); every other heading `BLOCK_LABEL_SECTION_HEADER` at its own level |
| `p`, loose inline text between blocks | `BLOCK_LABEL_PARAGRAPH`, inline markup flattened, whitespace collapsed; a block that is entirely one `<a>` carries `hyperlink`, and every inline element leaves a run |
| `b`/`strong`, `i`/`em`/`cite`/`dfn`/`var`, `u`, `ins`, `s`/`del`, `code`/`kbd`/`samp`, `mark`, `small`, `sub`, `sup`, `q` | span styles over the run they cover |
| `a href` | a span with the URL and `<a title>`; a `#fragment` also carries `reference_kind = CROSS_REF` |
| `abbr title`, `time datetime`, `data value` | the machine-readable value in the span's `raw` |
| `id` on any element | `TextBlock.anchor_id` on a block, `InlineSpan.anchor_id` on a run |
| `address`, `summary` | paragraphs with the element name as `role` |
| `blockquote` | `BLOCK_LABEL_QUOTE` per paragraph, every block inside carrying its `quote_depth` |
| `ul` / `ol` / `li` | `BLOCK_LABEL_LIST_ITEM` with nesting depth; `enumerated` for `ol`; a marker is written only when the source numbered the list (`ol start`) |
| `dl` / `dt` / `dd` | list items with the proto's `definition-term` / `definition-description` roles, descriptions one level under their term |
| `table` (+`thead`/`tbody`/`tfoot`, `caption`, `colspan`/`rowspan`, `colgroup`/`col`) | streamed table with the caption on `TableStart`, the `col` geometry as `columns` (a `span` repeated across the columns it covers), header cells and rows flagged, spans advancing the column index over an occupancy grid, each cell's own `align`/`valign` and its inline runs |
| `pre` (+`language-*` class) | `BLOCK_LABEL_CODE`, text verbatim |
| `img`, `figure`/`figcaption` | `ImageBlock` (`src`/`alt`/`title` verbatim; a `srcset` with no `src` lends its first candidate), the figcaption as a following `CAPTION` block |
| `picture` | exactly the one `ImageBlock` its `img` fallback names; `source` candidates are never pictures of their own |
| `svg` | one `ImageBlock` per `image` it holds, `href` preferred over `xlink:href`, the reference verbatim exactly as an `img src`; `aria-label`, the `<title>` child or the `<desc>` as `alt`, an unused `<desc>` as `title`; the `image`'s own `width`/`height`. An `svg` with drawn content and no `image` is one `ImageBlock` whose `src` is the element serialized as a `data:image/svg+xml;base64,` URI, only under a 32 KiB source cap (`html_pictures::INLINE_SVG_MAX_BYTES`). Decorative `svg` (`aria-hidden="true"`, `role="presentation"`/`"none"`, or both `width` and `height` at most 24 px) is nothing, and so is a sprite sheet of `defs`/`use` |
| `object data`, `embed src` | one `ImageBlock` when the `type` is `image/*` (or, with no `type`, the reference has an image extension); `title` as `title`, `aria-label` or the `object`'s fallback text as `alt`; anything else stays dropped |
| `details` | descended; its `summary` keeps its role |
| `script`, `style`, `head`, `template`, form controls, media embeds | never text |
| `br` | a `\n` in the enclosing block's text, cells included |
| `hr` | nothing + `WARNING_CODE_UNMAPPED_CONSTRUCT` |
| any element without a mapping | text flattened into the enclosing block + one aggregated `WARNING_CODE_UNMAPPED_CONSTRUCT` per element name |

`WARNING_CODE_MALFORMED_STRUCTURE` comes only from html5ever's own
tree-builder error list — repairs the parser actually made — with the
missing-doctype complaint excluded, because a body-only fragment is a legal
input. Deliberately out of scope: browser rendering, bounding boxes,
form/key-value extraction, checkbox recognition, block structure inside a
table cell (nested lists and tables flatten to the cell's text, though the
cell's own emphasis and links survive as runs), the `<inline-formula>`
JATS-ism, and image fetching.

### 4.3 AsciiDoc (line-oriented)

A line machine, not a full AsciiDoc implementation: it maps the structural
core of the format plus the three constructs the fleet needs most (listing
blocks, attribute entries, admonitions).

| Source construct | Wire |
|---|---|
| `:name: value` attribute entries | `MarkupInfo.metadata`, gathered wherever they appear |
| `= Title` | `BLOCK_LABEL_TITLE` (first; later ones level-1 headers) |
| `== `..`====== ` | `BLOCK_LABEL_SECTION_HEADER` at marker length minus one |
| consecutive non-empty lines | one `BLOCK_LABEL_PARAGRAPH`, joined with spaces, flushed at a blank line |
| `NOTE:` / `TIP:` / `IMPORTANT:` / `WARNING:` / `CAUTION:` paragraphs | `role: "admonition"` and the matching `admonition` kind, the label stripped |
| `*`/`**`/`.`/`..`/`-`/`1.` items | `BLOCK_LABEL_LIST_ITEM`, nesting by marker repetition, `enumerated` for dot and numbered markers, marker verbatim |
| `|===` blocks | streamed table; each row is a line whose cells split on `|`, with the cell specifier before each `|` honoured: `2+`/`2.3+` as `column_span`/`row_span`, `<`/`^`/`>` and their `.`-prefixed forms as `align`/`valign`, `3*` applying one specifier to the cells that follow, `h` marking a header cell and `e`/`s`/`m`/`l` putting a run over the cell text. A row may open with a specifier instead of a `|`. First row flagged as header; a preceding `.Caption` lands on `TableStart` |
| `[source,lang]` + `----` fences | `BLOCK_LABEL_CODE`, verbatim, with the declared language; an unclosed fence keeps its content and warns `MALFORMED_STRUCTURE` |
| `[[id]]` / `[[id,reftext]]` | on its own line, the `anchor_id` of the block below it; inside a paragraph, taken out of the text and left as an empty-range anchor run at the position it stood. The reference text is not the name and is dropped: this plane has no slot for a phrase a renderer would substitute |
| `image::path[alt, k=v]` | `ImageBlock` with the path verbatim (no `file:` URI rewriting — this service never dereferences); a pending `.Caption` follows as a `CAPTION` block |

Deliberately skipped where the format is richer than this fold: block
quotes (`____`), sidebars (`****`), literal blocks (`....`),
description lists, includes and conditional directives, inline
formatting other than anchors, and comment lines (`//` lines join
paragraphs as text). Two
choices made deliberately, both kinder to the author: nesting follows
marker depth (`**`) rather than leading indentation, and single-cell `|`
rows inside `|===` are kept.

### 4.4 LaTeX (hand-rolled tolerant reader)

pylatexenc has no Rust counterpart, so the node model — text runs, groups,
macros with signature-driven arguments, environments, math — is built
here, and the mapping covers the constructs below.

| Source construct | Wire |
|---|---|
| preamble | skipped; `\title` surfaces as `MarkupInfo.title` and a `BLOCK_LABEL_TITLE` block, `\author`/`\date` as paragraphs with their macro name as `role` |
| `\section` family | `BLOCK_LABEL_SECTION_HEADER` — levels: part/chapter/section 1, subsection 2, subsubsection 3, paragraph 4, subparagraph 5 |
| text runs | paragraphs split at blank lines; `\textbf`, `\textit`, `\emph`, `\texttt`, `\underline` and `\textsc` leave the run they cover, `\cite`/`\citep`/`\citet` leave a citation reference per key, `\ref` a cross reference and `\eqref` an equation reference, `\href{u}{t}` and `\url{u}` a link run, `$..$` a math run keeping its TeX, `~` a space, `\%` and friends their character; `%` comments stripped |
| `\label` | the `anchor_id` of the block it follows (a heading, a caption, a formula) or of the paragraph it sits in. Inside a `figure` or `table` float it names the float: the picture's or the table's own `anchor_id`, never the caption beside it, because that is what a `\ref` to a figure means |
| `\footnote` | a `FOOTNOTE` block whose `anchor_id` the marker run left in the interrupted paragraph points at |
| `itemize`/`enumerate`/`description` | nested `BLOCK_LABEL_LIST_ITEM`s, `enumerated` for enumerate, description terms as `term: ` lead-ins |
| `tabular` | table events: the preamble (`{|l|c|p{5cm}|}`) as the table's `columns`, `&` splits cells, `\\` ends rows, rules dropped, `\multicolumn` and `\multirow` as spans |
| `verbatim`/`lstlisting`/`minted` | `BLOCK_LABEL_CODE`, verbatim, `[language=x]` as the declared language; `tikzpicture` as code declared `tikz` (no renderer here) |
| equation family, `$$..$$`, `\[..\]` | `BLOCK_LABEL_FORMULA` with `\label{...}` lifted into `anchor_id`; `$..$` stays inline as a math run |
| `\includegraphics` | `ImageBlock`, path verbatim; `\caption` a `CAPTION` block; `\footnote`/`\marginpar` `FOOTNOTE` blocks |
| `quote`/`quotation`/`verse` | `BLOCK_LABEL_QUOTE` paragraphs, nesting carried as `quote_depth` |
| `abstract`, theorems, `proof`, `thebibliography` | an `Abstract` header with `role: "abstract"`, a `Theorem.`-style lead paragraph with the environment as `role`, a `References` header plus one list item per `\bibitem`, its key the item's `anchor_id` |
| unknown environments | descended into, one aggregated `WARNING_CODE_UNMAPPED_CONSTRUCT` per environment name |
| `\input`/`\include` | never read (diskless); one aggregated warning names the refusal |

Zero-argument `\newcommand` macros expand one level; parameterized custom
macros are deliberately not expanded (textual `#N` substitution would be
guessing at the author's intent). Also
deliberate: theorem lead-ins are plain text with a `role`, never
markdown-in-a-string; `\bibliography{file}` is consumed
silently rather than leaking its filename as text; accent macros drop the
accent and keep the base letter; no `source_line` (the reader is not
line-oriented and inventing lines would be guessing).

### 4.5 WebVTT (hand-written parser)

The `WEBVTT` header is mandatory
(`INVALID_ARGUMENT` without it, empty input included), header title and
`Key: value` lines land on `MarkupInfo`, `NOTE` / `STYLE` / `REGION` blocks
are skipped, and each cue becomes one `Cue` event: identifier, timings in
milliseconds (strict `mm:ss.ttt` grammar, optional hours), first `<v>`
annotation as `speaker`, payload with its tags removed from the text and
lines joined with `\n`. A malformed cue — no timings line, unparseable
timestamps, end before start — is skipped with one aggregated
`WARNING_CODE_MALFORMED_STRUCTURE`, which is WebVTT's own error recovery.

What the tags said survives beside the text rather than being deleted with
it:

| Cue markup | Wire |
|---|---|
| `<b>`, `<i>`, `<u>` | a span style over the run the tag covers |
| `.class` qualifiers on any cue tag (`<c.loud.narrow>`, `<b.loud>`) | `InlineSpan.style_name`, the class list verbatim, dots and all |
| `<lang fr>` | `InlineSpan.language` over its run |
| `<v Name>` | `Cue.speaker` (first annotation wins); its classes still leave a style name |
| `<ruby>` / `<rt>` | the base text stays in the cue's text and the `<rt>` reading rides as the run's `annotation`; the reading never enters the text it is printed over |
| `<00:00:06.000>` karaoke timestamps | `Cue.timings`, one `CueTiming` per run, tiling the whole text; the leading run starts at the cue's own start and the last ends at the cue's own end. A cue with no timestamp tag carries no segments, because its timing is already the cue's |

Cue text is not prose and is not collapsed: the spacing and line breaks the
author timed the audio against are kept verbatim, and the character
entities the grammar admits are decoded *during* the walk, so a `&amp;` is
five code points in the source and one in the string a range indexes.

### 4.6 BoxNote (ProseMirror `doc` tree)

A node-for-node walk of the tree: headings (first level-1 is the
title), paragraphs from styled runs (every mark leaves a run: the boolean
ones as styles, colour, family and size as the run's own typography, and
each safe-scheme link as a link run whatever else the block holds; the
block-level `hyperlink` still means "the whole block is one link"), bullet
/ ordered / check lists (checkbox
in `role`; BoxNote writes no markers so `marker` stays absent), code
blocks, blockquotes, spanned tables (`rowspan` / `colspan` advance the
column index over the occupied grid), images from the node's own attributes
(`boxSharedLink`, then `src`, then `fileName`), and wrappers
(`call_out_box`) recursed rather than dropped. Legacy `atext` notes are
refused as `INVALID_ARGUMENT` rather than silently mis-parsed.
An unknown node type with nothing inside warns `UNMAPPED_CONSTRUCT` naming
it.

### 4.7 Docling JSON (field-by-field ingestion)

The vendored `document.proto` mirrors the DoclingDocument v2
schema, so ingestion is a mapping, not an interpretation: `schema_name`,
`version`, `name`, `origin`, `body`, `furniture`, `groups`, `texts`
(variant chosen by the item's `label`), `tables` (cells with span
offsets, their own `align` / `valign`, both `grid` and `table_cells`),
`pictures` (with `ImageRef`), `pages`, plus `prov` (page, bbox, charspan),
`formatting`, `hyperlink`, the `spans` an item and a cell declare, the
`admonition_kind` an item is marked with, and the whole `self_ref` /
`parent` / `children` wiring. A span is read whole: its range (the
schema's object, or the two-element array the upstream dialect writes a
charspan as), its formatting, link and link title, its resolved `target`
with the range it narrows to, its reference key and kind, its typography,
language, field code, named style and highlight. Identity is strict —
not JSON, or `schema_name` not naming a DoclingDocument, is
`INVALID_ARGUMENT` — while unknown labels and layers degrade to defaults
with a warning.

Every key the mapping does not consume produces one aggregated
`WARNING_CODE_UNMAPPED_CONSTRUCT` per key path, array indexes normalized
(`$.pictures[].annotations`), with a count.

The typed events for this format are a projection: the body is walked in
reading order and its items re-emitted as `TextBlock` / table / `ImageBlock`
events (list depth from the group nesting, table captions resolved onto
`TableStart`, an item's runs and callout kind carried back onto the wire's
own `spans` and `admonition`). The `document` event is the point of the
format: the ingested document carried over item for item, with this
collector's `CollectorSource` appended to each item's sources.

Two facts do not survive the trip back to the *typed wire*, because it has
no field for them and minting one to hold a value no fold produces would be
worse than saying so: an `overline`, and a highlight stated as a colour
rather than as a `mark`. Both survive on the `document` event, which is
where a round trip is measured.

## 5. The Document projection, and what it drops

The fold (`src/document_fold.rs`) consumes this service's own emitted
events — grpc-epub's pattern — and produces a merge-safe fragment: dense
local refs, reciprocal `parent` / `children`, `CollectorSource{collector:
"markup", model: <fold>, version}` on every item, format-native facts in
`meta.custom_fields` under the fold's prefix. `integrity_errors` is the
merge contract as a check; every fold test asserts it is empty.

Structure: a heading-as-parent ladder (a section header of level N parents
under the nearest open header of a lower level), plus one
`GROUP_LABEL_LIST` / `GROUP_LABEL_ORDERED_LIST` group per contiguous list
with nested lists as nested groups, plus one group per level of block
quotation. A quotation group is the schema's own raw-label fallback
(`label_raw: "quote"`, `name: "quote"`) because the group vocabulary has no
quotation member and borrowing one that means something else would be
worse. A block states the depth it sat at and the stack follows it, so a
quote inside a quote is a group inside a group and a list inside a
quotation is a list group under the quotation's group.

Carried across from the typed wire, into slots the schema already had:

- `TextBlock.spans` and `TableCell.spans` become `InlineSpan` runs on
  `TextItemBase.spans` and `TableCell.spans`, styles folded onto
  `Formatting` (bold, italic, underline, strikethrough, monospace, small
  caps, math, mark, small, insertion, abbreviation, quote) and `Script`. A
  run with no typography carries no all-false `Formatting`. A link's title
  and the raw payload behind projected text (an abbreviation's expansion,
  inline math in its source syntax) ride on the span.
- A `CAPTION` block adjacent to a picture, on either side, becomes that
  picture's `captions` reference: an HTML `figcaption` beside its image, an
  AsciiDoc `.Title` before its `image::`, a LaTeX `\caption` next to its
  `\includegraphics`. Any other item in between breaks the adjacency and
  the caption stays a free-standing `CAPTION` item. A table's caption
  never waits here; `TableStart` carries it and the table owns it.
- A block's `admonition` becomes `TextItemBase.admonition_kind`, lower-cased.
- A list item's `task_state` becomes its label:
  `DOC_ITEM_LABEL_CHECKBOX_SELECTED` or `..._UNSELECTED`. It stays a
  `ListItem` variant, so its enumeration and its marker survive beside the
  checkbox, and it stays inside its list group: a checkbox does not break a
  contiguous list.
- Every `anchor_id` becomes a `NamedAnchor` in `Document.anchors`, and every
  span `reference` keeps its key verbatim *and* gains a `FineRef` into the
  item that declares that anchor. A key nothing declares keeps the key and
  no pointer. `ReferenceKind` carries across arm for arm, with one
  refinement read off the resolution rather than guessed from the key's
  spelling: a bare cross reference that lands on a formula item is an
  equation reference, one that lands on a picture is a figure reference,
  and one that lands on a table is a table reference. Resolution is a pass at the end of the parse, because a document
  refers forward as often as backward; a reference whose anchor the document
  never declares keeps no target rather than an invented one.
- A table cell's runs go through that same pass. A cell cannot name its site
  while the table is streaming — the item has no arena position until it is
  appended — so each cell records where its runs sit inside the table and
  the append turns those into sites once the index is known. Both shapes are
  patched, `grid` and `table_cells`, because they are the same run and a
  consumer that reads either must see the same pointer. An anchor a cell run
  declares points at the table and states no range: this plane addresses
  items, and a range into a table's text would name nothing.
- The heading ladder becomes `Document.outline`.
- `TableStart.columns` becomes `TableData.columns`, one `TableColumnSchema`
  per declared column with its alignment; `name` stays unset, because a
  markup table declares geometry and not a schema, and a header row is not
  a column-name declaration. A width that is a bare number is a page-unit
  `width`; `20%` and `5cm` keep their spelling in `width_raw`. A cell's own
  declared alignment lands on the cell. The wire merges both alignment axes
  into one enum and the folds keep them in the right field, so the
  projection splits them and refuses a vertical value in a horizontal slot.
- An image's declared `width` and `height` become `ImageRef.size` when the
  source stated both as bare numbers, and `ImageRef.size_raw` otherwise,
  with `size` unset: `Size` is two plain doubles with no unit beside them,
  so `20%`, `5cm` and `0.8\textwidth` cannot go in it without inventing a
  unit the source never named. The raw form names its dimensions
  (`width=50%,height=5cm`, `width=0.8\textwidth`), which is verbatim what a
  LaTeX option list wrote and a faithful transcription of what two HTML
  attributes said, and is what keeps a lone dimension unambiguous.
- A block's `raw` becomes `TextItemBase.raw`, the block-level twin of the
  `InlineSpan.raw` the schema already had for "the rendered text is a
  projection of this". Only a flattened HTML block carries one; prose is
  not a projection of anything.
- A block's `source_line` becomes a `ProvenanceItem` carrying a
  `LineSpan`, on text items, pictures and tables alike. The wire states
  where a block begins and not where it ends, so the range is the one line
  the fold knows about: half-open `[line, line+1)` is "line N", and a fold
  that ever learns a block's last line widens the end without changing the
  shape. No page and no box ride with it; a markup source has neither.
- `MarkupInfo.metadata` becomes `Document.source_meta`: title, authors,
  dates, language, generator and keywords in their own slots, other scalars
  in `extra` under their own names. A declared date lands in the shape the
  source actually stated: an offset means an instant and a `Timestamp`, a
  bare date or a wall-clock time means a `CivilDateTime` and no invented
  timezone, and either way the source's own spelling is kept in the `_raw`
  twin. An author list stays a list. The HTML
  fold's `meta.*` keys become typed `Document.meta_tags` entries instead.

Dropped or demoted per format, deliberately:

- **All formats:** `ParseStatus` (describes the stream, not the document)
  and its warnings. A block's `role` survives only as a custom field.
  Everything else the typed wire carries has a first-class slot on this
  plane.
- **HTML:** anything derived from a rendered page (provenance
  boxes, visibility, forms) never existed here; nested structure inside a
  table cell survives only as the cell's flattened text; `hr` and unmapped
  elements survive only as warnings.
- **AsciiDoc:** attribute entries live only on the body meta.
- **LaTeX:** everything TeX (macro definitions, spacing, counters, page
  furniture) dissolves; tikz survives only as its source; description-term
  structure survives only as a `term: ` prefix.
- **Markdown:** `DocItemLabel` has no quote member, so a quoted paragraph
  is a `TEXT` item under a quotation group with the quotation fact also on
  the item's custom fields, which is what an item read on its own can say; an unknown code language
  survives only as `code_language_raw`; image `alt` / `title` become custom
  fields, not caption items; thematic breaks are gone entirely (warned on
  the trailer), and an HTML block's own markup is kept in `raw` rather
  than lost with its tags.
- **VTT:** cue order is document order, the cue's own timings live in the
  `TrackSource` and its karaoke segments in `prov` as `TimeSpan`s with the
  `charspan` they cover; skipped NOTE/STYLE/REGION content never reaches the
  document.
- **BoxNote:** nothing beyond what every format drops.
- **Docling JSON:** nothing is re-folded — the ingested document passes
  through with stamps appended. What the *mapping* dropped (unconsumed
  keys: annotations, key-value items, form items) is exactly what the
  per-key-path warnings name.

## 5a. What the Document plane still does not carry

Every residual this repo's last report listed now has a typed slot and is
in it, so what remains is two deliberate calls rather than five gaps.

- **`ParseStatus` and its warnings** describe the stream and not the
  document, and stay off this plane by construction. That includes
  `evidence`, whose *conclusion* does land: the sniff's rung becomes
  `DocumentOrigin.mimetype_evidence` (`requested`, `signature`,
  `heuristic`), so a consumer reading `text/markdown` off a document can
  tell a stated fact from a classified guess without the stream's trailer.
- **A block's `role`** is an open refinement (`html`, `admonition`, a
  footnote label) with no closed vocabulary to type it against, and stays a
  custom field.

Two things still do not survive a round trip back to the *typed wire*,
which is a gap in this wire rather than in the Document plane, and stating
it is better than minting fields no fold in this build produces: the wire
has no overline style and no highlight colour. Both survive on the
`document` event, which is where a round trip is measured. If a fold ever
produces either, the wire gets a `SPAN_STYLE_OVERLINE` and an
`InlineSpan.highlight_color` in the same change.

## 6. Sniffing

Precedence, fixed:

1. **Explicit request wins outright.** The sniff never runs.
2. **Signatures:** the `WEBVTT` header (optional BOM already stripped); a
   JSON object whose `schema_name` names a DoclingDocument (`DOCLING_JSON`) or
   with a ProseMirror `doc` tree / legacy `atext` key (`BOXNOTE`);
   `<!DOCTYPE html` or `<html` (`HTML`); `\documentclass` or
   `\begin{document}` (`LATEX`). Every signature format is mapped by this
   build; if a registry entry were ever removed, the signature would still
   match and the refusal would name what was sniffed.
3. **Heuristics, Markdown, AsciiDoc and reStructuredText only:** ATX
   headings, fences, inline links or a front-matter fence of either dialect
   say Markdown; a `= `
   document title, `== ` sections, `:name: value` attribute entries or
   `----` fences say AsciiDoc; an explicit-markup block (`.. note::`,
   `.. _target:`, `.. [ref]`) or a section adornment says
   reStructuredText. The adornment test refuses `=` and `-`, which the
   other two own. More than one at once, or none, is `UNIMPLEMENTED` — the
   server never guesses between candidates it cannot separate, and plain
   prose is not blessed as Markdown by default.

   No build maps reStructuredText yet, so bytes that resolve to it are
   refused with `UNIMPLEMENTED` naming the format. That is the point of
   naming it: without `MARKUP_FORMAT_RST` a `.rst` file sniffed as Markdown
   and was mapped by the wrong fold.

Bytes that merely start like JSON but do not parse are not a signature;
they fall through to the heuristics and, in practice, to the refusal.

`MarkupInfo.evidence` reports which rung matched (`REQUESTED`,
`SIGNATURE`, `HEURISTIC`), so a surprising mapping can be explained without
re-running the sniff by hand.

## 7. Tests

- Unit tests per module: sniff signatures and heuristics, VTT timestamp
  grammar and tag stripping, BoxNote mark reading, the inline flattener's
  offsets, emitter aggregation and bounds, fold integrity and structure.
- Integration tests over a real socket (`tests/`), fixtures inline:
  per-format golden event sequences and envelope assertions
  (`info` first, `status` last, dense indexes), warning aggregation, the
  Document fold per format with `integrity_errors` asserted, sniff
  resolution and evidence values, framing violations, the byte cap tripping
  mid-upload, admission refusal, the `UNIMPLEMENTED` refusals, and
  `GetServiceInfo` coupled to the actual parse behaviour.
- Every span assertion slices the text by code points through one helper,
  and every fold's span fixture carries multi-byte text, so a range that is
  right in bytes and wrong in scalars fails rather than passes quietly.
- Empty input is per-format: Markdown parses to an empty document, VTT and
  the JSON formats are `INVALID_ARGUMENT`, an empty sniff is
  `UNIMPLEMENTED`.
