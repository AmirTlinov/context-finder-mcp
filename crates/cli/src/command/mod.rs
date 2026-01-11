mod context;
pub mod domain;
mod freshness;
pub mod infra;
mod path_filters;
mod services;
pub mod warm;

#[allow(unused_imports)]
pub use domain::{
    classify_error, CommandAction, CommandRequest, CommandResponse, CommandStatus,
    ContextPackOutput, ContextPackPayload, EvalCacheMode, EvalCaseResult, EvalCompareCase,
    EvalCompareConfig, EvalCompareOutput, EvalComparePayload, EvalCompareSummary, EvalDatasetMeta,
    EvalHit, EvalOutput, EvalPayload, EvalRun, EvalRunSummary, EvalSummary, EvidenceFetchOutput,
    EvidenceFetchPayload, EvidencePointer, Hint, HintKind, IndexPayload, IndexResponse,
    ListSymbolsPayload, MapOutput, MapPayload, MeaningFocusPayload, MeaningPackOutput,
    MeaningPackPayload, ResponseMeta, SearchOutput, SearchPayload, SearchStrategy,
    SearchWithContextPayload, SymbolsOutput, TaskPackOutput, TaskPackPayload, TextSearchOutput,
    TextSearchPayload,
};

use crate::cache::CacheConfig;
use context_protocol::ErrorEnvelope;
use domain::CommandOutcome;
use services::Services;
use std::time::Instant;

pub struct CommandHandler {
    services: Services,
}

impl CommandHandler {
    pub fn new(cache_cfg: CacheConfig) -> Self {
        Self {
            services: Services::new(cache_cfg),
        }
    }

    pub async fn execute(&self, request: CommandRequest) -> CommandResponse {
        let started = Instant::now();
        let CommandRequest {
            action,
            payload,
            options,
            config,
        } = request;
        let payload_for_meta = payload.clone();

        let ctx = context::CommandContext::new(config, options);
        let request_options = ctx.request_options();
        let attach_index_state_fallback = true;

        let mut guard_index_state = None;
        let mut guard_hints = Vec::new();
        let mut guard_index_updated = false;

        if freshness::action_requires_index(&action) {
            match ctx
                .resolve_project(freshness::extract_project_path(&payload))
                .await
            {
                Ok(project_ctx) => {
                    match freshness::enforce_stale_policy(
                        &project_ctx.root,
                        &project_ctx.profile_name,
                        &project_ctx.profile,
                        &request_options,
                    )
                    .await
                    {
                        Ok(Ok(gate)) => {
                            guard_index_state = Some(gate.index_state);
                            guard_hints.extend(gate.hints);
                            guard_index_updated |= gate.index_updated;
                        }
                        Ok(Err(block)) => {
                            let meta = ResponseMeta {
                                config_path: project_ctx.config_path,
                                profile: Some(project_ctx.profile_name),
                                profile_path: project_ctx.profile_path,
                                index_state: Some(block.index_state),
                                index_updated: Some(false),
                                duration_ms: Some(started.elapsed().as_millis() as u64),
                                ..Default::default()
                            };

                            let classification = classify_error(
                                &block.message,
                                Some(action),
                                Some(&payload_for_meta),
                            );
                            let mut hints = classification.hints;
                            hints.extend(block.hints);
                            hints.extend(project_ctx.hints);
                            let hint = classification
                                .hint
                                .or_else(|| hints.first().map(|h| h.text.clone()));

                            let error = ErrorEnvelope {
                                code: classification.code,
                                message: block.message.clone(),
                                details: None,
                                hint,
                                next_actions: classification.next_actions.clone(),
                            };

                            return CommandResponse {
                                status: CommandStatus::Error,
                                message: Some(block.message),
                                error: Some(error),
                                hints,
                                next_actions: classification.next_actions,
                                data: serde_json::Value::Null,
                                meta,
                            };
                        }
                        Err(err) => {
                            return error_response(err, started.elapsed().as_millis() as u64);
                        }
                    }
                }
                Err(err) => {
                    return error_response(err, started.elapsed().as_millis() as u64);
                }
            }
        }

        let outcome: std::result::Result<CommandOutcome, anyhow::Error> =
            self.services.route(action, payload, &ctx).await;

        let mut response = match outcome {
            Ok(mut outcome) => {
                if guard_index_updated {
                    outcome.meta.index_updated = Some(true);
                }
                outcome.hints.extend(guard_hints);
                if outcome.meta.index_state.is_none() {
                    outcome.meta.index_state = guard_index_state;
                }
                outcome.meta.duration_ms = outcome
                    .meta
                    .duration_ms
                    .or_else(|| Some(started.elapsed().as_millis() as u64));

                CommandResponse {
                    status: CommandStatus::Ok,
                    message: None,
                    error: None,
                    hints: outcome.hints,
                    next_actions: outcome.next_actions,
                    data: outcome.data,
                    meta: outcome.meta,
                }
            }
            Err(err) => {
                let message = format!("{err:#}");
                let classification =
                    classify_error(&message, Some(action), Some(&payload_for_meta));
                let mut hints = classification.hints;
                if guard_index_updated {
                    hints.push(Hint {
                        kind: HintKind::Cache,
                        text: "Auto reindex completed (stale_policy=auto)".to_string(),
                    });
                }
                hints.extend(guard_hints);
                let hint = classification
                    .hint
                    .or_else(|| hints.first().map(|h| h.text.clone()));
                let error = ErrorEnvelope {
                    code: classification.code,
                    message: message.clone(),
                    details: None,
                    hint,
                    next_actions: classification.next_actions.clone(),
                };
                let meta = ResponseMeta {
                    duration_ms: Some(started.elapsed().as_millis() as u64),
                    index_state: guard_index_state,
                    ..Default::default()
                };

                CommandResponse {
                    status: CommandStatus::Error,
                    message: Some(message),
                    error: Some(error),
                    hints,
                    next_actions: classification.next_actions,
                    data: serde_json::Value::Null,
                    meta,
                }
            }
        };

        if response.meta.index_state.is_none() && attach_index_state_fallback {
            if let Ok(project_ctx) = ctx
                .resolve_project(freshness::extract_project_path(&payload_for_meta))
                .await
            {
                if let Ok(state) =
                    freshness::gather_index_state(&project_ctx.root, &project_ctx.profile_name)
                        .await
                {
                    response.meta.index_state = Some(state);
                }
            };
        }

        response
    }
}

fn error_response(err: anyhow::Error, duration_ms: u64) -> CommandResponse {
    let message = format!("{err:#}");
    let classification = classify_error(&message, None, None);
    let hints = classification.hints;
    let hint = classification
        .hint
        .or_else(|| hints.first().map(|h| h.text.clone()));
    let error = ErrorEnvelope {
        code: classification.code,
        message: message.clone(),
        details: None,
        hint,
        next_actions: classification.next_actions.clone(),
    };
    CommandResponse {
        status: CommandStatus::Error,
        message: Some(message),
        error: Some(error),
        hints,
        next_actions: classification.next_actions,
        data: serde_json::Value::Null,
        meta: ResponseMeta {
            duration_ms: Some(duration_ms),
            ..Default::default()
        },
    }
}

pub async fn execute(request: CommandRequest, cache_cfg: CacheConfig) -> CommandResponse {
    CommandHandler::new(cache_cfg).execute(request).await
}
