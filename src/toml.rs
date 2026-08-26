// SPDX-License-Identifier: Apache-2.0

//! A reader for TOML front matter, producing the same
//! [`MetaValue`](crate::proto::v1::MetaValue) tree the YAML reader does.
//!
//! Static-site generators write front matter in TOML between `+++` fences
//! as readily as in YAML between `---` ones, and the Markdown fold used to
//! read only the second, so a TOML preamble was body text. This reads the
//! first onto the same shape, key for key.
//!
//! What it covers is what front matter is made of: key/value pairs, dotted
//! keys, `[table]` and `[[array.of.tables]]` headers, arrays, inline
//! tables, basic and literal strings in both their one-line and their
//! multi-line spellings, and comments.
//!
//! Two deliberate limits, both the same limit the YAML side has:
//!
//! - **Every scalar is verbatim text.** A `version = 1.10` arrives as the
//!   string "1.10", a `date = 2026-08-23` as "2026-08-23". The wire has no
//!   number arm to mangle a version or a date with, so the reader does not
//!   invent typing the consumer would have to undo.
//! - **Escapes are the ones the format defines**, decoded in the basic
//!   strings that admit them and left alone in the literal strings that do
//!   not.
//!
//! A document this cannot read returns `None`, and the caller warns rather
//! than emitting the preamble as prose.

use std::collections::HashMap;

use crate::proto::v1 as pb;

/// Parse a TOML document into the metadata tree, or `None` when the text is
/// not a TOML document this reader can complete.
#[must_use]
pub fn parse(text: &str) -> Option<HashMap<String, pb::MetaValue>> {
    let mut reader = Reader { rest: text };
    let mut root = HashMap::new();
    // The table header in force, as the path its keys hang under.
    let mut table: Vec<String> = Vec::new();
    loop {
        reader.trivia();
        if reader.rest.is_empty() {
            return Some(root);
        }
        if let Some(rest) = reader.rest.strip_prefix("[[") {
            reader.rest = rest;
            let path = reader.key_path("]]")?;
            append_table(&mut root, &path)?;
            table = path;
        } else if let Some(rest) = reader.rest.strip_prefix('[') {
            reader.rest = rest;
            table = reader.key_path("]")?;
            // A header declares its table even when no key follows it.
            entry_at(&mut root, &table)?;
        } else {
            let path = reader.key_path("=")?;
            let value = reader.value(&[])?;
            let mut full = table.clone();
            full.extend(path);
            insert(&mut root, &full, value)?;
        }
    }
}

/// The cursor over the remaining text.
struct Reader<'t> {
    rest: &'t str,
}

impl<'t> Reader<'t> {
    /// Skip whitespace, newlines and `#` comments.
    fn trivia(&mut self) {
        loop {
            let trimmed = self.rest.trim_start();
            if let Some(comment) = trimmed.strip_prefix('#') {
                self.rest = comment.split_once('\n').map_or("", |(_, rest)| rest);
                continue;
            }
            self.rest = trimmed;
            return;
        }
    }

    /// Skip spaces and tabs only, so a bare value stops at its own newline.
    fn spaces(&mut self) {
        self.rest = self.rest.trim_start_matches([' ', '\t']);
    }

    /// A dotted key path up to and including `terminator`.
    fn key_path(&mut self, terminator: &str) -> Option<Vec<String>> {
        let mut path = Vec::new();
        loop {
            self.spaces();
            path.push(self.key()?);
            self.spaces();
            if let Some(rest) = self.rest.strip_prefix(terminator) {
                self.rest = rest;
                return (!path.is_empty()).then_some(path);
            }
            self.rest = self.rest.strip_prefix('.')?;
        }
    }

    /// One key segment: bare, or quoted either way.
    fn key(&mut self) -> Option<String> {
        if self.rest.starts_with(['"', '\'']) {
            return self.quoted();
        }
        let end = self
            .rest
            .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '-'))
            .unwrap_or(self.rest.len());
        let (key, rest) = self.rest.split_at(end);
        self.rest = rest;
        (!key.is_empty()).then(|| key.to_owned())
    }

    /// One value. `stop` is the set of characters that end a bare value
    /// inside a collection; at the top level a bare value ends at its line.
    fn value(&mut self, stop: &[char]) -> Option<pb::MetaValue> {
        self.spaces();
        if self.rest.starts_with("\"\"\"") || self.rest.starts_with("'''") {
            return Some(scalar(self.multiline()?));
        }
        if self.rest.starts_with(['"', '\'']) {
            return Some(scalar(self.quoted()?));
        }
        if let Some(rest) = self.rest.strip_prefix('[') {
            self.rest = rest;
            return self.array();
        }
        if let Some(rest) = self.rest.strip_prefix('{') {
            self.rest = rest;
            return self.inline_table();
        }
        self.bare(stop)
    }

    /// A one-line basic or literal string, escapes decoded in the first.
    fn quoted(&mut self) -> Option<String> {
        let literal = self.rest.starts_with('\'');
        let quote = if literal { '\'' } else { '"' };
        let mut chars = self.rest[1..].char_indices();
        let mut out = String::new();
        while let Some((at, c)) = chars.next() {
            match c {
                _ if c == quote => {
                    self.rest = &self.rest[1 + at + c.len_utf8()..];
                    return Some(out);
                }
                '\\' if !literal => {
                    let (_, escaped) = chars.next()?;
                    out.push(unescape(escaped));
                }
                '\n' => return None,
                _ => out.push(c),
            }
        }
        None
    }

    /// A `"""` or `'''` string, verbatim but for the newline the format
    /// swallows right after the opening fence.
    fn multiline(&mut self) -> Option<String> {
        let fence = &self.rest[..3];
        let body = &self.rest[3..];
        let end = body.find(fence)?;
        let text = body[..end].strip_prefix('\n').unwrap_or(&body[..end]);
        self.rest = &body[end + 3..];
        Some(text.to_owned())
    }

    /// A `[a, b]` array, which may span lines and hold trailing commas.
    fn array(&mut self) -> Option<pb::MetaValue> {
        let mut items = Vec::new();
        loop {
            self.trivia();
            if let Some(rest) = self.rest.strip_prefix(']') {
                self.rest = rest;
                return Some(pb::MetaValue {
                    kind: Some(pb::meta_value::Kind::List(pb::MetaList { items })),
                });
            }
            items.push(self.value(&[',', ']'])?);
            self.trivia();
            if let Some(rest) = self.rest.strip_prefix(',') {
                self.rest = rest;
            }
        }
    }

    /// A `{ key = value, ... }` inline table.
    fn inline_table(&mut self) -> Option<pb::MetaValue> {
        let mut entries = HashMap::new();
        loop {
            self.trivia();
            if let Some(rest) = self.rest.strip_prefix('}') {
                self.rest = rest;
                return Some(pb::MetaValue {
                    kind: Some(pb::meta_value::Kind::Map(pb::MetaMap { entries })),
                });
            }
            let path = self.key_path("=")?;
            let value = self.value(&[',', '}'])?;
            insert(&mut entries, &path, value)?;
            self.trivia();
            if let Some(rest) = self.rest.strip_prefix(',') {
                self.rest = rest;
            }
        }
    }

    /// A number, boolean or date, kept exactly as the author wrote it.
    fn bare(&mut self, stop: &[char]) -> Option<pb::MetaValue> {
        let end = self
            .rest
            .find(|c: char| c == '\n' || c == '#' || stop.contains(&c))
            .unwrap_or(self.rest.len());
        let (text, rest) = self.rest.split_at(end);
        self.rest = rest;
        let text = text.trim();
        (!text.is_empty()).then(|| scalar(text.to_owned()))
    }
}

/// The escapes the basic-string grammar defines. An escape outside the set
/// keeps the character it introduced, which is what a reader can honestly
/// do without a Unicode escape table.
const fn unescape(c: char) -> char {
    match c {
        'n' => '\n',
        't' => '\t',
        'r' => '\r',
        'b' => '\u{8}',
        'f' => '\u{c}',
        other => other,
    }
}

/// One verbatim scalar.
fn scalar(text: String) -> pb::MetaValue {
    pb::MetaValue {
        kind: Some(pb::meta_value::Kind::Value(text)),
    }
}

/// The map a dotted path names, creating the tables on the way.
fn entry_at<'m>(
    root: &'m mut HashMap<String, pb::MetaValue>,
    path: &[String],
) -> Option<&'m mut HashMap<String, pb::MetaValue>> {
    let mut current = root;
    for segment in path {
        let entry = current
            .entry(segment.clone())
            .or_insert_with(|| pb::MetaValue {
                kind: Some(pb::meta_value::Kind::Map(pb::MetaMap {
                    entries: HashMap::new(),
                })),
            });
        current = match entry.kind.as_mut()? {
            pb::meta_value::Kind::Map(map) => &mut map.entries,
            // An array-of-tables header addresses its newest element.
            pb::meta_value::Kind::List(list) => match list.items.last_mut()?.kind.as_mut()? {
                pb::meta_value::Kind::Map(map) => &mut map.entries,
                pb::meta_value::Kind::Value(_) | pb::meta_value::Kind::List(_) => return None,
            },
            // A scalar where a table belongs is a document that contradicts
            // itself, and guessing which statement to keep would be worse
            // than refusing the whole preamble.
            pb::meta_value::Kind::Value(_) => return None,
        };
    }
    Some(current)
}

/// Put one value at a dotted path.
fn insert(
    root: &mut HashMap<String, pb::MetaValue>,
    path: &[String],
    value: pb::MetaValue,
) -> Option<()> {
    let (last, parents) = path.split_last()?;
    entry_at(root, parents)?.insert(last.clone(), value);
    Some(())
}

/// Start a new element of an array of tables.
fn append_table(root: &mut HashMap<String, pb::MetaValue>, path: &[String]) -> Option<()> {
    let (last, parents) = path.split_last()?;
    let parent = entry_at(root, parents)?;
    let entry = parent.entry(last.clone()).or_insert_with(|| pb::MetaValue {
        kind: Some(pb::meta_value::Kind::List(pb::MetaList {
            items: Vec::new(),
        })),
    });
    match entry.kind.as_mut()? {
        pb::meta_value::Kind::List(list) => {
            list.items.push(pb::MetaValue {
                kind: Some(pb::meta_value::Kind::Map(pb::MetaMap {
                    entries: HashMap::new(),
                })),
            });
            Some(())
        }
        pb::meta_value::Kind::Map(_) | pb::meta_value::Kind::Value(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(value: &pb::MetaValue) -> &str {
        match value.kind.as_ref() {
            Some(pb::meta_value::Kind::Value(text)) => text,
            other => panic!("expected a scalar, got {other:?}"),
        }
    }

    fn list(value: &pb::MetaValue) -> &[pb::MetaValue] {
        match value.kind.as_ref() {
            Some(pb::meta_value::Kind::List(list)) => &list.items,
            other => panic!("expected a list, got {other:?}"),
        }
    }

    fn map(value: &pb::MetaValue) -> &HashMap<String, pb::MetaValue> {
        match value.kind.as_ref() {
            Some(pb::meta_value::Kind::Map(map)) => &map.entries,
            other => panic!("expected a map, got {other:?}"),
        }
    }

    #[test]
    fn scalars_stay_the_text_the_author_wrote() {
        let parsed = parse(
            "title = \"Café littéraire\"\nversion = 1.10\ndraft = false\ndate = 2026-08-23\n",
        )
        .expect("a TOML document");
        assert_eq!(text(&parsed["title"]), "Café littéraire");
        assert_eq!(
            text(&parsed["version"]),
            "1.10",
            "a version is not a number to be normalized"
        );
        assert_eq!(text(&parsed["draft"]), "false");
        assert_eq!(text(&parsed["date"]), "2026-08-23");
    }

    #[test]
    fn strings_decode_the_escapes_their_own_spelling_admits() {
        let parsed = parse("basic = \"a\\tb\\\"c\"\nliteral = 'a\\tb'\n").expect("a TOML document");
        assert_eq!(text(&parsed["basic"]), "a\tb\"c");
        assert_eq!(
            text(&parsed["literal"]),
            "a\\tb",
            "a literal string has no escapes"
        );
    }

    #[test]
    fn a_multiline_string_swallows_only_the_newline_after_its_fence() {
        let parsed = parse("body = \"\"\"\nligne un\nligne deux\"\"\"\n").expect("a TOML document");
        assert_eq!(text(&parsed["body"]), "ligne un\nligne deux");
    }

    #[test]
    fn an_array_stays_a_list_across_lines_and_trailing_commas() {
        let parsed = parse("tags = [\n  \"café\",\n  \"thé\",\n]\n").expect("a TOML document");
        let items = list(&parsed["tags"]);
        assert_eq!(items.len(), 2);
        assert_eq!(text(&items[0]), "café");
        assert_eq!(text(&items[1]), "thé");
    }

    #[test]
    fn tables_dotted_keys_and_inline_tables_all_nest() {
        // A header is in force until the next one, so the root keys come
        // first; that is the format's own rule and this reader follows it.
        let parsed = parse(
            "site.url = \"https://example\"\ncover = { src = \"c.png\", alt = \"couverture\" }\n\
             \n[author]\nname = \"Ada\"\n\n[author.contact]\nmail = \"ada@example\"\n",
        )
        .expect("a TOML document");
        let author = map(&parsed["author"]);
        assert_eq!(text(&author["name"]), "Ada");
        assert_eq!(text(&map(&author["contact"])["mail"]), "ada@example");
        assert_eq!(text(&map(&parsed["site"])["url"]), "https://example");
        assert_eq!(text(&map(&parsed["cover"])["alt"]), "couverture");
    }

    #[test]
    fn an_array_of_tables_is_a_list_of_maps() {
        let parsed = parse("[[link]]\nname = \"un\"\n\n[[link]]\nname = \"deux\"\n")
            .expect("a TOML document");
        let links = list(&parsed["link"]);
        assert_eq!(links.len(), 2);
        assert_eq!(text(&map(&links[0])["name"]), "un");
        assert_eq!(text(&map(&links[1])["name"]), "deux");
    }

    #[test]
    fn comments_and_blank_lines_are_not_content() {
        let parsed = parse("# une note\n\ntitle = \"T\" # à droite\n").expect("a TOML document");
        assert_eq!(parsed.len(), 1);
        assert_eq!(text(&parsed["title"]), "T");
    }

    #[test]
    fn text_this_reader_cannot_complete_is_refused_rather_than_guessed() {
        for bad in [
            "title = \"unterminated\n",
            "tags = [\"a\", \"b\"\n",
            "= 1\n",
            "title = \"T\"\ntitle.sub = 1\n",
        ] {
            assert!(parse(bad).is_none(), "{bad:?}");
        }
    }
}
