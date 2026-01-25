use super::cursors::CursorHeader;
use super::{call_error, decode_cursor, trimmed_non_empty_str, ReadPackIntent};
use super::{ReadPackRequest, ToolResult, CURSOR_VERSION};

pub(super) fn resolve_intent(request: &ReadPackRequest) -> ToolResult<ReadPackIntent> {
    let mut intent = request.intent.unwrap_or(ReadPackIntent::Auto);
    if !matches!(intent, ReadPackIntent::Auto) {
        return Ok(intent);
    }

    if let Some(cursor) = trimmed_non_empty_str(request.cursor.as_deref()) {
        let header: CursorHeader = decode_cursor(cursor)
            .map_err(|err| call_error("invalid_cursor", format!("Invalid cursor: {err}")))?;
        if header.v != CURSOR_VERSION {
            return Err(call_error(
                "invalid_cursor",
                "Invalid cursor: wrong version",
            ));
        }
        intent = match header.tool.as_str() {
            "cat" | "file_slice" => ReadPackIntent::File,
            "rg" | "grep" | "grep_context" => ReadPackIntent::Grep,
            "read_pack" => match header.mode.as_deref() {
                Some("recall") => ReadPackIntent::Recall,
                Some("memory") => ReadPackIntent::Memory,
                _ => {
                    return Err(call_error(
                        "invalid_cursor",
                        "Invalid cursor: unsupported read_pack cursor mode",
                    ))
                }
            },
            _ => {
                return Err(call_error(
                    "invalid_cursor",
                    "Invalid cursor: unsupported tool for read_pack",
                ))
            }
        };
        return Ok(intent);
    }

    fn looks_like_onboarding_prompt(text: &str) -> bool {
        let text = text.trim();
        if text.is_empty() {
            return false;
        }
        let lower = text.to_ascii_lowercase();

        // Prefer high-precision triggers. "How to" alone is too broad; require onboarding-ish
        // keywords that strongly correlate with repo orientation or setup/run instructions.
        let keywords = [
            "onboarding",
            "getting started",
            "quick start",
            "where to start",
            "repo structure",
            "project structure",
            "architecture",
            "entry point",
            "entrypoints",
            "how to run",
            "how do i run",
            "run tests",
            "how to test",
            "build and run",
            "setup",
            "install",
            "ci",
            "deploy",
            // Russian
            "онбординг",
            "с чего начать",
            "как запустить",
            "как собрать",
            "как установить",
            "как прогнать тест",
            "как запустить тест",
            "архитектура",
            "структура репозит",
            "точка входа",
        ];
        keywords.iter().any(|needle| lower.contains(needle))
    }

    let has_onboarding_signal = trimmed_non_empty_str(request.ask.as_deref())
        .is_some_and(looks_like_onboarding_prompt)
        || request.questions.as_ref().is_some_and(|qs| {
            qs.iter()
                .filter_map(|q| trimmed_non_empty_str(Some(q)))
                .any(looks_like_onboarding_prompt)
        })
        || trimmed_non_empty_str(request.query.as_deref())
            .is_some_and(looks_like_onboarding_prompt);
    if has_onboarding_signal {
        return Ok(ReadPackIntent::Onboarding);
    }

    if trimmed_non_empty_str(request.ask.as_deref()).is_some()
        || request
            .questions
            .as_ref()
            .is_some_and(|qs| qs.iter().any(|q| !q.trim().is_empty()))
    {
        return Ok(ReadPackIntent::Recall);
    }

    if trimmed_non_empty_str(request.query.as_deref()).is_some() {
        return Ok(ReadPackIntent::Query);
    }
    if trimmed_non_empty_str(request.pattern.as_deref()).is_some() {
        return Ok(ReadPackIntent::Grep);
    }
    if trimmed_non_empty_str(request.file.as_deref()).is_some() {
        return Ok(ReadPackIntent::File);
    }

    Ok(ReadPackIntent::Memory)
}

pub(super) fn intent_label(intent: ReadPackIntent) -> &'static str {
    match intent {
        ReadPackIntent::Auto => "auto",
        ReadPackIntent::File => "file",
        ReadPackIntent::Grep => "grep",
        ReadPackIntent::Query => "query",
        ReadPackIntent::Onboarding => "onboarding",
        ReadPackIntent::Memory => "memory",
        ReadPackIntent::Recall => "recall",
    }
}

pub(super) fn read_pack_intent_label(intent: ReadPackIntent) -> &'static str {
    match intent {
        ReadPackIntent::Auto => "auto",
        ReadPackIntent::File => "file",
        ReadPackIntent::Grep => "grep",
        ReadPackIntent::Query => "query",
        ReadPackIntent::Onboarding => "onboarding",
        ReadPackIntent::Memory => "memory",
        ReadPackIntent::Recall => "recall",
    }
}
