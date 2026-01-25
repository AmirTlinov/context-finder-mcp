use super::{ContextFinderService, ReadPackResult, ReadPackSection};

pub(super) async fn note_session_working_set_from_read_pack_result(
    service: &ContextFinderService,
    result: &ReadPackResult,
) {
    let mut files: Vec<&str> = Vec::new();
    for section in &result.sections {
        match section {
            ReadPackSection::Snippet { result } => files.push(&result.file),
            ReadPackSection::FileSlice { result } => files.push(&result.file),
            ReadPackSection::Recall { result } => {
                for snippet in &result.snippets {
                    files.push(&snippet.file);
                }
            }
            _ => {}
        }
    }

    if files.is_empty() {
        return;
    }

    let mut session = service.session.lock().await;
    for file in files {
        session.note_seen_snippet_file(file);
    }
}
