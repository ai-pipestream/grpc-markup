// SPDX-License-Identifier: Apache-2.0

//! Golden mapping tests for the LaTeX fold, over a real socket.

mod common;

use common::{
    blocks_labelled, client, images, info, options, parse, parse_ok, sniff_options, status,
    table_rows, texts, warning,
};
use grpc_markup::proto::v1 as pb;

/// A document exercising the preamble (skipped machinery, surfaced
/// metadata, a custom macro), the section ladder, inline flattening,
/// citations, lists, a tabular with spans and rules, verbatim code,
/// display and inline math, a figure, a quote and an unknown environment.
const LATEX: &str = r#"\documentclass[11pt]{article}
\usepackage[utf8]{inputenc}
\newcommand{\engine}{Analytical Engine}
\title{Streaming Markup}
\author{Ada Lovelace}
\date{1843}

\begin{document}
\maketitle

\begin{abstract}
Programs are woven like brocade.
\end{abstract}

\section{Introduction}

An opening paragraph with \textbf{bold} and \emph{emphasis},
citing \cite{menabrea1843} about the \engine.

It even has $x^2$ inline math.

\subsection{Detail}

\begin{itemize}
  \item first bullet
  \item second bullet
  \begin{enumerate}
    \item nested one
  \end{enumerate}
\end{itemize}

\begin{quote}
A quoted sentence.
\end{quote}

\begin{tabular}{|l|c|}
\hline
Name & Role \\
\hline
Ada & \multicolumn{2}{c}{Analyst and lead} \\
\hline
\end{tabular}

\begin{verbatim}
fn main() {
    println!("hi");
}
\end{verbatim}

\begin{equation}
E = mc^2 \label{eq:emc}
\end{equation}

\begin{figure}
\includegraphics[width=\textwidth]{figures/engine.png}
\caption{The engine}
\end{figure}

\begin{mysteryenv}
Text inside an unknown environment survives.
\end{mysteryenv}

\end{document}
"#;

#[tokio::test]
async fn preamble_metadata_surfaces_and_machinery_does_not() {
    let client = client().await;
    let events = parse_ok(&client, LATEX, options(pb::MarkupFormat::Latex)).await;
    let info = info(&events);
    assert_eq!(info.format, pb::MarkupFormat::Latex as i32);
    assert_eq!(info.title.as_deref(), Some("Streaming Markup"));

    assert_eq!(
        texts(&blocks_labelled(&events, pb::BlockLabel::Title)),
        ["Streaming Markup"]
    );
    let paragraphs = blocks_labelled(&events, pb::BlockLabel::Paragraph);
    let author = paragraphs
        .iter()
        .find(|p| p.role == "author")
        .expect("the author paragraph");
    assert_eq!(author.text, "Ada Lovelace");
    let date = paragraphs
        .iter()
        .find(|p| p.role == "date")
        .expect("the date paragraph");
    assert_eq!(date.text, "1843");

    for block in common::text_blocks(&events) {
        assert!(!block.text.contains("documentclass"), "{}", block.text);
        assert!(!block.text.contains("usepackage"), "{}", block.text);
        assert!(!block.text.contains("11pt"), "preamble options leaked");
    }
}

#[tokio::test]
async fn the_section_ladder_uses_doclings_levels() {
    let client = client().await;
    let events = parse_ok(&client, LATEX, options(pb::MarkupFormat::Latex)).await;
    let headers = blocks_labelled(&events, pb::BlockLabel::SectionHeader);
    let names: Vec<(String, Option<u32>)> =
        headers.iter().map(|h| (h.text.clone(), h.level)).collect();
    assert!(names.contains(&("Abstract".to_owned(), Some(1))));
    assert!(names.contains(&("Introduction".to_owned(), Some(1))));
    assert!(names.contains(&("Detail".to_owned(), Some(2))));
    let abstract_header = headers.iter().find(|h| h.text == "Abstract").unwrap();
    assert_eq!(abstract_header.role, "abstract");
}

#[tokio::test]
async fn inline_commands_flatten_and_custom_macros_expand() {
    let client = client().await;
    let events = parse_ok(&client, LATEX, options(pb::MarkupFormat::Latex)).await;
    let paragraphs = blocks_labelled(&events, pb::BlockLabel::Paragraph);
    assert!(
        paragraphs.iter().any(|p| p.text
            == "An opening paragraph with bold and emphasis, citing [menabrea1843] about the \
                Analytical Engine."),
        "{:?}",
        texts(&paragraphs)
    );
    assert!(
        paragraphs
            .iter()
            .any(|p| p.text == "It even has $x^2$ inline math."),
        "inline math stays in the text stream: {:?}",
        texts(&paragraphs)
    );
}

#[tokio::test]
async fn lists_nest_and_enumerate_is_enumerated() {
    let client = client().await;
    let events = parse_ok(&client, LATEX, options(pb::MarkupFormat::Latex)).await;
    let items = blocks_labelled(&events, pb::BlockLabel::ListItem);
    assert_eq!(
        texts(&items),
        ["first bullet", "second bullet", "nested one"]
    );
    assert_eq!(items[0].list_depth, Some(1));
    assert!(!items[0].enumerated);
    assert!(items[0].marker.is_none(), "LaTeX numbering is implicit");
    assert_eq!(items[2].list_depth, Some(2));
    assert!(items[2].enumerated);
}

#[tokio::test]
async fn description_terms_lead_their_items() {
    let client = client().await;
    let latex = "\\begin{description}\\item[Engine] computes\\item[Loom] weaves\\end{description}";
    let events = parse_ok(&client, latex, options(pb::MarkupFormat::Latex)).await;
    let items = blocks_labelled(&events, pb::BlockLabel::ListItem);
    assert_eq!(texts(&items), ["Engine: computes", "Loom: weaves"]);
}

#[tokio::test]
async fn quotes_carry_the_quote_label() {
    let client = client().await;
    let events = parse_ok(&client, LATEX, options(pb::MarkupFormat::Latex)).await;
    assert_eq!(
        texts(&blocks_labelled(&events, pb::BlockLabel::Quote)),
        ["A quoted sentence."]
    );
}

#[tokio::test]
async fn tabular_splits_cells_drops_rules_and_carries_spans() {
    let client = client().await;
    let events = parse_ok(&client, LATEX, options(pb::MarkupFormat::Latex)).await;
    let rows = table_rows(&events);
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].cells[0].text, "Name");
    assert_eq!(rows[0].cells[1].text, "Role");
    assert_eq!(rows[0].cells[1].column_index, 1);
    assert_eq!(rows[1].cells[0].text, "Ada");
    assert_eq!(rows[1].cells[1].text, "Analyst and lead");
    assert_eq!(rows[1].cells[1].column_span, 2, "the multicolumn span");
    let end = events
        .iter()
        .find_map(|e| match e.event.as_ref() {
            Some(pb::parse_markup_response::Event::TableEnd(end)) => Some(end),
            _ => None,
        })
        .expect("the table closes");
    assert_eq!(end.column_count, 3, "one plain cell plus a two-wide span");
}

#[tokio::test]
async fn verbatim_keeps_its_body_and_lstlisting_declares_its_language() {
    let client = client().await;
    let events = parse_ok(&client, LATEX, options(pb::MarkupFormat::Latex)).await;
    let code = blocks_labelled(&events, pb::BlockLabel::Code);
    assert_eq!(code.len(), 1);
    assert_eq!(code[0].text, "fn main() {\n    println!(\"hi\");\n}");
    assert!(code[0].language.is_none());

    let listing = parse_ok(
        &client,
        "\\begin{lstlisting}[language=Java]\nint x = 1;\n\\end{lstlisting}",
        options(pb::MarkupFormat::Latex),
    )
    .await;
    let code = blocks_labelled(&listing, pb::BlockLabel::Code);
    assert_eq!(code[0].language.as_deref(), Some("Java"));
    assert_eq!(code[0].text, "int x = 1;");

    let tikz = parse_ok(
        &client,
        "\\begin{tikzpicture}\\draw (0,0) -- (1,1);\\end{tikzpicture}",
        options(pb::MarkupFormat::Latex),
    )
    .await;
    let code = blocks_labelled(&tikz, pb::BlockLabel::Code);
    assert_eq!(code[0].language.as_deref(), Some("tikz"));
}

#[tokio::test]
async fn display_math_is_a_formula_with_its_label_removed() {
    let client = client().await;
    let events = parse_ok(&client, LATEX, options(pb::MarkupFormat::Latex)).await;
    assert_eq!(
        texts(&blocks_labelled(&events, pb::BlockLabel::Formula)),
        ["E = mc^2"]
    );

    let brackets = parse_ok(
        &client,
        "text\n\n$$a+b$$\n\n\\[c-d\\]\n",
        options(pb::MarkupFormat::Latex),
    )
    .await;
    assert_eq!(
        texts(&blocks_labelled(&brackets, pb::BlockLabel::Formula)),
        ["a+b", "c-d"]
    );
}

#[tokio::test]
async fn includegraphics_is_an_image_and_its_caption_follows() {
    let client = client().await;
    let events = parse_ok(&client, LATEX, options(pb::MarkupFormat::Latex)).await;
    let pictures = images(&events);
    assert_eq!(pictures.len(), 1);
    assert_eq!(pictures[0].src, "figures/engine.png", "the path verbatim");
    assert_eq!(
        texts(&blocks_labelled(&events, pb::BlockLabel::Caption)),
        ["The engine"]
    );
}

#[tokio::test]
async fn unknown_environments_descend_with_one_aggregated_warning() {
    let client = client().await;
    let events = parse_ok(&client, LATEX, options(pb::MarkupFormat::Latex)).await;
    assert!(
        blocks_labelled(&events, pb::BlockLabel::Paragraph)
            .iter()
            .any(|p| p.text == "Text inside an unknown environment survives."),
        "the body is kept"
    );
    let unmapped = warning(&events, pb::WarningCode::UnmappedConstruct)
        .expect("the environment is named on the trailer");
    assert!(
        unmapped.message.contains("mysteryenv"),
        "{}",
        unmapped.message
    );
}

#[tokio::test]
async fn theorems_and_bibliographies_map_as_docling_maps_them() {
    let client = client().await;
    let latex = "\\begin{theorem}Numbers exist.\\end{theorem}\n\n\
                 \\begin{thebibliography}{9}\n\
                 \\bibitem{menabrea1843} L. Menabrea, Sketch of the Analytical Engine.\n\
                 \\end{thebibliography}\n";
    let events = parse_ok(&client, latex, options(pb::MarkupFormat::Latex)).await;
    let paragraphs = blocks_labelled(&events, pb::BlockLabel::Paragraph);
    let lead = paragraphs
        .iter()
        .find(|p| p.role == "theorem")
        .expect("the theorem lead-in");
    assert_eq!(lead.text, "Theorem.");
    let headers = blocks_labelled(&events, pb::BlockLabel::SectionHeader);
    assert!(headers.iter().any(|h| h.text == "References"));
    let items = blocks_labelled(&events, pb::BlockLabel::ListItem);
    assert_eq!(
        texts(&items),
        ["[menabrea1843] L. Menabrea, Sketch of the Analytical Engine."]
    );
}

#[tokio::test]
async fn a_fragment_without_begin_document_parses_whole() {
    let client = client().await;
    let events = parse_ok(
        &client,
        "\\section{Loose}\n\nA fragment without a document environment.\n",
        options(pb::MarkupFormat::Latex),
    )
    .await;
    assert_eq!(
        texts(&blocks_labelled(&events, pb::BlockLabel::SectionHeader)),
        ["Loose"]
    );
    assert_eq!(
        texts(&blocks_labelled(&events, pb::BlockLabel::Paragraph)),
        ["A fragment without a document environment."]
    );
}

#[tokio::test]
async fn sniffed_latex_dispatches_with_signature_evidence() {
    let client = client().await;
    for document in [
        "\\documentclass{article}\n\\begin{document}x\\end{document}\n",
        "\\begin{document}Just a body.\\end{document}\n",
    ] {
        let events = parse_ok(&client, document, sniff_options()).await;
        let info = info(&events);
        assert_eq!(info.format, pb::MarkupFormat::Latex as i32);
        assert_eq!(info.evidence, pb::FormatEvidence::Signature as i32);
    }
}

#[tokio::test]
async fn empty_latex_is_invalid_argument() {
    let client = client().await;
    for empty in ["", "   \n\t\n"] {
        let error = parse(&client, empty, options(pb::MarkupFormat::Latex))
            .await
            .expect_err("docling's own validity rule");
        assert_eq!(error.code(), tonic::Code::InvalidArgument, "{empty:?}");
    }
}

#[tokio::test]
async fn comments_never_reach_the_stream() {
    let client = client().await;
    let events = parse_ok(
        &client,
        "kept % a comment\nstill kept. 100\\% escaped.\n",
        options(pb::MarkupFormat::Latex),
    )
    .await;
    assert_eq!(
        texts(&blocks_labelled(&events, pb::BlockLabel::Paragraph)),
        ["kept still kept. 100% escaped."]
    );
    assert_eq!(status(&events).counts.as_ref().unwrap().text_blocks, 1);
}
