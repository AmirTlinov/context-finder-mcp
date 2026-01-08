use ignore::WalkBuilder;
use std::path::{Path, PathBuf};

/// Scanner for finding source files in a project
pub struct FileScanner {
    root: PathBuf,
}

/// Scan options for `FileScanner`.
///
/// Defaults are conservative and agent-friendly: skip secrets and most hidden files unless they
/// are explicitly allowlisted.
#[derive(Clone, Copy, Debug, Default)]
pub struct ScanOptions {
    /// When true, scan hidden (dot) files/directories beyond the hidden allowlist.
    ///
    /// Note: ignored scopes like `.git/` still remain excluded.
    pub allow_hidden: bool,
    /// When true, include potential secret files in the scan output.
    ///
    /// This is intended for opt-in workflows (e.g. explicit secret inspection) and should not be
    /// used for default indexing.
    pub allow_secrets: bool,
}

impl FileScanner {
    pub fn new(root: impl AsRef<Path>) -> Self {
        Self {
            root: root.as_ref().to_path_buf(),
        }
    }

    /// Filter an existing list of paths using the same rules as `scan_with_options`, without
    /// walking the filesystem tree.
    ///
    /// This is intended for watcher-driven incremental indexing where we already have candidate
    /// paths from FS events and want to avoid a full directory scan.
    #[must_use]
    pub fn filter_paths_with_options(
        &self,
        paths: &[PathBuf],
        options: ScanOptions,
    ) -> Vec<PathBuf> {
        let mut out = Vec::new();

        for path in paths {
            if !path.starts_with(&self.root) {
                continue;
            }

            if Self::is_ignored_scope(path, &self.root, options) {
                continue;
            }

            let Ok(meta) = std::fs::metadata(path) else {
                continue;
            };
            if !meta.is_file() {
                continue;
            }
            if meta.len() > MAX_FILE_SIZE_BYTES {
                continue;
            }

            if Self::is_noise_file(path) {
                continue;
            }

            let is_secret = Self::is_secret_file(path);
            if is_secret && !options.allow_secrets {
                continue;
            }

            let is_source = Self::is_source_file(path);
            if !(is_source || (options.allow_secrets && is_secret)) {
                continue;
            }

            out.push(path.clone());
        }

        out
    }

    /// Scan directory for source files (.gitignore aware)
    #[must_use]
    pub fn scan(&self) -> Vec<PathBuf> {
        self.scan_with_options(ScanOptions::default())
    }

    /// Scan directory with configurable inclusion options.
    #[must_use]
    pub fn scan_with_options(&self, options: ScanOptions) -> Vec<PathBuf> {
        let mut files = Vec::new();

        let root = self.root.clone();
        let mut builder = WalkBuilder::new(&self.root);
        builder
            .hidden(false) // we apply our own hidden allowlist (agent-friendly, safer defaults)
            .git_ignore(true)
            .git_global(true)
            .git_exclude(true);
        builder.filter_entry(move |entry| !Self::is_ignored_scope(entry.path(), &root, options));

        for result in builder.build() {
            match result {
                Ok(entry) => {
                    let Some(file_type) = entry.file_type() else {
                        continue;
                    };
                    if !file_type.is_file() {
                        continue;
                    }

                    let path = entry.path();
                    if let Ok(meta) = entry.metadata() {
                        if meta.len() > MAX_FILE_SIZE_BYTES {
                            log::debug!(
                                "Skipping large file {} ({} bytes > {})",
                                path.display(),
                                meta.len(),
                                MAX_FILE_SIZE_BYTES
                            );
                            continue;
                        }
                    }

                    if Self::is_noise_file(path) {
                        log::debug!("Skipping noisy artifact {}", path.display());
                        continue;
                    }

                    let is_secret = Self::is_secret_file(path);
                    if is_secret && !options.allow_secrets {
                        log::debug!("Skipping potential secret {}", path.display());
                        continue;
                    }

                    let is_source = Self::is_source_file(path);
                    if !(is_source || (options.allow_secrets && is_secret)) {
                        continue;
                    }

                    files.push(path.to_path_buf());
                }
                Err(e) => log::warn!("Failed to read entry: {e}"),
            }
        }

        log::info!("Found {} source files", files.len());
        files
    }

    /// Check if file is a source code file
    fn is_source_file(path: &Path) -> bool {
        if let Some(file_name) = path.file_name().and_then(|name| name.to_str()) {
            if matches!(
                file_name,
                "Dockerfile"
                    | "docker-compose.yml"
                    | "Makefile"
                    | "makefile"
                    | "Justfile"
                    | "JUSTFILE"
                    | "Gemfile"
            ) {
                return true;
            }

            // Safe env templates are useful to agents but don't use the `.env` extension.
            let lowered = file_name.to_lowercase();
            if ENV_TEMPLATE_FILE_NAMES.iter().any(|name| name == &lowered) {
                return true;
            }

            if HIDDEN_ALLOWLIST_FILES.iter().any(|name| name == &lowered) {
                return true;
            }

            if HIDDEN_ALLOWLIST_PREFIXES
                .iter()
                .any(|prefix| lowered.starts_with(prefix))
            {
                return true;
            }
        }

        if let Some(ext) = path.extension().and_then(|ext| ext.to_str()) {
            let ext = ext.to_lowercase();
            return SUPPORTED_EXTENSIONS
                .iter()
                .any(|candidate| candidate == &ext);
        }

        false
    }

    fn is_bench_logs_json(path: &Path) -> bool {
        let is_json = path
            .extension()
            .and_then(|ext| ext.to_str())
            .is_some_and(|ext| ext.eq_ignore_ascii_case("json"));
        if !is_json {
            return false;
        }

        let Some(parent) = path.parent() else {
            return false;
        };
        if !Self::component_matches(parent, "logs") {
            return false;
        }

        parent
            .parent()
            .is_some_and(|grand| Self::component_matches(grand, "bench"))
    }

    fn component_matches(path: &Path, target: &str) -> bool {
        path.file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.eq_ignore_ascii_case(target))
    }

    fn is_ignored_scope(path: &Path, root: &Path, options: ScanOptions) -> bool {
        if let Ok(relative) = path.strip_prefix(root) {
            for component in relative.components() {
                if let std::path::Component::Normal(name) = component {
                    let lowered = name.to_string_lossy().to_lowercase();
                    if IGNORED_SCOPES.iter().any(|ignored| ignored == &lowered) {
                        return true;
                    }

                    if lowered.starts_with('.')
                        && !options.allow_hidden
                        && !Self::is_allowlisted_hidden(&lowered)
                    {
                        return true;
                    }
                }
            }
        }
        false
    }

    pub(crate) fn is_allowlisted_hidden(name: &str) -> bool {
        HIDDEN_ALLOWLIST_FILES
            .iter()
            .any(|allowed| allowed == &name)
            || HIDDEN_ALLOWLIST_PREFIXES
                .iter()
                .any(|prefix| name.starts_with(prefix))
            || HIDDEN_ALLOWLIST_SCOPES
                .iter()
                .any(|allowed| allowed == &name)
            || ENV_TEMPLATE_FILE_NAMES
                .iter()
                .any(|allowed| allowed == &name)
    }

    pub(crate) fn is_noise_file(path: &Path) -> bool {
        if Self::is_bench_logs_json(path) {
            return true;
        }

        if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
            if NOISE_FILE_NAMES
                .iter()
                .any(|candidate| name.eq_ignore_ascii_case(candidate))
            {
                return true;
            }
        }

        false
    }

    pub(crate) fn is_secret_file(path: &Path) -> bool {
        let Some(file_name) = path.file_name().and_then(|n| n.to_str()) else {
            return false;
        };

        let lowered = file_name.to_lowercase();
        if SECRET_FILE_NAMES.iter().any(|name| name == &lowered) {
            return true;
        }

        // Cargo registry credentials can live under `.cargo/credentials(.toml)` and should never be indexed.
        if matches!(lowered.as_str(), "credentials" | "credentials.toml")
            && path
                .parent()
                .and_then(|p| p.file_name())
                .and_then(|n| n.to_str())
                .is_some_and(|parent| parent.eq_ignore_ascii_case(".cargo"))
        {
            return true;
        }

        if lowered.starts_with(".env.") && !ENV_TEMPLATE_FILE_NAMES.iter().any(|n| n == &lowered) {
            return true;
        }

        if let Some(ext) = path.extension().and_then(|ext| ext.to_str()) {
            let ext = ext.to_lowercase();
            if SECRET_EXTENSIONS.iter().any(|candidate| candidate == &ext) {
                return true;
            }
        }

        false
    }
}

pub(crate) const IGNORED_SCOPES: &[&str] = &[
    // VCS / tooling
    ".git",
    ".hg",
    ".svn",
    ".idea",
    ".changes",
    ".cursor",
    ".husky",
    ".yarn",
    ".npm",
    // caches / builds
    ".cache",
    "node_modules",
    ".next",
    ".turbo",
    ".parcel-cache",
    ".output",
    "build",
    "dist",
    "coverage",
    "storybook-static",
    "public",
    "static",
    "assets",
    ".nuxt",
    ".vite",
    ".vercel",
    ".svelte-kit",
    "logs",
    "tmp",
    "target",
    ".terraform",
    ".venv",
    "venorus-trash",
    ".context",
    ".context-finder",
    // data / vendor
    "datasets",
    "data",
    "vendor",
    "third_party",
    "third-party",
    "__pycache__",
];

const NOISE_FILE_NAMES: &[&str] = &[
    ".gitignore",
    ".gitmodules",
    "package-lock.json",
    "pnpm-lock.yaml",
    "yarn.lock",
    "makefile",
    "dockerfile",
    "docker-compose.yml",
];

/// Hidden scopes to index even when other dot-directories are skipped.
/// These are typically high-signal for agents (CI, build, devcontainer).
pub(crate) const HIDDEN_ALLOWLIST_SCOPES: &[&str] =
    &[".github", ".circleci", ".devcontainer", ".cargo", ".vscode"];

/// Hidden files that are commonly useful and non-secret.
const HIDDEN_ALLOWLIST_FILES: &[&str] = &[
    ".editorconfig",
    ".nvmrc",
    ".node-version",
    ".python-version",
    ".ruby-version",
    ".rust-toolchain",
    ".rust-toolchain.toml",
    ".tool-versions",
    ".gitlab-ci.yml",
    ".dockerignore",
    ".pre-commit-config.yaml",
    ".pre-commit-config.yml",
    ".golangci.yml",
    ".golangci.yaml",
    ".flake8",
    ".coveragerc",
    ".clang-format",
    ".clang-tidy",
    ".taplo.toml",
    ".ruff.toml",
    ".shellcheckrc",
    ".hadolint.yaml",
    ".hadolint.yml",
    ".yamllint",
    ".markdownlint.json",
    ".markdownlint.yaml",
    ".markdownlint.yml",
    ".sqlfluff",
];

/// Hidden config families that are broadly useful and typically non-secret.
/// Use prefixes to avoid enumerating every extension variant while keeping the allowlist explicit.
const HIDDEN_ALLOWLIST_PREFIXES: &[&str] = &[
    ".eslintrc",
    ".prettierrc",
    ".stylelintrc",
    ".commitlintrc",
    ".lintstagedrc",
    ".babelrc",
    ".renovaterc",
];

/// Safe env template conventions (useful to agents, typically non-secret).
/// Keep this list explicit: exclude `.env.local`, `.env.production`, etc.
const ENV_TEMPLATE_FILE_NAMES: &[&str] =
    &[".env.example", ".env.sample", ".env.template", ".env.dist"];

/// Conservative denylist to avoid indexing secrets by default.
const SECRET_FILE_NAMES: &[&str] = &[
    ".env",
    ".envrc",
    ".npmrc",
    ".pnpmrc",
    ".yarnrc",
    ".yarnrc.yml",
    ".pypirc",
    ".netrc",
    "id_rsa",
    "id_ed25519",
    "id_ecdsa",
    "id_dsa",
];

// Treat `*.env` as secret by default (projects often keep real secrets there).
// Safe templates like `.env.example` do not use the `.env` extension and remain allowlisted.
const SECRET_EXTENSIONS: &[&str] = &["pem", "key", "p12", "pfx", "env"];
const MAX_FILE_SIZE_BYTES: u64 = 1_048_576; // 1 MB

/// Broad set of extensions (code + docs + infra) to make the index maximally useful.
const SUPPORTED_EXTENSIONS: &[&str] = &[
    // General purpose languages
    "rs",
    "py",
    "pyw",
    "js",
    "mjs",
    "cjs",
    "ts",
    "tsx",
    "jsx",
    "java",
    "kt",
    "kts",
    "go",
    "c",
    "h",
    "cpp",
    "cc",
    "cxx",
    "hpp",
    "hh",
    "hxx",
    "cs",
    "rb",
    "swift",
    "php",
    "scala",
    "dart",
    "zig",
    "lua",
    "ex",
    "exs",
    "clj",
    "fs",
    "fsi",
    "el",
    // Scripts
    "sh",
    "bash",
    "zsh",
    "fish",
    "ps1",
    "bat",
    "cmd",
    // Docs
    "md",
    "mdx",
    "rst",
    "adoc",
    "txt",
    "context",
    // Config / data / infra
    "yaml",
    "yml",
    "json",
    "toml",
    "ini",
    "cfg",
    "conf",
    "properties",
    "env",
    "gradle",
    "groovy",
    "xml",
    "html",
    "css",
    "scss",
    "less",
    "sql",
    "dbml",
    "tf",
    "tfvars",
    "hcl",
    "dockerfile",
    "proto",
    "avsc",
    "pb",
];

#[cfg(test)]
mod tests {
    use super::FileScanner;
    use pretty_assertions::assert_eq;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn skips_bench_json_logs() {
        let temp = tempdir().unwrap();
        let bench_logs = temp.path().join("bench").join("logs");
        fs::create_dir_all(&bench_logs).unwrap();
        fs::write(bench_logs.join("empty.json"), b"").unwrap();
        fs::write(bench_logs.join("non_empty.json"), b"{\"ok\":true}").unwrap();
        fs::write(temp.path().join("main.rs"), b"fn main() {}").unwrap();

        let scanner = FileScanner::new(temp.path());
        let files = scanner.scan();

        assert!(files
            .iter()
            .all(|p| !p.to_string_lossy().contains("bench/logs")));
        assert!(files.iter().any(|p| p.ends_with("main.rs")));
        assert_eq!(files.len(), 1);
    }

    #[test]
    fn skips_ignored_directories() {
        let temp = tempdir().unwrap();
        let datasets_dir = temp.path().join("datasets").join("wip");
        fs::create_dir_all(&datasets_dir).unwrap();
        fs::write(datasets_dir.join("data.json"), b"{}").unwrap();
        fs::write(temp.path().join("src.rs"), b"fn main() {}").unwrap();
        fs::write(temp.path().join(".gitignore"), b"/datasets").unwrap();

        let scanner = FileScanner::new(temp.path());
        let files = scanner.scan();

        assert!(files
            .iter()
            .all(|p| !p.to_string_lossy().contains("datasets")));
        assert!(files.iter().any(|p| p.ends_with("src.rs")));
        assert!(files.iter().all(|p| !p.ends_with(".gitignore")));
    }

    #[test]
    fn indexes_selected_hidden_scopes_but_skips_secrets() {
        let temp = tempdir().unwrap();
        fs::create_dir_all(temp.path().join(".github").join("workflows")).unwrap();
        fs::write(
            temp.path().join(".github").join("workflows").join("ci.yml"),
            b"name: ci\non: push\n",
        )
        .unwrap();
        fs::write(temp.path().join(".nvmrc"), b"18\n").unwrap();
        fs::write(temp.path().join(".prettierrc"), br#"{"semi":false}"#).unwrap();
        fs::create_dir_all(temp.path().join(".cargo")).unwrap();
        fs::write(
            temp.path().join(".cargo").join("config.toml"),
            b"[build]\nrustflags=[]\n",
        )
        .unwrap();
        fs::write(
            temp.path().join(".cargo").join("credentials.toml"),
            b"[registries]\ncrates-io={token=\"SECRET\"}\n",
        )
        .unwrap();
        fs::write(temp.path().join(".env"), b"SECRET=1\n").unwrap();
        fs::write(temp.path().join(".env.example"), b"EXAMPLE=1\n").unwrap();
        fs::create_dir_all(temp.path().join(".context")).unwrap();
        fs::write(
            temp.path().join(".context").join("corpus.json"),
            b"{}",
        )
        .unwrap();

        let scanner = FileScanner::new(temp.path());
        let files = scanner.scan();

        assert!(files.iter().any(|p| p.ends_with("ci.yml")));
        assert!(files.iter().any(|p| p.ends_with(".nvmrc")));
        assert!(files.iter().any(|p| p.ends_with(".prettierrc")));
        assert!(files
            .iter()
            .any(|p| p.to_string_lossy().contains(".cargo/config.toml")));
        assert!(files
            .iter()
            .all(|p| !p.to_string_lossy().contains(".cargo/credentials")));
        assert!(files.iter().all(|p| !p.ends_with(".env")));
        assert!(files.iter().any(|p| p.ends_with(".env.example")));
        assert!(files
            .iter()
            .all(|p| !p.to_string_lossy().contains(".context")));
    }
}
