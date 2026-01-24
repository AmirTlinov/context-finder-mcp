use std::collections::{HashSet, VecDeque};
use std::path::PathBuf;

#[derive(Default)]
pub(in crate::tools::dispatch) struct SessionDefaults {
    /// Whether this connection completed an MCP `initialize` handshake in the current process.
    ///
    /// Some clients can reuse a shared-daemon transport across working directories and (buggily)
    /// issue tool calls without re-initializing. In daemon mode we fail-closed: do not persist or
    /// reuse session roots unless initialize has run.
    initialized: bool,
    root: Option<PathBuf>,
    root_display: Option<String>,
    focus_file: Option<String>,
    roots_pending: bool,
    /// Whether MCP `roots/list` returned multiple viable workspace roots and we refused to guess.
    ///
    /// In this state, callers must pass an explicit `path` (or an env override) to disambiguate.
    mcp_roots_ambiguous: bool,

    /// Canonical workspace roots reported by MCP `roots/list`.
    ///
    /// When non-empty, resolved roots must be within one of these directories.
    mcp_workspace_roots: Vec<PathBuf>,

    /// Fail-closed: when we detect that the session root is outside the MCP workspace roots,
    /// we record an error and refuse to serve requests without an explicit `path`.
    root_mismatch_error: Option<String>,
    // Working-set: ephemeral, per-connection state (no disk). Used to avoid repeating the same
    // anchors/snippets across multiple calls in one agent session.
    seen_snippet_files: VecDeque<String>,
    seen_snippet_files_set: HashSet<String>,
}

impl SessionDefaults {
    pub(in crate::tools::dispatch) fn initialized(&self) -> bool {
        self.initialized
    }

    pub(in crate::tools::dispatch) fn roots_pending(&self) -> bool {
        self.roots_pending
    }

    pub(in crate::tools::dispatch) fn set_roots_pending(&mut self, pending: bool) {
        self.roots_pending = pending;
    }

    pub(in crate::tools::dispatch) fn mcp_roots_ambiguous(&self) -> bool {
        self.mcp_roots_ambiguous
    }

    pub(in crate::tools::dispatch) fn set_mcp_roots_ambiguous(&mut self, value: bool) {
        self.mcp_roots_ambiguous = value;
    }

    pub(in crate::tools::dispatch) fn set_mcp_workspace_roots(&mut self, roots: Vec<PathBuf>) {
        self.mcp_workspace_roots = roots;
    }

    pub(in crate::tools::dispatch) fn mcp_workspace_roots(&self) -> &[PathBuf] {
        &self.mcp_workspace_roots
    }

    pub(in crate::tools::dispatch) fn root_allowed_by_workspace(
        &self,
        root: &std::path::Path,
    ) -> bool {
        if self.mcp_workspace_roots.is_empty() {
            return true;
        }
        self.mcp_workspace_roots
            .iter()
            .any(|candidate| root.starts_with(candidate))
    }

    pub(in crate::tools::dispatch) fn root_mismatch_error(&self) -> Option<&str> {
        self.root_mismatch_error.as_deref()
    }

    pub(in crate::tools::dispatch) fn set_root_mismatch_error(&mut self, message: String) {
        if self.root_mismatch_error.is_none() {
            self.root_mismatch_error = Some(message);
        }
    }

    pub(in crate::tools::dispatch) fn clone_root(&self) -> Option<(PathBuf, String)> {
        Some((self.root.clone()?, self.root_display.clone()?))
    }

    pub(in crate::tools::dispatch) fn root_display(&self) -> Option<String> {
        self.root_display.clone()
    }

    pub(in crate::tools::dispatch) fn focus_file(&self) -> Option<String> {
        self.focus_file.clone()
    }

    pub(in crate::tools::dispatch) fn seen_snippet_files_set_snapshot(&self) -> HashSet<String> {
        self.seen_snippet_files_set.clone()
    }

    pub(in crate::tools::dispatch) fn reset_for_initialize(&mut self, roots_pending: bool) {
        self.initialized = true;
        self.root = None;
        self.root_display = None;
        self.focus_file = None;
        self.roots_pending = roots_pending;
        self.mcp_roots_ambiguous = false;
        self.mcp_workspace_roots.clear();
        self.root_mismatch_error = None;
        self.clear_working_set();
    }

    pub(in crate::tools::dispatch) fn set_root(
        &mut self,
        root: PathBuf,
        root_display: String,
        focus_file: Option<String>,
    ) {
        let root_changed = match self.root.as_ref() {
            Some(prev) => prev != &root,
            None => true,
        };
        self.root = Some(root);
        self.root_display = Some(root_display);
        self.focus_file = focus_file;
        self.mcp_roots_ambiguous = false;
        self.root_mismatch_error = None;
        if root_changed {
            self.clear_working_set();
        }
    }

    fn clear_working_set(&mut self) {
        self.seen_snippet_files.clear();
        self.seen_snippet_files_set.clear();
    }

    pub(in crate::tools::dispatch) fn note_seen_snippet_file(&mut self, file: &str) {
        const MAX_SEEN: usize = 160;

        let trimmed = file.trim();
        if trimmed.is_empty() {
            return;
        }
        if !self.seen_snippet_files_set.insert(trimmed.to_string()) {
            return;
        }
        self.seen_snippet_files.push_back(trimmed.to_string());
        while self.seen_snippet_files.len() > MAX_SEEN {
            if let Some(old) = self.seen_snippet_files.pop_front() {
                self.seen_snippet_files_set.remove(&old);
            }
        }
    }
}

pub(in crate::tools::dispatch) fn trimmed_non_empty(input: Option<&str>) -> Option<&str> {
    input.map(str::trim).filter(|value| !value.is_empty())
}
