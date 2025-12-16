use ignore::WalkBuilder;
use std::path::{Path, PathBuf};

/// Scanner for finding source files in a project
pub struct FileScanner {
    root: PathBuf,
}

impl FileScanner {
    pub fn new(root: impl AsRef<Path>) -> Self {
        Self {
            root: root.as_ref().to_path_buf(),
        }
    }

    /// Scan directory for source files (.gitignore aware)
    pub fn scan(&self) -> Vec<PathBuf> {
        let mut files = Vec::new();

        let root = self.root.clone();
        let mut builder = WalkBuilder::new(&self.root);
        builder
            .hidden(true) // do not index hidden files by default
            .git_ignore(true)
            .git_global(true)
            .git_exclude(true);
        builder.filter_entry(move |entry| !FileScanner::is_ignored_scope(entry.path(), &root));

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

                    if !Self::is_source_file(path) {
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
            .map(|ext| ext.eq_ignore_ascii_case("json"))
            .unwrap_or(false);
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
            .map(|grand| Self::component_matches(grand, "bench"))
            .unwrap_or(false)
    }

    fn component_matches(path: &Path, target: &str) -> bool {
        path.file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.eq_ignore_ascii_case(target))
    }

    fn is_ignored_scope(path: &Path, root: &Path) -> bool {
        if let Ok(relative) = path.strip_prefix(root) {
            for component in relative.components() {
                if let std::path::Component::Normal(name) = component {
                    let lowered = name.to_string_lossy().to_lowercase();
                    if IGNORED_SCOPES.iter().any(|ignored| ignored == &lowered) {
                        return true;
                    }
                }
            }
        }
        false
    }

    fn is_noise_file(path: &Path) -> bool {
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
}

const IGNORED_SCOPES: &[&str] = &[
    // VCS / tooling
    ".git",
    ".hg",
    ".svn",
    ".idea",
    ".vscode",
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
}
