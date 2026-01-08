use std::path::Path;

/// Conservative denylist used to prevent accidental secret leakage in agent-facing read tools.
///
/// This intentionally matches the indexer's defaults: semantic indices skip these files, and
/// read-tools should also refuse (unless explicitly opted in).
///
/// The check is best-effort and filename-based; it does not attempt to classify arbitrary files.
pub(crate) fn is_potential_secret_path(candidate: &str) -> bool {
    let file_name = Path::new(candidate)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("")
        .to_lowercase();

    match file_name.as_str() {
        ".env" | ".envrc" | ".npmrc" | ".pnpmrc" | ".yarnrc" | ".yarnrc.yml" | ".pypirc"
        | ".netrc" | "id_rsa" | "id_ed25519" | "id_ecdsa" | "id_dsa" => return true,
        _ => {}
    }

    if file_name.starts_with(".env.") {
        // Allow only explicit, safe templates.
        match file_name.as_str() {
            ".env.example" | ".env.sample" | ".env.template" | ".env.dist" => {}
            _ => return true,
        }
    }

    let normalized = candidate.replace('\\', "/").to_lowercase();
    if normalized == ".cargo/credentials"
        || normalized == ".cargo/credentials.toml"
        || normalized.ends_with("/.cargo/credentials")
        || normalized.ends_with("/.cargo/credentials.toml")
    {
        return true;
    }

    let ext = Path::new(candidate)
        .extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or("")
        .to_lowercase();
    matches!(ext.as_str(), "pem" | "key" | "p12" | "pfx" | "env")
}

#[cfg(test)]
mod tests {
    use super::is_potential_secret_path;

    #[test]
    fn denies_common_secret_files() {
        for path in [
            ".env",
            ".env.local",
            "prod.env",
            ".npmrc",
            ".netrc",
            "id_rsa",
            "secrets/id_ed25519",
            "cert.pem",
            "keys/token.pfx",
            ".cargo/credentials",
        ] {
            assert!(is_potential_secret_path(path), "expected secret: {path}");
        }
    }

    #[test]
    fn allows_safe_env_templates() {
        for path in [".env.example", ".env.sample", ".env.template", ".env.dist"] {
            assert!(
                !is_potential_secret_path(path),
                "expected safe template: {path}"
            );
        }
    }
}
