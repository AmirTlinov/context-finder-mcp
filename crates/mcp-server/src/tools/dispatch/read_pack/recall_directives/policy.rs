#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(in crate::tools::dispatch::read_pack) enum RecallQuestionMode {
    #[default]
    Auto,
    Fast,
    Deep,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::tools::dispatch::read_pack) struct RecallQuestionPolicy {
    pub(in crate::tools::dispatch::read_pack) allow_semantic: bool,
}

pub(in crate::tools::dispatch::read_pack) fn recall_question_policy(
    mode: RecallQuestionMode,
    semantic_index_fresh: bool,
) -> RecallQuestionPolicy {
    let allow_semantic = match mode {
        RecallQuestionMode::Fast => false,
        RecallQuestionMode::Deep => true,
        RecallQuestionMode::Auto => semantic_index_fresh,
    };

    RecallQuestionPolicy { allow_semantic }
}

pub(in crate::tools::dispatch::read_pack) fn build_semantic_query(
    question: &str,
    topics: Option<&Vec<String>>,
) -> String {
    let Some(topics) = topics else {
        return question.to_string();
    };
    if topics.is_empty() {
        return question.to_string();
    }

    let joined = topics.join(", ");
    format!("{question}\n\nTopics: {joined}")
}
