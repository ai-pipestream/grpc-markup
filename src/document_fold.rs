// SPDX-License-Identifier: Apache-2.0

//! The collector-side fold from this service's event stream into one
//! `ai.pipestream.document.v1.Document`.
//!
//! The typed event stream is the primary, lossless wire. This module is the
//! structural projection of it, offered because a coordinator that only
//! wants the Document plane should not have to reimplement the mapping —
//! the canonical fold lives next to the collector that knows what the
//! events mean. (`docling-json` is the one format that does not come
//! through here: its payload already is a Document, and
//! [`crate::docling_json`] carries it over item for item instead of
//! re-folding a projection of a projection.)
//!
//! Three properties shape the code:
//!
//! - **Single pass, one event at a time.** [`DocumentFold::consume`] takes
//!   the same [`ParseMarkupResponse`](pb::ParseMarkupResponse) event the
//!   service is about to write to the socket and does a bounded amount of
//!   work with it. The only state that outlives an event is the table being
//!   streamed, the open heading ladder, the open list-group stack, and the
//!   last picture or caption, so that the two can bind.
//! - **A self-contained fragment.** Refs are dense and local (`#/texts/0`,
//!   `#/tables/1`), every item's `parent` is set and every parent lists the
//!   item in its `children`, so the coordinator's additive merge can
//!   renumber the whole fragment mechanically. [`integrity_errors`] is that
//!   contract as a check, and every fold test asserts it is empty.
//! - **Provenance honesty.** Markup has no pages and no boxes, so no item
//!   carries a `prov` box — except a cue, whose
//!   [`TrackSource`](doc::TrackSource) timing is real provenance and rides
//!   in `source`, and whose karaoke segments are real timed ranges and ride
//!   in `prov` as [`TimeSpan`](doc::TimeSpan)s. Format-native facts
//!   with no typed slot — a block's `role`, its source line, an image's alt
//!   text — go in per-item `meta.custom_fields` under the fold's own prefix
//!   (`markdown.`, `vtt.`, …), where they are data rather than a fabricated
//!   coordinate.
//!
//! Structure is a heading-as-parent ladder: a section header of level N is
//! parented to the nearest open header of a lower level (`#/body` when
//! there is none) and the content after it hangs off that header; each
//! contiguous list becomes a `GROUP_LABEL_LIST` / `GROUP_LABEL_ORDERED_LIST`
//! group, nested lists as nested groups.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::ops::Range;

use prost::Message;
use prost_types::Value;
use prost_types::value::Kind;

use crate::document::v1 as doc;
use crate::proto::v1 as pb;
use crate::{COLLECTOR, VERSION};

/// Value of `Document.schema_name`: the upstream docling schema this plane
/// tracks.
pub const SCHEMA_NAME: &str = "docling_document_v2";

/// Self ref of the body group: the parent of everything this fold makes
/// that is not under a section header or a list group.
const BODY_REF: &str = "#/body";

/// Self ref of the furniture group. Nothing is put in it: markup has no
/// page chrome to put there.
const FURNITURE_REF: &str = "#/furniture";

/// `name` and `label_raw` of a quotation group. The group vocabulary this
/// schema mirrors has no quotation member, and `label_raw` is the fallback
/// it provides for exactly that.
const QUOTE_GROUP: &str = "quote";

/// `name` and `label_raw` of a rich table cell's content group. The group
/// vocabulary has no rich-cell member either — `INLINE` means runs inside
/// a paragraph, not blocks inside a cell — so the group states its kind in
/// the same raw-label fallback a quotation group uses.
const RICH_CELL_GROUP: &str = "rich_cell";

/// A fold of one parse's events into one Document.
///
/// Feed it every event of one `ParseMarkup` response stream in order, then
/// call [`take`](Self::take). Events from two different parses must not be
/// mixed into one fold.
pub struct DocumentFold {
    document: doc::Document,
    /// Attribution stamped on every item, `model` filled in by `info`.
    source: doc::CollectorSource,
    /// The fold's custom-field prefix: the format model, once known.
    prefix: String,
    /// The table opened by a `table_start` and not yet closed.
    table: Option<PendingTable>,
    /// The open section headers, outermost first: (level, self ref).
    headings: Vec<(u32, String)>,
    /// The open list groups, outermost first: (depth, enumerated, self
    /// ref). Any non-list-item content closes the whole stack, which is
    /// what makes a group "one contiguous list".
    lists: Vec<(u32, bool, String)>,
    /// The open quotation groups, outermost first, one self ref per level
    /// of nesting. A block states the depth it sat at and the stack follows
    /// it, so a quote inside a quote is a group inside a group rather than
    /// two blocks that both say "quoted".
    quotes: Vec<String>,
    /// Anchor name to the ref of the item that declares it, first
    /// declaration winning. Both `Document.anchors` and every cross
    /// reference resolve through this.
    anchors: BTreeMap<String, doc::FineRef>,
    /// Cross references waiting for their anchor: where the run lives and
    /// the key it points at. A document refers forward as often as
    /// backward, so resolution is a pass at [`take`](DocumentFold::take)
    /// rather than a lookup at fold time.
    pending_refs: Vec<(Site, String)>,
    /// The picture a caption could still bind to, or the caption a picture
    /// could still claim: the item before this one, when it was either.
    adjacent: Adjacent,
}

/// What the previous event left for a caption or a picture to bind to.
///
/// A `CAPTION` block next to a picture is that picture's caption, the way
/// a `<figcaption>` sits beside its `<img>`, an AsciiDoc `.Title` precedes
/// its `image::` macro and a LaTeX `\caption` follows its
/// `\includegraphics`. Adjacency on either side binds; any other item in
/// between breaks it, and a caption a table start created never waits here
/// because the table already owns it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
enum Adjacent {
    /// The previous item was neither a picture nor an unbound caption.
    #[default]
    Nothing,
    /// The previous item was the picture at this ref, still without a
    /// following caption.
    Picture(String),
    /// The previous item was the caption at this ref, bound to nothing yet.
    Caption(String),
}

/// Where one run waiting for its anchor lives.
///
/// A table's runs cannot name their site while the table is streaming: the
/// item has no arena position until it is appended, so the cells record
/// their positions inside the table and [`DocumentFold::append_table`]
/// turns them into sites once the index is known.
#[derive(Debug, Clone, Copy)]
enum Site {
    /// Run `span` of the text item at arena index `item`.
    Text { item: usize, span: usize },
    /// Run `span` of the cell at index `flat` in `table_cells` of the table
    /// at arena index `table`. The same cell sits at every `grid` position
    /// its span covers, and the run is patched in all of them.
    Cell {
        table: usize,
        flat: usize,
        span: usize,
    },
}

/// A table being assembled from `table_start` / `table_row` / `table_end`.
struct PendingTable {
    /// The wire identifier rows and the end event carry.
    reference: String,
    /// Everything about the item except its arena position, which is only
    /// known when it is appended at `table_end`.
    item: doc::TableItem,
    /// The rows as the source wrote them: one entry per cell the row
    /// declares, with the column the wire placed it at. Short rows, spans
    /// and overlaps are settled by [`dense_table_data`] once the table's
    /// whole shape is known.
    rows: Vec<Vec<doc::TableCell>>,
    /// The column geometry the source declared, empty when it declared
    /// none.
    columns: Vec<doc::TableColumnSchema>,
    /// Cross references the cells' runs carry, positioned inside this table
    /// and waiting for the table's own arena index: (index in the flat cell
    /// list, span, the key it points at).
    cell_refs: Vec<(usize, usize, String)>,
    /// Anchors the cells' runs declare, in first-declaration order.
    cell_anchors: Vec<String>,
    /// Arena indexes of the rich-cell groups the rows created, waiting for
    /// the table's own arena index to become their parent, the way the
    /// cells' references wait for it.
    cell_groups: Vec<usize>,
    /// How many cells the rows so far hold: the flat index of the next
    /// row's first cell.
    cell_count: usize,
}

impl Default for DocumentFold {
    fn default() -> Self {
        Self::new()
    }
}

impl DocumentFold {
    /// An empty fold, with the two root groups already in place.
    #[must_use]
    pub fn new() -> Self {
        Self {
            document: skeleton(),
            source: doc::CollectorSource {
                collector: COLLECTOR.to_owned(),
                model: None,
                version: Some(VERSION.to_owned()),
                // A declarative markup mapping is deterministic, so there
                // is no confidence and no engine score behind one.
                confidence: None,
                ..doc::CollectorSource::default()
            },
            prefix: COLLECTOR.to_owned(),
            table: None,
            headings: Vec::new(),
            lists: Vec::new(),
            quotes: Vec::new(),
            anchors: BTreeMap::new(),
            pending_refs: Vec::new(),
            adjacent: Adjacent::Nothing,
        }
    }

    /// Fold one response event.
    ///
    /// Every variant is handled, and the ones that map to nothing say so:
    /// `status` is counts and warnings, which describe the stream rather
    /// than the document, and a `document` event is this fold's own output —
    /// folding one back in would double the fragment.
    pub fn consume(&mut self, event: &pb::parse_markup_response::Event) {
        use pb::parse_markup_response::Event;
        match event {
            Event::Info(info) => self.on_info(info),
            Event::TextBlock(block) => self.on_text_block(block),
            Event::TableStart(start) => self.on_table_start(start),
            Event::TableRow(row) => self.on_table_row(row),
            Event::TableEnd(end) => self.on_table_end(end),
            Event::Image(image) => self.on_image(image),
            Event::Cue(cue) => self.on_cue(cue),
            Event::Status(_) | Event::Document(_) => {}
        }
    }

    /// Finish the fragment and take it. The fold is empty afterwards.
    pub fn take(&mut self) -> doc::Document {
        // A table whose `table_end` never arrived still has rows worth
        // keeping; the parse that produced it failed, but the fold does not
        // get to decide that.
        if let Some(pending) = self.table.take() {
            self.append_table(pending);
        }
        self.resolve_references();
        self.document.anchors = self
            .anchors
            .iter()
            .map(|(name, target)| doc::NamedAnchor {
                name: name.clone(),
                target: Some(target.clone()),
            })
            .collect();
        self.headings.clear();
        self.lists.clear();
        self.quotes.clear();
        self.anchors.clear();
        self.pending_refs.clear();
        self.adjacent = Adjacent::Nothing;
        std::mem::replace(&mut self.document, skeleton())
    }

    /// Point every cross reference at the item that declares its anchor.
    ///
    /// A reference whose anchor the document never declares keeps no
    /// target: the Document plane models a resolved pointer, and inventing
    /// one that resolves to nothing would be worse than the wire's own
    /// `reference` field, which still carries the key either way.
    fn resolve_references(&mut self) {
        for (site, key) in std::mem::take(&mut self.pending_refs) {
            let Some(target) = self.anchors.get(&key).cloned() else {
                continue;
            };
            let resolved_kind = self.resolved_kind(&target.r#ref);
            for span in self.spans_at(site) {
                if span.reference_kind == Some(doc::ReferenceKind::CrossRef as i32)
                    && let Some(kind) = resolved_kind
                {
                    span.reference_kind = Some(kind);
                }
                span.target = Some(target.clone());
            }
        }
    }

    /// The run one site names. A cell run is several runs on this plane,
    /// one in `table_cells` and one at every `grid` position the cell's
    /// span covers, and all of them are the same run: a consumer that reads
    /// any of them must see the same pointer.
    fn spans_at(&mut self, site: Site) -> Vec<&mut doc::InlineSpan> {
        match site {
            Site::Text { item, span } => self
                .document
                .texts
                .get_mut(item)
                .and_then(|item| item.item.as_mut())
                .and_then(text_base_mut)
                .and_then(|base| base.spans.get_mut(span))
                .into_iter()
                .collect(),
            Site::Cell { table, flat, span } => {
                let Some(data) = self
                    .document
                    .tables
                    .get_mut(table)
                    .and_then(|table| table.data.as_mut())
                else {
                    return Vec::new();
                };
                let Some(footprint) = data.table_cells.get(flat).map(cell_footprint) else {
                    return Vec::new();
                };
                let (rows, cols) = footprint;
                let (cells, grid) = (&mut data.table_cells, &mut data.grid);
                let mut runs: Vec<&mut doc::InlineSpan> = cells
                    .get_mut(flat)
                    .and_then(|cell| cell.spans.get_mut(span))
                    .into_iter()
                    .collect();
                // Every grid position inside the footprint holds a copy of
                // this cell; a filler never does, because a filler sits only
                // where no cell's span reaches.
                for row in grid.iter_mut().take(rows.end).skip(rows.start) {
                    runs.extend(
                        row.cells
                            .iter_mut()
                            .take(cols.end)
                            .skip(cols.start)
                            .filter_map(|cell| cell.spans.get_mut(span)),
                    );
                }
                runs
            }
        }
    }

    /// What a bare cross reference turns out to point at, read off the item
    /// it resolved to rather than guessed from how the author spelled the
    /// key.
    ///
    /// A key that lands on a picture is a figure reference and one that
    /// lands on a table is a table reference, which is why a float's own
    /// label has to name the float: with the label on the caption, every
    /// such key resolved to a caption and both kinds stayed unused.
    fn resolved_kind(&self, self_ref: &str) -> Option<i32> {
        if self_ref.starts_with("#/pictures/") {
            return Some(doc::ReferenceKind::Figure as i32);
        }
        if self_ref.starts_with("#/tables/") {
            return Some(doc::ReferenceKind::Table as i32);
        }
        self.is_formula(self_ref)
            .then_some(doc::ReferenceKind::Equation as i32)
    }

    /// True when a ref names a formula item.
    fn is_formula(&self, self_ref: &str) -> bool {
        self_ref
            .strip_prefix("#/texts/")
            .and_then(|index| index.parse::<usize>().ok())
            .and_then(|index| self.document.texts.get(index)?.item.as_ref())
            .is_some_and(|item| matches!(item, doc::base_text_item::Item::Formula(_)))
    }

    // ------------------------------------------------------------------ info

    /// `MarkupInfo` names the document and the format that mapped it.
    fn on_info(&mut self, info: &pb::MarkupInfo) {
        let format = pb::MarkupFormat::try_from(info.format).unwrap_or_default();
        let model = model_name(format);
        if !model.is_empty() {
            self.source.model = Some(model.to_owned());
            self.prefix = model.to_owned();
        }
        if let Some(title) = info.title.as_ref().filter(|t| !t.is_empty()) {
            self.document.name.clone_from(title);
        }
        self.document.origin = Some(doc::DocumentOrigin {
            mimetype: mimetype(format).to_owned(),
            // This service is handed bytes on a gRPC stream; it is never
            // told a filename and it does not hash.
            binary_hash: 0,
            filename: String::new(),
            uri: None,
            mimetype_evidence: evidence_name(info.evidence).map(str::to_owned),
            ..doc::DocumentOrigin::default()
        });
        self.document.source_meta = source_meta(info.title.as_deref(), &info.metadata);
        self.document.meta_tags = meta_tags(&info.metadata);
        if !info.metadata.is_empty()
            && let Some(body) = self.document.body.as_mut()
        {
            let meta = body.meta.get_or_insert_default();
            // Deterministic order for a map the wire delivers unordered.
            for (key, value) in info.metadata.iter().collect::<BTreeMap<_, _>>() {
                // A namespaced meta tag (og:title, twitter:card, dc:subject)
                // is already in `meta_tags` under its own name, and a colon
                // is not a custom field key the document allows (only the
                // pinned `cell:?` escape is), so it stays in the typed list.
                if key.starts_with("meta.") && key.contains(':') && scalar_of(value).is_some() {
                    continue;
                }
                meta.custom_fields
                    .insert(format!("{}.{key}", self.prefix), meta_field(value));
            }
        }
    }

    // ------------------------------------------------------------------ text

    fn on_text_block(&mut self, block: &pb::TextBlock) {
        let label = pb::BlockLabel::try_from(block.label).unwrap_or_default();
        self.follow_quotes(block.quote_depth.unwrap_or(0));
        if label != pb::BlockLabel::Caption {
            // Any block that is not a caption stands between a picture and
            // whatever caption might have followed it.
            self.adjacent = Adjacent::Nothing;
        }
        if label == pb::BlockLabel::ListItem {
            self.push_list_item(block);
            return;
        }
        // Any non-list-item block ends the contiguous list.
        self.lists.clear();
        let level = block.level.unwrap_or(1);
        if label == pb::BlockLabel::SectionHeader {
            self.close_headings(level);
        }
        let parent = self.current_parent();
        let self_ref = format!("#/texts/{}", self.document.texts.len());
        let base = self.text_base(&self_ref, &parent, block, doc_label(label));
        let variant = self.text_variant(label, block, base);
        let index = self.document.texts.len();
        self.document.texts.push(doc::BaseTextItem {
            item: Some(variant),
        });
        self.register_refs(index, &self_ref, block);
        self.link_child(&parent, &self_ref);
        if label == pb::BlockLabel::Caption {
            self.bind_caption(self_ref.clone());
        }
        if matches!(label, pb::BlockLabel::Title | pb::BlockLabel::SectionHeader) {
            // The heading ladder is the document's own table of contents;
            // a source that declares one of its own would overwrite this.
            self.document.outline.push(doc::OutlineEntry {
                title: block.text.clone(),
                level: int(level),
                page_no: None,
                target: Some(doc::FineRef {
                    r#ref: self_ref.clone(),
                    range: None,
                }),
            });
        }
        if label == pb::BlockLabel::SectionHeader {
            self.headings.push((level, self_ref.clone()));
        }
        if self.document.name.is_empty() && label == pb::BlockLabel::Title {
            // A Document with no name is worse than one named after its own
            // title item.
            self.document.name.clone_from(&block.text);
        }
    }

    /// A caption item just created: the picture right before it takes it,
    /// otherwise it waits for the picture right after it.
    fn bind_caption(&mut self, caption_ref: String) {
        match std::mem::take(&mut self.adjacent) {
            Adjacent::Picture(picture_ref) => {
                if let Some(picture) = self.picture_at(&picture_ref) {
                    picture.captions.push(reference(&caption_ref));
                }
                // One caption per picture from adjacency: a second caption
                // block belongs to whatever comes next, not to the same
                // picture twice.
            }
            Adjacent::Nothing | Adjacent::Caption(_) => {
                self.adjacent = Adjacent::Caption(caption_ref);
            }
        }
    }

    /// The picture at a `#/pictures/N` ref.
    fn picture_at(&mut self, self_ref: &str) -> Option<&mut doc::PictureItem> {
        let index: usize = self_ref.strip_prefix("#/pictures/")?.parse().ok()?;
        self.document.pictures.get_mut(index)
    }

    /// The text variant one wire label maps to, with the block's own
    /// fields on it. `ListItem` is reachable only from a rich cell's
    /// blocks: a streamed list item goes through
    /// [`push_list_item`](Self::push_list_item) for its grouping.
    fn text_variant(
        &self,
        label: pb::BlockLabel,
        block: &pb::TextBlock,
        base: doc::TextItemBase,
    ) -> doc::base_text_item::Item {
        match label {
            pb::BlockLabel::Title => {
                doc::base_text_item::Item::Title(doc::TitleItem { base: Some(base) })
            }
            pb::BlockLabel::SectionHeader => {
                doc::base_text_item::Item::SectionHeader(doc::SectionHeaderItem {
                    base: Some(base),
                    level: int(block.level.unwrap_or(1)),
                })
            }
            pb::BlockLabel::ListItem => doc::base_text_item::Item::ListItem(doc::ListItem {
                base: Some(base),
                enumerated: block.enumerated,
                marker: block.marker.clone(),
            }),
            pb::BlockLabel::Formula => {
                doc::base_text_item::Item::Formula(doc::FormulaItem { base: Some(base) })
            }
            pb::BlockLabel::Code => doc::base_text_item::Item::Code(self.code_item(block, base)),
            _ => doc::base_text_item::Item::Text(doc::TextItem { base: Some(base) }),
        }
    }

    /// TRAP, and the reason this branch exists: `CodeItem` does not wrap a
    /// `TextItemBase`. Its base fields are inlined so that its single
    /// `meta` slot is a `FloatingMeta`; see the comment on `CodeItem` in
    /// document.proto.
    fn code_item(&self, block: &pb::TextBlock, base: doc::TextItemBase) -> doc::CodeItem {
        let (code_language, code_language_raw) = match block.language.as_deref() {
            None | Some("") => (None, None),
            Some(language) => match code_language_label(language) {
                Some(label) => (Some(label as i32), None),
                None => (None, Some(language.to_owned())),
            },
        };
        doc::CodeItem {
            self_ref: base.self_ref,
            parent: base.parent,
            children: base.children,
            content_layer: base.content_layer,
            meta: base.meta.map(|meta| doc::FloatingMeta {
                custom_fields: meta.custom_fields,
                ..doc::FloatingMeta::default()
            }),
            label: doc::DocItemLabel::Code as i32,
            prov: base.prov,
            orig: base.orig,
            text: base.text,
            hyperlink: base.hyperlink,
            source: base.source,
            code_language,
            code_language_raw,
            ..doc::CodeItem::default()
        }
    }

    /// The shared base of a text variant, custom fields included.
    fn text_base(
        &self,
        self_ref: &str,
        parent: &str,
        block: &pb::TextBlock,
        label: doc::DocItemLabel,
    ) -> doc::TextItemBase {
        let fields = self.block_fields(block);
        doc::TextItemBase {
            self_ref: self_ref.to_owned(),
            parent: Some(reference(parent)),
            content_layer: doc::ContentLayer::Body as i32,
            meta: (!fields.is_empty()).then(|| doc::BaseMeta {
                custom_fields: fields,
                ..doc::BaseMeta::default()
            }),
            label: label as i32,
            // No page and no box: markup has neither. The line the block
            // starts on is a real coordinate in a real space, and that is
            // the one this plane records.
            prov: line_prov(block.source_line),
            orig: block.text.clone(),
            text: block.text.clone(),
            hyperlink: block.hyperlink.clone(),
            source: vec![self.collector_source()],
            spans: doc_spans(&block.spans),
            admonition_kind: admonition_name(block.admonition).map(str::to_owned),
            raw: block.raw.clone(),
            ..doc::TextItemBase::default()
        }
    }

    /// Record what a block just appended at `index` declares and points at:
    /// the anchor on the block itself, the anchors its spans declare, and
    /// every cross reference waiting for one.
    fn register_refs(&mut self, index: usize, self_ref: &str, block: &pb::TextBlock) {
        if let Some(anchor) = block.anchor_id.as_ref().filter(|id| !id.is_empty()) {
            self.anchors.entry(anchor.clone()).or_insert(doc::FineRef {
                r#ref: self_ref.to_owned(),
                range: None,
            });
        }
        self.register_spans(index, self_ref, &block.spans);
    }

    /// Record what the runs of the text item just appended at `index`
    /// declare and point at. Split out from
    /// [`register_refs`](Self::register_refs) because a cue has runs and no
    /// block-level anchor.
    fn register_spans(&mut self, index: usize, self_ref: &str, spans: &[pb::InlineSpan]) {
        for (position, span) in spans.iter().enumerate() {
            let range = span.range.map(|range| doc::IntSpan {
                start: int(range.start),
                end: int(range.end),
            });
            if let Some(anchor) = span.anchor_id.as_ref().filter(|id| !id.is_empty()) {
                self.anchors.entry(anchor.clone()).or_insert(doc::FineRef {
                    r#ref: self_ref.to_owned(),
                    range,
                });
            }
            if let Some(key) = span.reference.as_ref().filter(|key| !key.is_empty()) {
                self.pending_refs.push((
                    Site::Text {
                        item: index,
                        span: position,
                    },
                    key.clone(),
                ));
            }
        }
    }

    /// Per-item `meta.custom_fields`: everything the wire block says that
    /// the Document plane has no typed slot for, under the format prefix.
    fn block_fields(&self, block: &pb::TextBlock) -> HashMap<String, Value> {
        let mut fields = HashMap::new();
        let label = pb::BlockLabel::try_from(block.label).unwrap_or_default();
        if label == pb::BlockLabel::Quote {
            // The schema has no quote label; the block folds as TEXT and
            // the quotation fact is recorded where the schema allows.
            fields.insert(format!("{}.quote", self.prefix), flag(true));
        }
        if !block.role.is_empty() {
            fields.insert(format!("{}.role", self.prefix), string(&block.role));
        }
        fields
    }

    // ----------------------------------------------------------------- quotes

    /// Bring the quotation stack to the depth the block states.
    ///
    /// The wire says how deep a paragraph sat, which is what the folds knew
    /// all along and used only to pick a label; the stack turns that number
    /// back into the structure it describes. Leaving or entering a
    /// quotation ends any contiguous list, because a list inside a
    /// quotation is not the same list as one outside it.
    fn follow_quotes(&mut self, depth: u32) {
        let depth = depth as usize;
        if self.quotes.len() != depth {
            self.lists.clear();
        }
        self.quotes.truncate(depth);
        while self.quotes.len() < depth {
            let parent = self.current_parent();
            let self_ref = format!("#/groups/{}", self.document.groups.len());
            let mut fields = HashMap::new();
            self.attribute(&mut fields);
            self.document.groups.push(doc::GroupItem {
                self_ref: self_ref.clone(),
                parent: Some(reference(&parent)),
                children: Vec::new(),
                content_layer: doc::ContentLayer::Body as i32,
                meta: Some(doc::BaseMeta {
                    custom_fields: fields,
                    ..doc::BaseMeta::default()
                }),
                name: Some(QUOTE_GROUP.to_owned()),
                // The group vocabulary has no quotation member, so the
                // group states its kind in the schema's own raw-label
                // fallback rather than borrowing a member that means
                // something else.
                label: doc::GroupLabel::Unspecified as i32,
                label_raw: Some(QUOTE_GROUP.to_owned()),
                ..doc::GroupItem::default()
            });
            self.link_child(&parent, &self_ref);
            self.quotes.push(self_ref);
        }
    }

    // ------------------------------------------------------------------ lists

    /// One list item, grouped under `GROUP_LABEL_LIST` /
    /// `GROUP_LABEL_ORDERED_LIST` groups, nested by depth.
    fn push_list_item(&mut self, block: &pb::TextBlock) {
        let depth = block.list_depth.unwrap_or(1).max(1);
        // Close lists deeper than this item, and a same-depth list of the
        // other kind: `1. a` after `- x` is a new list, not a continuation.
        while self.lists.last().is_some_and(|(open, enumerated, _)| {
            *open > depth || (*open == depth && *enumerated != block.enumerated)
        }) {
            self.lists.pop();
        }
        if self.lists.last().is_none_or(|(open, _, _)| *open < depth) {
            self.open_list_group(depth, block.enumerated);
        }
        let parent = self
            .lists
            .last()
            .map(|(_, _, group_ref)| group_ref.clone())
            .expect("a group was just ensured");
        let self_ref = format!("#/texts/{}", self.document.texts.len());
        // A ticked box is what the item *is* as far as the label vocabulary
        // goes; it stays a `ListItem` variant, so its enumeration and its
        // marker survive beside the checkbox.
        let label = match pb::TaskState::try_from(block.task_state).unwrap_or_default() {
            pb::TaskState::Unspecified => doc::DocItemLabel::ListItem,
            pb::TaskState::Checked => doc::DocItemLabel::CheckboxSelected,
            pb::TaskState::Unchecked => doc::DocItemLabel::CheckboxUnselected,
        };
        let base = self.text_base(&self_ref, &parent, block, label);
        let index = self.document.texts.len();
        self.document.texts.push(doc::BaseTextItem {
            item: Some(doc::base_text_item::Item::ListItem(doc::ListItem {
                base: Some(base),
                enumerated: block.enumerated,
                marker: block.marker.clone(),
            })),
        });
        self.register_refs(index, &self_ref, block);
        self.link_child(&parent, &self_ref);
    }

    /// Open one list group at `depth`, parented to the group above it, or
    /// to the current heading or body for a top-level list.
    fn open_list_group(&mut self, depth: u32, enumerated: bool) {
        let parent = self.lists.last().map_or_else(
            || self.current_parent(),
            |(_, _, group_ref)| group_ref.clone(),
        );
        let self_ref = format!("#/groups/{}", self.document.groups.len());
        let mut fields = HashMap::new();
        self.attribute(&mut fields);
        self.document.groups.push(doc::GroupItem {
            self_ref: self_ref.clone(),
            parent: Some(reference(&parent)),
            children: Vec::new(),
            content_layer: doc::ContentLayer::Body as i32,
            meta: Some(doc::BaseMeta {
                custom_fields: fields,
                ..doc::BaseMeta::default()
            }),
            name: Some("list".to_owned()),
            label: if enumerated {
                doc::GroupLabel::OrderedList as i32
            } else {
                doc::GroupLabel::List as i32
            },
            ..doc::GroupItem::default()
        });
        self.link_child(&parent, &self_ref);
        self.lists.push((depth, enumerated, self_ref));
    }

    // ----------------------------------------------------------------- tables

    fn on_table_start(&mut self, start: &pb::TableStart) {
        self.lists.clear();
        self.adjacent = Adjacent::Nothing;
        // An unclosed predecessor cannot happen on a conformant stream, but
        // dropping its rows silently if it did would be worse than keeping
        // them.
        if let Some(previous) = self.table.take() {
            self.append_table(previous);
        }
        let mut captions = Vec::new();
        if let Some(caption) = start.caption.as_ref().filter(|c| !c.is_empty()) {
            // The caption is an item in its own right — the Document plane
            // references captions, it does not inline them — so it is
            // created first and the table points at it.
            let parent = self.current_parent();
            let caption_ref = format!("#/texts/{}", self.document.texts.len());
            let base = self.text_base(
                &caption_ref,
                &parent,
                &pb::TextBlock {
                    text: caption.clone(),
                    source_line: start.source_line,
                    ..pb::TextBlock::default()
                },
                doc::DocItemLabel::Caption,
            );
            self.document.texts.push(doc::BaseTextItem {
                item: Some(doc::base_text_item::Item::Text(doc::TextItem {
                    base: Some(base),
                })),
            });
            self.link_child(&parent, &caption_ref);
            captions.push(reference(&caption_ref));
        }
        // The parent is the ladder as it stood when the table opened, not
        // as it stands when the last row lands.
        let parent = self.current_parent();
        self.table = Some(PendingTable {
            reference: start.table_ref.clone(),
            item: doc::TableItem {
                parent: Some(reference(&parent)),
                content_layer: doc::ContentLayer::Body as i32,
                label: doc::DocItemLabel::Table as i32,
                prov: line_prov(start.source_line),
                captions,
                source: vec![self.collector_source()],
                ..doc::TableItem::default()
            },
            rows: Vec::new(),
            columns: start.columns.iter().map(column_schema).collect(),
            cell_refs: Vec::new(),
            cell_anchors: start
                .anchor_id
                .iter()
                .filter(|id| !id.is_empty())
                .cloned()
                .collect(),
            cell_groups: Vec::new(),
            cell_count: 0,
        });
    }

    /// Keep one wire row as the source wrote it. The wire's `column_index`
    /// already accounts for the spans before each cell, so it is the
    /// column the cell asks for; [`dense_table_data`] settles the asks
    /// against each other once the whole table is known.
    fn on_table_row(&mut self, row: &pb::TableRow) {
        if !self
            .table
            .as_ref()
            .is_some_and(|table| table.reference == row.table_ref)
        {
            return;
        }
        // A rich cell's blocks become their group first: the group arena
        // is independent of the pending table, and the group names the
        // cell's `ref` before the cell exists.
        let rich: Vec<Option<usize>> = row
            .cells
            .iter()
            .map(|cell| (!cell.blocks.is_empty()).then(|| self.rich_cell_group(&cell.blocks)))
            .collect();
        let Some(table) = self.table.as_mut() else {
            return;
        };
        // The row's position among the rows kept, which is the row offset
        // the dense grid gives its cells; and where its cells land in the
        // flat list, which densification keeps in source order, so a cell
        // run can name its own position before the table has an arena
        // index.
        let row_index = table.rows.len();
        let flat_base = table.cell_count;
        let mut cells = Vec::with_capacity(row.cells.len());
        for (position, cell) in row.cells.iter().enumerate() {
            for (span, run) in cell.spans.iter().enumerate() {
                if let Some(anchor) = run.anchor_id.as_ref().filter(|id| !id.is_empty()) {
                    table.cell_anchors.push(anchor.clone());
                }
                if let Some(key) = run.reference.as_ref().filter(|key| !key.is_empty()) {
                    table
                        .cell_refs
                        .push((flat_base + position, span, key.clone()));
                }
            }
            let mut doc_cell = source_cell(cell, row_index, row.is_header || cell.is_header);
            if let Some(group) = rich[position] {
                table.cell_groups.push(group);
                doc_cell.r#ref = Some(reference(&format!("#/groups/{group}")));
            }
            cells.push(doc_cell);
        }
        table.cell_count += cells.len();
        table.rows.push(cells);
    }

    fn on_table_end(&mut self, end: &pb::TableEnd) {
        let Some(table) = self.table.as_ref() else {
            return;
        };
        if table.reference != end.table_ref {
            return;
        }
        let pending = self.table.take().expect("checked just above");
        self.append_table(pending);
    }

    /// Give the finished table its arena slot and link it in.
    fn append_table(&mut self, pending: PendingTable) {
        let mut item = pending.item;
        let index = self.document.tables.len();
        item.self_ref = format!("#/tables/{index}");
        // The table's position is only knowable here, which is why its
        // cells' runs waited for it. An anchor a cell declares points at
        // the table and states no range: the plane addresses items, and a
        // range into "the table's text" would name nothing.
        for anchor in pending.cell_anchors {
            self.anchors.entry(anchor).or_insert(doc::FineRef {
                r#ref: item.self_ref.clone(),
                range: None,
            });
        }
        for (flat, span, key) in pending.cell_refs {
            self.pending_refs.push((
                Site::Cell {
                    table: index,
                    flat,
                    span,
                },
                key,
            ));
        }
        let mut data = dense_table_data(pending.rows);
        data.columns = pending.columns;
        item.data = Some(data);
        let self_ref = item.self_ref.clone();
        // The rich-cell groups were minted while the table streamed; now
        // that the table has its arena slot, both halves of the link land:
        // the group names the table as its parent and the table lists it.
        for group in pending.cell_groups {
            let group_ref = format!("#/groups/{group}");
            if let Some(item_group) = self.document.groups.get_mut(group) {
                item_group.parent = Some(reference(&self_ref));
            }
            item.children.push(reference(&group_ref));
        }
        let parent = item
            .parent
            .as_ref()
            .map_or_else(|| BODY_REF.to_owned(), |parent| parent.r#ref.clone());
        self.document.tables.push(item);
        self.link_child(&parent, &self_ref);
    }

    // ----------------------------------------------------------- rich cells

    /// A rich cell's blocks as the group the cell's `ref` points at:
    /// docling's RichTableCell shape, where the flat cell text stays the
    /// display text and the structure survives as real items.
    ///
    /// Returns the group's arena index. The group's parent is the owning
    /// table, whose arena position only [`append_table`](Self::append_table)
    /// knows — it patches both halves of the link, the way it resolves the
    /// cells' waiting references. A table nested inside a cell knows its
    /// own position immediately and patches its own groups on the spot.
    fn rich_cell_group(&mut self, blocks: &[pb::CellBlock]) -> usize {
        let index = self.document.groups.len();
        let self_ref = format!("#/groups/{index}");
        let mut fields = HashMap::new();
        self.attribute(&mut fields);
        self.document.groups.push(doc::GroupItem {
            self_ref: self_ref.clone(),
            parent: None,
            children: Vec::new(),
            content_layer: doc::ContentLayer::Body as i32,
            meta: Some(doc::BaseMeta {
                custom_fields: fields,
                ..doc::BaseMeta::default()
            }),
            name: Some(RICH_CELL_GROUP.to_owned()),
            label: doc::GroupLabel::Unspecified as i32,
            label_raw: Some(RICH_CELL_GROUP.to_owned()),
            ..doc::GroupItem::default()
        });
        for block in blocks {
            // Each child links itself into the group, in block order.
            self.cell_block(block, &self_ref);
        }
        index
    }

    /// One cell block as a real item under the cell's group.
    fn cell_block(&mut self, block: &pb::CellBlock, parent: &str) {
        match block.kind.as_ref() {
            Some(pb::cell_block::Kind::Text(text)) => {
                self.cell_text(text, parent);
            }
            Some(pb::cell_block::Kind::Image(image)) => {
                self.append_picture(image, parent, Vec::new());
            }
            Some(pb::cell_block::Kind::Table(table)) => {
                self.cell_table(table, parent);
            }
            None => {}
        }
    }

    /// A text block of a rich cell: the same fold as a streamed block,
    /// minus the document-level state (heading ladder, list stack, caption
    /// adjacency) a cell never joins.
    fn cell_text(&mut self, block: &pb::TextBlock, parent: &str) {
        let label = pb::BlockLabel::try_from(block.label).unwrap_or_default();
        let self_ref = format!("#/texts/{}", self.document.texts.len());
        let base = self.text_base(&self_ref, parent, block, doc_label(label));
        let variant = self.text_variant(label, block, base);
        let index = self.document.texts.len();
        self.document.texts.push(doc::BaseTextItem {
            item: Some(variant),
        });
        self.register_refs(index, &self_ref, block);
        self.link_child(parent, &self_ref);
    }

    /// A table nested in a rich cell, folded as a real table item child of
    /// the cell's group. Unlike the streamed table it knows its arena slot
    /// up front, so its own rich cells link without waiting.
    ///
    /// The slot is claimed before the rows fold: a rich cell can hold a
    /// table of its own, which folds first and would otherwise take this
    /// table's index.
    fn cell_table(&mut self, table: &pb::CellTable, parent: &str) {
        let index = self.document.tables.len();
        self.document.tables.push(doc::TableItem::default());
        let self_ref = format!("#/tables/{index}");
        let mut item = doc::TableItem {
            self_ref: self_ref.clone(),
            parent: Some(reference(parent)),
            content_layer: doc::ContentLayer::Body as i32,
            label: doc::DocItemLabel::Table as i32,
            source: vec![self.collector_source()],
            ..doc::TableItem::default()
        };
        if let Some(caption) = table.caption.as_ref().filter(|c| !c.is_empty()) {
            // The streamed table's rule: the caption is its own item and
            // the table points at it. It parents to the table itself: the
            // cell's group holds the table, not the table's caption.
            let caption_ref = format!("#/texts/{}", self.document.texts.len());
            let base = self.text_base(
                &caption_ref,
                &self_ref,
                &pb::TextBlock {
                    text: caption.clone(),
                    ..pb::TextBlock::default()
                },
                doc::DocItemLabel::Caption,
            );
            self.document.texts.push(doc::BaseTextItem {
                item: Some(doc::base_text_item::Item::Text(doc::TextItem {
                    base: Some(base),
                })),
            });
            item.children.push(reference(&caption_ref));
            item.captions.push(reference(&caption_ref));
        }
        let mut rows: Vec<Vec<doc::TableCell>> = Vec::with_capacity(table.rows.len());
        let mut flat_base = 0usize;
        for row in &table.rows {
            // The same positions the streamed table hands its cells: the
            // row's place among the rows kept, and the cell's place in the
            // flat list, which densification keeps in source order.
            let row_index = rows.len();
            let mut row_cells = Vec::with_capacity(row.cells.len());
            for (position, cell) in row.cells.iter().enumerate() {
                // The same run bookkeeping the streamed table does, except
                // the arena index is known, so the sites are immediate.
                for (span, run) in cell.spans.iter().enumerate() {
                    if let Some(anchor) = run.anchor_id.as_ref().filter(|id| !id.is_empty()) {
                        self.anchors.entry(anchor.clone()).or_insert(doc::FineRef {
                            r#ref: self_ref.clone(),
                            range: None,
                        });
                    }
                    if let Some(key) = run.reference.as_ref().filter(|key| !key.is_empty()) {
                        self.pending_refs.push((
                            Site::Cell {
                                table: index,
                                flat: flat_base + position,
                                span,
                            },
                            key.clone(),
                        ));
                    }
                }
                let mut doc_cell = source_cell(cell, row_index, row.is_header || cell.is_header);
                if !cell.blocks.is_empty() {
                    let group = self.rich_cell_group(&cell.blocks);
                    let group_ref = format!("#/groups/{group}");
                    self.document.groups[group].parent = Some(reference(&self_ref));
                    item.children.push(reference(&group_ref));
                    doc_cell.r#ref = Some(reference(&group_ref));
                }
                row_cells.push(doc_cell);
            }
            flat_base += row_cells.len();
            rows.push(row_cells);
        }
        item.data = Some(dense_table_data(rows));
        self.document.tables[index] = item;
        self.link_child(parent, &self_ref);
    }

    // ---------------------------------------------------------------- images

    /// One image reference as a picture whose `ImageRef.uri` is the source's
    /// own string, verbatim. No bytes and no dpi: this collector is handed
    /// markup, and claiming pixels it never saw would be lying.
    ///
    /// The size is the one exception. Both dimensions as bare numbers make
    /// a `Size`: two plain doubles with no unit beside them. Anything else
    /// makes a `size_raw` and leaves `size` unset, because `20%` or `5cm`
    /// cannot go in a unitless double without inventing a unit the source
    /// never named.
    fn on_image(&mut self, image: &pb::ImageBlock) {
        self.lists.clear();
        let parent = self.current_parent();
        // A caption the previous block left waiting is this picture's; the
        // picture then waits for one more on its other side.
        let captions = match std::mem::take(&mut self.adjacent) {
            Adjacent::Caption(caption_ref) => vec![reference(&caption_ref)],
            Adjacent::Nothing | Adjacent::Picture(_) => Vec::new(),
        };
        let self_ref = self.append_picture(image, &parent, captions);
        self.adjacent = Adjacent::Picture(self_ref);
    }

    /// Append one picture under `parent`, returning its ref. Shared with
    /// the rich-cell path, which parents the picture to the cell's group
    /// and has no caption adjacency to settle.
    fn append_picture(
        &mut self,
        image: &pb::ImageBlock,
        parent: &str,
        captions: Vec<doc::RefItem>,
    ) -> String {
        let self_ref = format!("#/pictures/{}", self.document.pictures.len());
        let mut fields = HashMap::new();
        if !image.alt.is_empty() {
            fields.insert(format!("{}.alt", self.prefix), string(&image.alt));
        }
        if !image.title.is_empty() {
            fields.insert(format!("{}.title", self.prefix), string(&image.title));
        }
        let (size, size_raw) = declared_size(image);
        if let Some(anchor) = image.anchor_id.as_ref().filter(|id| !id.is_empty()) {
            // A picture is addressed as a whole; there is no range into it.
            self.anchors.entry(anchor.clone()).or_insert(doc::FineRef {
                r#ref: self_ref.clone(),
                range: None,
            });
        }
        self.document.pictures.push(doc::PictureItem {
            self_ref: self_ref.clone(),
            parent: Some(reference(parent)),
            content_layer: doc::ContentLayer::Body as i32,
            meta: (!fields.is_empty()).then(|| doc::PictureMeta {
                custom_fields: fields,
                ..doc::PictureMeta::default()
            }),
            label: doc::DocItemLabel::Picture as i32,
            prov: line_prov(image.source_line),
            captions,
            // The href of a link whose only content was the image: a click
            // on the picture follows it.
            hyperlink: image.hyperlink.clone(),
            image: Some(doc::ImageRef {
                mimetype: image_mimetype(&image.src).to_owned(),
                dpi: 0,
                size,
                uri: image.src.clone(),
                size_raw,
            }),
            source: vec![self.collector_source()],
            ..doc::PictureItem::default()
        });
        self.link_child(parent, &self_ref);
        self_ref
    }

    // ------------------------------------------------------------------ cues

    /// One WebVTT cue as a `TextItem` whose provenance is a `TrackSource`
    /// carrying the cue timings, real coordinates in seconds, alongside this
    /// collector's own stamp.
    ///
    /// A cue that times its own words gets one `ProvenanceItem` per karaoke
    /// segment: a `TimeSpan` over the media and the `charspan` it covers in
    /// the item's text. That is the one place in this fold where an item
    /// carries `prov`, and it is not an invented coordinate: the source
    /// wrote the timestamps.
    fn on_cue(&mut self, cue: &pb::Cue) {
        self.lists.clear();
        self.adjacent = Adjacent::Nothing;
        if cue.text.trim().is_empty() {
            // A cue with no visible text has nothing to put in a TextItem,
            // whose `text` the schema expects to mean something.
            return;
        }
        let parent = self.current_parent();
        let self_ref = format!("#/texts/{}", self.document.texts.len());
        let track = doc::SourceType {
            source: Some(doc::source_type::Source::Track(doc::TrackSource {
                start_time: ms_to_seconds(cue.start_ms),
                end_time: ms_to_seconds(cue.end_ms),
                identifier: cue.cue_id.clone(),
                voice: (!cue.speaker.is_empty()).then(|| cue.speaker.clone()),
            })),
        };
        let index = self.document.texts.len();
        let base = doc::TextItemBase {
            self_ref: self_ref.clone(),
            parent: Some(reference(&parent)),
            content_layer: doc::ContentLayer::Body as i32,
            label: doc::DocItemLabel::Text as i32,
            prov: cue.timings.iter().map(karaoke_prov).collect(),
            orig: cue.text.clone(),
            text: cue.text.clone(),
            source: vec![track, self.collector_source()],
            spans: doc_spans(&cue.spans),
            ..doc::TextItemBase::default()
        };
        self.document.texts.push(doc::BaseTextItem {
            item: Some(doc::base_text_item::Item::Text(doc::TextItem {
                base: Some(base),
            })),
        });
        self.register_spans(index, &self_ref, &cue.spans);
        self.link_child(&parent, &self_ref);
    }

    // ------------------------------------------------------------- primitives

    /// The ref new content parents to: the innermost open quotation group,
    /// then the innermost open section header, then the body.
    fn current_parent(&self) -> String {
        self.quotes.last().cloned().unwrap_or_else(|| {
            self.headings
                .last()
                .map_or_else(|| BODY_REF.to_owned(), |(_, self_ref)| self_ref.clone())
        })
    }

    /// Close every open header a level-`level` header ends: a header of the
    /// same level closes the one before it — siblings, not parent and
    /// child.
    fn close_headings(&mut self, level: u32) {
        while self.headings.last().is_some_and(|(open, _)| *open >= level) {
            self.headings.pop();
        }
    }

    /// Both halves of the parent link: the item names its parent, and the
    /// parent lists the item. The integrity check fails on either alone.
    fn link_child(&mut self, parent: &str, child: &str) {
        if parent == BODY_REF {
            if let Some(body) = self.document.body.as_mut() {
                body.children.push(reference(child));
            }
        } else if let Some(index) = parent
            .strip_prefix("#/groups/")
            .and_then(|index| index.parse::<usize>().ok())
        {
            if let Some(group) = self.document.groups.get_mut(index) {
                group.children.push(reference(child));
            }
        } else if let Some(base) = self.heading_base(parent) {
            base.children.push(reference(child));
        }
    }

    /// The base of the section header at a `#/texts/N` ref.
    fn heading_base(&mut self, self_ref: &str) -> Option<&mut doc::TextItemBase> {
        let index: usize = self_ref.strip_prefix("#/texts/")?.parse().ok()?;
        match self.document.texts.get_mut(index)?.item.as_mut()? {
            doc::base_text_item::Item::SectionHeader(header) => header.base.as_mut(),
            _ => None,
        }
    }

    /// This collector's attribution. `confidence` is unset: a declarative
    /// markup mapping is deterministic, so a confidence would be noise.
    fn collector_source(&self) -> doc::SourceType {
        doc::SourceType {
            source: Some(doc::source_type::Source::Collector(self.source.clone())),
        }
    }

    /// Attribute a *group* to this collector through its custom fields:
    /// `GroupItem` has no `source` slot in this schema, and leaving the one
    /// item kind this fold mints unattributable would be worse than the
    /// indirection.
    fn attribute(&self, fields: &mut HashMap<String, Value>) {
        let mut entry = BTreeMap::new();
        entry.insert("collector".to_owned(), string(COLLECTOR));
        entry.insert("version".to_owned(), string(VERSION));
        fields.insert(
            format!("{}.collector", self.prefix),
            Value {
                kind: Some(Kind::StructValue(prost_types::Struct { fields: entry })),
            },
        );
    }
}

/// An empty document with its two roots in place.
fn skeleton() -> doc::Document {
    doc::Document {
        schema_name: Some(SCHEMA_NAME.to_owned()),
        body: Some(root(BODY_REF, doc::ContentLayer::Body)),
        furniture: Some(root(FURNITURE_REF, doc::ContentLayer::Furniture)),
        ..doc::Document::default()
    }
}

/// One of the two root groups.
fn root(self_ref: &str, layer: doc::ContentLayer) -> doc::GroupItem {
    doc::GroupItem {
        self_ref: self_ref.to_owned(),
        content_layer: layer as i32,
        ..doc::GroupItem::default()
    }
}

/// A JSON-Pointer reference to another item.
fn reference(target: &str) -> doc::RefItem {
    doc::RefItem {
        r#ref: target.to_owned(),
    }
}

/// The Document label for a wire label. The two vocabularies were written
/// to match, so this is a rename and not an interpretation; `QUOTE` is the
/// one label with no slot, and it lands on `TEXT` with the quotation
/// recorded in the item's custom fields.
const fn doc_label(label: pb::BlockLabel) -> doc::DocItemLabel {
    match label {
        pb::BlockLabel::Title => doc::DocItemLabel::Title,
        pb::BlockLabel::SectionHeader => doc::DocItemLabel::SectionHeader,
        pb::BlockLabel::ListItem => doc::DocItemLabel::ListItem,
        pb::BlockLabel::Code => doc::DocItemLabel::Code,
        pb::BlockLabel::Formula => doc::DocItemLabel::Formula,
        pb::BlockLabel::Caption => doc::DocItemLabel::Caption,
        pb::BlockLabel::Footnote => doc::DocItemLabel::Footnote,
        pb::BlockLabel::Paragraph => doc::DocItemLabel::Paragraph,
        pb::BlockLabel::Quote | pb::BlockLabel::Unspecified => doc::DocItemLabel::Text,
    }
}

/// The fold model name for a wire format, empty for the unspecified value.
const fn model_name(format: pb::MarkupFormat) -> &'static str {
    match format {
        pb::MarkupFormat::Unspecified => "",
        pb::MarkupFormat::Markdown => "markdown",
        pb::MarkupFormat::Html => "html",
        pb::MarkupFormat::Asciidoc => "asciidoc",
        pb::MarkupFormat::Latex => "latex",
        pb::MarkupFormat::Vtt => "vtt",
        pb::MarkupFormat::Boxnote => "boxnote",
        pb::MarkupFormat::DoclingJson => "docling-json",
        pb::MarkupFormat::Rst => "rst",
    }
}

/// Media type of an image reference, derived from its source. An empty
/// mimetype is not a valid media type and breaks canonical-JSON consumers,
/// so unknown sources carry the generic binary type instead of nothing.
fn image_mimetype(src: &str) -> &'static str {
    let path = src
        .split(['?', '#'])
        .next()
        .unwrap_or(src)
        .rsplit('/')
        .next()
        .unwrap_or(src);
    let extension = path.rsplit('.').next().unwrap_or("");
    if let Some(data_type) = src
        .strip_prefix("data:")
        .and_then(|rest| rest.split([';', ',']).next())
        && !data_type.is_empty()
    {
        return match data_type {
            "image/png" => "image/png",
            "image/jpeg" => "image/jpeg",
            "image/gif" => "image/gif",
            "image/webp" => "image/webp",
            "image/svg+xml" => "image/svg+xml",
            "image/tiff" => "image/tiff",
            "image/bmp" => "image/bmp",
            _ => "application/octet-stream",
        };
    }
    match extension.to_ascii_lowercase().as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "svg" => "image/svg+xml",
        "tif" | "tiff" => "image/tiff",
        "bmp" => "image/bmp",
        "avif" => "image/avif",
        _ => "application/octet-stream",
    }
}

/// What the mimetype claim rests on, in the sniff's own vocabulary.
///
/// The mimetype here is derived from the format, so what the format
/// resolution rested on is exactly what the mimetype rests on: the request
/// said so, an unambiguous signature matched, or the bytes were classified
/// by structure. A consumer reading `text/markdown` off a document can tell
/// a stated fact from a guess.
fn evidence_name(evidence: i32) -> Option<&'static str> {
    match pb::FormatEvidence::try_from(evidence).ok()? {
        pb::FormatEvidence::Unspecified => None,
        pb::FormatEvidence::Requested => Some("requested"),
        pb::FormatEvidence::Signature => Some("signature"),
        pb::FormatEvidence::Heuristic => Some("heuristic"),
    }
}

/// `DocumentOrigin.mimetype` per format.
const fn mimetype(format: pb::MarkupFormat) -> &'static str {
    match format {
        pb::MarkupFormat::Unspecified => "text/plain",
        pb::MarkupFormat::Markdown => "text/markdown",
        pb::MarkupFormat::Html => "text/html",
        pb::MarkupFormat::Asciidoc => "text/asciidoc",
        pb::MarkupFormat::Latex => "application/x-latex",
        pb::MarkupFormat::Vtt => "text/vtt",
        pb::MarkupFormat::Boxnote => "application/vnd.box.boxnote",
        pb::MarkupFormat::DoclingJson => "application/json",
        pb::MarkupFormat::Rst => "text/x-rst",
    }
}

/// The `CodeLanguageLabel` whose enum name a declared language matches,
/// case-insensitively and with `-` read as `_`. `else` is a raw string in
/// `code_language_raw`, never a guess.
#[must_use]
pub fn code_language_label(language: &str) -> Option<doc::CodeLanguageLabel> {
    let wanted = format!(
        "CODE_LANGUAGE_LABEL_{}",
        language.to_ascii_uppercase().replace('-', "_")
    );
    (1..=61)
        .filter_map(|value| doc::CodeLanguageLabel::try_from(value).ok())
        .find(|label| label.as_str_name() == wanted)
}

/// One karaoke segment as the provenance it is: where the run sits in the
/// media timeline, and which code points of the item's text it covers.
///
/// No page and no box: a caption track has neither, and the two fields this
/// fills are the ones the source actually stated.
fn karaoke_prov(timing: &pb::CueTiming) -> doc::ProvenanceItem {
    doc::ProvenanceItem {
        page_no: 0,
        bbox: None,
        charspan: timing.range.map(|range| doc::IntSpan {
            start: int(range.start),
            end: int(range.end),
        }),
        time: Some(doc::TimeSpan {
            start_ms: ms_as_f64(timing.start_ms),
            end_ms: ms_as_f64(timing.end_ms),
            // One track: this collector is handed one caption file, and
            // numbering it would be inventing a mux it never saw.
            track: None,
            speaker: None,
        }),
        ..doc::ProvenanceItem::default()
    }
}

/// Milliseconds as the schema's floating milliseconds.
fn ms_as_f64(ms: u64) -> f64 {
    // Cue timings are far below 2^53 ms, so the conversion is exact.
    #[allow(clippy::cast_precision_loss)]
    let ms = ms as f64;
    ms
}

/// Milliseconds as the schema's floating seconds.
fn ms_to_seconds(ms: u64) -> f64 {
    // Cue timings are far below 2^53 ms; the division is exact to the
    // millisecond either way.
    #[allow(clippy::cast_precision_loss)]
    let ms = ms as f64;
    ms / 1000.0
}

/// A `MetaValue` tree as the `google.protobuf.Value` the Document plane's
/// custom fields hold: scalar to string, list to `ListValue`, map to
/// `Struct`, recursively. The Document schema is fleet-wide and stays
/// Struct-based; this direction is lossless because every scalar rides as
/// the verbatim string it already was.
fn meta_field(value: &pb::MetaValue) -> Value {
    match value.kind.as_ref() {
        Some(pb::meta_value::Kind::Value(text)) => string(text),
        Some(pb::meta_value::Kind::List(list)) => Value {
            kind: Some(Kind::ListValue(prost_types::ListValue {
                values: list.items.iter().map(meta_field).collect(),
            })),
        },
        Some(pb::meta_value::Kind::Map(map)) => Value {
            kind: Some(Kind::StructValue(prost_types::Struct {
                fields: map
                    .entries
                    .iter()
                    .map(|(key, nested)| (key.clone(), meta_field(nested)))
                    .collect(),
            })),
        },
        // A peer that set no arm said nothing; null records exactly that.
        None => Value {
            kind: Some(Kind::NullValue(0)),
        },
    }
}

/// A `google.protobuf.Value` holding a string.
fn string(text: &str) -> Value {
    Value {
        kind: Some(Kind::StringValue(text.to_owned())),
    }
}

/// A `google.protobuf.Value` holding a boolean.
const fn flag(value: bool) -> Value {
    Value {
        kind: Some(Kind::BoolValue(value)),
    }
}

/// The size a source asks an image to be, in whichever of the schema's two
/// shapes it actually stated.
///
/// Both dimensions as bare numbers are a `Size`. Anything else is a
/// `size_raw`, and `size` stays unset: a percentage, a length in a typeset
/// unit and a fraction of the text width are all things a document can ask
/// for, and none of them is a number that belongs in a unitless double.
///
/// The raw form names its dimensions, `width=..,height=..`, which is
/// verbatim what a LaTeX option list wrote and a faithful transcription of
/// what two HTML attributes said. Naming them is what keeps a lone
/// dimension unambiguous: `width=50%` is not a height.
fn declared_size(image: &pb::ImageBlock) -> (Option<doc::Size>, Option<String>) {
    let declared = |value: &Option<String>| {
        value
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
    };
    let (width, height) = (declared(&image.width), declared(&image.height));
    if let (Some(w), Some(h)) = (bare(width.as_deref()), bare(height.as_deref())) {
        return (
            Some(doc::Size {
                width: w,
                height: h,
            }),
            None,
        );
    }
    let spelled: Vec<String> = [("width", &width), ("height", &height)]
        .into_iter()
        .filter_map(|(name, value)| value.as_ref().map(|value| format!("{name}={value}")))
        .collect();
    (None, (!spelled.is_empty()).then(|| spelled.join(",")))
}

/// The line a block starts on, as the provenance it is.
///
/// The wire states where a block begins and not where it ends, so the range
/// is the one line the fold actually knows about: half-open `[line, line+1)`
/// is "line N", and a fold that ever learns a block's last line widens the
/// end without changing the shape. No page, no box: a markup source has
/// neither, and this is the space it does have.
fn line_prov(line: Option<u32>) -> Vec<doc::ProvenanceItem> {
    line.map(|line| doc::ProvenanceItem {
        page_no: 0,
        bbox: None,
        charspan: None,
        line_range: Some(doc::LineSpan {
            start: line,
            end: line.saturating_add(1),
        }),
        ..doc::ProvenanceItem::default()
    })
    .into_iter()
    .collect()
}

/// A declared dimension as a plain number, or nothing when the source
/// spelled it with a unit, a percentage or a macro. `Size` has no unit
/// beside its two doubles, so anything but a bare number would have to
/// invent one.
fn bare(declared: Option<&str>) -> Option<f64> {
    declared?
        .trim()
        .parse::<f64>()
        .ok()
        .filter(|value| value.is_finite())
}

/// A wire cell as the Document cell the source asked for: its row is the
/// row's place among the rows kept, its column the one the wire placed it
/// at, its spans as declared. [`dense_table_data`] settles the ask against
/// the other cells once the table's shape is known.
fn source_cell(cell: &pb::TableCell, row: usize, header: bool) -> doc::TableCell {
    let row_span = int(cell.row_span.max(1));
    let col_span = int(cell.column_span.max(1));
    let start_row = int_from_usize(row);
    let start_col = int(cell.column_index);
    doc::TableCell {
        row_span,
        col_span,
        start_row_offset_idx: start_row,
        end_row_offset_idx: start_row.saturating_add(row_span),
        start_col_offset_idx: start_col,
        end_col_offset_idx: start_col.saturating_add(col_span),
        text: cell.text.clone(),
        // A cell is a header when its row is one or when the source marked
        // the cell itself.
        column_header: header,
        spans: doc_spans(&cell.spans),
        align: horizontal(cell.align).map(|align| align as i32),
        valign: vertical(cell.valign).map(|valign| valign as i32),
        ..doc::TableCell::default()
    }
}

/// The rows and the columns a cell's offsets cover, as half-open ranges.
fn cell_footprint(cell: &doc::TableCell) -> (Range<usize>, Range<usize>) {
    let at = |offset: i32| usize::try_from(offset).unwrap_or(0);
    (
        at(cell.start_row_offset_idx)..at(cell.end_row_offset_idx),
        at(cell.start_col_offset_idx)..at(cell.end_col_offset_idx),
    )
}

/// The grid is materialised while it holds at most this many positions
/// per real cell, so a table of a few cells with enormous spans does not
/// expand into millions of copies of them. `table_cells` is complete
/// either way; a consumer that needs the grid of such a table derives it
/// from the offsets, which is what the grid states.
const GRID_POSITIONS_PER_CELL: usize = 8;

/// Below this many positions the grid is always materialised: the ratio
/// only matters once a table is large enough for the copies to cost.
const MIN_GRID_POSITIONS: usize = 65_536;

/// The grid is also materialised only while its encoded size stays within
/// this many times the encoded size of the real cells. Positions alone do
/// not bound it: one cell of a few megabytes spanning a thousand columns
/// and sixty rows is well under the position floor and would be copied
/// sixty thousand times.
const GRID_BYTES_PER_CELL_BYTE: usize = 4;

/// Below this many encoded bytes the grid is always materialised, so a
/// small table with one wide header is not denied its grid by the ratio.
const GRID_BYTES_FLOOR: usize = 1 << 20;

/// Above this many encoded bytes the grid is never materialised, whatever
/// the ratio says: the document cap is 256 MiB and the grid of a table
/// that large is a second copy of it nobody can afford.
const GRID_BYTES_CEILING: usize = 64 << 20;

/// One placed cell as the rows under its own see it: the columns it holds,
/// the first row it no longer covers, and its index in `table_cells`.
#[derive(Debug, Clone, Copy)]
struct Lane {
    start: usize,
    end: usize,
    until: usize,
    flat: usize,
}

/// Both shapes of a table's cells from the rows as the source wrote them.
///
/// `table_cells` is every source cell once, in source order, placed so
/// that no two overlap: a cell lands at the first free column at or after
/// the one it asked for, its column span is cut where the next cell on
/// that row begins and its row span where the table ends. The earlier
/// cell wins every contest, which is the HTML table model's own rule and
/// what the per-format folds already apply to their own spans; this is
/// the backstop for a source that states overlapping offsets outright, so
/// that the two shapes agree with each other and with the offsets.
///
/// `grid` is docling's dense reading of those cells: `num_rows` rows of
/// `num_cols` entries, each the cell whose span covers that position (the
/// same cell repeated across every position it spans, rows included), or
/// an empty single-position cell where no span reaches. A renderer walks
/// it without knowing about spans; an analyzer reads `table_cells` and
/// sees each cell once.
///
/// `num_rows` is the number of source rows and `num_cols` the furthest
/// column any cell reaches, so a row span that ran past the last row is
/// clamped rather than inventing rows the source never wrote.
fn dense_table_data(rows: Vec<Vec<doc::TableCell>>) -> doc::TableData {
    let num_rows = rows.len();
    let mut cells: Vec<doc::TableCell> = Vec::with_capacity(rows.iter().map(Vec::len).sum());
    // The cells still covering the row being placed, sorted by column and
    // pairwise disjoint: every cell placed so far whose span reaches this
    // far down, from this row or the ones above.
    let mut open: Vec<Lane> = Vec::new();
    for (row, source) in rows.into_iter().enumerate() {
        open.retain(|lane| lane.until > row);
        for mut cell in source {
            let (asked_rows, asked_cols) = cell_footprint(&cell);
            let height = asked_rows.len().clamp(1, num_rows - row);
            let mut start = asked_cols.start;
            let mut width = asked_cols.len().max(1);
            // Slide right past every lane holding the asked column, then
            // cut the width where the next lane begins.
            let mut at = open.partition_point(|lane| lane.start <= start);
            while at > 0 && open[at - 1].end > start {
                start = open[at - 1].end;
                at = open.partition_point(|lane| lane.start <= start);
            }
            if let Some(next) = open.get(at) {
                width = width.min(next.start - start);
            }
            let end = start.saturating_add(width);
            open.insert(
                at,
                Lane {
                    start,
                    end,
                    until: row + height,
                    flat: cells.len(),
                },
            );
            cell.start_row_offset_idx = int_from_usize(row);
            cell.end_row_offset_idx = int_from_usize(row + height);
            cell.row_span = int_from_usize(height);
            cell.start_col_offset_idx = int_from_usize(start);
            cell.end_col_offset_idx = int_from_usize(end);
            cell.col_span = int_from_usize(width);
            cells.push(cell);
        }
    }
    let num_cols = cells
        .iter()
        .map(|cell| cell_footprint(cell).1.end)
        .max()
        .unwrap_or(0);
    let grid = if grid_affordable(&cells, num_rows, num_cols) {
        dense_grid(&cells, num_rows, num_cols)
    } else {
        Vec::new()
    };
    doc::TableData {
        num_rows: int_from_usize(num_rows),
        num_cols: int_from_usize(num_cols),
        table_cells: cells,
        grid,
        ..doc::TableData::default()
    }
}

/// Whether the dense grid of these placed cells is worth sending: within
/// the position budget and within the byte budget, both stated against
/// the real cells so that a table costs at most a small multiple of
/// itself, never a multiple of what its spans claim.
fn grid_affordable(cells: &[doc::TableCell], num_rows: usize, num_cols: usize) -> bool {
    let positions = num_rows.saturating_mul(num_cols);
    if positions > MIN_GRID_POSITIONS.max(cells.len().saturating_mul(GRID_POSITIONS_PER_CELL)) {
        return false;
    }
    let mut real_bytes = 0usize;
    let mut covered = 0usize;
    let mut grid_bytes = 0usize;
    for cell in cells {
        let (rows, cols) = cell_footprint(cell);
        let area = rows.len().saturating_mul(cols.len());
        let bytes = cell.encoded_len();
        real_bytes = real_bytes.saturating_add(bytes);
        covered = covered.saturating_add(area);
        grid_bytes = grid_bytes.saturating_add(bytes.saturating_mul(area));
    }
    let fillers = positions.saturating_sub(covered);
    grid_bytes = grid_bytes.saturating_add(fillers.saturating_mul(filler_cell(0, 0).encoded_len()));
    grid_bytes <= GRID_BYTES_CEILING
        && grid_bytes <= GRID_BYTES_FLOOR.max(real_bytes.saturating_mul(GRID_BYTES_PER_CELL_BYTE))
}

/// The dense grid over cells already placed without overlap, in row-major
/// order: each cell at every position its footprint covers, fillers
/// everywhere else.
fn dense_grid(cells: &[doc::TableCell], num_rows: usize, num_cols: usize) -> Vec<doc::TableRow> {
    let mut grid = Vec::with_capacity(num_rows);
    let mut open: Vec<Lane> = Vec::new();
    let mut next = 0;
    for row in 0..num_rows {
        open.retain(|lane| lane.until > row);
        // The cells are in row-major order, so the ones starting on this
        // row are the next run of them.
        while let Some(cell) = cells.get(next) {
            let (rows, cols) = cell_footprint(cell);
            if rows.start != row {
                break;
            }
            let lane = Lane {
                start: cols.start,
                end: cols.end,
                until: rows.end,
                flat: next,
            };
            let at = open.partition_point(|open| open.start < lane.start);
            open.insert(at, lane);
            next += 1;
        }
        let mut entries = Vec::with_capacity(num_cols);
        for lane in &open {
            while entries.len() < lane.start {
                entries.push(filler_cell(row, entries.len()));
            }
            for _ in lane.start..lane.end {
                entries.push(cells[lane.flat].clone());
            }
        }
        while entries.len() < num_cols {
            entries.push(filler_cell(row, entries.len()));
        }
        grid.push(doc::TableRow { cells: entries });
    }
    grid
}

/// The empty cell a dense grid shows at a position no span reaches:
/// docling's own filler, one row by one column, with nothing in it.
fn filler_cell(row: usize, col: usize) -> doc::TableCell {
    let (row, col) = (int_from_usize(row), int_from_usize(col));
    doc::TableCell {
        row_span: 1,
        col_span: 1,
        start_row_offset_idx: row,
        end_row_offset_idx: row.saturating_add(1),
        start_col_offset_idx: col,
        end_col_offset_idx: col.saturating_add(1),
        ..doc::TableCell::default()
    }
}

/// A wire `uint32` as the schema's `int32`, saturating rather than
/// wrapping.
fn int(value: u32) -> i32 {
    i32::try_from(value).unwrap_or(i32::MAX)
}

/// A count as the schema's `int32`, saturating rather than wrapping.
fn int_from_usize(value: usize) -> i32 {
    i32::try_from(value).unwrap_or(i32::MAX)
}

/// Everything wrong with a folded fragment, as messages, empty when it is
/// sound.
///
/// This is the merge contract stated as a check, ported from
/// `docling_integrity_errors` in gRParse: the coordinator merges fragments
/// additively by renumbering refs, which only works if every ref is dense,
/// unique and reciprocated. Every fold test asserts this is empty, because
/// a fragment that fails it corrupts a document the fold never sees.
#[must_use]
pub fn integrity_errors(document: &doc::Document) -> Vec<String> {
    let mut walk = Walk::default();
    walk.roots(document);
    walk.arenas(document);
    walk.finish(document)
}

/// One pass over a document's refs, gathering what the checks need: which
/// refs exist, who lists whom as a child, and who claims whom as a parent.
#[derive(Default)]
struct Walk {
    refs: BTreeSet<String>,
    children: BTreeMap<String, BTreeSet<String>>,
    parents: Vec<(String, String)>,
    errors: Vec<String>,
}

impl Walk {
    /// The two groups that exist before any item does.
    fn roots(&mut self, document: &doc::Document) {
        for (name, group) in [
            (BODY_REF, document.body.as_ref()),
            (FURNITURE_REF, document.furniture.as_ref()),
        ] {
            self.refs.insert(name.to_owned());
            let entry = self.children.entry(name.to_owned()).or_default();
            if let Some(root) = group {
                for child in &root.children {
                    entry.insert(child.r#ref.clone());
                }
            } else {
                self.errors
                    .push(format!("the document has no {name} group"));
            }
        }
    }

    /// Every arena, in the order the refs number them.
    fn arenas(&mut self, document: &doc::Document) {
        for (index, group) in document.groups.iter().enumerate() {
            let expected = format!("#/groups/{index}");
            self.item(
                &group.self_ref,
                &expected,
                &group.children,
                group.parent.as_ref(),
            );
        }
        for (index, item) in document.texts.iter().enumerate() {
            let expected = format!("#/texts/{index}");
            match item.item.as_ref() {
                // CodeItem inlines its base fields, so it is walked
                // directly rather than through `text_base`.
                Some(doc::base_text_item::Item::Code(code)) => {
                    self.item(
                        &code.self_ref,
                        &expected,
                        &code.children,
                        code.parent.as_ref(),
                    );
                }
                Some(other) => match text_base(other) {
                    Some(base) => {
                        self.item(
                            &base.self_ref,
                            &expected,
                            &base.children,
                            base.parent.as_ref(),
                        );
                    }
                    None => self
                        .errors
                        .push(format!("text item {expected} has no base")),
                },
                None => self
                    .errors
                    .push(format!("text item {expected} has no variant set")),
            }
        }
        for (index, picture) in document.pictures.iter().enumerate() {
            let expected = format!("#/pictures/{index}");
            self.item(
                &picture.self_ref,
                &expected,
                &picture.children,
                picture.parent.as_ref(),
            );
        }
        for (index, table) in document.tables.iter().enumerate() {
            let expected = format!("#/tables/{index}");
            self.item(
                &table.self_ref,
                &expected,
                &table.children,
                table.parent.as_ref(),
            );
        }
    }

    /// One item: its ref must be present, unique and exactly its position.
    fn item(
        &mut self,
        self_ref: &str,
        expected: &str,
        children: &[doc::RefItem],
        parent: Option<&doc::RefItem>,
    ) {
        if self_ref.is_empty() {
            self.errors
                .push(format!("item at {expected} has an empty self_ref"));
            return;
        }
        if self_ref != expected {
            self.errors.push(format!(
                "self_ref {self_ref} does not match its arena position {expected}"
            ));
        }
        if !self.refs.insert(self_ref.to_owned()) {
            self.errors.push(format!("duplicate self_ref {self_ref}"));
        }
        let entry = self.children.entry(self_ref.to_owned()).or_default();
        for child in children {
            entry.insert(child.r#ref.clone());
        }
        match parent {
            Some(parent) => self
                .parents
                .push((self_ref.to_owned(), parent.r#ref.clone())),
            None => self
                .errors
                .push(format!("{self_ref} has no parent, so nothing reaches it")),
        }
    }

    /// Resolve every ref gathered, and every caption a table or picture
    /// points at.
    fn finish(mut self, document: &doc::Document) -> Vec<String> {
        for (parent, listed) in &self.children {
            for child in listed {
                if !self.refs.contains(child) {
                    self.errors
                        .push(format!("child {child} of {parent} does not resolve"));
                }
            }
        }
        for (child, parent) in &self.parents {
            if !self.refs.contains(parent) {
                self.errors
                    .push(format!("parent {parent} of {child} does not resolve"));
                continue;
            }
            if !self
                .children
                .get(parent)
                .is_some_and(|listed| listed.contains(child))
            {
                self.errors
                    .push(format!("parent {parent} does not list {child} as a child"));
            }
        }
        let captions = document
            .tables
            .iter()
            .map(|table| (table.self_ref.as_str(), table.captions.as_slice()))
            .chain(
                document
                    .pictures
                    .iter()
                    .map(|picture| (picture.self_ref.as_str(), picture.captions.as_slice())),
            );
        for (owner, listed) in captions {
            for caption in listed {
                if !self.refs.contains(&caption.r#ref) {
                    self.errors.push(format!(
                        "caption {} of {owner} does not resolve",
                        caption.r#ref
                    ));
                }
            }
        }
        self.errors
    }
}

/// What the source declares about itself, in the Document plane's own
/// first-class slots.
///
/// The wire delivers metadata keyed as the source spelled it, so the names
/// are matched case-insensitively and with a format's own prefix (the HTML
/// fold's `meta.`) stripped. A key with a first-class slot lands in it; a
/// scalar with none lands in `extra` under its own name. A key whose value
/// is a tree has no honest string form and stays where it already is, in
/// the body's custom fields, which carry the whole tree either way.
fn source_meta(
    title: Option<&str>,
    metadata: &HashMap<String, pb::MetaValue>,
) -> Option<doc::DocumentMeta> {
    let mut meta = doc::DocumentMeta {
        title: title.filter(|title| !title.is_empty()).map(str::to_owned),
        ..doc::DocumentMeta::default()
    };
    // Deterministic order for a map the wire delivers unordered.
    for (key, value) in metadata.iter().collect::<BTreeMap<_, _>>() {
        let name = key.rsplit('.').next().unwrap_or(key).to_ascii_lowercase();
        let slot = match name.as_str() {
            "title" | "doctitle" => &mut meta.title,
            "date" | "created" | "revdate" | "published" => {
                if meta.created_raw.is_none()
                    && let Some(text) = scalar_of(value)
                {
                    let (instant, civil) = declared_time(&text);
                    meta.created = instant;
                    meta.created_civil = civil;
                    meta.created_raw = Some(text);
                }
                continue;
            }
            "modified" | "updated" | "lastmod" => {
                if meta.modified_raw.is_none()
                    && let Some(text) = scalar_of(value)
                {
                    let (instant, civil) = declared_time(&text);
                    meta.modified = instant;
                    meta.modified_civil = civil;
                    meta.modified_raw = Some(text);
                }
                continue;
            }
            "lang" | "language" | "content-language" => &mut meta.language,
            "generator" => &mut meta.generator,
            "author" | "authors" | "creator" => {
                // An author list stays a list: one entry per author, never
                // one string with separators in it.
                meta.authors.extend(each_scalar(value));
                continue;
            }
            "keywords" | "tags" | "categories" => {
                meta.keywords.extend(keyword_list(value));
                continue;
            }
            _ => {
                // A meta tag already has a typed home in `meta_tags`; it
                // does not need a second one here.
                if !key.starts_with("meta.")
                    && let Some(text) = scalar_of(value)
                {
                    meta.extra.insert(key.clone(), text);
                }
                continue;
            }
        };
        if slot.is_none() {
            *slot = scalar_of(value);
        }
    }
    (meta != doc::DocumentMeta::default()).then_some(meta)
}

/// The page-level meta tags a web source declares.
///
/// `meta.<name>` is the HTML fold's own namespace for a `<meta>` element,
/// stated in the contract's comment on `MarkupInfo.metadata`, so a key in
/// it is a meta tag and belongs in the typed list rather than in a
/// free-form map.
fn meta_tags(metadata: &HashMap<String, pb::MetaValue>) -> Vec<doc::MetaTag> {
    metadata
        .iter()
        .collect::<BTreeMap<_, _>>()
        .into_iter()
        .filter_map(|(key, value)| {
            let name = key.strip_prefix("meta.").filter(|name| !name.is_empty())?;
            Some(doc::MetaTag {
                name: name.to_owned(),
                content: scalar_of(value)?,
            })
        })
        .collect()
}

/// A declared date as the two typed shapes the schema distinguishes.
///
/// A source that states an offset states an instant, and that is a
/// `Timestamp`. A source that writes `1843-10-01`, or a date and a
/// wall-clock time with no zone, has stated no instant, and inventing one
/// would be inventing a timezone; that is a `CivilDateTime`. Anything the
/// grammar does not cover parses to neither, and the caller keeps the
/// source's own spelling in the `_raw` twin either way.
fn declared_time(text: &str) -> (Option<prost_types::Timestamp>, Option<doc::CivilDateTime>) {
    let Some(parsed) = Iso8601::parse(text.trim()) else {
        return (None, None);
    };
    let Some(offset) = parsed.utc_offset_minutes else {
        return (None, Some(parsed.civil()));
    };
    let seconds = days_from_civil(
        i64::from(parsed.year),
        i64::from(parsed.month),
        i64::from(parsed.day),
    ) * 86_400
        + i64::from(parsed.hour) * 3_600
        + i64::from(parsed.minute) * 60
        + i64::from(parsed.second)
        - i64::from(offset) * 60;
    (
        Some(prost_types::Timestamp {
            seconds,
            nanos: parsed.nanos,
        }),
        None,
    )
}

/// One parsed ISO 8601 date, optionally with a time and an offset.
struct Iso8601 {
    year: i32,
    month: i32,
    day: i32,
    hour: i32,
    minute: i32,
    second: i32,
    nanos: i32,
    has_time: bool,
    utc_offset_minutes: Option<i32>,
}

impl Iso8601 {
    /// `YYYY-MM-DD`, optionally `THH:MM[:SS[.fff]]`, optionally `Z` or
    /// `+HH:MM`. Nothing looser: a date this cannot read is a date whose
    /// meaning this fold does not know.
    fn parse(text: &str) -> Option<Self> {
        let (date, rest) = text.split_at_checked(10)?;
        let mut parts = date.split('-');
        let year = field(parts.next()?, 4)?;
        let month = field(parts.next()?, 2)?;
        let day = field(parts.next()?, 2)?;
        if parts.next().is_some() || !(1..=12).contains(&month) || !(1..=31).contains(&day) {
            return None;
        }
        let mut parsed = Self {
            year,
            month,
            day,
            hour: 0,
            minute: 0,
            second: 0,
            nanos: 0,
            has_time: false,
            utc_offset_minutes: None,
        };
        if rest.is_empty() {
            return Some(parsed);
        }
        let rest = rest.strip_prefix(['T', 't', ' '])?;
        let (time, zone) = match rest.find(['Z', 'z', '+']) {
            Some(at) => rest.split_at(at),
            // A minus inside the time is impossible, so the first one is
            // the offset sign.
            None => match rest.rfind('-') {
                Some(at) => rest.split_at(at),
                None => (rest, ""),
            },
        };
        let (time, fraction) = time.split_once('.').unwrap_or((time, ""));
        let mut units = time.split(':');
        parsed.hour = field(units.next()?, 2)?;
        parsed.minute = field(units.next()?, 2)?;
        parsed.second = units.next().map_or(Some(0), |unit| field(unit, 2))?;
        if units.next().is_some() || parsed.hour > 23 || parsed.minute > 59 || parsed.second > 60 {
            return None;
        }
        if !fraction.is_empty() {
            let digits: String = fraction.chars().take(9).collect();
            if !digits.chars().all(|c| c.is_ascii_digit()) {
                return None;
            }
            let scale = 10i64.pow(9 - u32::try_from(digits.len()).ok()?);
            parsed.nanos = i32::try_from(digits.parse::<i64>().ok()? * scale).ok()?;
        }
        parsed.has_time = true;
        parsed.utc_offset_minutes = parse_offset(zone)?;
        Some(parsed)
    }

    /// The wall-clock shape, with the time only when the source wrote one.
    fn civil(&self) -> doc::CivilDateTime {
        doc::CivilDateTime {
            year: self.year,
            month: self.month,
            day: self.day,
            hour: self.has_time.then_some(self.hour),
            minute: self.has_time.then_some(self.minute),
            second: self.has_time.then_some(self.second),
            nanos: (self.has_time && self.nanos != 0).then_some(self.nanos),
            // Unset: an offset means an instant, and an instant is a
            // Timestamp rather than a civil value.
            utc_offset_minutes: None,
        }
    }
}

/// The offset a timestamp states, `None` when it states none. The outer
/// `Option` is the parse: an unreadable zone is not an offset of zero.
fn parse_offset(zone: &str) -> Option<Option<i32>> {
    if zone.is_empty() {
        return Some(None);
    }
    if zone.eq_ignore_ascii_case("z") {
        return Some(Some(0));
    }
    let sign = match zone.chars().next()? {
        '+' => 1,
        '-' => -1,
        _ => return None,
    };
    let digits: String = zone[1..].chars().filter(|c| *c != ':').collect();
    let (hours, minutes) = match digits.len() {
        2 => (field(&digits, 2)?, 0),
        4 => (field(&digits[..2], 2)?, field(&digits[2..], 2)?),
        _ => return None,
    };
    if hours > 23 || minutes > 59 {
        return None;
    }
    Some(Some(sign * (hours * 60 + minutes)))
}

/// A zero-padded decimal field of exactly `width` digits.
fn field(text: &str, width: usize) -> Option<i32> {
    if text.len() != width || !text.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    text.parse().ok()
}

/// Days from the Unix epoch to a proleptic Gregorian date, by Howard
/// Hinnant's `days_from_civil`.
const fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let month_index = (month + 9) % 12;
    let day_of_year = (153 * month_index + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

/// One metadata value as verbatim text, or nothing when it is a tree.
fn scalar_of(value: &pb::MetaValue) -> Option<String> {
    match value.kind.as_ref() {
        Some(pb::meta_value::Kind::Value(text)) if !text.is_empty() => Some(text.clone()),
        _ => None,
    }
}

/// Every scalar a value holds: one for a scalar, one per item for a
/// sequence, none for a mapping.
fn each_scalar(value: &pb::MetaValue) -> Vec<String> {
    match value.kind.as_ref() {
        Some(pb::meta_value::Kind::Value(text)) if !text.is_empty() => vec![text.clone()],
        Some(pb::meta_value::Kind::List(list)) => list.items.iter().filter_map(scalar_of).collect(),
        _ => Vec::new(),
    }
}

/// A keyword list: a sequence stays a sequence, and a scalar splits on
/// commas, which is what both an HTML `keywords` meta tag and a
/// front-matter `tags: a, b` line mean by one.
fn keyword_list(value: &pb::MetaValue) -> Vec<String> {
    match value.kind.as_ref() {
        Some(pb::meta_value::Kind::Value(text)) => text
            .split(',')
            .map(str::trim)
            .filter(|keyword| !keyword.is_empty())
            .map(str::to_owned)
            .collect(),
        _ => each_scalar(value),
    }
}

/// One declared column as the Document plane's own column schema.
///
/// A markup table declares geometry, not a schema: there is no column name
/// to state, and `name` is presence-tracked precisely so a geometry-only
/// column says nothing rather than saying "".
fn column_schema(column: &pb::ColumnSpec) -> doc::TableColumnSchema {
    // A bare number is a page-unit width; `20%` and `5cm` are spellings
    // the schema keeps verbatim rather than mangling into one.
    let declared = column
        .width
        .as_deref()
        .map(str::trim)
        .filter(|width| !width.is_empty());
    let width = declared.and_then(|width| width.parse::<f64>().ok());
    doc::TableColumnSchema {
        name: None,
        width,
        width_raw: declared.filter(|_| width.is_none()).map(str::to_owned),
        align: horizontal(column.align).map(|align| align as i32),
        valign: vertical(column.valign).map(|valign| valign as i32),
        ..doc::TableColumnSchema::default()
    }
}

/// The horizontal half of the wire's one alignment vocabulary. The wire
/// merges both axes into one enum and the folds keep them in the right
/// field; a vertical value in a horizontal slot is not an alignment here.
fn horizontal(alignment: i32) -> Option<doc::Alignment> {
    match pb::Alignment::try_from(alignment).ok()? {
        pb::Alignment::Left => Some(doc::Alignment::Left),
        pb::Alignment::Center => Some(doc::Alignment::Center),
        pb::Alignment::Right => Some(doc::Alignment::Right),
        pb::Alignment::Justify => Some(doc::Alignment::Justify),
        pb::Alignment::Unspecified
        | pb::Alignment::Top
        | pb::Alignment::Middle
        | pb::Alignment::Bottom => None,
    }
}

/// The vertical half of the same vocabulary.
fn vertical(alignment: i32) -> Option<doc::VerticalAlignment> {
    match pb::Alignment::try_from(alignment).ok()? {
        pb::Alignment::Top => Some(doc::VerticalAlignment::Top),
        pb::Alignment::Middle => Some(doc::VerticalAlignment::Middle),
        pb::Alignment::Bottom => Some(doc::VerticalAlignment::Bottom),
        pb::Alignment::Unspecified
        | pb::Alignment::Left
        | pb::Alignment::Center
        | pb::Alignment::Right
        | pb::Alignment::Justify => None,
    }
}

/// One wire run as the Document plane's own span.
///
/// `target` stays empty here: a cross reference points at an item, and
/// which item that is only becomes knowable once the whole parse has been
/// seen, so [`DocumentFold::resolve_references`] fills it at the end.
fn doc_span(span: &pb::InlineSpan) -> doc::InlineSpan {
    let styles: Vec<pb::SpanStyle> = span
        .styles
        .iter()
        .filter_map(|style| pb::SpanStyle::try_from(*style).ok())
        .collect();
    let has = |wanted: pb::SpanStyle| styles.contains(&wanted);
    let script = if has(pb::SpanStyle::Superscript) {
        doc::Script::Super
    } else if has(pb::SpanStyle::Subscript) {
        doc::Script::Sub
    } else {
        doc::Script::Unspecified
    };
    let formatting = doc::Formatting {
        bold: has(pb::SpanStyle::Bold),
        italic: has(pb::SpanStyle::Italic),
        underline: has(pb::SpanStyle::Underline),
        strikethrough: has(pb::SpanStyle::Strikethrough),
        script: script as i32,
        script_raw: None,
        monospace: has(pb::SpanStyle::Code),
        small_caps: has(pb::SpanStyle::SmallCaps),
        math: has(pb::SpanStyle::Math),
        mark: has(pb::SpanStyle::Mark),
        small: has(pb::SpanStyle::Small),
        insertion: has(pb::SpanStyle::Insertion),
        abbreviation: has(pb::SpanStyle::Abbreviation),
        quote: has(pb::SpanStyle::Quote),
        // No overline style on this wire.
        overline: false,
    };
    doc::InlineSpan {
        range: span.range.map(|range| doc::IntSpan {
            start: int(range.start),
            end: int(range.end),
        }),
        // A run that says nothing about its typography carries no
        // formatting rather than an all-false one.
        formatting: (formatting != doc::Formatting::default()).then_some(formatting),
        hyperlink: span.hyperlink.clone(),
        target: None,
        font_family: span.font_family.clone(),
        font_size_pt: span.font_size_pt,
        color: span.color.clone(),
        language: span.language.clone(),
        field_code: span.field_code.clone(),
        reference_kind: reference_kind(span.reference_kind).map(|kind| kind as i32),
        // The key rides along whether or not it resolves: a document that
        // refers to an anchor it never declares still said what it meant.
        reference: span.reference.clone(),
        link_title: span.link_title.clone(),
        raw: span.raw.clone(),
        style_name: span.style_name.clone(),
        // No highlight colours on this wire: no format in this build states
        // one as a colour rather than as a `mark`.
        highlight_color: None,
        annotation: span.annotation.clone(),
    }
}

/// The Document plane's own name for what a reference points at.
///
/// A bibliography entry is a citation; everything else has its own arm on
/// both sides.
fn reference_kind(kind: i32) -> Option<doc::ReferenceKind> {
    match pb::ReferenceKind::try_from(kind).ok()? {
        pb::ReferenceKind::Unspecified => None,
        pb::ReferenceKind::Footnote => Some(doc::ReferenceKind::Footnote),
        pb::ReferenceKind::Citation | pb::ReferenceKind::Bibliography => {
            Some(doc::ReferenceKind::Citation)
        }
        pb::ReferenceKind::CrossRef => Some(doc::ReferenceKind::CrossRef),
        pb::ReferenceKind::EquationRef => Some(doc::ReferenceKind::Equation),
    }
}

/// Every wire run of one text, in the order the wire sent them.
fn doc_spans(spans: &[pb::InlineSpan]) -> Vec<doc::InlineSpan> {
    spans.iter().map(doc_span).collect()
}

/// The admonition kind's name in the Document plane's own lower-case
/// vocabulary, or `None` when the block is not an admonition.
fn admonition_name(kind: i32) -> Option<&'static str> {
    match pb::AdmonitionKind::try_from(kind).ok()? {
        pb::AdmonitionKind::Unspecified => None,
        pb::AdmonitionKind::Note => Some("note"),
        pb::AdmonitionKind::Tip => Some("tip"),
        pb::AdmonitionKind::Important => Some("important"),
        pb::AdmonitionKind::Warning => Some("warning"),
        pb::AdmonitionKind::Caution => Some("caution"),
    }
}

/// The shared base of every text variant that has one. `CodeItem` has
/// none: its base fields are inlined.
fn text_base(item: &doc::base_text_item::Item) -> Option<&doc::TextItemBase> {
    match item {
        doc::base_text_item::Item::Title(i) => i.base.as_ref(),
        doc::base_text_item::Item::SectionHeader(i) => i.base.as_ref(),
        doc::base_text_item::Item::ListItem(i) => i.base.as_ref(),
        doc::base_text_item::Item::Formula(i) => i.base.as_ref(),
        doc::base_text_item::Item::Text(i) => i.base.as_ref(),
        doc::base_text_item::Item::FieldHeading(i) => i.base.as_ref(),
        doc::base_text_item::Item::FieldValue(i) => i.base.as_ref(),
        doc::base_text_item::Item::Code(_) => None,
    }
}

/// The mutable shared base of a text variant, for the passes that finish an
/// item after it was appended. `CodeItem` has none, and carries no spans.
fn text_base_mut(item: &mut doc::base_text_item::Item) -> Option<&mut doc::TextItemBase> {
    match item {
        doc::base_text_item::Item::Title(i) => i.base.as_mut(),
        doc::base_text_item::Item::SectionHeader(i) => i.base.as_mut(),
        doc::base_text_item::Item::ListItem(i) => i.base.as_mut(),
        doc::base_text_item::Item::Formula(i) => i.base.as_mut(),
        doc::base_text_item::Item::Text(i) => i.base.as_mut(),
        doc::base_text_item::Item::FieldHeading(i) => i.base.as_mut(),
        doc::base_text_item::Item::FieldValue(i) => i.base.as_mut(),
        doc::base_text_item::Item::Code(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn image_mimetype_derives_from_the_source() {
        assert_eq!(image_mimetype("figures/engine.png"), "image/png");
        assert_eq!(
            image_mimetype("https://a.example/x/y.JPEG?w=200#frag"),
            "image/jpeg"
        );
        assert_eq!(image_mimetype("chart.svg"), "image/svg+xml");
        assert_eq!(image_mimetype("data:image/webp;base64,AAAA"), "image/webp");
        assert_eq!(
            image_mimetype("data:image/x-exotic,payload"),
            "application/octet-stream"
        );
        // No recognizable extension: a generic binary type, never empty.
        assert_eq!(
            image_mimetype("https://cdn.example/asset/12345"),
            "application/octet-stream"
        );
        assert_eq!(image_mimetype(""), "application/octet-stream");
    }

    fn block(label: pb::BlockLabel, text: &str) -> pb::parse_markup_response::Event {
        pb::parse_markup_response::Event::TextBlock(pb::TextBlock {
            label: label as i32,
            text: text.to_owned(),
            ..pb::TextBlock::default()
        })
    }

    fn list_item(text: &str, depth: u32, enumerated: bool) -> pb::parse_markup_response::Event {
        pb::parse_markup_response::Event::TextBlock(pb::TextBlock {
            label: pb::BlockLabel::ListItem as i32,
            text: text.to_owned(),
            enumerated,
            list_depth: Some(depth),
            marker: Some(if enumerated { "1." } else { "-" }.to_owned()),
            ..pb::TextBlock::default()
        })
    }

    fn fold(events: &[pb::parse_markup_response::Event]) -> doc::Document {
        let mut fold = DocumentFold::new();
        for event in events {
            fold.consume(event);
        }
        let document = fold.take();
        assert_eq!(
            integrity_errors(&document),
            Vec::<String>::new(),
            "every fold test asserts a merge-safe fragment"
        );
        document
    }

    fn info(format: pb::MarkupFormat) -> pb::parse_markup_response::Event {
        pb::parse_markup_response::Event::Info(pb::MarkupInfo {
            format: format as i32,
            evidence: pb::FormatEvidence::Requested as i32,
            title: Some("Named".to_owned()),
            metadata: std::collections::HashMap::from([(
                "author".to_owned(),
                crate::parse::scalar("Ada"),
            )]),
        })
    }

    #[test]
    fn an_empty_fold_is_a_sound_empty_fragment() {
        let document = fold(&[]);
        assert_eq!(document.schema_name.as_deref(), Some(SCHEMA_NAME));
    }

    #[test]
    fn info_names_the_document_and_lands_metadata_on_the_body() {
        let document = fold(&[info(pb::MarkupFormat::Markdown)]);
        assert_eq!(document.name, "Named");
        assert_eq!(
            document.origin.as_ref().map(|o| o.mimetype.as_str()),
            Some("text/markdown")
        );
        let meta = document.body.as_ref().unwrap().meta.as_ref().unwrap();
        assert_eq!(
            meta.custom_fields["markdown.author"].kind,
            Some(Kind::StringValue("Ada".to_owned()))
        );
    }

    #[test]
    fn a_metadata_tree_lands_as_the_same_tree_of_struct_values() {
        let tree = pb::MetaValue {
            kind: Some(pb::meta_value::Kind::List(pb::MetaList {
                items: vec![pb::MetaValue {
                    kind: Some(pb::meta_value::Kind::Map(pb::MetaMap {
                        entries: std::collections::HashMap::from([(
                            "name".to_owned(),
                            crate::parse::scalar("Ada Lovelace"),
                        )]),
                    })),
                }],
            })),
        };
        let document = fold(&[pb::parse_markup_response::Event::Info(pb::MarkupInfo {
            format: pb::MarkupFormat::Markdown as i32,
            evidence: pb::FormatEvidence::Requested as i32,
            title: None,
            metadata: std::collections::HashMap::from([("authors".to_owned(), tree)]),
        })]);
        let meta = document.body.as_ref().unwrap().meta.as_ref().unwrap();
        let Some(Kind::ListValue(authors)) = meta.custom_fields["markdown.authors"].kind.as_ref()
        else {
            panic!("a MetaList folds as a ListValue");
        };
        let Some(Kind::StructValue(author)) = authors.values[0].kind.as_ref() else {
            panic!("a MetaMap folds as a Struct");
        };
        assert_eq!(
            author.fields["name"].kind,
            Some(Kind::StringValue("Ada Lovelace".to_owned())),
            "the scalar leaf is the verbatim string, not a JSON encoding"
        );
    }

    #[test]
    fn a_heading_parents_the_content_that_follows_it() {
        let document = fold(&[
            block(pb::BlockLabel::SectionHeader, "Intro"),
            block(pb::BlockLabel::Paragraph, "under it"),
        ]);
        let Some(doc::base_text_item::Item::Text(paragraph)) = document.texts[1].item.as_ref()
        else {
            panic!("a paragraph folds to a TextItem");
        };
        assert_eq!(
            paragraph
                .base
                .as_ref()
                .unwrap()
                .parent
                .as_ref()
                .unwrap()
                .r#ref,
            "#/texts/0"
        );
    }

    #[test]
    fn a_contiguous_list_becomes_one_group_and_nesting_nests() {
        let document = fold(&[
            list_item("top one", 1, false),
            list_item("nested a", 2, false),
            list_item("nested b", 2, false),
            list_item("top two", 1, false),
            block(pb::BlockLabel::Paragraph, "breaks the list"),
            list_item("a new list", 1, true),
        ]);
        assert_eq!(document.groups.len(), 3, "outer, nested, and the new one");
        assert_eq!(document.groups[0].label, doc::GroupLabel::List as i32);
        assert_eq!(
            document.groups[1].parent.as_ref().unwrap().r#ref,
            "#/groups/0",
            "the nested group hangs off the outer one"
        );
        assert_eq!(
            document.groups[2].label,
            doc::GroupLabel::OrderedList as i32
        );
        assert_eq!(
            document.groups[2].parent.as_ref().unwrap().r#ref,
            BODY_REF,
            "the paragraph closed the first list"
        );
        let Some(doc::base_text_item::Item::ListItem(item)) = document.texts[0].item.as_ref()
        else {
            panic!("list items fold as ListItem");
        };
        assert_eq!(item.marker.as_deref(), Some("-"));
        assert!(!item.enumerated);
    }

    #[test]
    fn switching_list_kind_at_the_same_depth_opens_a_new_group() {
        let document = fold(&[
            list_item("bulleted", 1, false),
            list_item("numbered", 1, true),
        ]);
        assert_eq!(document.groups.len(), 2);
        assert_eq!(document.groups[0].label, doc::GroupLabel::List as i32);
        assert_eq!(
            document.groups[1].label,
            doc::GroupLabel::OrderedList as i32
        );
    }

    #[test]
    fn quote_folds_as_text_with_the_quotation_recorded() {
        let document = fold(&[block(pb::BlockLabel::Quote, "quoted words")]);
        let Some(doc::base_text_item::Item::Text(item)) = document.texts[0].item.as_ref() else {
            panic!("quotes fold as TextItem");
        };
        let base = item.base.as_ref().unwrap();
        assert_eq!(base.label, doc::DocItemLabel::Text as i32);
        assert_eq!(
            base.meta.as_ref().unwrap().custom_fields["markup.quote"].kind,
            Some(Kind::BoolValue(true))
        );
    }

    #[test]
    fn code_maps_a_known_language_to_the_enum_and_an_unknown_to_raw() {
        let events = [
            pb::parse_markup_response::Event::TextBlock(pb::TextBlock {
                label: pb::BlockLabel::Code as i32,
                text: "fn main() {}".to_owned(),
                language: Some("rust".to_owned()),
                ..pb::TextBlock::default()
            }),
            pb::parse_markup_response::Event::TextBlock(pb::TextBlock {
                label: pb::BlockLabel::Code as i32,
                text: "PRINT 1".to_owned(),
                language: Some("qbasic".to_owned()),
                ..pb::TextBlock::default()
            }),
        ];
        let document = fold(&events);
        let Some(doc::base_text_item::Item::Code(rust)) = document.texts[0].item.as_ref() else {
            panic!("code folds as CodeItem");
        };
        assert_eq!(
            rust.code_language,
            Some(doc::CodeLanguageLabel::Rust as i32)
        );
        assert!(rust.code_language_raw.is_none());
        let Some(doc::base_text_item::Item::Code(qbasic)) = document.texts[1].item.as_ref() else {
            panic!("code folds as CodeItem");
        };
        assert!(qbasic.code_language.is_none());
        assert_eq!(qbasic.code_language_raw.as_deref(), Some("qbasic"));
    }

    #[test]
    fn a_table_streams_into_grid_and_flat_cells_with_its_caption_referenced() {
        let mut fold_state = DocumentFold::new();
        fold_state.consume(&pb::parse_markup_response::Event::TableStart(
            pb::TableStart {
                index: 0,
                table_ref: "t0".to_owned(),
                caption: Some("Numbers".to_owned()),
                source_line: Some(3),
                source: None,
                columns: Vec::new(),
                anchor_id: None,
            },
        ));
        fold_state.consume(&pb::parse_markup_response::Event::TableRow(pb::TableRow {
            table_ref: "t0".to_owned(),
            row_index: 0,
            is_header: true,
            cells: vec![
                pb::TableCell {
                    column_index: 0,
                    text: "a".to_owned(),
                    column_span: 1,
                    row_span: 1,
                    is_header: true,
                    ..pb::TableCell::default()
                },
                pb::TableCell {
                    column_index: 1,
                    text: "b".to_owned(),
                    column_span: 2,
                    row_span: 1,
                    is_header: true,
                    ..pb::TableCell::default()
                },
            ],
        }));
        fold_state.consume(&pb::parse_markup_response::Event::TableEnd(pb::TableEnd {
            table_ref: "t0".to_owned(),
            row_count: 1,
            column_count: 3,
        }));
        let document = fold_state.take();
        assert!(integrity_errors(&document).is_empty());
        let table = &document.tables[0];
        let data = table.data.as_ref().unwrap();
        assert_eq!(data.num_rows, 1);
        assert_eq!(data.num_cols, 3, "spans widen the table");
        assert_eq!(data.grid[0].cells[1].end_col_offset_idx, 3);
        assert!(data.grid[0].cells[0].column_header);
        assert_eq!(table.captions[0].r#ref, "#/texts/0");
        let Some(doc::base_text_item::Item::Text(caption)) = document.texts[0].item.as_ref() else {
            panic!("the caption is its own item");
        };
        assert_eq!(
            caption.base.as_ref().unwrap().label,
            doc::DocItemLabel::Caption as i32
        );
    }

    #[test]
    fn a_cue_folds_as_text_with_track_provenance() {
        let document = fold(&[
            info(pb::MarkupFormat::Vtt),
            pb::parse_markup_response::Event::Cue(pb::Cue {
                index: 0,
                cue_id: Some("intro".to_owned()),
                start_ms: 1_500,
                end_ms: 4_000,
                speaker: "Ada".to_owned(),
                text: "Hello".to_owned(),
                source: None,
                spans: Vec::new(),
                timings: Vec::new(),
            }),
        ]);
        let Some(doc::base_text_item::Item::Text(item)) = document.texts[0].item.as_ref() else {
            panic!("cues fold as TextItem");
        };
        let base = item.base.as_ref().unwrap();
        assert_eq!(base.source.len(), 2, "track provenance plus the collector");
        let Some(doc::source_type::Source::Track(track)) = base.source[0].source.as_ref() else {
            panic!("the first source is the track");
        };
        assert!((track.start_time - 1.5).abs() < f64::EPSILON);
        assert!((track.end_time - 4.0).abs() < f64::EPSILON);
        assert_eq!(track.identifier.as_deref(), Some("intro"));
        assert_eq!(track.voice.as_deref(), Some("Ada"));
        let Some(doc::source_type::Source::Collector(collector)) = base.source[1].source.as_ref()
        else {
            panic!("the second source is this collector");
        };
        assert_eq!(collector.collector, COLLECTOR);
        assert_eq!(collector.model.as_deref(), Some("vtt"));
    }

    #[test]
    fn an_image_becomes_a_picture_whose_uri_is_the_src_verbatim() {
        let document = fold(&[
            info(pb::MarkupFormat::Markdown),
            pb::parse_markup_response::Event::Image(pb::ImageBlock {
                index: 0,
                src: "figures/pipeline.png".to_owned(),
                alt: "the pipeline".to_owned(),
                title: "Fig 1".to_owned(),
                source_line: Some(9),
                source: None,
                width: None,
                height: None,
                anchor_id: None,
                hyperlink: None,
            }),
        ]);
        let picture = &document.pictures[0];
        assert_eq!(picture.image.as_ref().unwrap().uri, "figures/pipeline.png");
        assert!(picture.image.as_ref().unwrap().size.is_none());
        let fields = &picture.meta.as_ref().unwrap().custom_fields;
        assert_eq!(
            fields["markdown.alt"].kind,
            Some(Kind::StringValue("the pipeline".to_owned()))
        );
    }

    #[test]
    fn a_caption_beside_a_picture_binds_to_it_and_a_separated_one_does_not() {
        let image = |src: &str| {
            pb::parse_markup_response::Event::Image(pb::ImageBlock {
                src: src.to_owned(),
                ..pb::ImageBlock::default()
            })
        };
        let document = fold(&[
            info(pb::MarkupFormat::Html),
            image("after.png"),
            block(pb::BlockLabel::Caption, "after"),
            block(pb::BlockLabel::Caption, "before"),
            image("before.png"),
            image("alone.png"),
            block(pb::BlockLabel::Paragraph, "between"),
            block(pb::BlockLabel::Caption, "stray"),
        ]);
        assert_eq!(integrity_errors(&document), Vec::<String>::new());
        assert_eq!(document.pictures[0].captions[0].r#ref, "#/texts/0");
        assert_eq!(document.pictures[1].captions[0].r#ref, "#/texts/1");
        assert!(document.pictures[2].captions.is_empty());
        assert_eq!(
            document.texts.len(),
            4,
            "the stray caption is still an item"
        );
    }

    #[test]
    fn a_linked_image_and_a_linked_caption_carry_their_targets() {
        let document = fold(&[
            info(pb::MarkupFormat::Html),
            pb::parse_markup_response::Event::Image(pb::ImageBlock {
                src: "plate.png".to_owned(),
                hyperlink: Some("https://example.com/plate".to_owned()),
                ..pb::ImageBlock::default()
            }),
            pb::parse_markup_response::Event::TextBlock(pb::TextBlock {
                label: pb::BlockLabel::Caption as i32,
                text: "Photo by Ada.".to_owned(),
                hyperlink: Some("https://ada.example".to_owned()),
                ..pb::TextBlock::default()
            }),
        ]);
        let picture = &document.pictures[0];
        assert_eq!(
            picture.hyperlink.as_deref(),
            Some("https://example.com/plate"),
            "the link that wrapped the image lands on the picture"
        );
        assert_eq!(picture.captions[0].r#ref, "#/texts/0");
        let Some(doc::base_text_item::Item::Text(caption)) = document.texts[0].item.as_ref() else {
            panic!("the caption is a text item");
        };
        assert_eq!(
            caption.base.as_ref().unwrap().hyperlink.as_deref(),
            Some("https://ada.example"),
            "the caption item keeps the link the caption block carried"
        );
    }

    #[test]
    fn a_rich_cells_blocks_fold_into_a_group_the_cell_references() {
        let wire_cell = |text: &str, blocks: Vec<pb::CellBlock>| pb::TableCell {
            text: text.to_owned(),
            blocks,
            ..pb::TableCell::default()
        };
        let cell_text = |label: pb::BlockLabel, text: &str| pb::CellBlock {
            kind: Some(pb::cell_block::Kind::Text(pb::TextBlock {
                label: label as i32,
                text: text.to_owned(),
                ..pb::TextBlock::default()
            })),
        };
        // A nested table whose own cell is rich in turn: its group knows
        // its parent on the spot, where the outer table's groups wait for
        // `append_table`.
        let nested = pb::CellBlock {
            kind: Some(pb::cell_block::Kind::Table(pb::CellTable {
                rows: vec![pb::TableRow {
                    table_ref: String::new(),
                    row_index: 0,
                    is_header: false,
                    cells: vec![wire_cell(
                        "in",
                        vec![cell_text(pb::BlockLabel::Paragraph, "deep")],
                    )],
                }],
                caption: Some("inner".to_owned()),
            })),
        };
        let image = pb::CellBlock {
            kind: Some(pb::cell_block::Kind::Image(pb::ImageBlock {
                src: "i.png".to_owned(),
                ..pb::ImageBlock::default()
            })),
        };
        let document = fold(&[
            info(pb::MarkupFormat::Html),
            pb::parse_markup_response::Event::TableStart(pb::TableStart {
                table_ref: "t0".to_owned(),
                ..pb::TableStart::default()
            }),
            pb::parse_markup_response::Event::TableRow(pb::TableRow {
                table_ref: "t0".to_owned(),
                row_index: 0,
                is_header: false,
                cells: vec![
                    wire_cell(
                        "one in i.png",
                        vec![cell_text(pb::BlockLabel::Paragraph, "one"), nested, image],
                    ),
                    wire_cell("plain", Vec::new()),
                ],
            }),
            pb::parse_markup_response::Event::TableEnd(pb::TableEnd {
                table_ref: "t0".to_owned(),
                row_count: 1,
                column_count: 2,
            }),
        ]);
        // Navigate by ref rather than by arena index: a nested table is
        // appended before its still-streaming owner, so the arenas number
        // inside out (dense either way; integrity asserted by `fold`).
        let outer = document
            .tables
            .iter()
            .find(|table| table.parent.as_ref().unwrap().r#ref == BODY_REF)
            .expect("the streamed table");
        let data = outer.data.as_ref().unwrap();
        let group_ref = data.grid[0].cells[0]
            .r#ref
            .as_ref()
            .expect("the rich cell points")
            .r#ref
            .clone();
        assert_eq!(
            data.table_cells[0].r#ref.as_ref().unwrap().r#ref,
            group_ref,
            "both shapes of the cell name the same group"
        );
        assert!(
            data.grid[0].cells[1].r#ref.is_none(),
            "a plain cell points nowhere"
        );
        let group = document
            .groups
            .iter()
            .find(|group| group.self_ref == group_ref)
            .expect("the group the cell names");
        assert_eq!(
            group.parent.as_ref().unwrap().r#ref,
            outer.self_ref,
            "patched once the table had its arena slot"
        );
        assert_eq!(group.label, doc::GroupLabel::Unspecified as i32);
        assert_eq!(group.label_raw.as_deref(), Some("rich_cell"));
        assert!(
            outer.children.iter().any(|child| child.r#ref == group_ref),
            "the table lists the group, reciprocally"
        );
        let inner = document
            .tables
            .iter()
            .find(|table| table.parent.as_ref().unwrap().r#ref == group_ref)
            .expect("the nested table is the group's child");
        assert_eq!(
            group
                .children
                .iter()
                .map(|child| child.r#ref.as_str())
                .collect::<Vec<_>>(),
            ["#/texts/0", inner.self_ref.as_str(), "#/pictures/0"],
            "paragraph, nested table, picture, in block order"
        );
        let Some(doc::base_text_item::Item::Text(item)) = document.texts[0].item.as_ref() else {
            panic!("the cell's prose is a text item");
        };
        assert_eq!(
            item.base.as_ref().unwrap().parent.as_ref().unwrap().r#ref,
            group_ref
        );
        // The nested table's caption and its own rich cell.
        assert_eq!(inner.captions[0].r#ref, "#/texts/1");
        let inner_cell_group = inner.data.as_ref().unwrap().grid[0].cells[0]
            .r#ref
            .as_ref()
            .expect("the nested cell is rich too")
            .r#ref
            .clone();
        let inner_group = document
            .groups
            .iter()
            .find(|group| group.self_ref == inner_cell_group)
            .expect("the nested group");
        assert_eq!(
            inner_group.parent.as_ref().unwrap().r#ref,
            inner.self_ref,
            "a nested table's own rich groups link on the spot"
        );
        assert_eq!(document.pictures[0].image.as_ref().unwrap().uri, "i.png");
    }

    #[test]
    fn taking_twice_does_not_repeat_the_first_fragment() {
        let mut fold = DocumentFold::new();
        fold.consume(&block(pb::BlockLabel::Paragraph, "one"));
        assert_eq!(fold.take().texts.len(), 1);
        assert_eq!(fold.take().texts.len(), 0);
    }

    #[test]
    fn integrity_errors_catch_a_broken_fragment() {
        let mut document = fold(&[block(pb::BlockLabel::Paragraph, "one")]);
        document
            .body
            .as_mut()
            .unwrap()
            .children
            .push(reference("#/texts/9"));
        let dangling = integrity_errors(&document);
        assert!(
            dangling.iter().any(|e| e.contains("#/texts/9")),
            "{dangling:?}"
        );

        let mut document = fold(&[block(pb::BlockLabel::Paragraph, "one")]);
        document.body.as_mut().unwrap().children.clear();
        let disowned = integrity_errors(&document);
        assert!(
            disowned.iter().any(|e| e.contains("does not list")),
            "{disowned:?}"
        );

        let mut document = fold(&[block(pb::BlockLabel::Paragraph, "one")]);
        document.furniture = None;
        let missing = integrity_errors(&document);
        assert!(
            missing.iter().any(|e| e.contains(FURNITURE_REF)),
            "{missing:?}"
        );
    }

    /// A block carrying inline runs, for the span mapping tests.
    fn spanned_block(text: &str, spans: Vec<pb::InlineSpan>) -> pb::TextBlock {
        pb::TextBlock {
            label: pb::BlockLabel::Paragraph as i32,
            text: text.to_owned(),
            spans,
            ..pb::TextBlock::default()
        }
    }

    fn span(start: u32, end: u32) -> pb::InlineSpan {
        pb::InlineSpan {
            range: Some(pb::IntRange { start, end }),
            ..pb::InlineSpan::default()
        }
    }

    #[test]
    fn styles_fold_onto_the_formatting_the_schema_already_has() {
        let bold_italic = pb::InlineSpan {
            styles: vec![
                pb::SpanStyle::Bold as i32,
                pb::SpanStyle::Italic as i32,
                pb::SpanStyle::Superscript as i32,
            ],
            ..span(0, 4)
        };
        let linked = pb::InlineSpan {
            hyperlink: Some("https://example.com".to_owned()),
            ..span(5, 9)
        };
        let document = fold(&[pb::parse_markup_response::Event::TextBlock(spanned_block(
            "bold link",
            vec![bold_italic, linked],
        ))]);
        let Some(doc::base_text_item::Item::Text(item)) = document.texts[0].item.as_ref() else {
            panic!("a paragraph folds to a TextItem");
        };
        let spans = &item.base.as_ref().unwrap().spans;
        assert_eq!(spans.len(), 2);
        assert_eq!(spans[0].range, Some(doc::IntSpan { start: 0, end: 4 }));
        let formatting = spans[0].formatting.as_ref().expect("styled");
        assert!(formatting.bold && formatting.italic);
        assert!(!formatting.underline);
        assert_eq!(formatting.script, doc::Script::Super as i32);
        assert_eq!(
            spans[1].hyperlink.as_deref(),
            Some("https://example.com"),
            "an in-prose link survives as a span, not only as an item hyperlink"
        );

        // The three styles the schema gained homes for.
        let typographic = pb::InlineSpan {
            styles: vec![
                pb::SpanStyle::Code as i32,
                pb::SpanStyle::SmallCaps as i32,
                pb::SpanStyle::Math as i32,
            ],
            ..span(0, 4)
        };
        let document = fold(&[pb::parse_markup_response::Event::TextBlock(spanned_block(
            "code",
            vec![typographic],
        ))]);
        let Some(doc::base_text_item::Item::Text(item)) = document.texts[0].item.as_ref() else {
            panic!("a paragraph folds to a TextItem");
        };
        let formatting = item.base.as_ref().unwrap().spans[0]
            .formatting
            .as_ref()
            .expect("styled");
        assert!(formatting.monospace && formatting.small_caps && formatting.math);

        // And the five the web and note formats state.
        let web = pb::InlineSpan {
            styles: vec![
                pb::SpanStyle::Mark as i32,
                pb::SpanStyle::Small as i32,
                pb::SpanStyle::Insertion as i32,
                pb::SpanStyle::Abbreviation as i32,
                pb::SpanStyle::Quote as i32,
            ],
            raw: Some("HyperText Markup Language".to_owned()),
            link_title: Some("Her page".to_owned()),
            ..span(0, 4)
        };
        let document = fold(&[pb::parse_markup_response::Event::TextBlock(spanned_block(
            "HTML",
            vec![web],
        ))]);
        let Some(doc::base_text_item::Item::Text(item)) = document.texts[0].item.as_ref() else {
            panic!("a paragraph folds to a TextItem");
        };
        let span = &item.base.as_ref().unwrap().spans[0];
        let formatting = span.formatting.as_ref().expect("styled");
        assert!(
            formatting.mark
                && formatting.small
                && formatting.insertion
                && formatting.abbreviation
                && formatting.quote
        );
        assert_eq!(
            span.raw.as_deref(),
            Some("HyperText Markup Language"),
            "the payload behind the projected text"
        );
        assert_eq!(span.link_title.as_deref(), Some("Her page"));
        assert!(
            spans[1].formatting.is_none(),
            "a run with no typography carries no all-false formatting"
        );
    }

    #[test]
    fn a_cross_reference_resolves_to_the_item_that_declares_its_anchor() {
        // The reference comes first: a document refers forward as often as
        // backward, so the fold cannot resolve at the moment it sees one.
        let referring = spanned_block(
            "as shown in eq:mass",
            vec![pb::InlineSpan {
                reference: Some("eq:mass".to_owned()),
                reference_kind: pb::ReferenceKind::EquationRef as i32,
                ..span(12, 19)
            }],
        );
        let anchored = pb::TextBlock {
            label: pb::BlockLabel::Formula as i32,
            text: "E = mc^2".to_owned(),
            anchor_id: Some("eq:mass".to_owned()),
            ..pb::TextBlock::default()
        };
        let document = fold(&[
            pb::parse_markup_response::Event::TextBlock(referring),
            pb::parse_markup_response::Event::TextBlock(anchored),
        ]);
        let Some(doc::base_text_item::Item::Text(item)) = document.texts[0].item.as_ref() else {
            panic!("a paragraph folds to a TextItem");
        };
        let span = &item.base.as_ref().unwrap().spans[0];
        let target = span.target.as_ref().expect("the reference resolved");
        assert_eq!(target.r#ref, "#/texts/1");
        assert_eq!(
            span.reference_kind,
            Some(doc::ReferenceKind::Equation as i32)
        );
        assert_eq!(
            span.reference.as_deref(),
            Some("eq:mass"),
            "the key rides along beside the resolved pointer"
        );
        assert_eq!(
            document.anchors,
            vec![doc::NamedAnchor {
                name: "eq:mass".to_owned(),
                target: Some(doc::FineRef {
                    r#ref: "#/texts/1".to_owned(),
                    range: None,
                }),
            }],
            "the anchor itself is registered, not only pointed at"
        );
    }

    #[test]
    fn a_bare_cross_reference_that_lands_on_a_formula_is_an_equation_reference() {
        // The wire says only "cross reference"; which one it is, is read
        // off the item the key actually resolved to.
        let referring = spanned_block(
            "see eq:mass and sec:intro",
            vec![
                pb::InlineSpan {
                    reference: Some("eq:mass".to_owned()),
                    reference_kind: pb::ReferenceKind::CrossRef as i32,
                    ..span(4, 11)
                },
                pb::InlineSpan {
                    reference: Some("sec:intro".to_owned()),
                    reference_kind: pb::ReferenceKind::CrossRef as i32,
                    ..span(16, 25)
                },
            ],
        );
        let formula = pb::TextBlock {
            label: pb::BlockLabel::Formula as i32,
            text: "E = mc^2".to_owned(),
            anchor_id: Some("eq:mass".to_owned()),
            ..pb::TextBlock::default()
        };
        let heading = pb::TextBlock {
            label: pb::BlockLabel::SectionHeader as i32,
            text: "Intro".to_owned(),
            level: Some(1),
            anchor_id: Some("sec:intro".to_owned()),
            ..pb::TextBlock::default()
        };
        let document = fold(&[
            pb::parse_markup_response::Event::TextBlock(referring),
            pb::parse_markup_response::Event::TextBlock(formula),
            pb::parse_markup_response::Event::TextBlock(heading),
        ]);
        let Some(doc::base_text_item::Item::Text(item)) = document.texts[0].item.as_ref() else {
            panic!("a paragraph folds to a TextItem");
        };
        let spans = &item.base.as_ref().unwrap().spans;
        assert_eq!(
            spans[0].reference_kind,
            Some(doc::ReferenceKind::Equation as i32)
        );
        assert_eq!(
            spans[1].reference_kind,
            Some(doc::ReferenceKind::CrossRef as i32),
            "one that lands on a heading stays a plain cross reference"
        );
    }

    #[test]
    fn a_reference_nothing_declares_keeps_no_invented_target() {
        let document = fold(&[pb::parse_markup_response::Event::TextBlock(spanned_block(
            "see nowhere",
            vec![pb::InlineSpan {
                reference: Some("missing".to_owned()),
                reference_kind: pb::ReferenceKind::CrossRef as i32,
                ..span(4, 11)
            }],
        ))]);
        let Some(doc::base_text_item::Item::Text(item)) = document.texts[0].item.as_ref() else {
            panic!("a paragraph folds to a TextItem");
        };
        let span = &item.base.as_ref().unwrap().spans[0];
        assert!(span.target.is_none(), "no invented pointer");
        assert_eq!(
            span.reference.as_deref(),
            Some("missing"),
            "but the key the source wrote is kept"
        );
        assert!(document.anchors.is_empty());
    }

    #[test]
    fn headings_build_the_outline_and_admonitions_keep_their_kind() {
        let document = fold(&[
            block(pb::BlockLabel::Title, "The Title"),
            pb::parse_markup_response::Event::TextBlock(pb::TextBlock {
                label: pb::BlockLabel::SectionHeader as i32,
                text: "Setup".to_owned(),
                level: Some(2),
                ..pb::TextBlock::default()
            }),
            pb::parse_markup_response::Event::TextBlock(pb::TextBlock {
                label: pb::BlockLabel::Paragraph as i32,
                text: "Mind the gap.".to_owned(),
                admonition: pb::AdmonitionKind::Warning as i32,
                ..pb::TextBlock::default()
            }),
        ]);
        assert_eq!(
            document
                .outline
                .iter()
                .map(|entry| (entry.title.as_str(), entry.level))
                .collect::<Vec<_>>(),
            vec![("The Title", 1), ("Setup", 2)],
            "only the headings, at their own levels"
        );
        assert_eq!(
            document.outline[1].target.as_ref().unwrap().r#ref,
            "#/texts/1"
        );
        let Some(doc::base_text_item::Item::Text(item)) = document.texts[2].item.as_ref() else {
            panic!("a paragraph folds to a TextItem");
        };
        assert_eq!(
            item.base.as_ref().unwrap().admonition_kind.as_deref(),
            Some("warning")
        );
    }

    #[test]
    fn declared_metadata_lands_in_the_first_class_slots() {
        let mut metadata = std::collections::HashMap::new();
        metadata.insert(
            "authors".to_owned(),
            pb::MetaValue {
                kind: Some(pb::meta_value::Kind::List(pb::MetaList {
                    items: vec![
                        crate::parse::scalar("Ada Lovelace"),
                        crate::parse::scalar("Grace Hopper"),
                    ],
                })),
            },
        );
        metadata.insert("date".to_owned(), crate::parse::scalar("1843-10-01"));
        metadata.insert(
            "meta.keywords".to_owned(),
            crate::parse::scalar("engine, looms"),
        );
        metadata.insert("meta.language".to_owned(), crate::parse::scalar("en"));
        metadata.insert("licence".to_owned(), crate::parse::scalar("CC-BY-4.0"));
        let document = fold(&[pb::parse_markup_response::Event::Info(pb::MarkupInfo {
            format: pb::MarkupFormat::Markdown as i32,
            evidence: pb::FormatEvidence::Requested as i32,
            title: Some("Sketch of the Engine".to_owned()),
            metadata,
        })]);
        let meta = document.source_meta.as_ref().expect("declared metadata");
        assert_eq!(meta.title.as_deref(), Some("Sketch of the Engine"));
        assert_eq!(meta.authors, vec!["Ada Lovelace", "Grace Hopper"]);
        assert_eq!(meta.created_raw.as_deref(), Some("1843-10-01"));
        assert_eq!(
            meta.created_civil,
            Some(doc::CivilDateTime {
                year: 1843,
                month: 10,
                day: 1,
                ..doc::CivilDateTime::default()
            }),
            "a date with no time of day states no instant"
        );
        assert!(meta.created.is_none(), "and no instant is invented for it");
        assert_eq!(meta.language.as_deref(), Some("en"));
        assert_eq!(meta.keywords, vec!["engine", "looms"]);
        assert_eq!(
            meta.extra.get("licence").map(String::as_str),
            Some("CC-BY-4.0"),
            "a scalar with no first-class slot keeps its own name"
        );
        assert!(!meta.extra.contains_key("authors"), "no key lands twice");
    }

    #[test]
    fn a_declared_date_lands_in_the_shape_the_source_actually_stated() {
        // No time of day: a wall-clock date, and no invented timezone.
        let (instant, civil) = declared_time("1843-10-01");
        assert!(instant.is_none());
        assert_eq!(
            civil,
            Some(doc::CivilDateTime {
                year: 1843,
                month: 10,
                day: 1,
                ..doc::CivilDateTime::default()
            })
        );

        // A time but no offset is still wall-clock.
        let (instant, civil) = declared_time("2026-08-25T09:30:00");
        assert!(instant.is_none());
        let civil = civil.expect("a civil datetime");
        assert_eq!(
            (civil.hour, civil.minute, civil.second),
            (Some(9), Some(30), Some(0))
        );
        assert!(civil.utc_offset_minutes.is_none());

        // An offset states an instant.
        let (instant, civil) = declared_time("1970-01-01T00:00:00Z");
        assert!(civil.is_none());
        assert_eq!(instant.expect("an instant").seconds, 0);
        let (instant, _) = declared_time("2026-08-25T09:30:00.5-04:00");
        let instant = instant.expect("an instant");
        assert_eq!(instant.seconds, 1_787_664_600);
        assert_eq!(instant.nanos, 500_000_000);

        // Anything the grammar does not cover parses to neither shape; the
        // caller still keeps the source's own spelling.
        for unreadable in [
            "October 1843",
            "1843",
            "1843-13-01",
            "2026-08-25T25:00:00Z",
            "",
        ] {
            assert_eq!(declared_time(unreadable), (None, None), "{unreadable}");
        }
    }

    #[test]
    fn declared_table_geometry_reaches_the_column_schemas_and_the_cells() {
        let mut fold_state = DocumentFold::new();
        fold_state.consume(&pb::parse_markup_response::Event::TableStart(
            pb::TableStart {
                table_ref: "t0".to_owned(),
                columns: vec![
                    pb::ColumnSpec {
                        align: pb::Alignment::Left as i32,
                        valign: pb::Alignment::Unspecified as i32,
                        width: None,
                    },
                    pb::ColumnSpec {
                        align: pb::Alignment::Justify as i32,
                        valign: pb::Alignment::Top as i32,
                        width: Some("5cm".to_owned()),
                    },
                    pb::ColumnSpec {
                        align: pb::Alignment::Right as i32,
                        valign: pb::Alignment::Unspecified as i32,
                        width: Some("120".to_owned()),
                    },
                ],
                ..pb::TableStart::default()
            },
        ));
        fold_state.consume(&pb::parse_markup_response::Event::TableRow(pb::TableRow {
            table_ref: "t0".to_owned(),
            row_index: 0,
            is_header: false,
            cells: vec![pb::TableCell {
                column_index: 0,
                text: "a".to_owned(),
                column_span: 1,
                row_span: 1,
                align: pb::Alignment::Center as i32,
                valign: pb::Alignment::Bottom as i32,
                ..pb::TableCell::default()
            }],
        }));
        fold_state.consume(&pb::parse_markup_response::Event::TableEnd(pb::TableEnd {
            table_ref: "t0".to_owned(),
            row_count: 1,
            column_count: 3,
        }));
        let document = fold_state.take();
        assert!(integrity_errors(&document).is_empty());
        let data = document.tables[0].data.as_ref().unwrap();

        assert_eq!(data.columns.len(), 3);
        assert!(
            data.columns.iter().all(|column| column.name.is_none()),
            "a markup table declares geometry, not column names"
        );
        assert_eq!(
            data.columns
                .iter()
                .map(|column| column.align)
                .collect::<Vec<_>>(),
            vec![
                Some(doc::Alignment::Left as i32),
                Some(doc::Alignment::Justify as i32),
                Some(doc::Alignment::Right as i32),
            ]
        );
        assert_eq!(
            data.columns[1].valign,
            Some(doc::VerticalAlignment::Top as i32)
        );
        assert!(data.columns[0].valign.is_none());
        assert_eq!(
            data.columns[1].width_raw.as_deref(),
            Some("5cm"),
            "a length spelling is kept verbatim rather than mangled"
        );
        assert!(data.columns[1].width.is_none());
        assert_eq!(
            data.columns[2].width,
            Some(120.0),
            "a bare number is a page-unit width"
        );
        assert!(data.columns[2].width_raw.is_none());

        let cell = &data.grid[0].cells[0];
        assert_eq!(cell.align, Some(doc::Alignment::Center as i32));
        assert_eq!(cell.valign, Some(doc::VerticalAlignment::Bottom as i32));
    }

    #[test]
    fn the_two_alignment_axes_never_cross_over() {
        // The wire merges both axes into one enum; a vertical value in a
        // horizontal slot is not an alignment on this plane.
        assert_eq!(
            horizontal(pb::Alignment::Justify as i32),
            Some(doc::Alignment::Justify)
        );
        assert!(horizontal(pb::Alignment::Middle as i32).is_none());
        assert_eq!(
            vertical(pb::Alignment::Bottom as i32),
            Some(doc::VerticalAlignment::Bottom)
        );
        assert!(vertical(pb::Alignment::Left as i32).is_none());
        assert!(horizontal(pb::Alignment::Unspecified as i32).is_none());
        assert!(vertical(pb::Alignment::Unspecified as i32).is_none());
    }

    #[test]
    fn code_language_lookup_matches_enum_names_only() {
        assert_eq!(
            code_language_label("Rust"),
            Some(doc::CodeLanguageLabel::Rust)
        );
        assert_eq!(
            code_language_label("objectivec"),
            Some(doc::CodeLanguageLabel::Objectivec)
        );
        assert_eq!(code_language_label("c++"), None, "not an enum name");
        assert_eq!(code_language_label(""), None);
    }

    // ------------------------------------------------------- dense grids

    fn wire_cell(column_index: u32, text: &str, column_span: u32, row_span: u32) -> pb::TableCell {
        pb::TableCell {
            column_index,
            text: text.to_owned(),
            column_span,
            row_span,
            ..pb::TableCell::default()
        }
    }

    /// One streamed table, `rows` being each row's cells in source order
    /// with the columns the wire placed them at.
    fn table_events(rows: Vec<Vec<pb::TableCell>>) -> Vec<pb::parse_markup_response::Event> {
        use pb::parse_markup_response::Event;
        let mut events = vec![Event::TableStart(pb::TableStart {
            table_ref: "t0".to_owned(),
            ..pb::TableStart::default()
        })];
        for (row_index, cells) in rows.into_iter().enumerate() {
            events.push(Event::TableRow(pb::TableRow {
                table_ref: "t0".to_owned(),
                row_index: u32::try_from(row_index).unwrap(),
                is_header: false,
                cells,
            }));
        }
        events.push(Event::TableEnd(pb::TableEnd {
            table_ref: "t0".to_owned(),
            row_count: 0,
            column_count: 0,
        }));
        events
    }

    /// The dense-grid contract as the real-data battery checks it: every
    /// cell inside `num_rows` x `num_cols` with spans equal to its offsets,
    /// no two cells overlapping, and a materialised grid of exactly
    /// `num_rows` rows of `num_cols` entries.
    fn grid_problems(data: &doc::TableData) -> Vec<String> {
        let (num_rows, num_cols) = (
            usize::try_from(data.num_rows).unwrap(),
            usize::try_from(data.num_cols).unwrap(),
        );
        let mut problems = Vec::new();
        let mut occupied = BTreeSet::new();
        for cell in &data.table_cells {
            let (rows, cols) = cell_footprint(cell);
            let at = format!("({},{})", rows.start, cols.start);
            if rows.is_empty() || cols.is_empty() {
                problems.push(format!("cell {at} has an empty span"));
                continue;
            }
            if rows.end > num_rows || cols.end > num_cols {
                problems.push(format!("cell {at} outside {num_rows}x{num_cols}"));
                continue;
            }
            if int_from_usize(rows.len()) != cell.row_span
                || int_from_usize(cols.len()) != cell.col_span
            {
                problems.push(format!("cell {at} spans differ from its offsets"));
            }
            for row in rows {
                for col in cols.clone() {
                    if !occupied.insert((row, col)) {
                        problems.push(format!(
                            "cell {at} overlaps an earlier cell at ({row},{col})"
                        ));
                    }
                }
            }
        }
        // A grid is checked only where it is materialised, as the check
        // does; the offsets alone are the shape then.
        if !data.grid.is_empty() && data.grid.len() != num_rows {
            problems.push(format!(
                "grid has {} rows, num_rows says {num_rows}",
                data.grid.len()
            ));
        }
        for (index, row) in data.grid.iter().enumerate() {
            if row.cells.len() != num_cols {
                problems.push(format!(
                    "grid row {index} has {} cells, num_cols says {num_cols}",
                    row.cells.len()
                ));
            }
        }
        problems
    }

    fn texts_of(data: &doc::TableData) -> Vec<Vec<&str>> {
        data.grid
            .iter()
            .map(|row| row.cells.iter().map(|cell| cell.text.as_str()).collect())
            .collect()
    }

    fn table_data(document: &doc::Document) -> &doc::TableData {
        document.tables[0]
            .data
            .as_ref()
            .expect("the table has data")
    }

    #[test]
    fn a_short_row_is_padded_with_empty_cells_to_the_table_width() {
        let document = fold(&table_events(vec![
            vec![
                wire_cell(0, "a", 1, 1),
                wire_cell(1, "b", 1, 1),
                wire_cell(2, "c", 1, 1),
            ],
            vec![wire_cell(0, "d", 1, 1)],
        ]));
        let data = table_data(&document);
        assert_eq!(grid_problems(data), Vec::<String>::new());
        assert_eq!(texts_of(data), [["a", "b", "c"], ["d", "", ""]]);
        assert_eq!(data.table_cells.len(), 4, "fillers are not cells");
        let filler = &data.grid[1].cells[2];
        assert_eq!(
            (
                filler.start_row_offset_idx,
                filler.end_row_offset_idx,
                filler.start_col_offset_idx,
                filler.end_col_offset_idx,
            ),
            (1, 2, 2, 3),
            "a filler states the one position it holds"
        );
    }

    #[test]
    fn a_colspan_repeats_its_cell_across_the_columns_it_covers() {
        let document = fold(&table_events(vec![vec![
            wire_cell(0, "a", 2, 1),
            wire_cell(2, "b", 1, 1),
        ]]));
        let data = table_data(&document);
        assert_eq!(grid_problems(data), Vec::<String>::new());
        assert_eq!(texts_of(data), [["a", "a", "b"]]);
        assert_eq!(data.table_cells.len(), 2);
        assert_eq!(
            data.grid[0].cells[1].start_col_offset_idx, 0,
            "the copy is the same cell"
        );
        assert_eq!(data.grid[0].cells[1].col_span, 2);
    }

    #[test]
    fn a_rowspan_fills_the_rows_beneath_it() {
        let document = fold(&table_events(vec![
            vec![wire_cell(0, "a", 1, 2), wire_cell(1, "b", 1, 1)],
            vec![wire_cell(1, "c", 1, 1)],
        ]));
        let data = table_data(&document);
        assert_eq!(grid_problems(data), Vec::<String>::new());
        assert_eq!(texts_of(data), [["a", "b"], ["a", "c"]]);
        assert_eq!(data.table_cells.len(), 3);
        assert_eq!(data.grid[1].cells[0].start_row_offset_idx, 0);
        assert_eq!(data.grid[1].cells[0].end_row_offset_idx, 2);
    }

    #[test]
    fn a_rowspan_past_the_last_row_is_clamped_in_both_shapes() {
        let document = fold(&table_events(vec![
            vec![wire_cell(0, "a", 1, 5), wire_cell(1, "b", 1, 1)],
            vec![wire_cell(1, "c", 1, 1)],
        ]));
        let data = table_data(&document);
        assert_eq!(grid_problems(data), Vec::<String>::new());
        assert_eq!(data.num_rows, 2, "no rows the source never wrote");
        for (where_, cell) in [
            ("table_cells", &data.table_cells[0]),
            ("grid row 0", &data.grid[0].cells[0]),
            ("grid row 1", &data.grid[1].cells[0]),
        ] {
            assert_eq!(cell.row_span, 2, "{where_}");
            assert_eq!(cell.end_row_offset_idx, 2, "{where_}");
        }
    }

    #[test]
    fn a_rich_cell_in_a_spanned_position_links_its_group_once() {
        let rich = pb::TableCell {
            blocks: vec![pb::CellBlock {
                kind: Some(pb::cell_block::Kind::Text(pb::TextBlock {
                    label: pb::BlockLabel::Paragraph as i32,
                    text: "inside".to_owned(),
                    ..pb::TextBlock::default()
                })),
            }],
            ..wire_cell(0, "inside", 2, 2)
        };
        let document = fold(&table_events(vec![
            vec![rich, wire_cell(2, "b", 1, 1)],
            vec![wire_cell(2, "c", 1, 1)],
        ]));
        let data = table_data(&document);
        assert_eq!(grid_problems(data), Vec::<String>::new());
        assert_eq!(
            texts_of(data),
            [["inside", "inside", "b"], ["inside", "inside", "c"]]
        );
        assert_eq!(document.groups.len(), 1, "one group for the one rich cell");
        assert_eq!(
            document.tables[0]
                .children
                .iter()
                .map(|child| child.r#ref.as_str())
                .collect::<Vec<_>>(),
            ["#/groups/0"],
            "the group is linked once, however many positions the cell covers"
        );
        assert_eq!(
            document.groups[0].parent.as_ref().unwrap().r#ref,
            "#/tables/0"
        );
        for row in &data.grid {
            for cell in &row.cells[..2] {
                assert_eq!(
                    cell.r#ref.as_ref().map(|r| r.r#ref.as_str()),
                    Some("#/groups/0"),
                    "every copy of the cell points at the same group"
                );
            }
            assert!(row.cells[2].r#ref.is_none());
        }
        assert_eq!(
            data.table_cells[0].r#ref.as_ref().unwrap().r#ref,
            "#/groups/0"
        );
    }

    #[test]
    fn a_reference_in_a_densified_row_resolves_in_every_copy_of_its_cell() {
        let anchor = pb::parse_markup_response::Event::TextBlock(pb::TextBlock {
            label: pb::BlockLabel::SectionHeader as i32,
            text: "Target".to_owned(),
            anchor_id: Some("sec".to_owned()),
            ..pb::TextBlock::default()
        });
        let referring = pb::TableCell {
            spans: vec![pb::InlineSpan {
                range: Some(pb::IntRange { start: 0, end: 3 }),
                reference: Some("sec".to_owned()),
                ..pb::InlineSpan::default()
            }],
            ..wire_cell(0, "see", 2, 1)
        };
        let mut events = vec![anchor];
        events.extend(table_events(vec![
            vec![
                wire_cell(0, "a", 1, 1),
                wire_cell(1, "b", 1, 1),
                wire_cell(2, "c", 1, 1),
            ],
            vec![referring],
        ]));
        let document = fold(&events);
        let data = table_data(&document);
        assert_eq!(grid_problems(data), Vec::<String>::new());
        assert_eq!(texts_of(data), [["a", "b", "c"], ["see", "see", ""]]);
        for (where_, cell) in [
            ("table_cells", &data.table_cells[3]),
            ("grid copy 0", &data.grid[1].cells[0]),
            ("grid copy 1", &data.grid[1].cells[1]),
        ] {
            assert_eq!(cell.text, "see", "{where_}: the cell the run belongs to");
            assert_eq!(
                cell.spans[0]
                    .target
                    .as_ref()
                    .map(|target| target.r#ref.as_str()),
                Some("#/texts/0"),
                "{where_}: the run points at the heading that declares the anchor"
            );
        }
        assert!(
            data.grid[0].cells.iter().all(|cell| cell.spans.is_empty()),
            "no other cell gained a run"
        );
    }

    #[test]
    fn overlapping_spans_are_settled_in_favour_of_the_earlier_cell() {
        // `b` asks for column 1, which `a` already holds; `c` asks for
        // column 0 on the row `a` still covers. Both move right.
        let document = fold(&table_events(vec![
            vec![wire_cell(0, "a", 2, 2), wire_cell(1, "b", 1, 1)],
            vec![wire_cell(0, "c", 1, 1)],
        ]));
        let data = table_data(&document);
        assert_eq!(grid_problems(data), Vec::<String>::new());
        assert_eq!(texts_of(data), [["a", "a", "b"], ["a", "a", "c"]]);
        assert_eq!(data.table_cells.len(), 3, "no cell is dropped");
        assert_eq!(data.table_cells[1].start_col_offset_idx, 2);
        assert_eq!(data.table_cells[2].start_col_offset_idx, 2);
    }

    #[test]
    fn an_overlapping_span_is_cut_where_the_next_cell_begins() {
        // `z` asks for five columns from 0; `x` holds column 0 and `y`
        // column 2 on its row, so it gets column 1 alone.
        let document = fold(&table_events(vec![
            vec![wire_cell(0, "x", 1, 2), wire_cell(2, "y", 1, 2)],
            vec![wire_cell(0, "z", 5, 1)],
        ]));
        let data = table_data(&document);
        assert_eq!(grid_problems(data), Vec::<String>::new());
        assert_eq!(texts_of(data), [["x", "", "y"], ["x", "z", "y"]]);
        let z = &data.table_cells[2];
        assert_eq!(
            (z.start_col_offset_idx, z.end_col_offset_idx, z.col_span),
            (1, 2, 1)
        );
    }

    #[test]
    fn a_big_cell_spanning_many_positions_keeps_its_cells_and_no_grid() {
        // Under the position floor (65 x 1001), so only the byte budget
        // stands between one 2 MiB cell and sixty-five thousand copies of
        // it.
        let text = "x".repeat(2 << 20);
        let mut rows = vec![vec![
            wire_cell(0, &text, 1000, 65),
            wire_cell(1000, "y", 1, 1),
        ]];
        rows.extend((1..65).map(|_| vec![wire_cell(1000, "y", 1, 1)]));
        let document = fold(&table_events(rows));
        let data = table_data(&document);
        assert_eq!((data.num_rows, data.num_cols), (65, 1001));
        assert_eq!(data.table_cells.len(), 66, "every real cell is kept");
        assert_eq!(data.table_cells[0].text.len(), 2 << 20);
        assert!(
            data.grid.is_empty(),
            "the grid would be a hundred gigabytes of one cell"
        );
        assert_eq!(grid_problems(data), Vec::<String>::new());
    }

    #[test]
    fn a_wide_header_over_a_small_table_still_gets_its_grid() {
        // Five copies of the header exceed four times the real bytes, but
        // the whole grid is far under the byte floor.
        let document = fold(&table_events(vec![
            vec![wire_cell(0, &"h".repeat(10_000), 5, 1)],
            vec![wire_cell(0, "a", 1, 1)],
        ]));
        let data = table_data(&document);
        assert_eq!(data.grid.len(), 2);
        assert_eq!(grid_problems(data), Vec::<String>::new());
    }

    #[test]
    fn a_sparse_table_too_wide_to_materialise_keeps_its_cells_and_no_grid() {
        let document = fold(&table_events(vec![vec![wire_cell(0, "wide", 100_000, 1)]]));
        let data = table_data(&document);
        assert_eq!(data.num_cols, 100_000);
        assert_eq!(data.table_cells.len(), 1);
        assert!(
            data.grid.is_empty(),
            "a hundred thousand copies of one cell is not a grid worth sending"
        );
        assert_eq!(grid_problems(data), Vec::<String>::new());
    }
}
