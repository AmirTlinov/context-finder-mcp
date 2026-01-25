use super::super::candidates::DEFAULT_MEMORY_FILE_CANDIDATES;
use super::super::{ReadPackContext, ResponseMode};

pub(super) struct MemoryDocBudget {
    pub docs_limit: usize,
    pub doc_max_chars: usize,
    pub doc_max_lines: usize,
    pub focus_reserved_chars: usize,
}

impl MemoryDocBudget {
    pub(super) fn new(
        ctx: &ReadPackContext,
        response_mode: ResponseMode,
        wants_entrypoint: bool,
        wants_focus_file: bool,
    ) -> Self {
        let entry_reserved_chars = if wants_entrypoint {
            (ctx.inner_max_chars / 8)
                .clamp(240, 3_000)
                .min(ctx.inner_max_chars.saturating_sub(200))
        } else {
            0
        };
        let focus_reserved_chars = if wants_focus_file {
            (ctx.inner_max_chars / 10)
                .clamp(200, 1_500)
                .min(ctx.inner_max_chars.saturating_sub(200))
        } else {
            0
        };

        let docs_budget_chars = ctx
            .inner_max_chars
            .saturating_sub(entry_reserved_chars)
            .saturating_sub(focus_reserved_chars);

        // Budgeting heuristic (agent-native):
        // - under tight budgets, prefer fewer, larger snippets (more useful than many tiny 200-char peeks)
        // - under larger budgets, expand up to a small cap to keep "memory pack" dense but broad
        //
        // The target size is intentionally coarse and deterministic: it keeps behavior stable across
        // runs and projects, while still letting callers steer results by adjusting `max_chars`.
        const MEMORY_DOC_TARGET_CHARS: usize = 800;
        let docs_limit = ((docs_budget_chars.saturating_add(MEMORY_DOC_TARGET_CHARS - 1))
            / MEMORY_DOC_TARGET_CHARS)
            .clamp(1, 6)
            .min(DEFAULT_MEMORY_FILE_CANDIDATES.len());
        let mut doc_max_chars = (docs_budget_chars / docs_limit.max(1))
            .clamp(160, 6_000)
            .min(ctx.inner_max_chars);
        if ctx.max_chars <= 1_200 {
            // Under very small budgets, prefer a smaller snippet payload so we can keep at least one
            // snippet alongside `project_facts` without popping sections during trimming.
            doc_max_chars = doc_max_chars.clamp(160, 320);
        } else if response_mode != ResponseMode::Full {
            // In low-noise modes, snippets are returned inline in the `read_pack` JSON payload.
            // JSON escaping and per-section key overhead can exceed the envelope headroom estimate
            // under small budgets, causing the final trimming pass to drop an entire snippet.
            //
            // Agent-native behavior: prefer slightly smaller snippets so the pack more often fits
            // 2+ "must-have" sections (e.g. AGENTS + README) instead of losing one to trimming.
            let (num, den) = if ctx.max_chars <= 2_500 {
                (2usize, 3usize) // tighter budgets need more headroom
            } else if ctx.max_chars <= 5_000 {
                (3usize, 4usize)
            } else {
                (4usize, 5usize)
            };
            doc_max_chars = (doc_max_chars.saturating_mul(num) / den)
                .clamp(160, 6_000)
                .min(ctx.inner_max_chars);
        }

        Self {
            docs_limit,
            doc_max_chars,
            doc_max_lines: 180,
            focus_reserved_chars,
        }
    }
}
