/// Minimal `.context` document helpers.
///
/// The goal is agent-native, low-noise payloads: the output should be mostly *project content*.
pub(crate) struct ContextDocBuilder {
    out: String,
}

impl ContextDocBuilder {
    const QUOTE_PREFIX: &'static str = " ";

    #[must_use]
    pub(crate) fn new() -> Self {
        // Agent-native output: keep the payload dense.
        // `[LEGEND]` is provided via the `help` tool to avoid wasting budget on every call.
        let mut out = String::new();
        out.push_str("[CONTENT]\n");
        Self { out }
    }

    #[must_use]
    pub(crate) fn finish(self) -> String {
        self.out
    }

    /// Finish the document, ensuring the returned string is within `max_chars` (UTF-8 characters).
    ///
    /// This is a fail-soft guardrail: even under extremely small budgets we prefer returning a
    /// minimally useful (potentially truncated) `.context` document over failing the tool call.
    #[must_use]
    pub(crate) fn finish_bounded(self, max_chars: usize) -> (String, bool) {
        if max_chars == 0 {
            return (String::new(), true);
        }
        let raw = self.out;
        if raw.chars().count() <= max_chars {
            return (raw, false);
        }
        (crate::tools::util::truncate_to_chars(&raw, max_chars), true)
    }

    pub(crate) fn push_line(&mut self, line: &str) {
        self.out.push_str(line);
        self.out.push('\n');
    }

    pub(crate) fn push_blank(&mut self) {
        if !self.out.ends_with('\n') {
            self.out.push('\n');
        }
        self.out.push('\n');
    }

    pub(crate) fn push_answer(&mut self, text: &str) {
        self.push_line(&format!("A: {text}"));
    }

    pub(crate) fn push_note(&mut self, text: &str) {
        self.push_line(&format!("N: {text}"));
    }

    pub(crate) fn push_root_fingerprint(&mut self, root_fingerprint: Option<u64>) {
        if let Some(fp) = root_fingerprint {
            self.push_note(&format!("root_fingerprint={fp}"));
        }
    }

    pub(crate) fn push_ref_header(&mut self, file: &str, line: usize, label: Option<&str>) {
        match label {
            Some(label) if !label.trim().is_empty() => {
                self.push_line(&format!("R: {file}:{line} {label}"));
            }
            _ => {
                self.push_line(&format!("R: {file}:{line}"));
            }
        }
    }

    /// Append a continuation cursor as the *obvious* next step under truncation.
    pub(crate) fn push_cursor(&mut self, cursor: &str) {
        self.push_blank();
        self.push_note("next: call again with cursor");
        self.push_line(&format!("M: {cursor}"));
    }

    fn block_needs_quoting(block: &str) -> bool {
        block.lines().any(|line| {
            let trimmed = line.trim_start();
            trimmed.starts_with("[LEGEND]")
                || trimmed.starts_with("[CONTENT]")
                || trimmed.starts_with("A:")
                || trimmed.starts_with("N:")
                || trimmed.starts_with("R:")
                || trimmed.starts_with("M:")
        })
    }

    /// Push a multi-line block, keeping overhead minimal while still avoiding accidental clashes
    /// with the `.context` envelope markers (`A:`, `R:`, `[CONTENT]`, etc).
    pub(crate) fn push_block_smart(&mut self, block: &str) {
        // Quote only lines that would otherwise collide with envelope markers.
        // Quoting the whole block is safe but wastes budget; per-line quoting keeps payload dense.
        for line in block.lines() {
            let quote = Self::block_needs_quoting(line);
            if quote {
                self.out.push_str(Self::QUOTE_PREFIX);
            }
            self.out.push_str(line);
            self.out.push('\n');
        }
    }
}
