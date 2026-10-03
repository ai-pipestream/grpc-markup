// SPDX-License-Identifier: Apache-2.0

//! The LaTeX fold: a hand-rolled, tolerant LaTeX reader (pylatexenc has no
//! Rust counterpart, so the node model is built here) mapping the
//! constructs below onto the shared block vocabulary.
//!
//! Shape of the mapping:
//!
//! - The preamble is skipped: the body is `\begin{document}` ..
//!   `\end{document}`, or the whole input when the environment is absent.
//!   `\title` / `\author` / `\date` found in the preamble still surface —
//!   the title as `MarkupInfo.title` and a `BLOCK_LABEL_TITLE` block,
//!   author and date as paragraphs with their macro name as `role`.
//! - `\section` / `\subsection` / `\subsubsection` (and `\part`,
//!   `\chapter`, `\paragraph`, `\subparagraph`) ladder onto
//!   `BLOCK_LABEL_SECTION_HEADER` with a fixed level table
//!   (part/chapter/section 1 through subparagraph 5).
//! - Text runs split into paragraphs at blank lines; inline commands
//!   flatten to their text and leave the run they covered: `\textbf`,
//!   `\emph` and their siblings as styles, `\cite`/`\ref`/`\eqref` as
//!   typed references to their key, `\href{u}{t}` and `\url{u}` as links,
//!   `$..$` as a math run keeping its TeX, `~` as a space, escaped
//!   characters as themselves; `%` comments are stripped with `\%`
//!   respected.
//! - `itemize` / `enumerate` / `description` nest; every flushed run
//!   inside an item is a `BLOCK_LABEL_LIST_ITEM`, `enumerated` for
//!   `enumerate`, a `description` term prefixed `term: `.
//! - `tabular` becomes table events: the preamble (`{|l|c|p{5cm}|}`) is
//!   the table's column geometry, `&` splits cells, `\\` ends rows, rule
//!   macros (`\hline`, `\toprule`, ...) are dropped, `\multicolumn`
//!   carries `column_span` and `\multirow` `row_span`.
//! - `verbatim` / `lstlisting` / `minted` keep their body verbatim as
//!   `BLOCK_LABEL_CODE` (a `[language=x]` option declares the language);
//!   `tikzpicture` keeps its source as code declared `tikz`.
//! - `equation` and its display-math family, `$$..$$` and `\[..\]` are
//!   `BLOCK_LABEL_FORMULA`, verbatim with `\label{...}` lifted out into the
//!   block's `anchor_id`, which is what an `\eqref` resolves to; `$..$`
//!   stays inline in the enclosing text as a math run.
//! - `\includegraphics` is an `ImageBlock` with the path verbatim; a
//!   `\footnote` leaves a marker run where it interrupted the paragraph,
//!   pointing at the anchor its body carries.
//! - A `\label` anchors the block it follows (a heading, a caption) or the
//!   paragraph it sits inside. Inside a `figure` or `table` float it
//!   anchors the float itself: the picture or the table, never the caption
//!   that sits beside it, because that is what a `\ref` to a figure means.
//! - `quote` / `quotation` / `verse` paragraphs are `BLOCK_LABEL_QUOTE`;
//!   `abstract` opens a level-1 `Abstract` header with `role: "abstract"`;
//!   theorem environments and `proof` lead with a labelled paragraph;
//!   `thebibliography` becomes a `References` header plus one list item
//!   per `\bibitem`, the key its `anchor_id` rather than a prefix on its
//!   text.
//! - An unknown environment is descended into and flattened, with one
//!   aggregated `WARNING_CODE_UNMAPPED_CONSTRUCT` per environment name —
//!   tolerance, made visible.
//!
//! Zero-argument `\newcommand` macros are expanded, one level;
//! parameterized custom macros are not (see `docs/design.md`).

use std::cell::Cell;
use std::collections::HashMap;

use crate::parse::{Block, Cancel, Emitter, Image, ParseError, Table};
use crate::proto::v1 as pb;
use crate::spans::{Inline, Mark};

/// Heading macros and their docling levels.
const HEADINGS: &[(&str, u32)] = &[
    ("part", 1),
    ("chapter", 1),
    ("section", 1),
    ("subsection", 2),
    ("subsubsection", 3),
    ("paragraph", 4),
    ("subparagraph", 5),
];

/// Macros whose single argument flattens to its own text, each with the
/// run that argument carries. The macro name is an exact styling signal;
/// the ones with no run (`\textsf`, `\mbox`) say nothing the wire models.
const TEXT_WRAPPERS: &[(&str, Option<pb::SpanStyle>)] = &[
    ("textbf", Some(pb::SpanStyle::Bold)),
    ("textit", Some(pb::SpanStyle::Italic)),
    ("emph", Some(pb::SpanStyle::Italic)),
    ("texttt", Some(pb::SpanStyle::Code)),
    ("underline", Some(pb::SpanStyle::Underline)),
    ("textsc", Some(pb::SpanStyle::SmallCaps)),
    ("textsf", None),
    ("textrm", None),
    ("textnormal", None),
    ("mbox", None),
];

/// True when a macro wraps text it does not otherwise change.
fn is_text_wrapper(name: &str) -> bool {
    TEXT_WRAPPERS.iter().any(|(wrapper, _)| *wrapper == name)
}

/// The run one text wrapper puts over its argument.
fn wrapper_mark(name: &str) -> Option<Mark> {
    TEXT_WRAPPERS
        .iter()
        .find(|(wrapper, _)| *wrapper == name)
        .and_then(|(_, style)| *style)
        .map(Mark::style)
}

/// Citation and reference macros, each with what its key points at. The
/// keys used to become the string `[key]`, which nothing could read back.
const CITATIONS: &[(&str, pb::ReferenceKind)] = &[
    ("cite", pb::ReferenceKind::Citation),
    ("citep", pb::ReferenceKind::Citation),
    ("citet", pb::ReferenceKind::Citation),
    ("ref", pb::ReferenceKind::CrossRef),
    ("eqref", pb::ReferenceKind::EquationRef),
];

/// What a citation or cross-reference macro's key points at.
fn citation_kind(name: &str) -> Option<pb::ReferenceKind> {
    CITATIONS
        .iter()
        .find(|(macro_name, _)| *macro_name == name)
        .map(|(_, kind)| *kind)
}

/// Escaped characters that are their own macro name.
const ESCAPED: &[&str] = &["&", "%", "$", "#", "_", "{", "}"];

/// Environments whose whole body is one display formula.
const MATH_ENVS: &[&str] = &[
    "equation",
    "align",
    "gather",
    "multline",
    "flalign",
    "alignat",
    "displaymath",
    "eqnarray",
    "dmath",
    "dgroup",
    "darray",
    "math",
];

/// Theorem-like environments led by a labelled paragraph.
const THEOREM_ENVS: &[&str] = &[
    "theorem",
    "lemma",
    "corollary",
    "proposition",
    "definition",
    "remark",
    "example",
    "conjecture",
];

/// Environments whose body is kept verbatim as code.
const VERBATIM_ENVS: &[&str] = &["verbatim", "lstlisting", "minted"];

/// Table rule macros dropped inside `tabular`.
const TABLE_RULES: &[&str] = &[
    "hline",
    "cline",
    "toprule",
    "midrule",
    "bottomrule",
    "cmidrule",
    "specialrule",
];

/// Macros that are consumed together with every bracket or brace argument
/// glued to them, and produce nothing: preamble machinery, spacing,
/// counters, page furniture. `\vspace{-1mm}` must not emit `-1mm`.
const GREEDY_IGNORED: &[&str] = &[
    "documentclass",
    "usepackage",
    "geometry",
    "hypersetup",
    "lstset",
    "bibliography",
    "bibliographystyle",
    "newcommand",
    "renewcommand",
    "providecommand",
    "newenvironment",
    "renewenvironment",
    "newtheorem",
    "theoremstyle",
    "setlength",
    "addtolength",
    "setcounter",
    "newcounter",
    "newlength",
    "vspace",
    "hspace",
    "vskip",
    "hskip",
    "definecolor",
    "colorlet",
    "color",
    "pagestyle",
    "thispagestyle",
    "pagenumbering",
    "markboth",
    "markright",
    "lhead",
    "rhead",
    "cfoot",
    "hyphenation",
    "rule",
    "phantom",
    "hphantom",
    "vphantom",
];

/// Bare macros silently discarded: spacing, layout, legacy formatting.
const BARE_IGNORED: &[&str] = &[
    "noindent",
    "par",
    "smallskip",
    "medskip",
    "bigskip",
    "vfill",
    "hfill",
    "centering",
    "raggedright",
    "raggedleft",
    "maketitle",
    "thanks",
    "tableofcontents",
    "listoffigures",
    "listoftables",
    "appendix",
    "clearpage",
    "cleardoublepage",
    "newpage",
    "protect",
    "makeatletter",
    "makeatother",
    "qed",
    "qedsymbol",
    "item",
    "newline",
    "break",
    "bf",
    "it",
    "rm",
    "sc",
    "sf",
    "sl",
    "tt",
    "cal",
    "em",
    "tiny",
    "scriptsize",
    "footnotesize",
    "small",
    "large",
    "Large",
    "LARGE",
    "huge",
    "Huge",
];

/// Map one LaTeX document.
///
/// # Errors
///
/// [`ParseError::Invalid`] for empty input — docling's own validity rule —
/// and [`ParseError::ConsumerGone`] when the client stopped reading.
/// Everything else is read tolerantly, as docling reads it.
pub fn fold(text: &str, emitter: &mut Emitter<'_>) -> Result<(), ParseError> {
    if text.trim().is_empty() {
        return Err(ParseError::Invalid(
            "empty LaTeX input; a LaTeX document has at least some text".to_owned(),
        ));
    }
    let mut parser = Parser::with_cancel(text, emitter.cancel().clone());
    let nodes = parser.nodes(None);
    if let Some(error) = parser.cancelled.take() {
        return Err(error);
    }
    emitter.warn_n(
        pb::WarningCode::UnmappedConstruct,
        &format!("groups nested deeper than {MAX_NESTING} levels read as plain text"),
        parser.flattened,
    );

    // Zero-argument custom macros, expanded the way docling expands them.
    let mut definitions = HashMap::new();
    collect_newcommands(text, &mut definitions);
    let macros = Macros::new(definitions, text.len());

    // Preamble metadata surfaces even though the preamble is skipped.
    let mut title = None;
    let mut author = None;
    let mut date = None;
    scan_metadata(&nodes, &mut title, &mut author, &mut date);

    emitter.info(title.clone(), HashMap::new())?;

    let mut walk = Walk {
        emitter,
        macros,
        title_taken: false,
        pending_anchor: None,
        float_anchor: None,
        footnotes: 0,
    };
    if let Some(text) = title {
        walk.title_taken = true;
        walk.emitter.text_block(Block {
            label: pb::BlockLabel::Title,
            text,
            ..Block::default()
        })?;
    }
    for (role, value) in [("author", author), ("date", date)] {
        if let Some(text) = value {
            walk.emitter.text_block(Block {
                label: pb::BlockLabel::Paragraph,
                role: role.to_owned(),
                text,
                ..Block::default()
            })?;
        }
    }

    let ctx = Ctx::default();
    match find_document(&nodes) {
        Some(body) => walk.nodes(body, &ctx)?,
        None => walk.nodes(&nodes, &ctx)?,
    }
    let starved = walk.macros.starved.get();
    walk.emitter.warn_n(
        pb::WarningCode::UnmappedConstruct,
        "\\newcommand expansion budget exhausted; later uses were left unexpanded",
        starved,
    );
    Ok(())
}

// ------------------------------------------------------------------- lexer

/// One parsed LaTeX node.
#[derive(Debug, Clone)]
enum Node {
    /// A character run, comments stripped.
    Text(String),
    /// A `{...}` group.
    Group(Vec<Node>),
    /// A control sequence with the arguments its signature takes.
    Macro {
        name: String,
        opt: Option<String>,
        args: Vec<Vec<Node>>,
    },
    /// A `\begin{name}...\end{name}` environment. `raw` carries the body
    /// verbatim for the environments that need it (math, verbatim, tikz);
    /// `body` carries the parsed nodes for the rest.
    Environment {
        name: String,
        opt: Option<String>,
        raw: String,
        body: Vec<Node>,
    },
    /// `$..$`, `$$..$$`, `\[..\]` or `\(..\)`, body verbatim.
    Math { display: bool, text: String },
}

/// How many arguments a macro consumes at parse time.
enum Sig {
    /// No arguments; adjacent groups are ordinary content.
    None,
    /// An optional `[..]` then `n` brace groups.
    Fixed(usize),
    /// Only an optional `[..]` (the `\item` shape).
    OptOnly,
    /// Every adjacent `[..]` and `{..}`, discarded with the macro.
    Greedy,
}

/// The signature of a macro name.
fn signature(name: &str) -> Sig {
    if GREEDY_IGNORED.contains(&name) {
        return Sig::Greedy;
    }
    if name == "item" {
        return Sig::OptOnly;
    }
    if name == "href" || name == "textcolor" || name == "colorbox" {
        return Sig::Fixed(2);
    }
    if name == "multicolumn" || name == "multirow" {
        return Sig::Fixed(3);
    }
    let fixed_one = HEADINGS.iter().any(|(heading, _)| *heading == name)
        || is_text_wrapper(name)
        || citation_kind(name).is_some()
        || matches!(
            name,
            "title"
                | "author"
                | "date"
                | "url"
                | "caption"
                | "footnote"
                | "marginpar"
                | "label"
                | "includegraphics"
                | "input"
                | "include"
                | "bibitem"
        );
    if fixed_one { Sig::Fixed(1) } else { Sig::None }
}

/// How deep `{..}` groups and environments may nest before the reader
/// stops building structure and keeps the rest of a group as text.
///
/// The reader, the flatteners and the drop of the node tree all recurse
/// once per level, so a few kilobytes of `{` would overflow the blocking
/// pool's 2 MiB stack and abort the process. Real documents nest a handful
/// of levels.
pub const MAX_NESTING: usize = 128;

/// The longest environment name [`Parser::peek_end_name`] looks for.
const MAX_ENV_NAME: usize = 128;

/// How many reader steps pass between cancellation checks.
const CANCEL_STRIDE: u32 = 4096;

/// A recursive-descent reader over the source bytes.
struct Parser<'t> {
    text: &'t str,
    at: usize,
    /// Groups and environments currently open.
    depth: usize,
    /// Groups and environments read as flat text because they sat deeper
    /// than [`MAX_NESTING`].
    flattened: u64,
    /// The first offset after which no `}` occurs, once a search has
    /// proved it, so a run of unclosed openers costs one scan, not one
    /// scan each.
    no_brace_from: usize,
    /// The same for `]`.
    no_bracket_from: usize,
    /// When to stop reading.
    cancel: Cancel,
    /// Steps since the last cancellation check.
    ticks: u32,
    /// Set when `cancel` fired; the reader then runs to EOF at once.
    cancelled: Option<ParseError>,
}

impl<'t> Parser<'t> {
    fn new(text: &'t str) -> Self {
        Self::with_cancel(text, Cancel::default())
    }

    fn with_cancel(text: &'t str, cancel: Cancel) -> Self {
        Self {
            text,
            at: 0,
            depth: 0,
            flattened: 0,
            no_brace_from: usize::MAX,
            no_bracket_from: usize::MAX,
            cancel,
            ticks: 0,
            cancelled: None,
        }
    }

    /// Check for cancellation every [`CANCEL_STRIDE`] steps; once it fires
    /// the cursor jumps to EOF so every open level unwinds at once.
    fn tick(&mut self) {
        self.ticks += 1;
        if self.ticks < CANCEL_STRIDE {
            return;
        }
        self.ticks = 0;
        if let Err(error) = self.cancel.check() {
            self.cancelled = Some(error);
            self.at = self.text.len();
        }
    }

    /// Parse nodes until end of input, a closing `}` (when `until_brace`),
    /// or a `\end{terminator}`.
    fn nodes(&mut self, terminator: Option<&str>) -> Vec<Node> {
        let mut nodes = Vec::new();
        let mut run = String::new();
        while self.at < self.text.len() {
            self.tick();
            if self.at >= self.text.len() {
                break;
            }
            let rest = &self.text[self.at..];
            let c = rest.chars().next().expect("at is on a char boundary");
            match c {
                '%' => {
                    // A comment runs to the end of the line; the newline
                    // itself stays so paragraph breaks survive.
                    let line_end = rest.find('\n').map_or(self.text.len(), |n| self.at + n);
                    self.at = line_end;
                }
                '{' => {
                    flush_run(&mut run, &mut nodes);
                    self.at += 1;
                    nodes.push(Node::Group(self.group()));
                }
                '}' => {
                    // The caller that opened a group consumes this; at the
                    // top level it is a stray brace, read tolerantly.
                    if terminator.is_some() {
                        break;
                    }
                    self.at += 1;
                }
                '$' => {
                    flush_run(&mut run, &mut nodes);
                    nodes.push(self.math());
                }
                '\\' => {
                    if let Some(name) = self.peek_end_name()
                        && Some(name.as_str()) == terminator
                    {
                        // The terminator is consumed by the caller.
                        break;
                    }
                    flush_run(&mut run, &mut nodes);
                    if let Some(node) = self.control() {
                        nodes.push(node);
                    }
                }
                '~' => {
                    // A bare tilde is TeX's non-breaking space, not a
                    // macro; `\~` (the accent) comes through `control`.
                    run.push(' ');
                    self.at += 1;
                }
                _ => {
                    run.push(c);
                    self.at += c.len_utf8();
                }
            }
        }
        flush_run(&mut run, &mut nodes);
        nodes
    }

    /// A `{...}` group whose opening brace is already consumed.
    fn group(&mut self) -> Vec<Node> {
        if self.depth >= MAX_NESTING {
            return self.flat_group();
        }
        self.depth += 1;
        let inner = self.nodes(Some(""));
        self.depth -= 1;
        // Consume the closing brace when it is there; a truncated group
        // just ends at EOF.
        if self.text[self.at..].starts_with('}') {
            self.at += 1;
        }
        inner
    }

    /// A group past [`MAX_NESTING`], read without recursing: its source
    /// text to the matching `}` (or EOF) as one text node, braces dropped.
    fn flat_group(&mut self) -> Vec<Node> {
        self.flattened += 1;
        let mut depth = 1usize;
        let mut text = String::new();
        let mut chars = self.text[self.at..].char_indices();
        let mut end = self.text.len();
        while let Some((offset, c)) = chars.next() {
            match c {
                '\\' => {
                    // An escaped brace is text, not structure.
                    text.push(c);
                    if let Some((_, escaped)) = chars.next() {
                        text.push(escaped);
                    }
                }
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        end = self.at + offset + 1;
                        break;
                    }
                }
                _ => text.push(c),
            }
        }
        self.at = end;
        if text.is_empty() {
            Vec::new()
        } else {
            vec![Node::Text(text)]
        }
    }

    /// `$..$` or `$$..$$`, verbatim.
    fn math(&mut self) -> Node {
        let display = self.text[self.at..].starts_with("$$");
        let delim = if display { "$$" } else { "$" };
        self.at += delim.len();
        let start = self.at;
        let end = self.text[self.at..]
            .find(delim)
            .map_or(self.text.len(), |n| self.at + n);
        let body = self.text[start..end].to_owned();
        self.at = (end + delim.len()).min(self.text.len());
        Node::Math {
            display,
            text: body,
        }
    }

    /// The name of a `\end{...}` at the cursor, without consuming it.
    ///
    /// The search for the closing brace is bounded: this runs at every
    /// backslash, and an unbounded one made a run of unclosed `\end{`
    /// quadratic.
    fn peek_end_name(&self) -> Option<String> {
        let rest = self.text[self.at..].strip_prefix("\\end")?;
        let rest = rest.trim_start();
        let inner = rest.strip_prefix('{')?;
        let window = inner
            .char_indices()
            .nth(MAX_ENV_NAME)
            .map_or(inner.len(), |(offset, _)| offset);
        let close = inner[..window].find('}')?;
        Some(inner[..close].trim().to_owned())
    }

    /// A control sequence at the cursor: an environment, math delimiters,
    /// an escaped character, or a macro with its signature's arguments.
    fn control(&mut self) -> Option<Node> {
        self.at += 1; // the backslash
        let rest = &self.text[self.at..];
        let mut chars = rest.chars();
        let first = chars.next()?;

        if !first.is_ascii_alphabetic() {
            self.at += first.len_utf8();
            return match first {
                '[' => Some(self.bracket_math("\\]", true)),
                '(' => Some(self.bracket_math("\\)", false)),
                _ => Some(Node::Macro {
                    name: first.to_string(),
                    opt: None,
                    args: Vec::new(),
                }),
            };
        }

        let letters = rest.chars().take_while(|c| c.is_ascii_alphabetic()).count();
        let mut name_end = self.at + letters;
        if self.text[name_end..].starts_with('*') {
            name_end += 1;
        }
        let name = self.text[self.at..name_end].to_owned();
        self.at = name_end;

        if name == "begin" {
            return self.environment();
        }
        if name == "end" {
            // A stray \end{...} with no matching begin: consume and drop.
            self.skip_ws();
            if self.text[self.at..].starts_with('{') {
                self.at += 1;
                let _ = self.take_until('}');
            }
            return None;
        }

        let base = name.trim_end_matches('*');
        let (opt, args) = match signature(base) {
            Sig::None => (None, Vec::new()),
            Sig::OptOnly => (self.take_opt(), Vec::new()),
            Sig::Fixed(count) => {
                let opt = self.take_opt();
                let mut args = Vec::new();
                for _ in 0..count {
                    let saved = self.at;
                    self.skip_ws();
                    if self.text[self.at..].starts_with('{') {
                        self.at += 1;
                        args.push(self.group());
                    } else {
                        self.at = saved;
                        break;
                    }
                }
                (opt, args)
            }
            Sig::Greedy => {
                loop {
                    let saved = self.at;
                    self.skip_ws();
                    if self.text[self.at..].starts_with('[') {
                        self.at += 1;
                        let _ = self.take_until(']');
                    } else if self.text[self.at..].starts_with('{') {
                        self.at += 1;
                        let _ = self.group();
                    } else {
                        self.at = saved;
                        break;
                    }
                }
                (None, Vec::new())
            }
        };
        Some(Node::Macro {
            name: base.to_owned(),
            opt,
            args,
        })
    }

    /// `\[..\]` or `\(..\)`, opener already consumed.
    fn bracket_math(&mut self, closer: &str, display: bool) -> Node {
        let start = self.at;
        let end = self.text[self.at..]
            .find(closer)
            .map_or(self.text.len(), |n| self.at + n);
        let body = self.text[start..end].to_owned();
        self.at = (end + closer.len()).min(self.text.len());
        Node::Math {
            display,
            text: body,
        }
    }

    /// `\begin{name}[opt]...\end{name}`, `\begin` already consumed.
    fn environment(&mut self) -> Option<Node> {
        self.skip_ws();
        if !self.text[self.at..].starts_with('{') {
            return None;
        }
        self.at += 1;
        let name = self.take_until('}')?;
        let name = name.trim().to_owned();
        let opt = self.take_opt();

        let base = name.trim_end_matches('*');
        if self.depth >= MAX_NESTING {
            // Too deep to give structure to: the body is read into the
            // enclosing level and its `\end` is dropped as a stray one.
            self.flattened += 1;
            return None;
        }
        if VERBATIM_ENVS.contains(&base) || MATH_ENVS.contains(&base) || base == "tikzpicture" {
            // Raw capture to the matching \end{name}; these bodies are
            // text, not TeX to interpret.
            let marker = format!("\\end{{{name}}}");
            let start = self.at;
            let end = self.text[self.at..]
                .find(&marker)
                .map_or(self.text.len(), |n| self.at + n);
            let raw = self.text[start..end].to_owned();
            self.at = (end + marker.len()).min(self.text.len());
            return Some(Node::Environment {
                name,
                opt,
                raw,
                body: Vec::new(),
            });
        }

        self.depth += 1;
        let body = self.nodes(Some(&name));
        self.depth -= 1;
        // Consume the terminator the recursive call stopped at.
        if self.text[self.at..].starts_with("\\end") {
            self.at += 4;
            self.skip_ws();
            if self.text[self.at..].starts_with('{') {
                self.at += 1;
                let _ = self.take_until('}');
            }
        }
        Some(Node::Environment {
            name,
            opt,
            raw: String::new(),
            body,
        })
    }

    /// An optional `[...]` argument at the cursor.
    fn take_opt(&mut self) -> Option<String> {
        let saved = self.at;
        self.skip_ws();
        if self.text[self.at..].starts_with('[') {
            self.at += 1;
            if let Some(inner) = self.take_until(']') {
                return Some(inner);
            }
        }
        self.at = saved;
        None
    }

    /// Text up to (and consuming) `close`; `None` at EOF.
    ///
    /// A miss is remembered: no `close` after this offset means none after
    /// any later one, so a run of unclosed openers is scanned once.
    fn take_until(&mut self, close: char) -> Option<String> {
        let absent_from = match close {
            '}' => &mut self.no_brace_from,
            ']' => &mut self.no_bracket_from,
            _ => return self.scan_until(close),
        };
        if self.at >= *absent_from {
            return None;
        }
        let Some(end) = self.text[self.at..].find(close) else {
            *absent_from = self.at;
            return None;
        };
        let inner = self.text[self.at..self.at + end].to_owned();
        self.at += end + close.len_utf8();
        Some(inner)
    }

    /// [`Self::take_until`] for a closer with no remembered miss.
    fn scan_until(&mut self, close: char) -> Option<String> {
        let end = self.text[self.at..].find(close)?;
        let inner = self.text[self.at..self.at + end].to_owned();
        self.at += end + close.len_utf8();
        Some(inner)
    }

    fn skip_ws(&mut self) {
        while self.text[self.at..].starts_with([' ', '\t']) {
            self.at += 1;
        }
    }
}

/// Close a pending text run into the node list.
fn flush_run(run: &mut String, nodes: &mut Vec<Node>) {
    if !run.is_empty() {
        nodes.push(Node::Text(std::mem::take(run)));
    }
}

// -------------------------------------------------------------- pre-scans

/// The body of the `document` environment, wherever it nests.
fn find_document(nodes: &[Node]) -> Option<&Vec<Node>> {
    for node in nodes {
        match node {
            Node::Environment { name, body, .. } => {
                if name == "document" {
                    return Some(body);
                }
                if let Some(found) = find_document(body) {
                    return Some(found);
                }
            }
            Node::Group(inner) => {
                if let Some(found) = find_document(inner) {
                    return Some(found);
                }
            }
            _ => {}
        }
    }
    None
}

/// `\title` / `\author` / `\date` before the document body begins.
fn scan_metadata(
    nodes: &[Node],
    title: &mut Option<String>,
    author: &mut Option<String>,
    date: &mut Option<String>,
) {
    for node in nodes {
        match node {
            Node::Environment { name, .. } if name == "document" => return,
            Node::Macro { name, args, .. } => {
                let slot = match name.as_str() {
                    "title" => &mut *title,
                    "author" => &mut *author,
                    "date" => &mut *date,
                    _ => continue,
                };
                if slot.is_none()
                    && let Some(arg) = args.first()
                {
                    let text = flatten_nodes(arg, &Macros::default());
                    if !text.is_empty() {
                        *slot = Some(text);
                    }
                }
            }
            _ => {}
        }
    }
}

/// Zero-argument `\newcommand{\name}{definition}` bodies, by name.
///
/// Collected from the source text rather than the node tree, because the
/// parser already discarded newcommand arguments as preamble machinery.
/// Parameterized macros (`[1]` and up) are deliberately not expanded.
fn collect_newcommands(text: &str, macros: &mut HashMap<String, String>) {
    for keyword in ["\\newcommand", "\\renewcommand", "\\providecommand"] {
        let mut rest = text;
        while let Some(hit) = rest.find(keyword) {
            let after = &rest[hit + keyword.len()..];
            rest = after;
            let after = after.strip_prefix('*').unwrap_or(after);
            let after = after.trim_start();
            // {\name} or \name
            let (name, after) = if let Some(braced) = after.strip_prefix('{') {
                // A macro name is short; an unbounded search here made a
                // run of `\newcommand{` with no `}` quadratic.
                let window = braced
                    .char_indices()
                    .nth(MAX_ENV_NAME)
                    .map_or(braced.len(), |(offset, _)| offset);
                let Some(close) = braced[..window].find('}') else {
                    continue;
                };
                (braced[..close].trim(), &braced[close + 1..])
            } else if let Some(unescaped) = after.strip_prefix('\\') {
                let letters = unescaped
                    .chars()
                    .take_while(|c| c.is_ascii_alphabetic())
                    .count();
                (&after[..1 + letters], &after[1 + letters..])
            } else {
                continue;
            };
            let name = name.trim_start_matches('\\');
            if name.is_empty() {
                continue;
            }
            let after = after.trim_start();
            if after.starts_with('[') {
                // Parameterized: not expanded, matching the documented
                // limit.
                continue;
            }
            let Some(braced) = after.strip_prefix('{') else {
                continue;
            };
            // Balance braces for the definition body.
            let mut depth = 1usize;
            let mut end = None;
            for (index, c) in braced.char_indices() {
                match c {
                    '{' => depth += 1,
                    '}' => {
                        depth -= 1;
                        if depth == 0 {
                            end = Some(index);
                            break;
                        }
                    }
                    _ => {}
                }
            }
            let Some(end) = end else {
                // An unbalanced body runs to EOF, as it does in TeX, so
                // nothing after it can be a definition: stop rather than
                // rescan the rest once per later keyword.
                break;
            };
            macros.insert(name.to_owned(), braced[..end].to_owned());
            // Resume after the body: a definition nested inside another is
            // not one TeX would make until the outer one expands, and
            // rescanning every body made nested definitions quadratic.
            rest = &braced[end + 1..];
        }
    }
}

// ----------------------------------------------------------- text flatten

/// The smallest `\newcommand` expansion budget, in definition bytes.
const MIN_EXPANSION_BUDGET: usize = 1 << 20;

/// The expansion budget per input byte, past the minimum.
const EXPANSION_BUDGET_PER_BYTE: usize = 4;

/// Zero-argument `\newcommand` definitions, and the budget every expansion
/// in one parse draws from.
///
/// One level of expansion is linear in the definition, but a 100-byte
/// definition used a hundred thousand times is still ten megabytes of
/// text from a few hundred kilobytes; the budget caps the total at a
/// multiple of the input.
#[derive(Default)]
struct Macros {
    defs: HashMap<String, String>,
    /// Definition bytes still allowed to expand.
    budget: Cell<usize>,
    /// Uses left unexpanded because the budget ran out.
    starved: Cell<u64>,
}

impl Macros {
    fn new(defs: HashMap<String, String>, input_len: usize) -> Self {
        Self {
            defs,
            budget: Cell::new(
                input_len
                    .saturating_mul(EXPANSION_BUDGET_PER_BYTE)
                    .max(MIN_EXPANSION_BUDGET),
            ),
            starved: Cell::new(0),
        }
    }

    /// Draw `cost` bytes from the budget; false (and counted) when it is
    /// spent.
    fn spend(&self, cost: usize) -> bool {
        let left = self.budget.get();
        if cost > left {
            self.starved.set(self.starved.get() + 1);
            return false;
        }
        self.budget.set(left - cost);
        true
    }
}

/// Nodes flattened to inline text, with the runs they carry recorded.
fn flatten_nodes(nodes: &[Node], macros: &Macros) -> String {
    let mut out = Inline::default();
    flatten_into(nodes, macros, 0, &mut out);
    out.take().0
}

fn flatten_into(nodes: &[Node], macros: &Macros, depth: usize, out: &mut Inline) {
    for node in nodes {
        match node {
            Node::Text(text) => out.push(text),
            Node::Group(inner) => flatten_into(inner, macros, depth, out),
            Node::Math { text, .. } => {
                // The delimiters no longer ride in the text; the run says
                // it is math and `raw` carries the TeX.
                out.push_marked(text, Mark::style(pb::SpanStyle::Math).with_raw(text));
            }
            Node::Environment { body, .. } => flatten_into(body, macros, depth, out),
            Node::Macro { name, opt, args } => {
                flatten_macro(name, opt.as_deref(), args, macros, depth, out);
            }
        }
    }
}

/// One macro's inline text and the run it carries.
fn flatten_macro(
    name: &str,
    opt: Option<&str>,
    args: &[Vec<Node>],
    macros: &Macros,
    depth: usize,
    out: &mut Inline,
) {
    if ESCAPED.contains(&name) {
        out.push(name);
        return;
    }
    match name {
        "~" | " " | "\\" => out.space(),
        _ if is_text_wrapper(name) => {
            if let Some(arg) = args.first() {
                match wrapper_mark(name) {
                    Some(mark) => {
                        out.open(mark);
                        flatten_into(arg, macros, depth, out);
                        out.close();
                    }
                    None => flatten_into(arg, macros, depth, out),
                }
            }
        }
        _ if citation_kind(name).is_some() => {
            let kind = citation_kind(name).expect("just matched");
            let Some(arg) = args.first() else { return };
            let keys = flatten_nodes(arg, macros);
            // `\cite{a,b}` names two works, so it leaves two references.
            for (position, key) in keys.split(',').map(str::trim).enumerate() {
                if key.is_empty() {
                    continue;
                }
                if position > 0 {
                    out.push(", ");
                }
                out.push_marked(key, Mark::reference(key, kind));
            }
        }
        "url" => {
            if let Some(arg) = args.first() {
                let url = flatten_nodes(arg, macros);
                if !url.is_empty() {
                    out.open_link(&url, None);
                    out.push(&url);
                    out.close_link();
                }
            }
        }
        "href" => {
            let url = args.first().map(|arg| flatten_nodes(arg, macros));
            let display = args.get(1).map(|arg| flatten_nodes(arg, macros));
            match (display, url) {
                (Some(display), Some(url)) if !display.is_empty() && !url.is_empty() => {
                    // The link used to be smuggled into the prose as the
                    // literal `[text](url)`; it is a run now.
                    out.open_link(&url, None);
                    out.push(&display);
                    out.close_link();
                }
                (Some(text), _) | (_, Some(text)) => out.push(&text),
                _ => {}
            }
        }
        "textcolor" | "colorbox" => {
            if let Some(arg) = args.last() {
                flatten_into(arg, macros, depth, out);
            }
        }
        "item" => {
            // A description term reads as a lead-in.
            if let Some(term) = opt {
                out.push(term.trim());
                out.push(": ");
            }
        }
        "footnote" | "marginpar" | "caption" | "label" => {}
        _ if BARE_IGNORED.contains(&name) || GREEDY_IGNORED.contains(&name) => {}
        _ => {
            if let Some(definition) = macros.defs.get(name) {
                // One level, as documented: a use inside an expansion is
                // not expanded again, so a self-referencing or mutually
                // recursive definition costs one pass, not branching^depth.
                if depth == 0 && macros.spend(definition.len()) {
                    let expanded = Parser::new(definition).nodes(None);
                    flatten_into(&expanded, macros, depth + 1, out);
                }
                return;
            }
            // Unknown macro: its argument text survives, the command
            // itself does not.
            for arg in args {
                flatten_into(arg, macros, depth, out);
                out.space();
            }
        }
    }
}

/// A formula body cleaned: `\label{...}` lifted out, delimiters trimmed.
///
/// The label used to be deleted here, which is why every `\eqref` pointed
/// at nothing. It comes back as the formula's anchor.
fn clean_math(text: &str) -> (String, Option<String>) {
    let mut out = String::with_capacity(text.len());
    let mut label = None;
    let mut rest = text;
    while let Some(hit) = rest.find("\\label") {
        out.push_str(&rest[..hit]);
        let after = rest[hit + 6..].trim_start();
        if let Some(braced) = after.strip_prefix('{') {
            match braced.find('}') {
                Some(close) => {
                    if label.is_none() {
                        let key = braced[..close].trim();
                        if !key.is_empty() {
                            label = Some(key.to_owned());
                        }
                    }
                    rest = &braced[close + 1..];
                }
                None => {
                    rest = "";
                }
            }
        } else {
            out.push_str("\\label");
            rest = &rest[hit + 6..];
        }
    }
    out.push_str(rest);
    (out.trim().to_owned(), label)
}

// ---------------------------------------------------------------- mapping

/// What the surrounding structure makes of a flushed text run.
#[derive(Clone, Default)]
struct Ctx {
    /// How many `quote` / `quotation` / `verse` environments are open. A
    /// quotation inside a quotation is depth 2; zero is not a quotation.
    quote: u32,
    /// Inside a list item: every flushed run is a list item at this depth.
    list: Option<(bool, u32)>,
}

/// The mapping walk over parsed nodes.
struct Walk<'w, 'e> {
    emitter: &'w mut Emitter<'e>,
    macros: Macros,
    title_taken: bool,
    /// A `\label` seen inside running text, waiting for the block that
    /// text becomes.
    pending_anchor: Option<String>,
    /// The `\label` of the float being walked. A `figure` or `table`
    /// environment's label names the float itself, not the caption that
    /// happens to sit beside it, so it waits for the picture or the table
    /// rather than being claimed by the block before it.
    float_anchor: Option<String>,
    /// Footnotes emitted so far, for minting the marker a `\footnote`
    /// leaves behind in the paragraph it interrupts.
    footnotes: u32,
}

impl Walk<'_, '_> {
    /// Process a node run: buffer inline content, flush at block
    /// boundaries — docling's `_process_nodes`.
    fn nodes(&mut self, nodes: &[Node], ctx: &Ctx) -> Result<(), ParseError> {
        let mut buffer = Inline::default();
        // A `\label` that follows a structural macro belongs to the block
        // that macro made, not to the text after it.
        let mut claimed: Option<usize> = None;
        for (index, node) in nodes.iter().enumerate() {
            match node {
                Node::Text(text) => {
                    // A blank line closes the paragraph before it; the text
                    // after the last one keeps buffering, so a paragraph
                    // can continue into the following inline nodes.
                    let mut parts = text.split("\n\n");
                    if let Some(first) = parts.next() {
                        buffer.push(first);
                    }
                    for part in parts {
                        self.flush(&mut buffer, ctx)?;
                        buffer.push(part);
                    }
                }
                Node::Group(inner) => {
                    if text_only(inner) {
                        flatten_into(inner, &self.macros, 0, &mut buffer);
                    } else {
                        self.flush(&mut buffer, ctx)?;
                        self.nodes(inner, ctx)?;
                    }
                }
                Node::Math { display, text } => {
                    if *display {
                        self.flush(&mut buffer, ctx)?;
                        self.formula(text, None)?;
                    } else {
                        buffer.push_marked(text, Mark::style(pb::SpanStyle::Math).with_raw(text));
                    }
                }
                Node::Macro { name, opt, args } => {
                    if name == "label" {
                        if claimed == Some(index) {
                            continue;
                        }
                        // A bare `\label` inside running text anchors the
                        // block that text is about to become.
                        if let Some(key) = label_key(args, &self.macros) {
                            self.pending_anchor = Some(key);
                        }
                        continue;
                    }
                    if self.is_structural(name) {
                        let found = label_after(nodes, index, &self.macros);
                        claimed = found.as_ref().map(|(_, at)| *at);
                        let mut anchor = found.map(|(key, _)| key);
                        if matches!(name.as_str(), "footnote" | "marginpar") {
                            // The footnote body interrupts the paragraph;
                            // the marker it leaves behind is what says
                            // where in the paragraph it belonged.
                            let (key, marker) = self.footnote_key();
                            let key = anchor.clone().unwrap_or(key);
                            buffer.push_marked(
                                &marker,
                                Mark::reference(&key, pb::ReferenceKind::Footnote)
                                    .with_style(pb::SpanStyle::Superscript),
                            );
                            anchor = Some(key);
                        }
                        self.flush(&mut buffer, ctx)?;
                        self.structural(name, opt.as_deref(), args, anchor)?;
                    } else {
                        flatten_macro(name, opt.as_deref(), args, &self.macros, 0, &mut buffer);
                    }
                }
                Node::Environment {
                    name,
                    opt,
                    raw,
                    body,
                } => {
                    self.flush(&mut buffer, ctx)?;
                    self.environment(name, opt.as_deref(), raw, body, ctx)?;
                }
            }
        }
        self.flush(&mut buffer, ctx)
    }

    /// True for macros that produce their own block rather than inline
    /// text.
    fn is_structural(&self, name: &str) -> bool {
        HEADINGS.iter().any(|(heading, _)| *heading == name)
            || matches!(
                name,
                "title"
                    | "author"
                    | "date"
                    | "caption"
                    | "footnote"
                    | "marginpar"
                    | "includegraphics"
                    | "input"
                    | "include"
            )
    }

    /// One structural macro.
    fn structural(
        &mut self,
        name: &str,
        opt: Option<&str>,
        args: &[Vec<Node>],
        anchor: Option<String>,
    ) -> Result<(), ParseError> {
        let arg_text = args
            .first()
            .map(|arg| flatten_nodes(arg, &self.macros))
            .unwrap_or_default();
        if let Some((_, level)) = HEADINGS.iter().find(|(heading, _)| *heading == name) {
            if arg_text.is_empty() {
                return Ok(());
            }
            return self.emitter.text_block(Block {
                label: pb::BlockLabel::SectionHeader,
                text: arg_text,
                level: Some(*level),
                anchor_id: anchor,
                ..Block::default()
            });
        }
        match name {
            "title" => {
                if arg_text.is_empty() {
                    return Ok(());
                }
                if self.title_taken {
                    self.emitter.text_block(Block {
                        label: pb::BlockLabel::Paragraph,
                        role: "title".to_owned(),
                        text: arg_text,
                        ..Block::default()
                    })
                } else {
                    self.title_taken = true;
                    self.emitter.text_block(Block {
                        label: pb::BlockLabel::Title,
                        text: arg_text,
                        ..Block::default()
                    })
                }
            }
            "author" | "date" => {
                if arg_text.is_empty() {
                    return Ok(());
                }
                self.emitter.text_block(Block {
                    label: pb::BlockLabel::Paragraph,
                    role: name.to_owned(),
                    text: arg_text,
                    ..Block::default()
                })
            }
            "caption" => {
                if arg_text.is_empty() {
                    return Ok(());
                }
                self.emitter.text_block(Block {
                    label: pb::BlockLabel::Caption,
                    text: arg_text,
                    anchor_id: anchor,
                    ..Block::default()
                })
            }
            "footnote" | "marginpar" => {
                if arg_text.is_empty() {
                    return Ok(());
                }
                self.emitter.text_block(Block {
                    label: pb::BlockLabel::Footnote,
                    text: arg_text,
                    // The body is the anchor the marker left behind in the
                    // paragraph it interrupted points at.
                    anchor_id: anchor,
                    ..Block::default()
                })
            }
            "includegraphics" => {
                if arg_text.is_empty() {
                    return Ok(());
                }
                // The option list is the author's own key=value vocabulary;
                // the two keys that say how big the figure should be are
                // kept verbatim, units and macros and all.
                self.emitter.image(Image {
                    src: arg_text,
                    width: graphics_option(opt, "width"),
                    height: graphics_option(opt, "height"),
                    // A \ref to a figure means the figure, so the float's
                    // own label lands here rather than on its caption.
                    anchor_id: self.float_anchor.take().or(anchor),
                    ..Image::default()
                })
            }
            "input" | "include" => {
                // Diskless by doctrine: the referenced file is never read.
                self.emitter.warn(
                    pb::WarningCode::UnmappedConstruct,
                    &format!("\\{name} is not resolved; this service reads no files"),
                );
                Ok(())
            }
            _ => Ok(()),
        }
    }

    /// One environment.
    fn environment(
        &mut self,
        name: &str,
        opt: Option<&str>,
        raw: &str,
        body: &[Node],
        ctx: &Ctx,
    ) -> Result<(), ParseError> {
        let base = name.trim_end_matches('*');
        match base {
            "document" | "subequations" | "center" => self.nodes(body, ctx),
            // A float's `\label` names the float. The label node comes out
            // of the body so the caption beside it cannot claim it, and the
            // picture or the table inside takes it instead.
            "figure" | "figure*" | "table" | "table*" => {
                let (body, anchor) = take_float_label(body, &self.macros);
                self.float_anchor = anchor;
                let result = self.nodes(&body, ctx);
                // A float with no picture and no table has nothing to
                // anchor; the key does not leak into the next one.
                self.float_anchor = None;
                result
            }
            "abstract" => {
                self.emitter.text_block(Block {
                    label: pb::BlockLabel::SectionHeader,
                    role: "abstract".to_owned(),
                    text: "Abstract".to_owned(),
                    level: Some(1),
                    ..Block::default()
                })?;
                self.nodes(body, ctx)
            }
            _ if MATH_ENVS.contains(&base) => self.formula(raw, None),
            _ if THEOREM_ENVS.contains(&base) => {
                let mut lead = base.to_owned();
                lead[..1].make_ascii_uppercase();
                self.emitter.text_block(Block {
                    label: pb::BlockLabel::Paragraph,
                    role: base.to_owned(),
                    text: format!("{lead}."),
                    ..Block::default()
                })?;
                self.nodes(body, ctx)
            }
            "proof" => {
                self.emitter.text_block(Block {
                    label: pb::BlockLabel::Paragraph,
                    role: "proof".to_owned(),
                    text: "Proof.".to_owned(),
                    ..Block::default()
                })?;
                self.nodes(body, ctx)
            }
            "quote" | "quotation" | "verse" => {
                let quoted = Ctx {
                    quote: ctx.quote + 1,
                    list: ctx.list,
                };
                self.nodes(body, &quoted)
            }
            "itemize" => self.list(body, false, ctx),
            "enumerate" => self.list(body, true, ctx),
            "description" => self.list(body, false, ctx),
            "tabular" => self.table(body),
            "verbatim" | "lstlisting" | "minted" => {
                let text = raw.trim_matches('\n');
                if text.is_empty() {
                    return Ok(());
                }
                let language = opt.and_then(|options| {
                    options.split(',').find_map(|entry| {
                        let (key, value) = entry.split_once('=')?;
                        (key.trim() == "language").then(|| value.trim().to_owned())
                    })
                });
                self.emitter.text_block(Block {
                    label: pb::BlockLabel::Code,
                    text: text.to_owned(),
                    language,
                    ..Block::default()
                })
            }
            "tikzpicture" => {
                let text = raw.trim_matches('\n');
                if text.is_empty() {
                    return Ok(());
                }
                // No renderer in this service: the picture's source is the
                // most faithful thing to carry, exactly docling's fallback.
                self.emitter.text_block(Block {
                    label: pb::BlockLabel::Code,
                    text: text.to_owned(),
                    language: Some("tikz".to_owned()),
                    ..Block::default()
                })
            }
            "thebibliography" => {
                self.emitter.text_block(Block {
                    label: pb::BlockLabel::SectionHeader,
                    role: "bibliography".to_owned(),
                    text: "References".to_owned(),
                    level: Some(1),
                    ..Block::default()
                })?;
                self.bibliography(body)
            }
            "filecontents" => Ok(()),
            _ => {
                self.emitter.warn(
                    pb::WarningCode::UnmappedConstruct,
                    &format!("environment `{base}` has no mapping; its body was kept"),
                );
                self.nodes(body, ctx)
            }
        }
    }

    /// A display formula, cleaned. Its own `\label` is the anchor an
    /// `\eqref` resolves to, rather than something the cleaning deletes.
    fn formula(&mut self, raw: &str, anchor: Option<String>) -> Result<(), ParseError> {
        let (text, label) = clean_math(raw);
        if text.is_empty() {
            return Ok(());
        }
        self.emitter.text_block(Block {
            label: pb::BlockLabel::Formula,
            text,
            anchor_id: anchor.or(label),
            ..Block::default()
        })
    }

    /// The anchor a footnote body carries when the source names none, and
    /// the marker its reference leaves in the text.
    fn footnote_key(&mut self) -> (String, String) {
        self.footnotes += 1;
        (
            format!("footnote-{}", self.footnotes),
            self.footnotes.to_string(),
        )
    }

    /// An `itemize` / `enumerate` / `description` body: nodes split on
    /// `\item`, each item's runs flushed as list items.
    fn list(&mut self, body: &[Node], enumerated: bool, ctx: &Ctx) -> Result<(), ParseError> {
        let depth = ctx.list.map_or(1, |(_, depth)| depth + 1);
        let item_ctx = Ctx {
            quote: ctx.quote,
            list: Some((enumerated, depth)),
        };
        let mut current: Vec<Node> = Vec::new();
        let mut seen_item = false;
        for node in body {
            if let Node::Macro { name, opt, .. } = node
                && name == "item"
            {
                if seen_item && !current.is_empty() {
                    self.nodes(&current, &item_ctx)?;
                }
                current = Vec::new();
                seen_item = true;
                if let Some(term) = opt {
                    // The description term leads the item text.
                    current.push(Node::Text(format!("{}: ", term.trim())));
                }
                continue;
            }
            current.push(node.clone());
        }
        if !current.is_empty() {
            let tail_ctx = if seen_item { &item_ctx } else { ctx };
            self.nodes(&current, tail_ctx)?;
        }
        Ok(())
    }

    /// A `thebibliography` body: one list item per `\bibitem`, its key the
    /// anchor every `\cite` of that entry resolves to rather than a
    /// bracketed prefix on the text.
    fn bibliography(&mut self, body: &[Node]) -> Result<(), ParseError> {
        let mut current = Inline::default();
        let mut key: Option<String> = None;
        let mut in_item = false;
        for node in body {
            if !in_item && matches!(node, Node::Group(_)) {
                // The environment's widest-label argument (`{9}`), not an
                // entry.
                continue;
            }
            if let Node::Macro { name, args, .. } = node
                && name == "bibitem"
            {
                if in_item {
                    self.bib_entry(&mut current, key.take())?;
                }
                in_item = true;
                key = args
                    .first()
                    .map(|arg| flatten_nodes(arg, &self.macros))
                    .filter(|key| !key.is_empty());
                continue;
            }
            flatten_into(std::slice::from_ref(node), &self.macros, 0, &mut current);
        }
        if in_item {
            self.bib_entry(&mut current, key)?;
        }
        Ok(())
    }

    /// One bibliography entry, anchored by its `\bibitem` key.
    fn bib_entry(&mut self, current: &mut Inline, key: Option<String>) -> Result<(), ParseError> {
        let (text, spans) = current.take();
        if text.is_empty() {
            return Ok(());
        }
        self.emitter.text_block(Block {
            label: pb::BlockLabel::ListItem,
            role: "bibliography".to_owned(),
            text,
            list_depth: Some(1),
            spans,
            anchor_id: key,
            ..Block::default()
        })
    }

    /// A `tabular` body as table events.
    fn table(&mut self, body: &[Node]) -> Result<(), ParseError> {
        // The first group is the column specification, not a cell. It used
        // to be skipped past; it is the table's column geometry.
        let mut columns = Vec::new();
        let cells_start = body
            .iter()
            .position(|node| !matches!(node, Node::Text(text) if text.trim().is_empty()))
            .map_or(0, |first| match body.get(first) {
                Some(Node::Group(spec)) => {
                    columns = column_specs(spec);
                    first + 1
                }
                _ => first,
            });

        self.emitter.table_start(Table {
            columns,
            // The float's own label names the table, not the caption that
            // happens to sit beside it.
            anchor_id: self.float_anchor.take(),
            ..Table::default()
        })?;
        let mut row = RowBuilder::default();
        for node in &body[cells_start..] {
            match node {
                Node::Text(text) => {
                    if text.contains('&') {
                        let mut parts = text.split('&');
                        if let Some(first) = parts.next() {
                            row.cell.push(first);
                        }
                        for part in parts {
                            row.finish_cell();
                            row.cell.push(part);
                        }
                    } else {
                        row.cell.push(text);
                    }
                }
                Node::Macro { name, opt, args } => match name.as_str() {
                    "\\" => {
                        // Row break: close the pending cell and the row.
                        if let Some(cells) = row.take_row() {
                            self.emitter.table_row(false, cells)?;
                        }
                    }
                    "multicolumn" => {
                        if let Some(count) = args
                            .first()
                            .map(|arg| flatten_nodes(arg, &self.macros))
                            .and_then(|n| n.parse::<u32>().ok())
                        {
                            row.spans.0 = count.max(1);
                        }
                        if let Some(content) = args.get(2) {
                            flatten_into(content, &self.macros, 0, &mut row.cell);
                        }
                    }
                    "multirow" => {
                        if let Some(count) = args
                            .first()
                            .map(|arg| flatten_nodes(arg, &self.macros))
                            .and_then(|n| n.parse::<u32>().ok())
                        {
                            row.spans.1 = count.max(1);
                        }
                        if let Some(content) = args.get(2) {
                            flatten_into(content, &self.macros, 0, &mut row.cell);
                        }
                    }
                    _ if TABLE_RULES.contains(&name.as_str()) => {}
                    _ => flatten_macro(name, opt.as_deref(), args, &self.macros, 0, &mut row.cell),
                },
                other => {
                    flatten_into(std::slice::from_ref(other), &self.macros, 0, &mut row.cell);
                }
            }
        }
        // The last row often has no trailing \\.
        if let Some(cells) = row.take_row() {
            self.emitter.table_row(false, cells)?;
        }
        self.emitter.table_end()
    }

    /// Flush the inline buffer as one block, labelled by context.
    fn flush(&mut self, buffer: &mut Inline, ctx: &Ctx) -> Result<(), ParseError> {
        let hyperlink = buffer.hyperlink();
        let (text, spans) = buffer.take();
        if text.is_empty() {
            self.pending_anchor = None;
            return Ok(());
        }
        let anchor = self.pending_anchor.take();
        let block = if let Some((enumerated, depth)) = ctx.list {
            Block {
                label: pb::BlockLabel::ListItem,
                text,
                enumerated,
                // LaTeX numbering is implicit; there is no marker "as
                // written" to carry.
                marker: None,
                list_depth: Some(depth),
                hyperlink,
                spans,
                anchor_id: anchor,
                quote_depth: (ctx.quote > 0).then_some(ctx.quote),
                ..Block::default()
            }
        } else {
            Block {
                label: if ctx.quote > 0 {
                    pb::BlockLabel::Quote
                } else {
                    pb::BlockLabel::Paragraph
                },
                text,
                hyperlink,
                spans,
                anchor_id: anchor,
                quote_depth: (ctx.quote > 0).then_some(ctx.quote),
                ..Block::default()
            }
        };
        self.emitter.text_block(block)
    }
}

/// Split a float's body from the `\label` that names it.
///
/// The label may sit anywhere in the body, and in practice sits after the
/// caption, which is exactly why it cannot be left there: the caption would
/// claim it and a `\ref` to the figure would point at its caption text.
fn take_float_label(body: &[Node], macros: &Macros) -> (Vec<Node>, Option<String>) {
    let found = body.iter().position(|node| {
        matches!(node, Node::Macro { name, args, .. }
            if name == "label" && label_key(args, macros).is_some())
    });
    let Some(at) = found else {
        return (body.to_vec(), None);
    };
    let key = match &body[at] {
        Node::Macro { args, .. } => label_key(args, macros),
        _ => None,
    };
    let mut kept = body.to_vec();
    kept.remove(at);
    (kept, key)
}

/// One key of an `\includegraphics` option list, verbatim.
///
/// The list is `key=value` pairs separated by commas, and the value is
/// whatever the author wrote: `5cm`, `0.8\textwidth`, `120`. Nothing is
/// converted, because a length in a TeX unit or in a macro the preamble
/// defines is not a number this fold can honestly produce.
fn graphics_option(options: Option<&str>, key: &str) -> Option<String> {
    options?.split(',').find_map(|option| {
        let (name, value) = option.split_once('=')?;
        (name.trim().eq_ignore_ascii_case(key))
            .then(|| value.trim())
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
    })
}

/// The `\label` key of a label macro's argument.
fn label_key(args: &[Vec<Node>], macros: &Macros) -> Option<String> {
    args.first()
        .map(|arg| flatten_nodes(arg, macros))
        .filter(|key| !key.is_empty())
}

/// The `\label` that follows the node at `index`, skipping only
/// whitespace: `\section{Intro}\label{sec:intro}` anchors the heading, not
/// the text after it. Returns the key and the index it was found at.
fn label_after(nodes: &[Node], index: usize, macros: &Macros) -> Option<(String, usize)> {
    for (at, node) in nodes.iter().enumerate().skip(index + 1) {
        match node {
            Node::Text(text) if text.trim().is_empty() => {}
            Node::Macro { name, args, .. } if name == "label" => {
                return label_key(args, macros).map(|key| (key, at));
            }
            _ => return None,
        }
    }
    None
}

/// The column geometry a `tabular` preamble declares, one entry per
/// column. Rules (`|`), spacing (`@{...}`) and cell decorators (`>{...}`)
/// are not columns and are skipped.
fn column_specs(spec: &[Node]) -> Vec<pb::ColumnSpec> {
    let text = spec_text(spec);
    let mut columns = Vec::new();
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        let align = match c {
            'l' => pb::Alignment::Left,
            'c' => pb::Alignment::Center,
            'r' => pb::Alignment::Right,
            // A fixed-width column sets its own width and justifies.
            'p' | 'm' | 'b' => pb::Alignment::Justify,
            '@' | '>' | '<' | '!' => {
                braced(&mut chars);
                continue;
            }
            _ => continue,
        };
        let width = matches!(c, 'p' | 'm' | 'b')
            .then(|| braced(&mut chars))
            .flatten();
        columns.push(pb::ColumnSpec {
            align: align as i32,
            valign: match c {
                'm' => pb::Alignment::Middle as i32,
                'b' => pb::Alignment::Bottom as i32,
                'p' => pb::Alignment::Top as i32,
                _ => pb::Alignment::Unspecified as i32,
            },
            width,
        });
    }
    columns
}

/// The content of a `{...}` group at the cursor, consumed.
fn braced(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) -> Option<String> {
    if chars.peek() != Some(&'{') {
        return None;
    }
    chars.next();
    let mut depth = 1usize;
    let mut out = String::new();
    for c in chars.by_ref() {
        match c {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    break;
                }
            }
            _ => {}
        }
        out.push(c);
    }
    Some(out).filter(|text| !text.is_empty())
}

/// A column specification rendered back to source-ish text, braces kept,
/// so `p{5cm}` stays one column with a width.
fn spec_text(nodes: &[Node]) -> String {
    let mut out = String::new();
    for node in nodes {
        match node {
            Node::Text(text) => out.push_str(text),
            Node::Group(inner) => {
                out.push('{');
                out.push_str(&spec_text(inner));
                out.push('}');
            }
            Node::Macro { name, .. } => {
                out.push('\\');
                out.push_str(name);
            }
            _ => {}
        }
    }
    out
}

/// One `tabular` row being assembled: the pending cell's text, its spans
/// (set by `\multicolumn` / `\multirow`), and the finished cells.
#[derive(Default)]
struct RowBuilder {
    cell: Inline,
    spans: (u32, u32),
    column: u32,
    cells: Vec<pb::TableCell>,
}

impl RowBuilder {
    /// Close the pending cell into the row.
    fn finish_cell(&mut self) {
        let (column_span, row_span) = self.spans;
        let column_span = column_span.max(1);
        self.spans = (1, 1);
        let (text, spans) = self.cell.take();
        self.cells.push(pb::TableCell {
            column_index: self.column,
            text,
            column_span,
            row_span: row_span.max(1),
            is_header: false,
            spans,
            ..pb::TableCell::default()
        });
        self.column += column_span;
    }

    /// Close the row, returning its cells when it carries any content.
    /// Rule-only and whitespace-only rows dissolve.
    fn take_row(&mut self) -> Option<Vec<pb::TableCell>> {
        if !self.cell.is_empty() || !self.cells.is_empty() {
            self.finish_cell();
        }
        self.column = 0;
        let cells = std::mem::take(&mut self.cells);
        if cells.iter().any(|cell| !cell.text.is_empty()) {
            Some(cells)
        } else {
            None
        }
    }
}

/// True when a group holds only inline content, docling's
/// `_is_text_only_group`.
fn text_only(nodes: &[Node]) -> bool {
    nodes.iter().all(|node| match node {
        Node::Environment { .. } => false,
        Node::Group(inner) => text_only(inner),
        Node::Macro { name, .. } => {
            !HEADINGS.iter().any(|(heading, _)| *heading == name)
                && !matches!(
                    name.as_str(),
                    "title" | "author" | "caption" | "footnote" | "includegraphics"
                )
        }
        _ => true,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn flat(latex: &str) -> String {
        let nodes = Parser::new(latex).nodes(None);
        flatten_nodes(&nodes, &Macros::default())
    }

    #[test]
    fn inline_commands_flatten_to_their_text() {
        assert_eq!(flat("\\textbf{bold} and \\emph{em}"), "bold and em");
        assert_eq!(flat("A\\footnote{dropped} B"), "A B");
        assert_eq!(flat("x~y"), "x y");
        assert_eq!(flat("50\\% \\& \\$5"), "50% & $5");
    }

    /// The text and the runs a snippet flattens to.
    fn runs(latex: &str) -> (String, Vec<pb::InlineSpan>) {
        let nodes = Parser::new(latex).nodes(None);
        let mut out = Inline::default();
        flatten_into(&nodes, &Macros::default(), 0, &mut out);
        out.take()
    }

    /// The text one run covers, sliced by code points.
    fn covered(text: &str, span: &pb::InlineSpan) -> String {
        let range = span.range.expect("a run has a range");
        text.chars()
            .skip(range.start as usize)
            .take((range.end - range.start) as usize)
            .collect()
    }

    #[test]
    fn citations_and_refs_keep_their_keys_and_their_kind() {
        let (text, spans) = runs("see \\cite{knuth84}");
        assert_eq!(text, "see knuth84", "no bracketed placeholder");
        assert_eq!(covered(&text, &spans[0]), "knuth84");
        assert_eq!(spans[0].reference.as_deref(), Some("knuth84"));
        assert_eq!(spans[0].reference_kind, pb::ReferenceKind::Citation as i32);

        let (text, spans) = runs("Fig.~\\ref{fig:a}");
        assert_eq!(text, "Fig. fig:a");
        assert_eq!(
            spans[0].reference_kind,
            pb::ReferenceKind::CrossRef as i32,
            "a cross reference is not a citation"
        );

        let (text, spans) = runs("\\eqref{eq:mass}");
        assert_eq!(text, "eq:mass");
        assert_eq!(
            spans[0].reference_kind,
            pb::ReferenceKind::EquationRef as i32
        );

        // One macro, two works.
        let (text, spans) = runs("\\cite{a,b}");
        assert_eq!(text, "a, b");
        assert_eq!(spans.len(), 2);
        assert_eq!(covered(&text, &spans[1]), "b");
    }

    #[test]
    fn href_and_url_leave_a_link_run_not_punctuation() {
        let (text, spans) = runs("\\href{https://example.com}{Docs}");
        assert_eq!(text, "Docs", "the URL is not smuggled into the prose");
        assert_eq!(covered(&text, &spans[0]), "Docs");
        assert_eq!(spans[0].hyperlink.as_deref(), Some("https://example.com"));

        let (text, spans) = runs("\\url{https://example.com}");
        assert_eq!(text, "https://example.com");
        assert_eq!(spans[0].hyperlink.as_deref(), Some("https://example.com"));
    }

    #[test]
    fn text_wrappers_name_the_run_they_cover() {
        let (text, spans) = runs("\\textbf{bold} et \\emph{prêt} et \\texttt{code}");
        assert_eq!(text, "bold et prêt et code");
        let seen: Vec<(String, Vec<i32>)> = spans
            .iter()
            .map(|span| (covered(&text, span), span.styles.clone()))
            .collect();
        assert_eq!(
            seen,
            vec![
                ("bold".to_owned(), vec![pb::SpanStyle::Bold as i32]),
                ("prêt".to_owned(), vec![pb::SpanStyle::Italic as i32]),
                ("code".to_owned(), vec![pb::SpanStyle::Code as i32]),
            ],
            "the accented run's range is in code points"
        );
    }

    #[test]
    fn a_column_specification_is_geometry_rather_than_something_to_skip() {
        let nodes = Parser::new("{|l|c|r|}").nodes(None);
        let Some(Node::Group(spec)) = nodes.first() else {
            panic!("one group, got {nodes:?}");
        };
        assert_eq!(
            column_specs(spec)
                .iter()
                .map(|column| column.align)
                .collect::<Vec<_>>(),
            vec![
                pb::Alignment::Left as i32,
                pb::Alignment::Center as i32,
                pb::Alignment::Right as i32,
            ]
        );

        let nodes = Parser::new("{l p{5cm}}").nodes(None);
        let Some(Node::Group(spec)) = nodes.first() else {
            panic!("one group");
        };
        let columns = column_specs(spec);
        assert_eq!(columns.len(), 2);
        assert_eq!(columns[1].align, pb::Alignment::Justify as i32);
        assert_eq!(columns[1].width.as_deref(), Some("5cm"));
    }

    #[test]
    fn comments_strip_but_escaped_percent_survives() {
        assert_eq!(flat("kept % dropped\nline"), "kept line");
        assert_eq!(flat("100\\% sure"), "100% sure");
    }

    #[test]
    fn spacing_macros_take_their_arguments_with_them() {
        assert_eq!(flat("a\\vspace{-1mm} b"), "a b");
        assert_eq!(flat("\\usepackage[utf8]{inputenc}x"), "x");
    }

    #[test]
    fn zero_argument_newcommands_expand() {
        let mut macros = HashMap::new();
        collect_newcommands("\\newcommand{\\etal}{et al.}", &mut macros);
        assert_eq!(macros.get("etal").map(String::as_str), Some("et al."));

        let mut with_args = HashMap::new();
        collect_newcommands("\\newcommand{\\pair}[2]{#1-#2}", &mut with_args);
        assert!(
            with_args.is_empty(),
            "parameterized macros are deliberately not expanded"
        );

        let nodes = Parser::new("Knuth \\etal shipped.").nodes(None);
        assert_eq!(
            flatten_nodes(&nodes, &Macros::new(macros, 0)),
            "Knuth et al. shipped."
        );
    }

    #[test]
    fn math_cleaning_lifts_the_label_out_instead_of_deleting_it() {
        let (text, label) = clean_math("  E = mc^2 \\label{eq:emc}  ");
        assert_eq!(text, "E = mc^2");
        assert_eq!(
            label.as_deref(),
            Some("eq:emc"),
            "the anchor every eqref points at"
        );
        let (text, label) = clean_math("a\\label{x}b");
        assert_eq!(text, "ab");
        assert_eq!(label.as_deref(), Some("x"));
    }

    #[test]
    fn the_document_environment_is_found_wherever_it_sits() {
        let nodes = Parser::new("preamble \\begin{document}body\\end{document}").nodes(None);
        let body = find_document(&nodes).expect("found");
        assert_eq!(flatten_nodes(body, &Macros::default()), "body");
        assert!(find_document(&Parser::new("no env").nodes(None)).is_none());
    }

    #[test]
    fn preamble_metadata_is_scanned_before_the_body() {
        let nodes = Parser::new(
            "\\title{The Title}\\author{Ada}\\date{1843}\\begin{document}\\title{Ignored}\\end{document}",
        )
        .nodes(None);
        let (mut title, mut author, mut date) = (None, None, None);
        scan_metadata(&nodes, &mut title, &mut author, &mut date);
        assert_eq!(title.as_deref(), Some("The Title"));
        assert_eq!(author.as_deref(), Some("Ada"));
        assert_eq!(date.as_deref(), Some("1843"));
    }

    #[test]
    fn verbatim_bodies_are_captured_raw() {
        let nodes =
            Parser::new("\\begin{verbatim}\\textbf{not parsed} & $x$\n\\end{verbatim}").nodes(None);
        let Some(Node::Environment { name, raw, .. }) = nodes.first() else {
            panic!("one environment, got {nodes:?}");
        };
        assert_eq!(name, "verbatim");
        assert_eq!(raw.trim(), "\\textbf{not parsed} & $x$");
    }
}
