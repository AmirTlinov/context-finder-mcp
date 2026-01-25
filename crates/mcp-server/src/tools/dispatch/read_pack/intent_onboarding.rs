use super::super::{compute_repo_onboarding_pack_result, RepoOnboardingPackRequest};
use super::onboarding_command::{maybe_add_command_snippet, CommandSnippetParams};
use super::onboarding_docs::{
    append_onboarding_docs, fallback_onboarding_docs, OnboardingDocsBudget, OnboardingDocsParams,
};
use super::onboarding_topics::{classify_onboarding_topic, onboarding_prompt};
use super::{call_error, ReadPackContext, ReadPackRequest, ReadPackSection, ResponseMode};

pub(super) async fn handle_onboarding_intent(
    ctx: &ReadPackContext,
    request: &ReadPackRequest,
    response_mode: ResponseMode,
    facts: &super::ProjectFactsResult,
    sections: &mut Vec<ReadPackSection>,
) -> super::ToolResult<()> {
    let prompt = onboarding_prompt(request);
    let topic = classify_onboarding_topic(&prompt);

    if response_mode == ResponseMode::Full {
        let onboarding_request = RepoOnboardingPackRequest {
            path: Some(ctx.root_display.clone()),
            map_depth: None,
            map_limit: None,
            doc_paths: None,
            docs_limit: None,
            doc_max_lines: None,
            doc_max_chars: None,
            max_chars: Some(ctx.inner_max_chars),
            response_mode: None,
            auto_index: None,
            auto_index_budget_ms: None,
        };

        let pack =
            compute_repo_onboarding_pack_result(&ctx.root, &ctx.root_display, &onboarding_request)
                .await
                .map_err(|err| call_error("internal", format!("Error: {err:#}")))?;
        sections.push(ReadPackSection::RepoOnboardingPack {
            result: Box::new(pack),
        });
        return Ok(());
    }

    // Facts/minimal mode is `.context`-first. Avoid computing a full repo_onboarding_pack (map +
    // next_actions) just to emit a couple of doc snippets: produce a cheap, deterministic set of
    // anchors and (when the prompt is about running/building/testing) add a "command needle" via
    // bounded grep.
    let mut docs_budget = OnboardingDocsBudget::new(ctx, response_mode);
    let found_command = maybe_add_command_snippet(CommandSnippetParams {
        ctx,
        response_mode,
        topic,
        facts,
        sections,
    })
    .await?;
    if found_command {
        docs_budget.reduce_after_command();
    }

    let added = append_onboarding_docs(OnboardingDocsParams {
        ctx,
        response_mode,
        topic,
        budget: &docs_budget,
        sections,
    });

    if added == 0 {
        // Fallback: preserve the old behavior (structured pack conversion) so non-doc repos
        // still return something instead of an empty onboarding.
        fallback_onboarding_docs(ctx, response_mode, &docs_budget, sections).await?;
    }

    Ok(())
}
