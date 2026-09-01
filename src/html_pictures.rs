// SPDX-License-Identifier: Apache-2.0

//! Picture sources of the HTML fold other than a plain `<img src>`.
//!
//! Every function here turns one element into the same [`Image`] an `<img>`
//! produces, so downstream nothing can tell the two apart: `src` is the
//! reference as the author wrote it, never resolved and never fetched, and
//! the Document fold projects it to `ImageRef.uri` verbatim. A consumer that
//! re-points chapter pictures at an archive entry (an EPUB reader does)
//! therefore sees a cover wrapped in `<svg><image href=...></svg>` exactly
//! as it would see `<img src=...>`.
//!
//! The family covered, and what each yields:
//!
//! | Source | Pictures |
//! |---|---|
//! | `<img srcset>` with no `src` | one, from the first srcset candidate |
//! | `<svg>` with `<image>` descendants | one per `<image>`, `href` preferred over `xlink:href` |
//! | `<svg>` with vector content and no `<image>` | one, the element serialized as a `data:image/svg+xml;base64,` URI, only under [`INLINE_SVG_MAX_BYTES`] |
//! | decorative `<svg>` | none (see [`is_decorative_svg`]) |
//! | `<object data type="image/*">`, `<embed src type="image/*">` | one, from `data` / `src` |
//!
//! Alternative text follows the SVG accessibility mapping: `aria-label`
//! first, then the element's own `<title>` child, then its `<desc>`; a
//! `<desc>` not consumed as the name rides as the picture's `title`. An
//! `<image>` with no text of its own borrows the enclosing `<svg>`'s.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use scraper::ElementRef;
use scraper::node::Node;

use crate::parse::Image;

/// The largest serialized inline `<svg>` carried as a `data:` URI, in bytes
/// of markup before base64 expansion (which adds a third).
///
/// The fold's Document is merged into every downstream consumer's copy of
/// the work, so an inline picture is paid for on every hop. 32 KiB covers
/// logos, cover frames and simple diagrams, which is what authors inline;
/// a vector map or a plotted chart runs to hundreds of kilobytes and is
/// dropped rather than multiplied. The cap is on the source markup, not on
/// the URI, so the number is one an author can check against their file.
pub const INLINE_SVG_MAX_BYTES: usize = 32 * 1024;

/// The largest rendered box, in CSS pixels, that still marks an `<svg>` as
/// decorative when both `width` and `height` state it.
///
/// Icon sets ship at 16, 20 and 24 px; a picture the reader is meant to
/// look at is larger. Only bare numbers and `px` lengths count; an `em` or
/// a percentage says nothing about pixels and never makes an icon.
pub const DECORATIVE_SVG_MAX_PX: f64 = 24.0;

/// The SVG namespace, which html5ever puts on every element under `<svg>`.
const SVG_NS: &str = "http://www.w3.org/2000/svg";
/// The XLink namespace html5ever assigns to a foreign `xlink:href`.
const XLINK_NS: &str = "http://www.w3.org/1999/xlink";
/// The XML namespace, for `xml:lang` and `xml:space` on serialized nodes.
const XML_NS: &str = "http://www.w3.org/XML/1998/namespace";
/// The namespace of `xmlns` declarations themselves, which the serializer
/// rewrites rather than copies.
const XMLNS_NS: &str = "http://www.w3.org/2000/xmlns/";

/// Elements that draw something on their own. An `<svg>` made only of
/// `<defs>`, `<symbol>` or `<use>` is a sprite sheet or a reference into
/// one, and serializing it alone renders nothing.
const VECTOR_CONTENT: &[&str] = &[
    "path", "rect", "circle", "ellipse", "line", "polyline", "polygon", "text",
];

/// File extensions that mark an `<object data>` or `<embed src>` as an image
/// when the element declares no `type`.
const IMAGE_EXTENSIONS: &[&str] = &[
    "svg", "svgz", "png", "jpg", "jpeg", "gif", "webp", "bmp", "tif", "tiff", "avif", "ico",
];

/// The first candidate URL of a `srcset`.
///
/// A candidate is a URL followed by an optional width or density
/// descriptor, candidates separated by commas; the first URL is the one a
/// browser falls back to when nothing else matches, which makes it the
/// representative reference. The URL is the leading run of non-whitespace
/// with a trailing comma stripped, so a `data:` URL with commas inside
/// survives as long as the author left no whitespace in it.
pub fn srcset_first_candidate(srcset: &str) -> Option<String> {
    let first = srcset.split_whitespace().next()?;
    let url = first.trim_end_matches(',');
    (!url.is_empty()).then(|| url.to_owned())
}

/// Every picture an `<svg>` element contributes, in document order.
///
/// A decorative element contributes nothing. One with `<image>` descendants
/// contributes one picture per `<image>` that names an `href`. One with
/// drawn content and no `<image>` contributes itself, serialized, when it
/// fits under [`INLINE_SVG_MAX_BYTES`].
pub fn svg_pictures(svg: &ElementRef<'_>) -> Vec<Image> {
    if is_decorative_svg(svg) {
        return Vec::new();
    }
    let images: Vec<Image> = svg_elements(svg, "image")
        .filter_map(|image| svg_image_picture(svg, &image))
        .collect();
    if !images.is_empty() {
        return with_svg_anchor(svg, images);
    }
    inline_svg_picture(svg).into_iter().collect()
}

/// Whether an `<svg>` is presentation rather than content: hidden from the
/// accessibility tree, given a presentational role, or an icon-sized box.
pub fn is_decorative_svg(svg: &ElementRef<'_>) -> bool {
    let hidden = trimmed_attr(svg, "aria-hidden").is_some_and(|value| value == "true");
    let presentational = trimmed_attr(svg, "role")
        .is_some_and(|role| matches!(role.to_ascii_lowercase().as_str(), "presentation" | "none"));
    hidden || presentational || is_icon_sized(svg)
}

/// Both dimensions stated as pixel lengths and neither past the icon cap.
fn is_icon_sized(svg: &ElementRef<'_>) -> bool {
    let pixels = |name: &str| trimmed_attr(svg, name).and_then(|value| pixel_length(&value));
    matches!(
        (pixels("width"), pixels("height")),
        (Some(width), Some(height))
            if width <= DECORATIVE_SVG_MAX_PX && height <= DECORATIVE_SVG_MAX_PX
    )
}

/// A CSS pixel length: a bare number or one with a `px` suffix. Any other
/// unit is not a pixel count and yields nothing.
fn pixel_length(value: &str) -> Option<f64> {
    let number = value.strip_suffix("px").unwrap_or(value).trim();
    number.parse::<f64>().ok().filter(|px| px.is_finite())
}

/// The picture one `<image>` inside an `<svg>` names, when it names one.
fn svg_image_picture(svg: &ElementRef<'_>, image: &ElementRef<'_>) -> Option<Image> {
    let src = href_of(image)?;
    let (alt, title) = labels_of(image).unwrap_or_else(|| labels_of(svg).unwrap_or_default());
    Some(Image {
        src,
        alt,
        title,
        source_line: None,
        width: trimmed_attr(image, "width"),
        height: trimmed_attr(image, "height"),
        anchor_id: trimmed_attr(image, "id"),
    })
}

/// An `id` on the `<svg>` itself names the first picture it produced, so a
/// fragment link to the figure still lands on something.
fn with_svg_anchor(svg: &ElementRef<'_>, mut images: Vec<Image>) -> Vec<Image> {
    if let Some(first) = images.first_mut()
        && first.anchor_id.is_none()
    {
        first.anchor_id = trimmed_attr(svg, "id");
    }
    images
}

/// The `<svg>` itself as a picture: its drawn content serialized into a
/// `data:image/svg+xml;base64,` URI, when there is drawn content and the
/// markup fits under the cap.
fn inline_svg_picture(svg: &ElementRef<'_>) -> Option<Image> {
    if !has_vector_content(svg) {
        return None;
    }
    let markup = serialize_svg(svg);
    if markup.len() > INLINE_SVG_MAX_BYTES {
        return None;
    }
    let (alt, title) = labels_of(svg).unwrap_or_default();
    Some(Image {
        src: format!("data:image/svg+xml;base64,{}", BASE64.encode(markup)),
        alt,
        title,
        source_line: None,
        width: trimmed_attr(svg, "width"),
        height: trimmed_attr(svg, "height"),
        anchor_id: trimmed_attr(svg, "id"),
    })
}

/// Whether anything under the `<svg>` draws on its own.
fn has_vector_content(svg: &ElementRef<'_>) -> bool {
    VECTOR_CONTENT
        .iter()
        .any(|name| svg_elements(svg, name).next().is_some())
}

/// The picture an `<object>` or `<embed>` embeds, when what it embeds is an
/// image: a `type` in the `image/` tree, or, with no `type` declared, a
/// reference whose extension is an image's. `reference_attr` is `data` for
/// `<object>` and `src` for `<embed>`.
pub fn embedded_picture(element: &ElementRef<'_>, reference_attr: &str) -> Option<Image> {
    let src = trimmed_attr(element, reference_attr)?;
    let is_image = match trimmed_attr(element, "type") {
        Some(declared) => declared.to_ascii_lowercase().starts_with("image/"),
        None => has_image_extension(&src),
    };
    if !is_image {
        return None;
    }
    // An <object>'s fallback content is what a reader sees when the image
    // cannot render: the nearest thing the element has to alternative text.
    let alt = trimmed_attr(element, "aria-label")
        .or_else(|| non_empty(collapse(&element.text().collect::<String>())))
        .unwrap_or_default();
    Some(Image {
        src,
        alt,
        title: trimmed_attr(element, "title").unwrap_or_default(),
        source_line: None,
        width: trimmed_attr(element, "width"),
        height: trimmed_attr(element, "height"),
        anchor_id: trimmed_attr(element, "id"),
    })
}

/// Whether a reference's path ends in an image extension, query and
/// fragment ignored.
fn has_image_extension(reference: &str) -> bool {
    let path = reference.split(['?', '#']).next().unwrap_or(reference);
    let extension = path
        .rsplit('/')
        .next()
        .and_then(|file| file.rsplit_once('.'))
        .map(|(_, extension)| extension.to_ascii_lowercase());
    extension.is_some_and(|extension| IMAGE_EXTENSIONS.contains(&extension.as_str()))
}

// -------------------------------------------------------------- attributes

/// The `href` of a foreign element: the unprefixed attribute first, the
/// XLink one when that is all the author wrote. html5ever puts `xlink:href`
/// in the XLink namespace with the local name `href`, so a plain attribute
/// lookup by either spelling misses it.
fn href_of(element: &ElementRef<'_>) -> Option<String> {
    let mut plain = None;
    let mut xlink = None;
    for (name, value) in element.value().attrs.iter() {
        if &*name.local != "href" {
            continue;
        }
        match &*name.ns {
            "" => plain = Some(value),
            XLINK_NS => xlink = Some(value),
            _ => {}
        }
    }
    plain
        .or(xlink)
        .and_then(|value| non_empty(value.trim().to_owned()))
}

/// The accessible name and description of an SVG element as
/// `(alt, title)`: `aria-label` or the `<title>` child names it, the
/// `<desc>` child describes it. With no name, the description stands in as
/// the name so the text is not lost. `None` when the element says nothing.
fn labels_of(element: &ElementRef<'_>) -> Option<(String, String)> {
    let name = trimmed_attr(element, "aria-label").or_else(|| child_text(element, "title"));
    let description = child_text(element, "desc");
    match (name, description) {
        (None, None) => None,
        (Some(name), description) => Some((name, description.unwrap_or_default())),
        (None, Some(description)) => Some((description, String::new())),
    }
}

/// The collapsed text of the first child element named `name`, when it is
/// not blank.
fn child_text(element: &ElementRef<'_>, name: &str) -> Option<String> {
    element
        .child_elements()
        .find(|child| child.value().name() == name)
        .and_then(|child| non_empty(collapse(&child.text().collect::<String>())))
}

/// Every descendant element with local name `name` in the SVG namespace,
/// in document order.
fn svg_elements<'a>(
    svg: &ElementRef<'a>,
    name: &'a str,
) -> impl Iterator<Item = ElementRef<'a>> + 'a {
    svg.descendants()
        .filter_map(ElementRef::wrap)
        .filter(move |element| {
            element.value().name() == name && &*element.value().name.ns == SVG_NS
        })
}

/// An attribute's value trimmed, when it is present and not blank.
fn trimmed_attr(element: &ElementRef<'_>, name: &str) -> Option<String> {
    element
        .attr(name)
        .and_then(|value| non_empty(value.trim().to_owned()))
}

fn non_empty(value: String) -> Option<String> {
    (!value.is_empty()).then_some(value)
}

/// Runs of whitespace collapsed to single spaces, ends trimmed.
fn collapse(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

// ----------------------------------------------------------- serialization

/// The `<svg>` subtree as a standalone SVG document body: XML escaping,
/// every namespace declaration the content needs written on the root and
/// the author's own declarations dropped in favour of them.
fn serialize_svg(svg: &ElementRef<'_>) -> String {
    let mut out = String::new();
    write_element(&mut out, svg, true);
    out
}

fn write_element(out: &mut String, element: &ElementRef<'_>, root: bool) {
    let name = element.value().name();
    out.push('<');
    out.push_str(name);
    if root {
        out.push_str(" xmlns=\"");
        out.push_str(SVG_NS);
        out.push('"');
        if uses_namespace(element, XLINK_NS) {
            out.push_str(" xmlns:xlink=\"");
            out.push_str(XLINK_NS);
            out.push('"');
        }
    }
    for (attribute, value) in element.value().attrs.iter() {
        let Some(prefix) = attribute_prefix(&attribute.ns) else {
            continue;
        };
        out.push(' ');
        out.push_str(prefix);
        out.push_str(&attribute.local);
        out.push_str("=\"");
        push_escaped(out, value, true);
        out.push('"');
    }
    out.push('>');
    for child in element.children() {
        match child.value() {
            Node::Text(text) => push_escaped(out, text, false),
            Node::Element(_) => {
                if let Some(child) = ElementRef::wrap(child) {
                    write_element(out, &child, false);
                }
            }
            _ => {}
        }
    }
    out.push_str("</");
    out.push_str(name);
    out.push('>');
}

/// How an attribute's namespace is spelled on the wire, or `None` for a
/// namespace declaration, which the root writes itself.
fn attribute_prefix(namespace: &str) -> Option<&'static str> {
    match namespace {
        "" => Some(""),
        XLINK_NS => Some("xlink:"),
        XML_NS => Some("xml:"),
        XMLNS_NS => None,
        _ => Some(""),
    }
}

/// Whether any attribute in the subtree lives in `namespace`.
fn uses_namespace(element: &ElementRef<'_>, namespace: &str) -> bool {
    element
        .descendants()
        .filter_map(ElementRef::wrap)
        .any(|node| {
            node.value()
                .attrs
                .iter()
                .any(|(name, _)| &*name.ns == namespace)
        })
}

/// XML-escape text into `out`; attribute values also escape the quote.
fn push_escaped(out: &mut String, text: &str, attribute: bool) {
    for character in text.chars() {
        match character {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' if attribute => out.push_str("&quot;"),
            other => out.push(other),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use scraper::{Html, Selector};

    fn first<'a>(html: &'a Html, selector: &str) -> ElementRef<'a> {
        html.select(&Selector::parse(selector).unwrap())
            .next()
            .expect("the fixture contains the element")
    }

    #[test]
    fn the_first_srcset_candidate_wins_with_or_without_a_descriptor() {
        assert_eq!(
            srcset_first_candidate("a.jpg 1x, b.jpg 2x").as_deref(),
            Some("a.jpg")
        );
        assert_eq!(
            srcset_first_candidate(" small.png, large.png 800w").as_deref(),
            Some("small.png")
        );
        assert_eq!(
            srcset_first_candidate("only.webp").as_deref(),
            Some("only.webp")
        );
        assert_eq!(srcset_first_candidate("   "), None);
        assert_eq!(srcset_first_candidate(","), None);
    }

    #[test]
    fn a_plain_href_wins_over_xlink_and_xlink_stands_alone() {
        let html = Html::parse_fragment(
            r#"<svg><image href="new.jpg" xlink:href="old.jpg"/></svg>
               <svg><image xlink:href="only.jpg"/></svg>"#,
        );
        let images: Vec<Image> = html
            .select(&Selector::parse("svg").unwrap())
            .flat_map(|svg| svg_pictures(&svg))
            .collect();
        assert_eq!(images.len(), 2);
        assert_eq!(images[0].src, "new.jpg");
        assert_eq!(images[1].src, "only.jpg");
    }

    #[test]
    fn svg_text_names_the_picture_in_accessibility_order() {
        let html = Html::parse_fragment(
            r#"<svg aria-label="Label"><title>Title</title><desc>Desc</desc><image href="a.png"/></svg>
               <svg><title>Title</title><desc>Desc</desc><image href="b.png"/></svg>
               <svg><desc>Desc only</desc><image href="c.png"/></svg>
               <svg><image href="d.png"><title>Own</title></image></svg>"#,
        );
        let images: Vec<Image> = html
            .select(&Selector::parse("svg").unwrap())
            .flat_map(|svg| svg_pictures(&svg))
            .collect();
        assert_eq!(
            (images[0].alt.as_str(), images[0].title.as_str()),
            ("Label", "Desc")
        );
        assert_eq!(
            (images[1].alt.as_str(), images[1].title.as_str()),
            ("Title", "Desc")
        );
        assert_eq!(
            (images[2].alt.as_str(), images[2].title.as_str()),
            ("Desc only", "")
        );
        assert_eq!(images[3].alt, "Own", "an image's own title beats the svg's");
    }

    #[test]
    fn decorative_svgs_are_recognized_by_aria_role_and_icon_size() {
        let cases = [
            (
                r#"<svg aria-hidden="true"><image href="x.png"/></svg>"#,
                true,
            ),
            (
                r#"<svg role="presentation"><image href="x.png"/></svg>"#,
                true,
            ),
            (r#"<svg role="None"><image href="x.png"/></svg>"#, true),
            (
                r#"<svg width="24" height="24px"><image href="x.png"/></svg>"#,
                true,
            ),
            (
                r#"<svg width="24" height="25"><image href="x.png"/></svg>"#,
                false,
            ),
            (r#"<svg width="16"><image href="x.png"/></svg>"#, false),
            (
                r#"<svg width="1em" height="1em"><image href="x.png"/></svg>"#,
                false,
            ),
            (
                r#"<svg aria-hidden="false"><image href="x.png"/></svg>"#,
                false,
            ),
        ];
        for (markup, decorative) in cases {
            let html = Html::parse_fragment(markup);
            let svg = first(&html, "svg");
            assert_eq!(is_decorative_svg(&svg), decorative, "{markup}");
            assert_eq!(svg_pictures(&svg).is_empty(), decorative, "{markup}");
        }
    }

    #[test]
    fn an_image_borrows_the_svg_anchor_only_when_it_has_none() {
        let html = Html::parse_fragment(
            r#"<svg id="fig"><image href="a.png"/><image id="own" href="b.png"/></svg>"#,
        );
        let images = svg_pictures(&first(&html, "svg"));
        assert_eq!(images[0].anchor_id.as_deref(), Some("fig"));
        assert_eq!(images[1].anchor_id.as_deref(), Some("own"));
    }

    #[test]
    fn drawn_content_serializes_to_a_standalone_data_uri() {
        let html = Html::parse_fragment(
            r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 10 10" width="100" height="100">
                 <title>Dot</title><path d="M0 0h10v10z" fill="#000"/><text>a&amp;b</text>
               </svg>"##,
        );
        let images = svg_pictures(&first(&html, "svg"));
        assert_eq!(images.len(), 1);
        assert_eq!(images[0].alt, "Dot");
        assert_eq!(images[0].width.as_deref(), Some("100"));
        let encoded = images[0]
            .src
            .strip_prefix("data:image/svg+xml;base64,")
            .expect("a base64 SVG data URI");
        let markup = String::from_utf8(BASE64.decode(encoded).unwrap()).unwrap();
        assert!(
            markup.starts_with(r#"<svg xmlns="http://www.w3.org/2000/svg""#),
            "{markup}"
        );
        assert_eq!(
            markup.matches("xmlns=").count(),
            1,
            "one declaration: {markup}"
        );
        assert!(
            markup.contains(r#"viewBox="0 0 10 10""#),
            "camel case survives: {markup}"
        );
        assert!(
            markup.contains("<text>a&amp;b</text>"),
            "XML escaping: {markup}"
        );
        assert!(
            markup.contains(r##"<path d="M0 0h10v10z" fill="#000"></path>"##),
            "{markup}"
        );
    }

    #[test]
    fn xlink_use_inside_drawn_content_declares_its_namespace() {
        let html = Html::parse_fragment(
            r##"<svg><defs><path id="p" d="M0 0h1"/></defs><use xlink:href="#p"/><rect width="1" height="1"/></svg>"##,
        );
        let images = svg_pictures(&first(&html, "svg"));
        let encoded = images[0]
            .src
            .strip_prefix("data:image/svg+xml;base64,")
            .unwrap();
        let markup = String::from_utf8(BASE64.decode(encoded).unwrap()).unwrap();
        assert!(
            markup.contains(r#"xmlns:xlink="http://www.w3.org/1999/xlink""#),
            "{markup}"
        );
        assert!(
            markup.contains(r##"<use xlink:href="#p"></use>"##),
            "{markup}"
        );
    }

    #[test]
    fn an_svg_with_nothing_drawn_or_too_much_drawn_is_no_picture() {
        let sprite =
            Html::parse_fragment(r##"<svg><defs><symbol id="s"/></defs><use href="#s"/></svg>"##);
        assert!(svg_pictures(&first(&sprite, "svg")).is_empty());

        let empty = Html::parse_fragment("<svg viewBox=\"0 0 1 1\"></svg>");
        assert!(svg_pictures(&first(&empty, "svg")).is_empty());

        let big_path = format!(
            r#"<svg><path d="{}"/></svg>"#,
            "M0 0h1 ".repeat(INLINE_SVG_MAX_BYTES / 4)
        );
        let big = Html::parse_fragment(&big_path);
        assert!(svg_pictures(&first(&big, "svg")).is_empty(), "over the cap");
    }

    #[test]
    fn objects_and_embeds_are_pictures_only_when_they_embed_an_image() {
        let html = Html::parse_fragment(
            r#"<object data="chart.svg" type="image/svg+xml" width="300">A chart of sales</object>
               <object data="movie.swf" type="application/x-shockwave-flash"></object>
               <object data="untyped.PNG"></object>
               <object data="report.pdf"></object>
               <embed src="photo.webp" type="image/webp" title="A photo">
               <embed src="clip.mp4" type="video/mp4">"#,
        );
        let objects: Vec<Option<Image>> = html
            .select(&Selector::parse("object").unwrap())
            .map(|object| embedded_picture(&object, "data"))
            .collect();
        let chart = objects[0].as_ref().expect("an svg object is a picture");
        assert_eq!(chart.src, "chart.svg");
        assert_eq!(chart.alt, "A chart of sales", "fallback content is the alt");
        assert_eq!(chart.width.as_deref(), Some("300"));
        assert!(objects[1].is_none(), "a non-image type is skipped");
        assert_eq!(objects[2].as_ref().unwrap().src, "untyped.PNG");
        assert!(objects[3].is_none(), "a document is not a picture");

        let embeds: Vec<Option<Image>> = html
            .select(&Selector::parse("embed").unwrap())
            .map(|embed| embedded_picture(&embed, "src"))
            .collect();
        let photo = embeds[0].as_ref().expect("an image embed is a picture");
        assert_eq!(photo.src, "photo.webp");
        assert_eq!(photo.title, "A photo");
        assert!(embeds[1].is_none());
    }

    #[test]
    fn image_extensions_ignore_query_fragment_and_case() {
        assert!(has_image_extension("a/b/c.JPG?x=1#y"));
        assert!(has_image_extension("cover.svgz"));
        assert!(!has_image_extension("doc.pdf"));
        assert!(!has_image_extension("noext"));
        assert!(!has_image_extension("dir.png/file"));
    }
}
