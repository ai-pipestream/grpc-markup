// SPDX-License-Identifier: Apache-2.0

//! The Docling JSON fold: a `DoclingDocument` serialized as JSON,
//! re-ingested into the Document plane.
//!
//! The vendored `document.proto` deliberately mirrors the DoclingDocument
//! v2 schema, so this is a field-by-field mapping of the
//! core arrays — `schema_name`, `version`, `name`, `origin`, `body`,
//! `furniture`, `groups`, `texts`, `tables`, `pictures`, `pages` — with the
//! `self_ref` / `parent` / `children` wiring carried over as written. Every
//! key the mapping does not consume produces one aggregated
//! `WARNING_CODE_UNMAPPED_CONSTRUCT` per key path (array indexes
//! normalized to `[]`, so ten thousand annotated pictures make one warning
//! with a count, not ten thousand warnings).
//!
//! The typed event stream for this format is a *projection*: the ingested
//! body is walked in reading order and its items re-emitted as
//! `TextBlock` / table / `ImageBlock` events, so a client that only speaks
//! the typed wire still gets the content. The `document` event — when
//! `emit_document` is set — carries the full mapping, and is the point of
//! this format: what went in comes out, item for item, with this
//! collector's stamp appended to each item's sources (sources never
//! overwrite each other; attribution is additive).
//!
//! Validation is strict about identity and lenient about everything else:
//! bytes that are not JSON, or JSON whose `schema_name` does not name a
//! `DoclingDocument`, are `INVALID_ARGUMENT`; unknown labels and layers
//! degrade to defaults with a warning rather than failing a document whose
//! `schema_name` already identified it.

use std::collections::{BTreeMap, HashMap, HashSet};

use serde_json::Value;

use crate::document::v1 as doc;
use crate::document_fold::code_language_label;
use crate::parse::{Block, Emitter, Image, ParseError, Table, scalar};
use crate::proto::v1 as pb;
use crate::{COLLECTOR, VERSION};

/// Map one serialized `DoclingDocument`.
///
/// # Errors
///
/// [`ParseError::Invalid`] when the bytes are not JSON or the JSON has no
/// recognizable `DoclingDocument` shape; [`ParseError::ConsumerGone`] when
/// the client stopped reading.
pub fn fold(text: &str, emitter: &mut Emitter<'_>) -> Result<(), ParseError> {
    let value: Value = serde_json::from_str(text)
        .map_err(|e| ParseError::Invalid(format!("not a DoclingDocument: invalid JSON: {e}")))?;
    let Some(root) = value.as_object() else {
        return Err(ParseError::Invalid(
            "not a DoclingDocument: the payload is not a JSON object".to_owned(),
        ));
    };
    let schema_name = root.get("schema_name").and_then(Value::as_str);
    if !schema_name.is_some_and(|name| name.to_ascii_lowercase().contains("docling")) {
        return Err(ParseError::Invalid(format!(
            "not a DoclingDocument: schema_name is {:?}, expected one naming DoclingDocument",
            schema_name.unwrap_or("absent")
        )));
    }

    let mut unmapped = Unmapped::default();
    let mut document = map_document(root, &mut unmapped);

    let mut metadata = HashMap::new();
    if let Some(name) = document.schema_name.clone() {
        metadata.insert("schema_name".to_owned(), scalar(name));
    }
    if let Some(version) = document.version.clone() {
        metadata.insert("version".to_owned(), scalar(version));
    }
    let title = (!document.name.is_empty()).then(|| document.name.clone());
    emitter.info(title, metadata)?;

    for (path, count) in unmapped.paths {
        emitter.warn_n(
            pb::WarningCode::UnmappedConstruct,
            &format!("unmapped DoclingDocument key {path}"),
            count,
        );
    }

    project(&document, emitter)?;

    if emitter.wants_document() {
        stamp(&mut document);
        emitter.set_document(document);
    }
    Ok(())
}

// ----------------------------------------------------------------- tracking

/// The key paths the mapping did not consume, with occurrence counts.
/// Array indexes are normalized to `[]` so the same shape aggregates.
#[derive(Default)]
struct Unmapped {
    paths: BTreeMap<String, u64>,
}

impl Unmapped {
    fn note(&mut self, path: &str, key: &str) {
        *self.paths.entry(format!("{path}.{key}")).or_default() += 1;
    }
}

/// One JSON object being consumed key by key; whatever is left when it is
/// dropped becomes unmapped-key warnings.
struct Obj<'a, 'u> {
    map: &'a serde_json::Map<String, Value>,
    used: HashSet<&'a str>,
    path: &'a str,
    unmapped: &'u mut Unmapped,
}

impl<'a, 'u> Obj<'a, 'u> {
    fn new(
        map: &'a serde_json::Map<String, Value>,
        path: &'a str,
        unmapped: &'u mut Unmapped,
    ) -> Self {
        Self {
            map,
            used: HashSet::new(),
            path,
            unmapped,
        }
    }

    fn get(&mut self, key: &'static str) -> Option<&'a Value> {
        self.used.insert(key);
        self.map.get(key).filter(|value| !value.is_null())
    }

    fn string(&mut self, key: &'static str) -> Option<String> {
        self.get(key).and_then(Value::as_str).map(str::to_owned)
    }

    fn text(&mut self, key: &'static str) -> String {
        self.string(key).unwrap_or_default()
    }

    fn int(&mut self, key: &'static str) -> i32 {
        self.get(key)
            .and_then(Value::as_i64)
            .and_then(|n| i32::try_from(n).ok())
            .unwrap_or_default()
    }

    fn float(&mut self, key: &'static str) -> f64 {
        self.get(key).and_then(Value::as_f64).unwrap_or_default()
    }

    fn boolean(&mut self, key: &'static str) -> bool {
        self.get(key).and_then(Value::as_bool).unwrap_or_default()
    }

    fn finish(self) {
        for key in self.map.keys() {
            if !self.used.contains(key.as_str()) {
                self.unmapped.note(self.path, key);
            }
        }
    }
}

// ------------------------------------------------------------------ mapping

/// The whole document, top-level key by top-level key.
fn map_document(root: &serde_json::Map<String, Value>, unmapped: &mut Unmapped) -> doc::Document {
    let mut obj = Obj::new(root, "$", unmapped);
    let schema_name = obj.string("schema_name");
    let version = obj.string("version");
    let name = obj.text("name");
    let origin = obj.get("origin").cloned();
    let body = obj.get("body").cloned();
    let furniture = obj.get("furniture").cloned();
    let groups = obj.get("groups").cloned();
    let texts = obj.get("texts").cloned();
    let tables = obj.get("tables").cloned();
    let pictures = obj.get("pictures").cloned();
    let pages = obj.get("pages").cloned();
    obj.finish();

    doc::Document {
        schema_name,
        version,
        name,
        origin: origin
            .as_ref()
            .and_then(|value| map_origin(value, unmapped)),
        furniture: furniture
            .as_ref()
            .and_then(|value| map_group(value, "$.furniture", unmapped)),
        body: body
            .as_ref()
            .and_then(|value| map_group(value, "$.body", unmapped)),
        groups: array(groups.as_ref())
            .iter()
            .filter_map(|value| map_group(value, "$.groups[]", unmapped))
            .collect(),
        texts: array(texts.as_ref())
            .iter()
            .filter_map(|value| map_text(value, unmapped))
            .collect(),
        tables: array(tables.as_ref())
            .iter()
            .filter_map(|value| map_table(value, unmapped))
            .collect(),
        pictures: array(pictures.as_ref())
            .iter()
            .filter_map(|value| map_picture(value, unmapped))
            .collect(),
        pages: map_pages(pages.as_ref(), unmapped),
        ..doc::Document::default()
    }
}

/// A borrowed array, or nothing.
fn array(value: Option<&Value>) -> &[Value] {
    value.and_then(Value::as_array).map_or(&[], Vec::as_slice)
}

fn map_origin(value: &Value, unmapped: &mut Unmapped) -> Option<doc::DocumentOrigin> {
    let mut obj = Obj::new(value.as_object()?, "$.origin", unmapped);
    let origin = doc::DocumentOrigin {
        mimetype: obj.text("mimetype"),
        binary_hash: obj
            .get("binary_hash")
            .and_then(Value::as_u64)
            .unwrap_or_default(),
        filename: obj.text("filename"),
        uri: obj.string("uri"),
        ..doc::DocumentOrigin::default()
    };
    obj.finish();
    Some(origin)
}

fn map_group(value: &Value, path: &str, unmapped: &mut Unmapped) -> Option<doc::GroupItem> {
    let mut obj = Obj::new(value.as_object()?, path, unmapped);
    let group = doc::GroupItem {
        self_ref: obj.text("self_ref"),
        parent: obj.get("parent").and_then(map_ref),
        children: refs(obj.get("children")),
        content_layer: layer(obj.string("content_layer").as_deref()) as i32,
        meta: None,
        name: obj.string("name"),
        label: obj
            .string("label")
            .as_deref()
            .and_then(group_label)
            .unwrap_or(doc::GroupLabel::Unspecified) as i32,
        ..doc::GroupItem::default()
    };
    obj.finish();
    Some(group)
}

/// One entry of `texts`, its variant chosen by the docling label.
fn map_text(value: &Value, unmapped: &mut Unmapped) -> Option<doc::BaseTextItem> {
    let map = value.as_object()?;
    let label_text = map
        .get("label")
        .and_then(Value::as_str)
        .unwrap_or("text")
        .to_owned();
    let mut obj = Obj::new(map, "$.texts[]", unmapped);
    let _ = obj.get("label");
    if doc_item_label(&label_text).is_none() {
        // The item still lands — as plain text — but the vocabulary miss
        // is recorded rather than silently flattened.
        obj.unmapped
            .note("$.texts[]", &format!("label={label_text}"));
    }
    let item = match label_text.as_str() {
        "title" => doc::base_text_item::Item::Title(doc::TitleItem {
            base: Some(text_base(&mut obj, &label_text)),
        }),
        "section_header" => {
            let base = text_base(&mut obj, &label_text);
            doc::base_text_item::Item::SectionHeader(doc::SectionHeaderItem {
                base: Some(base),
                level: obj.int("level").max(1),
            })
        }
        "list_item" => {
            let base = text_base(&mut obj, &label_text);
            doc::base_text_item::Item::ListItem(doc::ListItem {
                base: Some(base),
                enumerated: obj.boolean("enumerated"),
                marker: obj.string("marker"),
            })
        }
        "formula" => doc::base_text_item::Item::Formula(doc::FormulaItem {
            base: Some(text_base(&mut obj, &label_text)),
        }),
        "code" => {
            let base = text_base(&mut obj, &label_text);
            let (code_language, code_language_raw) = match obj.string("code_language") {
                None => (None, None),
                Some(language) => match code_language_label(&language) {
                    Some(label) => (Some(label as i32), None),
                    None => (None, Some(language)),
                },
            };
            doc::base_text_item::Item::Code(doc::CodeItem {
                self_ref: base.self_ref,
                parent: base.parent,
                children: base.children,
                content_layer: base.content_layer,
                meta: None,
                label: doc::DocItemLabel::Code as i32,
                prov: base.prov,
                orig: base.orig,
                text: base.text,
                formatting: base.formatting,
                hyperlink: base.hyperlink,
                captions: refs(obj.get("captions")),
                references: refs(obj.get("references")),
                footnotes: refs(obj.get("footnotes")),
                image: obj
                    .get("image")
                    .and_then(|value| map_image_ref(value, obj.unmapped)),
                code_language,
                code_language_raw,
                ..doc::CodeItem::default()
            })
        }
        _ => doc::base_text_item::Item::Text(doc::TextItem {
            base: Some(text_base(&mut obj, &label_text)),
        }),
    };
    obj.finish();
    Some(doc::BaseTextItem { item: Some(item) })
}

/// The base fields every text variant shares.
fn text_base(obj: &mut Obj<'_, '_>, label_text: &str) -> doc::TextItemBase {
    doc::TextItemBase {
        self_ref: obj.text("self_ref"),
        parent: obj.get("parent").and_then(map_ref),
        children: refs(obj.get("children")),
        content_layer: layer(obj.string("content_layer").as_deref()) as i32,
        meta: None,
        label: doc_item_label(label_text).unwrap_or(doc::DocItemLabel::Text) as i32,
        prov: array(obj.get("prov"))
            .iter()
            .filter_map(|value| map_prov(value, obj.unmapped))
            .collect(),
        orig: obj.text("orig"),
        text: obj.text("text"),
        formatting: obj
            .get("formatting")
            .and_then(|value| map_formatting(value, obj.unmapped)),
        hyperlink: obj.string("hyperlink"),
        spans: array(obj.get("spans"))
            .iter()
            .filter_map(|value| map_span(value, obj.unmapped))
            .collect(),
        admonition_kind: obj.string("admonition_kind"),
        raw: obj.string("raw"),
        // Element identity belongs to markup sources that keep it. This
        // fold states none of its own, and ingests what it is handed.
        source_element_name: obj.string("source_element_name"),
        source_namespace: obj.string("source_namespace"),
        ..doc::TextItemBase::default()
    }
}

fn map_table(value: &Value, unmapped: &mut Unmapped) -> Option<doc::TableItem> {
    let mut obj = Obj::new(value.as_object()?, "$.tables[]", unmapped);
    let table = doc::TableItem {
        self_ref: obj.text("self_ref"),
        parent: obj.get("parent").and_then(map_ref),
        children: refs(obj.get("children")),
        content_layer: layer(obj.string("content_layer").as_deref()) as i32,
        meta: None,
        label: obj
            .string("label")
            .as_deref()
            .and_then(doc_item_label)
            .unwrap_or(doc::DocItemLabel::Table) as i32,
        prov: array(obj.get("prov"))
            .iter()
            .filter_map(|value| map_prov(value, obj.unmapped))
            .collect(),
        captions: refs(obj.get("captions")),
        references: refs(obj.get("references")),
        footnotes: refs(obj.get("footnotes")),
        image: obj
            .get("image")
            .and_then(|value| map_image_ref(value, obj.unmapped)),
        data: obj
            .get("data")
            .and_then(|value| map_table_data(value, obj.unmapped)),
        ..doc::TableItem::default()
    };
    obj.finish();
    Some(table)
}

fn map_table_data(value: &Value, unmapped: &mut Unmapped) -> Option<doc::TableData> {
    let mut obj = Obj::new(value.as_object()?, "$.tables[].data", unmapped);
    let data = doc::TableData {
        table_cells: array(obj.get("table_cells"))
            .iter()
            .filter_map(|value| map_table_cell(value, obj.unmapped))
            .collect(),
        num_rows: obj.int("num_rows"),
        num_cols: obj.int("num_cols"),
        grid: array(obj.get("grid"))
            .iter()
            .map(|row| doc::TableRow {
                cells: array(Some(row))
                    .iter()
                    .filter_map(|value| map_table_cell(value, obj.unmapped))
                    .collect(),
            })
            .collect(),
        ..doc::TableData::default()
    };
    obj.finish();
    Some(data)
}

fn map_table_cell(value: &Value, unmapped: &mut Unmapped) -> Option<doc::TableCell> {
    let mut obj = Obj::new(value.as_object()?, "$.tables[].data.cells[]", unmapped);
    let cell = doc::TableCell {
        bbox: obj
            .get("bbox")
            .and_then(|value| map_bbox(value, obj.unmapped)),
        row_span: obj.int("row_span").max(1),
        col_span: obj.int("col_span").max(1),
        start_row_offset_idx: obj.int("start_row_offset_idx"),
        end_row_offset_idx: obj.int("end_row_offset_idx"),
        start_col_offset_idx: obj.int("start_col_offset_idx"),
        end_col_offset_idx: obj.int("end_col_offset_idx"),
        text: obj.text("text"),
        column_header: obj.boolean("column_header"),
        row_header: obj.boolean("row_header"),
        row_section: obj.boolean("row_section"),
        fillable: obj.boolean("fillable"),
        r#ref: None,
        spans: array(obj.get("spans"))
            .iter()
            .filter_map(|value| map_span(value, obj.unmapped))
            .collect(),
        align: obj
            .string("align")
            .as_deref()
            .and_then(alignment)
            .map(|align| align as i32),
        valign: obj
            .string("valign")
            .as_deref()
            .and_then(vertical_alignment)
            .map(|valign| valign as i32),
        ..doc::TableCell::default()
    };
    obj.finish();
    Some(cell)
}

fn map_picture(value: &Value, unmapped: &mut Unmapped) -> Option<doc::PictureItem> {
    let mut obj = Obj::new(value.as_object()?, "$.pictures[]", unmapped);
    let picture = doc::PictureItem {
        self_ref: obj.text("self_ref"),
        parent: obj.get("parent").and_then(map_ref),
        children: refs(obj.get("children")),
        content_layer: layer(obj.string("content_layer").as_deref()) as i32,
        meta: None,
        label: obj
            .string("label")
            .as_deref()
            .and_then(doc_item_label)
            .unwrap_or(doc::DocItemLabel::Picture) as i32,
        prov: array(obj.get("prov"))
            .iter()
            .filter_map(|value| map_prov(value, obj.unmapped))
            .collect(),
        captions: refs(obj.get("captions")),
        references: refs(obj.get("references")),
        footnotes: refs(obj.get("footnotes")),
        image: obj
            .get("image")
            .and_then(|value| map_image_ref(value, obj.unmapped)),
        hyperlink: obj.string("hyperlink"),
        ..doc::PictureItem::default()
    };
    obj.finish();
    Some(picture)
}

fn map_pages(value: Option<&Value>, unmapped: &mut Unmapped) -> HashMap<i32, doc::PageItem> {
    let Some(pages) = value.and_then(Value::as_object) else {
        return HashMap::new();
    };
    let mut out = HashMap::new();
    for (key, page) in pages {
        let Ok(number) = key.parse::<i32>() else {
            unmapped.note("$.pages", key);
            continue;
        };
        let Some(map) = page.as_object() else {
            continue;
        };
        let mut obj = Obj::new(map, "$.pages[]", unmapped);
        out.insert(
            number,
            doc::PageItem {
                size: obj
                    .get("size")
                    .and_then(|value| map_size(value, obj.unmapped)),
                image: obj
                    .get("image")
                    .and_then(|value| map_image_ref(value, obj.unmapped)),
                page_no: obj.int("page_no"),
                ..doc::PageItem::default()
            },
        );
        obj.finish();
    }
    out
}

fn map_prov(value: &Value, unmapped: &mut Unmapped) -> Option<doc::ProvenanceItem> {
    let mut obj = Obj::new(value.as_object()?, "$..prov[]", unmapped);
    let charspan = obj.get("charspan").and_then(Value::as_array);
    let span = charspan.and_then(|span| {
        Some(doc::IntSpan {
            start: i32::try_from(span.first()?.as_i64()?).ok()?,
            end: i32::try_from(span.get(1)?.as_i64()?).ok()?,
        })
    });
    let prov = doc::ProvenanceItem {
        page_no: obj.int("page_no"),
        bbox: obj
            .get("bbox")
            .and_then(|value| map_bbox(value, obj.unmapped)),
        charspan: span,
        line_range: obj.get("line_range").and_then(map_line_span),
        ..doc::ProvenanceItem::default()
    };
    obj.finish();
    Some(prov)
}

fn map_bbox(value: &Value, unmapped: &mut Unmapped) -> Option<doc::BoundingBox> {
    let mut obj = Obj::new(value.as_object()?, "$..bbox", unmapped);
    let origin = obj.string("coord_origin");
    let (coord_origin, coord_origin_raw) = match origin.as_deref() {
        None => (None, None),
        Some("TOPLEFT") => (Some(doc::CoordOrigin::Topleft as i32), None),
        Some("BOTTOMLEFT") => (Some(doc::CoordOrigin::Bottomleft as i32), None),
        Some(other) => (None, Some(other.to_owned())),
    };
    let bbox = doc::BoundingBox {
        l: obj.float("l"),
        t: obj.float("t"),
        r: obj.float("r"),
        b: obj.float("b"),
        coord_origin,
        coord_origin_raw,
    };
    obj.finish();
    Some(bbox)
}

fn map_size(value: &Value, unmapped: &mut Unmapped) -> Option<doc::Size> {
    let mut obj = Obj::new(value.as_object()?, "$..size", unmapped);
    let size = doc::Size {
        width: obj.float("width"),
        height: obj.float("height"),
    };
    obj.finish();
    Some(size)
}

fn map_image_ref(value: &Value, unmapped: &mut Unmapped) -> Option<doc::ImageRef> {
    let mut obj = Obj::new(value.as_object()?, "$..image", unmapped);
    let image = doc::ImageRef {
        mimetype: obj.text("mimetype"),
        dpi: obj.int("dpi"),
        size: obj
            .get("size")
            .and_then(|value| map_size(value, obj.unmapped)),
        uri: obj.text("uri"),
        size_raw: obj.string("size_raw"),
    };
    obj.finish();
    Some(image)
}

fn map_formatting(value: &Value, unmapped: &mut Unmapped) -> Option<doc::Formatting> {
    let mut obj = Obj::new(value.as_object()?, "$..formatting", unmapped);
    let script_text = obj.string("script");
    let (script, script_raw) = match script_text.as_deref() {
        None | Some("baseline") => (doc::Script::Baseline, None),
        Some("sub") => (doc::Script::Sub, None),
        Some("super") => (doc::Script::Super, None),
        Some(other) => (doc::Script::Unspecified, Some(other.to_owned())),
    };
    let formatting = doc::Formatting {
        bold: obj.boolean("bold"),
        italic: obj.boolean("italic"),
        underline: obj.boolean("underline"),
        strikethrough: obj.boolean("strikethrough"),
        script: script as i32,
        script_raw,
        monospace: obj.boolean("monospace"),
        small_caps: obj.boolean("small_caps"),
        math: obj.boolean("math"),
        mark: obj.boolean("mark"),
        small: obj.boolean("small"),
        insertion: obj.boolean("insertion"),
        abbreviation: obj.boolean("abbreviation"),
        quote: obj.boolean("quote"),
        overline: obj.boolean("overline"),
    };
    obj.finish();
    Some(formatting)
}

/// One character run of an item's text, as the schema states it.
///
/// The runs are what a document says about the inside of its own strings, so
/// re-ingesting them is the difference between a round trip and a
/// paraphrase: a document that went out with a bold run and a resolved
/// cross reference comes back with both.
fn map_span(value: &Value, unmapped: &mut Unmapped) -> Option<doc::InlineSpan> {
    let mut obj = Obj::new(value.as_object()?, "$..spans[]", unmapped);
    let span = doc::InlineSpan {
        range: obj.get("range").and_then(map_int_span),
        formatting: obj
            .get("formatting")
            .and_then(|value| map_formatting(value, obj.unmapped)),
        hyperlink: obj.string("hyperlink"),
        target: obj.get("target").and_then(map_fine_ref),
        font_family: obj.string("font_family"),
        font_size_pt: obj.get("font_size_pt").and_then(Value::as_f64),
        color: obj.string("color"),
        language: obj.string("language"),
        field_code: obj.string("field_code"),
        reference_kind: obj
            .string("reference_kind")
            .as_deref()
            .and_then(reference_kind)
            .map(|kind| kind as i32),
        reference: obj.string("reference"),
        link_title: obj.string("link_title"),
        raw: obj.string("raw"),
        style_name: obj.string("style_name"),
        highlight_color: obj.string("highlight_color"),
        annotation: obj.string("annotation"),
    };
    obj.finish();
    Some(span)
}

/// A `LineSpan`, written as the object the schema declares or as the same
/// two-element array shorthand the spans admit.
fn map_line_span(value: &Value) -> Option<doc::LineSpan> {
    let span = map_int_span(value)?;
    Some(doc::LineSpan {
        start: u32::try_from(span.start.max(0)).ok()?,
        end: u32::try_from(span.end.max(0)).ok()?,
    })
}

/// An `IntSpan`, written either as the object the schema declares or as the
/// two-element array the upstream dialect writes a `charspan` as.
fn map_int_span(value: &Value) -> Option<doc::IntSpan> {
    if let Some(pair) = value.as_array() {
        return Some(doc::IntSpan {
            start: i32::try_from(pair.first()?.as_i64()?).ok()?,
            end: i32::try_from(pair.get(1)?.as_i64()?).ok()?,
        });
    }
    let map = value.as_object()?;
    Some(doc::IntSpan {
        start: i32::try_from(map.get("start")?.as_i64()?).ok()?,
        end: i32::try_from(map.get("end")?.as_i64()?).ok()?,
    })
}

/// A `FineRef`: a ref spelled either way, plus the range it narrows to.
fn map_fine_ref(value: &Value) -> Option<doc::FineRef> {
    let map = value.as_object()?;
    let target = map
        .get("$ref")
        .or_else(|| map.get("ref"))
        .and_then(Value::as_str)?;
    Some(doc::FineRef {
        r#ref: target.to_owned(),
        range: map.get("range").and_then(map_int_span),
    })
}

/// The `ReferenceKind` vocabulary, snake case.
fn reference_kind(text: &str) -> Option<doc::ReferenceKind> {
    let wanted = format!("REFERENCE_KIND_{}", text.to_ascii_uppercase());
    (1..=8)
        .filter_map(|value| doc::ReferenceKind::try_from(value).ok())
        .find(|kind| kind.as_str_name() == wanted)
}

/// The horizontal `Alignment` vocabulary, snake case.
fn alignment(text: &str) -> Option<doc::Alignment> {
    let wanted = format!("ALIGNMENT_{}", text.to_ascii_uppercase());
    (1..=4)
        .filter_map(|value| doc::Alignment::try_from(value).ok())
        .find(|align| align.as_str_name() == wanted)
}

/// The `VerticalAlignment` vocabulary, snake case.
fn vertical_alignment(text: &str) -> Option<doc::VerticalAlignment> {
    let wanted = format!("VERTICAL_ALIGNMENT_{}", text.to_ascii_uppercase());
    (1..=3)
        .filter_map(|value| doc::VerticalAlignment::try_from(value).ok())
        .find(|valign| valign.as_str_name() == wanted)
}

/// A `$ref` object — or a bare string — as a `RefItem`.
fn map_ref(value: &Value) -> Option<doc::RefItem> {
    let target = match value {
        Value::String(text) => text.clone(),
        Value::Object(map) => map.get("$ref")?.as_str()?.to_owned(),
        _ => return None,
    };
    Some(doc::RefItem { r#ref: target })
}

/// A `children` / `captions` array of refs.
fn refs(value: Option<&Value>) -> Vec<doc::RefItem> {
    array(value).iter().filter_map(map_ref).collect()
}

/// The docling `content_layer` vocabulary.
fn layer(text: Option<&str>) -> doc::ContentLayer {
    match text {
        Some("furniture") => doc::ContentLayer::Furniture,
        Some("background") => doc::ContentLayer::Background,
        Some("invisible") => doc::ContentLayer::Invisible,
        Some("notes") => doc::ContentLayer::Notes,
        // Body is the schema's own default; an unknown layer degrades to it
        // rather than failing the document.
        _ => doc::ContentLayer::Body,
    }
}

/// The docling `label` vocabulary, snake case, onto `DocItemLabel`.
fn doc_item_label(text: &str) -> Option<doc::DocItemLabel> {
    let wanted = format!("DOC_ITEM_LABEL_{}", text.to_ascii_uppercase());
    (1..=30)
        .filter_map(|value| doc::DocItemLabel::try_from(value).ok())
        .find(|label| label.as_str_name() == wanted)
}

/// The docling group `label` vocabulary onto `GroupLabel`.
fn group_label(text: &str) -> Option<doc::GroupLabel> {
    let wanted = format!("GROUP_LABEL_{}", text.to_ascii_uppercase());
    (1..=11)
        .filter_map(|value| doc::GroupLabel::try_from(value).ok())
        .find(|label| label.as_str_name() == wanted)
}

// --------------------------------------------------------------- projection

/// Walk the ingested body in reading order and re-emit its items as typed
/// events, so the typed wire carries the content even for a format whose
/// real payload is the Document itself.
fn project(document: &doc::Document, emitter: &mut Emitter<'_>) -> Result<(), ParseError> {
    let mut walk = Project {
        document,
        emitter,
        visited: HashSet::new(),
        pending: Vec::new(),
    };
    if let Some(body) = document.body.as_ref() {
        walk.children(&body.children, 0);
    }
    while let Some((self_ref, list_depth)) = walk.pending.pop() {
        walk.item(&self_ref, list_depth)?;
    }
    Ok(())
}

/// The projection walk: refs resolved against the arenas, cycles and
/// double-references broken by the visited set.
///
/// The walk is depth-first and in reading order, but iterative: refs are
/// flat strings, so serde_json's nesting limit says nothing about how deep
/// a `children` chain goes, and a recursive walk over a long chain of
/// groups overflowed the stack and aborted the process.
struct Project<'w, 'e> {
    document: &'w doc::Document,
    emitter: &'w mut Emitter<'e>,
    visited: HashSet<String>,
    /// Refs still to visit, with the list depth they sit at; the next one
    /// in reading order is on top.
    pending: Vec<(String, u32)>,
}

impl Project<'_, '_> {
    fn item(&mut self, self_ref: &str, list_depth: u32) -> Result<(), ParseError> {
        if !self.visited.insert(self_ref.to_owned()) {
            return Ok(());
        }
        if let Some(index) = arena_index(self_ref, "#/texts/") {
            if let Some(item) = self.document.texts.get(index) {
                self.text(item, list_depth)?;
            }
            return Ok(());
        }
        if let Some(index) = arena_index(self_ref, "#/tables/") {
            if let Some(table) = self.document.tables.get(index).cloned() {
                self.table(&table)?;
            }
            return Ok(());
        }
        if let Some(index) = arena_index(self_ref, "#/pictures/") {
            if let Some(picture) = self.document.pictures.get(index) {
                let uri = picture
                    .image
                    .as_ref()
                    .map(|image| image.uri.clone())
                    .unwrap_or_default();
                let hyperlink = picture.hyperlink.clone();
                self.emitter.image(Image {
                    src: uri,
                    hyperlink,
                    // The declared dimensions of an ingested picture are on
                    // its own ImageRef, which the document event carries.
                    ..Image::default()
                })?;
                let children = picture.children.clone();
                self.children(&children, list_depth);
            }
            return Ok(());
        }
        if let Some(index) = arena_index(self_ref, "#/groups/")
            && let Some(group) = self.document.groups.get(index)
        {
            let label = doc::GroupLabel::try_from(group.label).unwrap_or_default();
            let depth = if matches!(label, doc::GroupLabel::List | doc::GroupLabel::OrderedList) {
                list_depth + 1
            } else {
                list_depth
            };
            let children = group.children.clone();
            self.children(&children, depth);
        }
        Ok(())
    }

    /// Queue an item's children to be visited next, first child on top.
    fn children(&mut self, children: &[doc::RefItem], list_depth: u32) {
        self.pending.extend(
            children
                .iter()
                .rev()
                .map(|child| (child.r#ref.clone(), list_depth)),
        );
    }

    fn text(&mut self, item: &doc::BaseTextItem, list_depth: u32) -> Result<(), ParseError> {
        let Some(variant) = item.item.as_ref() else {
            return Ok(());
        };
        let (block, children) = match variant {
            doc::base_text_item::Item::Title(title) => {
                let Some(base) = title.base.as_ref() else {
                    return Ok(());
                };
                (
                    Block {
                        label: pb::BlockLabel::Title,
                        text: base.text.clone(),
                        hyperlink: base.hyperlink.clone(),
                        spans: wire_spans(&base.spans),
                        admonition: wire_admonition(base.admonition_kind.as_ref()),
                        ..Block::default()
                    },
                    base.children.clone(),
                )
            }
            doc::base_text_item::Item::SectionHeader(header) => {
                let Some(base) = header.base.as_ref() else {
                    return Ok(());
                };
                (
                    Block {
                        label: pb::BlockLabel::SectionHeader,
                        text: base.text.clone(),
                        level: Some(u32::try_from(header.level.max(1)).unwrap_or(1)),
                        hyperlink: base.hyperlink.clone(),
                        spans: wire_spans(&base.spans),
                        admonition: wire_admonition(base.admonition_kind.as_ref()),
                        ..Block::default()
                    },
                    base.children.clone(),
                )
            }
            doc::base_text_item::Item::ListItem(list_item) => {
                let Some(base) = list_item.base.as_ref() else {
                    return Ok(());
                };
                (
                    Block {
                        label: pb::BlockLabel::ListItem,
                        text: base.text.clone(),
                        enumerated: list_item.enumerated,
                        marker: list_item.marker.clone(),
                        list_depth: Some(list_depth.max(1)),
                        hyperlink: base.hyperlink.clone(),
                        spans: wire_spans(&base.spans),
                        admonition: wire_admonition(base.admonition_kind.as_ref()),
                        ..Block::default()
                    },
                    base.children.clone(),
                )
            }
            doc::base_text_item::Item::Code(code) => (
                Block {
                    label: pb::BlockLabel::Code,
                    text: code.text.clone(),
                    language: code
                        .code_language_raw
                        .clone()
                        .or_else(|| enum_language(code.code_language)),
                    hyperlink: code.hyperlink.clone(),
                    ..Block::default()
                },
                code.children.clone(),
            ),
            doc::base_text_item::Item::Formula(formula) => {
                let Some(base) = formula.base.as_ref() else {
                    return Ok(());
                };
                (
                    Block {
                        label: pb::BlockLabel::Formula,
                        text: base.text.clone(),
                        spans: wire_spans(&base.spans),
                        ..Block::default()
                    },
                    base.children.clone(),
                )
            }
            doc::base_text_item::Item::Text(text) => {
                let Some(base) = text.base.as_ref() else {
                    return Ok(());
                };
                let (label, role) = block_label_for(base.label);
                (
                    Block {
                        label,
                        role,
                        text: base.text.clone(),
                        hyperlink: base.hyperlink.clone(),
                        spans: wire_spans(&base.spans),
                        admonition: wire_admonition(base.admonition_kind.as_ref()),
                        raw: base.raw.clone(),
                        ..Block::default()
                    },
                    base.children.clone(),
                )
            }
            doc::base_text_item::Item::FieldHeading(_)
            | doc::base_text_item::Item::FieldValue(_) => return Ok(()),
        };
        if !block.text.is_empty() {
            self.emitter.text_block(block)?;
        }
        self.children(&children, list_depth);
        Ok(())
    }

    fn table(&mut self, table: &doc::TableItem) -> Result<(), ParseError> {
        let caption = table.captions.first().and_then(|caption| {
            let index = arena_index(&caption.r#ref, "#/texts/")?;
            // The caption item is consumed here; mark it visited so the
            // walk does not emit it a second time as its own block.
            self.visited.insert(caption.r#ref.clone());
            let item = self.document.texts.get(index)?;
            match item.item.as_ref()? {
                doc::base_text_item::Item::Text(text) => {
                    Some(text.base.as_ref()?.text.clone()).filter(|t| !t.is_empty())
                }
                _ => None,
            }
        });
        self.emitter.table_start(Table {
            caption,
            ..Table::default()
        })?;
        if let Some(data) = table.data.as_ref() {
            for (row_index, row) in data.grid.iter().enumerate() {
                if row.cells.is_empty() {
                    continue;
                }
                // A dense grid shows a spanning cell at every position it
                // covers; the wire states each cell once, at the position
                // it starts. A copy is a cell whose span reaches down from
                // an earlier row, or one that repeats the entry before it
                // across a column span.
                let row_offset = i32::try_from(row_index).unwrap_or(i32::MAX);
                let mut previous: Option<&doc::TableCell> = None;
                let cells: Vec<pb::TableCell> = row
                    .cells
                    .iter()
                    .filter(|cell| {
                        let from_above =
                            cell.row_span > 1 && cell.start_row_offset_idx < row_offset;
                        let repeat = cell.col_span > 1
                            && previous.is_some_and(|before| {
                                before.start_row_offset_idx == cell.start_row_offset_idx
                                    && before.start_col_offset_idx == cell.start_col_offset_idx
                            });
                        previous = Some(cell);
                        !from_above && !repeat
                    })
                    .map(|cell| pb::TableCell {
                        column_index: u32::try_from(cell.start_col_offset_idx.max(0))
                            .unwrap_or_default(),
                        text: cell.text.clone(),
                        column_span: u32::try_from(cell.col_span.max(1)).unwrap_or(1),
                        row_span: u32::try_from(cell.row_span.max(1)).unwrap_or(1),
                        is_header: cell.column_header,
                        spans: wire_spans(&cell.spans),
                        align: wire_alignment(cell.align) as i32,
                        valign: wire_valignment(cell.valign) as i32,
                        // A rich cell's group lives on the document event,
                        // which this format carries item for item.
                        blocks: Vec::new(),
                    })
                    .collect();
                let is_header = row.cells.iter().all(|cell| cell.column_header);
                self.emitter.table_row(is_header, cells)?;
            }
        }
        self.emitter.table_end()
    }
}

/// The Document plane's runs as the typed wire's, so a client that only
/// speaks the events sees the same runs the `document` event carries.
///
/// The mapping is the inverse of the one [`crate::document_fold`] applies,
/// arm for arm. Two things do not survive the trip back, because the wire
/// has no field for them and inventing one to hold a value no fold produces
/// would be worse than saying so: an overline, and a highlight stated as a
/// colour rather than as a `mark`.
fn wire_spans(spans: &[doc::InlineSpan]) -> Vec<pb::InlineSpan> {
    spans
        .iter()
        .map(|span| pb::InlineSpan {
            range: span.range.map(|range| pb::IntRange {
                start: u32::try_from(range.start.max(0)).unwrap_or_default(),
                end: u32::try_from(range.end.max(0)).unwrap_or_default(),
            }),
            styles: wire_styles(span.formatting.as_ref()),
            hyperlink: span.hyperlink.clone(),
            link_title: span.link_title.clone(),
            // An anchor is a document-level fact on this plane: it lives in
            // `Document.anchors`, not on the run that declared it.
            anchor_id: None,
            reference: span.reference.clone(),
            reference_kind: wire_reference_kind(span.reference_kind) as i32,
            raw: span.raw.clone(),
            font_family: span.font_family.clone(),
            font_size_pt: span.font_size_pt,
            color: span.color.clone(),
            language: span.language.clone(),
            field_code: span.field_code.clone(),
            style_name: span.style_name.clone(),
            annotation: span.annotation.clone(),
        })
        .collect()
}

/// The rendering signals a `Formatting` states, as the wire's own styles.
fn wire_styles(formatting: Option<&doc::Formatting>) -> Vec<i32> {
    let Some(formatting) = formatting else {
        return Vec::new();
    };
    let mut styles = Vec::new();
    for (set, style) in [
        (formatting.bold, pb::SpanStyle::Bold),
        (formatting.italic, pb::SpanStyle::Italic),
        (formatting.underline, pb::SpanStyle::Underline),
        (formatting.strikethrough, pb::SpanStyle::Strikethrough),
        (formatting.monospace, pb::SpanStyle::Code),
        (formatting.small_caps, pb::SpanStyle::SmallCaps),
        (formatting.math, pb::SpanStyle::Math),
        (formatting.mark, pb::SpanStyle::Mark),
        (formatting.small, pb::SpanStyle::Small),
        (formatting.insertion, pb::SpanStyle::Insertion),
        (formatting.abbreviation, pb::SpanStyle::Abbreviation),
        (formatting.quote, pb::SpanStyle::Quote),
    ] {
        if set {
            styles.push(style as i32);
        }
    }
    match doc::Script::try_from(formatting.script).unwrap_or_default() {
        doc::Script::Super => styles.push(pb::SpanStyle::Superscript as i32),
        doc::Script::Sub => styles.push(pb::SpanStyle::Subscript as i32),
        doc::Script::Unspecified | doc::Script::Baseline => {}
    }
    styles
}

/// The wire's name for what a reference points at.
///
/// The wire is the finer vocabulary in one direction and the coarser in the
/// other: it separates a citation from a bibliography entry, and it has no
/// arm of its own for a section, a figure, a table or a claim. All four are
/// references into the document, so all four are cross references here.
fn wire_reference_kind(kind: Option<i32>) -> pb::ReferenceKind {
    let Some(kind) = kind.and_then(|kind| doc::ReferenceKind::try_from(kind).ok()) else {
        return pb::ReferenceKind::Unspecified;
    };
    match kind {
        doc::ReferenceKind::Unspecified => pb::ReferenceKind::Unspecified,
        doc::ReferenceKind::Citation => pb::ReferenceKind::Citation,
        doc::ReferenceKind::Footnote => pb::ReferenceKind::Footnote,
        doc::ReferenceKind::Equation => pb::ReferenceKind::EquationRef,
        doc::ReferenceKind::Claim
        | doc::ReferenceKind::Section
        | doc::ReferenceKind::CrossRef
        | doc::ReferenceKind::Figure
        | doc::ReferenceKind::Table => pb::ReferenceKind::CrossRef,
    }
}

/// The callout kind an item declares, as the wire's own enum. A name outside
/// the vocabulary is not a callout the wire can state.
fn wire_admonition(kind: Option<&String>) -> Option<pb::AdmonitionKind> {
    let wanted = format!("ADMONITION_KIND_{}", kind?.to_ascii_uppercase());
    (1..=5)
        .filter_map(|value| pb::AdmonitionKind::try_from(value).ok())
        .find(|kind| kind.as_str_name() == wanted)
}

/// The wire's one alignment vocabulary, which merges both axes; the folds
/// keep each value in the field that owns its axis.
const fn wire_alignment(align: Option<i32>) -> pb::Alignment {
    match align {
        Some(1) => pb::Alignment::Left,
        Some(2) => pb::Alignment::Center,
        Some(3) => pb::Alignment::Right,
        Some(4) => pb::Alignment::Justify,
        _ => pb::Alignment::Unspecified,
    }
}

/// The vertical half of the same vocabulary.
const fn wire_valignment(valign: Option<i32>) -> pb::Alignment {
    match valign {
        Some(1) => pb::Alignment::Top,
        Some(2) => pb::Alignment::Middle,
        Some(3) => pb::Alignment::Bottom,
        _ => pb::Alignment::Unspecified,
    }
}

/// `#/texts/3` to `3`, for one arena prefix.
fn arena_index(self_ref: &str, prefix: &str) -> Option<usize> {
    self_ref.strip_prefix(prefix)?.parse().ok()
}

/// A stored enum language back to a declarable string.
fn enum_language(code_language: Option<i32>) -> Option<String> {
    let label = doc::CodeLanguageLabel::try_from(code_language?).ok()?;
    if label == doc::CodeLanguageLabel::Unspecified {
        return None;
    }
    Some(
        label
            .as_str_name()
            .trim_start_matches("CODE_LANGUAGE_LABEL_")
            .to_ascii_lowercase(),
    )
}

/// A stored `DocItemLabel` back to the wire block vocabulary, with the
/// docling label carried in `role` when the block vocabulary is coarser.
fn block_label_for(label: i32) -> (pb::BlockLabel, String) {
    let label = doc::DocItemLabel::try_from(label).unwrap_or(doc::DocItemLabel::Text);
    let block = match label {
        doc::DocItemLabel::Title => pb::BlockLabel::Title,
        doc::DocItemLabel::SectionHeader => pb::BlockLabel::SectionHeader,
        doc::DocItemLabel::Paragraph => pb::BlockLabel::Paragraph,
        doc::DocItemLabel::ListItem => pb::BlockLabel::ListItem,
        doc::DocItemLabel::Code => pb::BlockLabel::Code,
        doc::DocItemLabel::Formula => pb::BlockLabel::Formula,
        doc::DocItemLabel::Caption => pb::BlockLabel::Caption,
        doc::DocItemLabel::Footnote => pb::BlockLabel::Footnote,
        _ => pb::BlockLabel::Paragraph,
    };
    let role = if block == pb::BlockLabel::Paragraph && label != doc::DocItemLabel::Paragraph {
        label
            .as_str_name()
            .trim_start_matches("DOC_ITEM_LABEL_")
            .to_ascii_lowercase()
    } else {
        String::new()
    };
    (block, role)
}

// ----------------------------------------------------------------- stamping

/// Append this collector's `CollectorSource` to every item's sources.
///
/// Additive on purpose: sources never overwrite each other, so the
/// document keeps whatever attribution it arrived with and gains a record
/// of having passed through this collector.
fn stamp(document: &mut doc::Document) {
    let source = doc::SourceType {
        source: Some(doc::source_type::Source::Collector(doc::CollectorSource {
            collector: COLLECTOR.to_owned(),
            model: Some("docling-json".to_owned()),
            version: Some(VERSION.to_owned()),
            confidence: None,
            ..doc::CollectorSource::default()
        })),
    };
    for item in &mut document.texts {
        match item.item.as_mut() {
            Some(doc::base_text_item::Item::Code(code)) => code.source.push(source.clone()),
            Some(other) => {
                if let Some(base) = text_base_mut(other) {
                    base.source.push(source.clone());
                }
            }
            None => {}
        }
    }
    for table in &mut document.tables {
        table.source.push(source.clone());
    }
    for picture in &mut document.pictures {
        picture.source.push(source.clone());
    }
}

/// The mutable base of every text variant that has one.
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
