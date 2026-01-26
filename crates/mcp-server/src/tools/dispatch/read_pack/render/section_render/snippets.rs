use super::super::super::{ReadPackSnippetKind, ResponseMode};
use crate::tools::context_doc::ContextDocBuilder;
use crate::tools::schemas::read_pack::{ReadPackRecallResult, ReadPackSnippet};

pub(super) fn render_snippet(
    doc: &mut ContextDocBuilder,
    snippet: &ReadPackSnippet,
    response_mode: ResponseMode,
) {
    let label = snippet_label(snippet.kind);
    doc.push_ref_header(&snippet.file, snippet.start_line, label);
    if response_mode == ResponseMode::Full {
        if let Some(reason) = snippet
            .reason
            .as_deref()
            .filter(|reason| !reason.trim().is_empty())
        {
            doc.push_note(&format!("reason: {reason}"));
        }
    }
    doc.push_block_smart(&snippet.content);
    doc.push_blank();
}

pub(super) fn render_recall(
    doc: &mut ContextDocBuilder,
    recall: &ReadPackRecallResult,
    response_mode: ResponseMode,
) {
    doc.push_note(&format!("recall: {}", recall.question));
    for snippet in &recall.snippets {
        let label = snippet_label(snippet.kind);
        doc.push_ref_header(&snippet.file, snippet.start_line, label);
        if response_mode == ResponseMode::Full {
            if let Some(reason) = snippet
                .reason
                .as_deref()
                .filter(|reason| !reason.trim().is_empty())
            {
                doc.push_note(&format!("reason: {reason}"));
            }
        }
        doc.push_block_smart(&snippet.content);
        doc.push_blank();
    }
}

fn snippet_label(kind: Option<ReadPackSnippetKind>) -> Option<&'static str> {
    match kind {
        Some(ReadPackSnippetKind::Code) => Some("code"),
        Some(ReadPackSnippetKind::Doc) => Some("doc"),
        Some(ReadPackSnippetKind::Config) => Some("config"),
        None => None,
    }
}
