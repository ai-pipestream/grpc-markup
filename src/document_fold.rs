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
//!   streamed, the open heading ladder, and the open list-group stack.
//! - **A self-contained fragment.** Refs are dense and local (`#/texts/0`,
//!   `#/tables/1`), every item's `parent` is set and every parent lists the
//!   item in its `children`, so the coordinator's additive merge can
//!   renumber the whole fragment mechanically. [`integrity_errors`] is that
//!   contract as a check, and every fold test asserts it is empty.
//! - **Provenance honesty.** Markup has no pages and no boxes, so no item
//!   carries `prov` — except a cue, whose [`TrackSource`](doc::TrackSource)
//!   timing is real provenance and rides in `source`. Format-native facts
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
}

/// A table being assembled from `table_start` / `table_row` / `table_end`.
struct PendingTable {
    /// The wire identifier rows and the end event carry.
    reference: String,
    /// Everything about the item except its arena position, which is only
    /// known when it is appended at `table_end`.
    item: doc::TableItem,
    grid: Vec<doc::TableRow>,
    cells: Vec<doc::TableCell>,
    num_cols: i32,
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
                confidence: None,
            },
            prefix: COLLECTOR.to_owned(),
            table: None,
            headings: Vec::new(),
            lists: Vec::new(),
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
        self.headings.clear();
        self.lists.clear();
        std::mem::replace(&mut self.document, skeleton())
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
            ..doc::DocumentOrigin::default()
        });
        if !info.metadata.is_empty()
            && let Some(body) = self.document.body.as_mut()
        {
            let meta = body.meta.get_or_insert_default();
            // Deterministic order for a map the wire delivers unordered.
            for (key, value) in info.metadata.iter().collect::<BTreeMap<_, _>>() {
                meta.custom_fields
                    .insert(format!("{}.{key}", self.prefix), meta_field(value));
            }
        }
    }

    // ------------------------------------------------------------------ text

    fn on_text_block(&mut self, block: &pb::TextBlock) {
        let label = pb::BlockLabel::try_from(block.label).unwrap_or_default();
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
        let variant = match label {
            pb::BlockLabel::Title => {
                doc::base_text_item::Item::Title(doc::TitleItem { base: Some(base) })
            }
            pb::BlockLabel::SectionHeader => {
                doc::base_text_item::Item::SectionHeader(doc::SectionHeaderItem {
                    base: Some(base),
                    level: int(level),
                })
            }
            pb::BlockLabel::Formula => {
                doc::base_text_item::Item::Formula(doc::FormulaItem { base: Some(base) })
            }
            pb::BlockLabel::Code => doc::base_text_item::Item::Code(self.code_item(block, base)),
            _ => doc::base_text_item::Item::Text(doc::TextItem { base: Some(base) }),
        };
        self.document.texts.push(doc::BaseTextItem {
            item: Some(variant),
        });
        self.link_child(&parent, &self_ref);
        if label == pb::BlockLabel::SectionHeader {
            self.headings.push((level, self_ref.clone()));
        }
        if self.document.name.is_empty() && label == pb::BlockLabel::Title {
            // A Document with no name is worse than one named after its own
            // title item.
            self.document.name.clone_from(&block.text);
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
            // No prov: markup has no pages and no boxes, and an invented
            // one would outlive the honesty of this comment.
            orig: block.text.clone(),
            text: block.text.clone(),
            hyperlink: block.hyperlink.clone(),
            source: vec![self.collector_source()],
            ..doc::TextItemBase::default()
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
        if let Some(line) = block.source_line {
            fields.insert(
                format!("{}.source_line", self.prefix),
                number(f64::from(line)),
            );
        }
        fields
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
        let base = self.text_base(&self_ref, &parent, block, doc::DocItemLabel::ListItem);
        self.document.texts.push(doc::BaseTextItem {
            item: Some(doc::base_text_item::Item::ListItem(doc::ListItem {
                base: Some(base),
                enumerated: block.enumerated,
                marker: block.marker.clone(),
            })),
        });
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
        // An unclosed predecessor cannot happen on a conformant stream, but
        // dropping its rows silently if it did would be worse than keeping
        // them.
        if let Some(previous) = self.table.take() {
            self.append_table(previous);
        }
        let mut fields = HashMap::new();
        if let Some(line) = start.source_line {
            fields.insert(
                format!("{}.source_line", self.prefix),
                number(f64::from(line)),
            );
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
                meta: (!fields.is_empty()).then(|| doc::FloatingMeta {
                    custom_fields: fields,
                    ..doc::FloatingMeta::default()
                }),
                label: doc::DocItemLabel::Table as i32,
                captions,
                source: vec![self.collector_source()],
                ..doc::TableItem::default()
            },
            grid: Vec::new(),
            cells: Vec::new(),
            num_cols: 0,
        });
    }

    /// Lay one wire row into the grid. The wire's `column_index` already
    /// accounts for the spans before each cell, so it is trusted rather
    /// than recomputed.
    fn on_table_row(&mut self, row: &pb::TableRow) {
        let Some(table) = self.table.as_mut() else {
            return;
        };
        if table.reference != row.table_ref {
            return;
        }
        let row_index = int(row.row_index);
        let mut cells = Vec::with_capacity(row.cells.len());
        for cell in &row.cells {
            let start_col = int(cell.column_index);
            let col_span = int(cell.column_span.max(1));
            let row_span = int(cell.row_span.max(1));
            let end_col = start_col.saturating_add(col_span);
            cells.push(doc::TableCell {
                row_span,
                col_span,
                start_row_offset_idx: row_index,
                end_row_offset_idx: row_index.saturating_add(row_span),
                start_col_offset_idx: start_col,
                end_col_offset_idx: end_col,
                text: cell.text.clone(),
                // A cell is a header when its row is one or when the source
                // marked the cell itself.
                column_header: row.is_header || cell.is_header,
                ..doc::TableCell::default()
            });
            table.num_cols = table.num_cols.max(end_col);
        }
        table.cells.extend(cells.iter().cloned());
        table.grid.push(doc::TableRow { cells });
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
        item.self_ref = format!("#/tables/{}", self.document.tables.len());
        item.data = Some(doc::TableData {
            num_rows: int_from_usize(pending.grid.len()),
            num_cols: pending.num_cols,
            // Both shapes are populated: `grid` is the row structure the
            // renderers walk, `table_cells` the flat list the analyzers
            // read.
            table_cells: pending.cells,
            grid: pending.grid,
            ..doc::TableData::default()
        });
        let self_ref = item.self_ref.clone();
        let parent = item
            .parent
            .as_ref()
            .map_or_else(|| BODY_REF.to_owned(), |parent| parent.r#ref.clone());
        self.document.tables.push(item);
        self.link_child(&parent, &self_ref);
    }

    // ---------------------------------------------------------------- images

    /// One image reference as a picture whose `ImageRef.uri` is the source's
    /// own string, verbatim. No bytes, no size, no dpi: this collector is
    /// handed markup, and claiming pixels it never saw would be lying.
    fn on_image(&mut self, image: &pb::ImageBlock) {
        self.lists.clear();
        let parent = self.current_parent();
        let self_ref = format!("#/pictures/{}", self.document.pictures.len());
        let mut fields = HashMap::new();
        if !image.alt.is_empty() {
            fields.insert(format!("{}.alt", self.prefix), string(&image.alt));
        }
        if !image.title.is_empty() {
            fields.insert(format!("{}.title", self.prefix), string(&image.title));
        }
        if let Some(line) = image.source_line {
            fields.insert(
                format!("{}.source_line", self.prefix),
                number(f64::from(line)),
            );
        }
        self.document.pictures.push(doc::PictureItem {
            self_ref: self_ref.clone(),
            parent: Some(reference(&parent)),
            content_layer: doc::ContentLayer::Body as i32,
            meta: (!fields.is_empty()).then(|| doc::PictureMeta {
                custom_fields: fields,
                ..doc::PictureMeta::default()
            }),
            label: doc::DocItemLabel::Picture as i32,
            image: Some(doc::ImageRef {
                mimetype: image_mimetype(&image.src).to_owned(),
                dpi: 0,
                size: None,
                uri: image.src.clone(),
            }),
            source: vec![self.collector_source()],
            ..doc::PictureItem::default()
        });
        self.link_child(&parent, &self_ref);
    }

    // ------------------------------------------------------------------ cues

    /// One WebVTT cue as a `TextItem` whose provenance is a `TrackSource`
    /// carrying the cue timings — real coordinates, in seconds — alongside
    /// this collector's own stamp.
    fn on_cue(&mut self, cue: &pb::Cue) {
        self.lists.clear();
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
        let base = doc::TextItemBase {
            self_ref: self_ref.clone(),
            parent: Some(reference(&parent)),
            content_layer: doc::ContentLayer::Body as i32,
            label: doc::DocItemLabel::Text as i32,
            orig: cue.text.clone(),
            text: cue.text.clone(),
            source: vec![track, self.collector_source()],
            ..doc::TextItemBase::default()
        };
        self.document.texts.push(doc::BaseTextItem {
            item: Some(doc::base_text_item::Item::Text(doc::TextItem {
                base: Some(base),
            })),
        });
        self.link_child(&parent, &self_ref);
    }

    // ------------------------------------------------------------- primitives

    /// The ref new content parents to: the innermost open section header,
    /// or the body when no header has opened yet.
    fn current_parent(&self) -> String {
        self.headings
            .last()
            .map_or_else(|| BODY_REF.to_owned(), |(_, self_ref)| self_ref.clone())
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

/// A `google.protobuf.Value` holding a number.
const fn number(value: f64) -> Value {
    Value {
        kind: Some(Kind::NumberValue(value)),
    }
}

/// A `google.protobuf.Value` holding a boolean.
const fn flag(value: bool) -> Value {
    Value {
        kind: Some(Kind::BoolValue(value)),
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
                },
                pb::TableCell {
                    column_index: 1,
                    text: "b".to_owned(),
                    column_span: 2,
                    row_span: 1,
                    is_header: true,
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
}
