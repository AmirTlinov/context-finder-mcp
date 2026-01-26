use super::super::intent_file::handle_file_intent;
use super::super::intent_grep::handle_grep_intent;
use super::super::intent_memory::handle_memory_intent;
use super::super::intent_onboarding::handle_onboarding_intent;
use super::super::intent_query::{handle_query_intent, QueryIntentPolicy};
use super::super::intent_recall::handle_recall_intent;
use super::super::{
    ContextFinderService, ProjectFactsResult, ReadPackContext, ReadPackIntent, ReadPackNextAction,
    ReadPackRequest, ReadPackSection, ResponseMode, ToolResult,
};

pub(super) struct DispatchIntentParams<'a> {
    pub(super) service: &'a ContextFinderService,
    pub(super) ctx: &'a ReadPackContext,
    pub(super) request: &'a ReadPackRequest,
    pub(super) response_mode: ResponseMode,
    pub(super) intent: ReadPackIntent,
    pub(super) allow_secrets: bool,
    pub(super) semantic_index_fresh: bool,
    pub(super) facts: &'a ProjectFactsResult,
    pub(super) sections: &'a mut Vec<ReadPackSection>,
    pub(super) next_actions: &'a mut Vec<ReadPackNextAction>,
    pub(super) next_cursor: &'a mut Option<String>,
}

pub(super) async fn dispatch_intent(params: DispatchIntentParams<'_>) -> ToolResult<()> {
    match params.intent {
        ReadPackIntent::Auto => unreachable!("auto intent resolved above"),
        ReadPackIntent::File => {
            handle_file_intent(
                params.service,
                params.ctx,
                params.request,
                params.response_mode,
                params.sections,
                params.next_actions,
                params.next_cursor,
            )
            .await
        }
        ReadPackIntent::Grep => {
            handle_grep_intent(
                params.service,
                params.ctx,
                params.request,
                params.response_mode,
                params.sections,
                params.next_actions,
                params.next_cursor,
            )
            .await
        }
        ReadPackIntent::Query => {
            handle_query_intent(
                params.service,
                params.ctx,
                params.request,
                params.response_mode,
                QueryIntentPolicy {
                    allow_secrets: params.allow_secrets,
                },
                params.sections,
            )
            .await
        }
        ReadPackIntent::Onboarding => {
            handle_onboarding_intent(
                params.ctx,
                params.request,
                params.response_mode,
                params.facts,
                params.sections,
            )
            .await
        }
        ReadPackIntent::Memory => {
            handle_memory_intent(
                params.service,
                params.ctx,
                params.request,
                params.response_mode,
                params.sections,
                params.next_actions,
                params.next_cursor,
            )
            .await
        }
        ReadPackIntent::Recall => {
            handle_recall_intent(
                params.service,
                params.ctx,
                params.request,
                params.response_mode,
                params.semantic_index_fresh,
                params.sections,
                params.next_cursor,
            )
            .await
        }
    }
}
