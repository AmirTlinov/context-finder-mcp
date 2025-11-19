use crate::error::Result;
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
    pub fn scan(&self) -> Result<Vec<PathBuf>> {
        let mut files = Vec::new();

        for result in WalkBuilder::new(&self.root).hidden(false).build() {
            match result {
                Ok(entry) => {
                    if entry.file_type().map_or(false, |ft| ft.is_file()) {
                        if Self::is_source_file(&entry.path()) {
                            files.push(entry.path().to_path_buf());
                        }
                    }
                }
                Err(e) => log::warn!("Failed to read entry: {}", e),
            }
        }

        log::info!("Found {} source files", files.len());
        Ok(files)
    }

    /// Check if file is a source code file
    fn is_source_file(path: &Path) -> bool {
        path.extension()
            .and_then(|ext| ext.to_str())
            .map(|ext| matches!(ext, "rs" | "py" | "js" | "ts" | "tsx" | "jsx"))
            .unwrap_or(false)
    }
}
