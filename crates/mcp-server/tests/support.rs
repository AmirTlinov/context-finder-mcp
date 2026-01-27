use anyhow::{Context, Result};
use std::path::PathBuf;

pub fn locate_context_mcp_bin() -> Result<PathBuf> {
    // Prefer the primary binary.
    if let Some(path) = option_env!("CARGO_BIN_EXE_context-mcp") {
        return Ok(PathBuf::from(path));
    }
    // Back-compat for older build invocations.
    if let Some(path) = option_env!("CARGO_BIN_EXE_context-finder-mcp") {
        return Ok(PathBuf::from(path));
    }

    // Try to resolve from the current test executable location.
    if let Ok(exe) = std::env::current_exe() {
        if let Some(target_profile_dir) = exe.parent().and_then(|p| p.parent()) {
            for name in ["context-mcp", "context-finder-mcp"] {
                let candidate = target_profile_dir.join(name);
                if candidate.exists() {
                    return Ok(candidate);
                }
            }
        }
    }

    // Final fallback: search the repo target dirs.
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let repo_root = manifest_dir
        .ancestors()
        .nth(2)
        .context("failed to resolve repo root from CARGO_MANIFEST_DIR")?;
    for rel in [
        "target/debug/context-mcp",
        "target/debug/context-finder-mcp",
        "target/release/context-mcp",
        "target/release/context-finder-mcp",
    ] {
        let candidate = repo_root.join(rel);
        if candidate.exists() {
            return Ok(candidate);
        }
    }

    anyhow::bail!(
        "failed to locate context-mcp binary (or legacy context-finder-mcp); build with: cargo build -p context-mcp --bin context-mcp"
    )
}
