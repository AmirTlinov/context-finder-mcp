pub fn is_active(
    include_paths: &[String],
    exclude_paths: &[String],
    file_pattern: Option<&str>,
) -> bool {
    include_paths
        .iter()
        .any(|p| !normalize_filter_path(p).is_empty())
        || exclude_paths
            .iter()
            .any(|p| !normalize_filter_path(p).is_empty())
        || file_pattern.map(str::trim).is_some_and(|p| !p.is_empty())
}

pub fn path_allowed(
    rel_path: &str,
    include_paths: &[String],
    exclude_paths: &[String],
    file_pattern: Option<&str>,
) -> bool {
    let rel_path = rel_path.replace('\\', "/");

    if !include_paths.is_empty() {
        let mut has_valid_include = false;
        let mut matched = false;
        for include in include_paths {
            let normalized = normalize_filter_path(include);
            if normalized.is_empty() {
                continue;
            }
            has_valid_include = true;
            if path_prefix_matches_normalized(&normalized, &rel_path) {
                matched = true;
                break;
            }
        }
        if has_valid_include && !matched {
            return false;
        }
    }

    for exclude in exclude_paths {
        let normalized = normalize_filter_path(exclude);
        if normalized.is_empty() {
            continue;
        }
        if path_prefix_matches_normalized(&normalized, &rel_path) {
            return false;
        }
    }

    matches_file_pattern(
        &rel_path,
        file_pattern.map(str::trim).filter(|p| !p.is_empty()),
    )
}

fn normalize_filter_path(raw: &str) -> String {
    let mut value = raw.trim().replace('\\', "/");
    while value.starts_with("./") {
        value = value[2..].to_string();
    }
    let value = value.trim_matches('/');
    if value == "." {
        return String::new();
    }
    value.to_string()
}

fn path_prefix_matches_normalized(prefix: &str, path: &str) -> bool {
    if path == prefix {
        return true;
    }

    if !path.starts_with(prefix) {
        return false;
    }

    path.as_bytes().get(prefix.len()) == Some(&b'/')
}

fn matches_file_pattern(path: &str, pattern: Option<&str>) -> bool {
    let Some(pattern) = pattern else {
        return true;
    };

    if !pattern.contains('*') && !pattern.contains('?') {
        return path.contains(pattern);
    }

    glob::Pattern::new(pattern)
        .map(|p| p.matches(path))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn include_paths_is_prefix_match() {
        let include = vec!["src".to_string()];
        let exclude: Vec<String> = Vec::new();
        assert!(path_allowed("src/lib.rs", &include, &exclude, None));
        assert!(!path_allowed("src2/lib.rs", &include, &exclude, None));
        assert!(!path_allowed("docs/README.md", &include, &exclude, None));
    }

    #[test]
    fn exclude_paths_wins() {
        let include = vec!["src".to_string()];
        let exclude = vec!["src/gen".to_string()];
        assert!(path_allowed("src/lib.rs", &include, &exclude, None));
        assert!(!path_allowed("src/gen/mod.rs", &include, &exclude, None));
    }

    #[test]
    fn file_pattern_supports_substring_and_glob() {
        let include: Vec<String> = Vec::new();
        let exclude: Vec<String> = Vec::new();
        assert!(path_allowed(
            "src/lib.rs",
            &include,
            &exclude,
            Some("lib.rs")
        ));
        assert!(!path_allowed(
            "src/main.rs",
            &include,
            &exclude,
            Some("lib.rs")
        ));
        assert!(path_allowed(
            "src/lib.rs",
            &include,
            &exclude,
            Some("src/*.rs")
        ));
        assert!(!path_allowed(
            "src/lib.ts",
            &include,
            &exclude,
            Some("src/*.rs")
        ));
    }

    #[test]
    fn invalid_prefixes_do_not_activate_filters() {
        let include = vec![
            "".to_string(),
            ".".to_string(),
            "./".to_string(),
            "/".to_string(),
        ];
        let exclude = vec!["".to_string(), ".".to_string(), "////".to_string()];
        assert!(!is_active(&include, &exclude, None));
        assert!(path_allowed("src/lib.rs", &include, &exclude, None));
        assert!(path_allowed("docs/README.md", &include, &exclude, None));
    }
}
