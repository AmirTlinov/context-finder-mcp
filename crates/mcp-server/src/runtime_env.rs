use rmcp::schemars;
use serde::Serialize;
use std::env;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use context_vector_store::gpu_env as vector_gpu_env;

const ORT_PROVIDER_SO: &str = "libonnxruntime_providers_cuda.so";

const CUBLAS_LT_CANDIDATES: &[&str] = &["libcublasLt.so.12", "libcublasLt.so.13"];
const CUBLAS_CANDIDATES: &[&str] = &["libcublas.so.12", "libcublas.so.13"];
const NVRTC_CANDIDATES: &[&str] = &["libnvrtc.so.12"];

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct GpuEnvReport {
    pub ort_lib_location: Option<String>,
    pub ld_library_path: Option<String>,
    pub provider_present: bool,
    pub cublas_present: bool,
    pub nvrtc_present: bool,
    pub provider_dir: Option<String>,
    pub cublas_dir: Option<String>,
    pub nvrtc_dir: Option<String>,
    pub searched_paths: Vec<String>,
}

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct BootstrapReport {
    pub repo_root: Option<String>,
    pub model_dir: Option<String>,
    pub applied_env: Vec<String>,
    pub warnings: Vec<String>,
    pub gpu: GpuEnvReport,
}

pub fn bootstrap_best_effort() -> BootstrapReport {
    let repo_root = infer_repo_root_best_effort();
    bootstrap_from_repo_root(repo_root.as_deref())
}

fn infer_repo_root_best_effort() -> Option<PathBuf> {
    if let Some(root) = env_root_override() {
        return Some(root);
    }

    if let Some(root) = env::current_exe()
        .ok()
        .as_deref()
        .and_then(infer_repo_root_from_exe)
    {
        return Some(root);
    }

    let cwd = env::current_dir().ok()?;
    find_context_finder_repo_root_from(&cwd)
}

fn env_root_override() -> Option<PathBuf> {
    for key in ["CONTEXT_ROOT", "CONTEXT_PROJECT_ROOT"] {
        if let Ok(value) = env::var(key) {
            let trimmed = value.trim();
            if !trimmed.is_empty() {
                return Some(PathBuf::from(trimmed));
            }
        }
    }
    None
}

fn is_context_finder_repo_root(path: &Path) -> bool {
    // Heuristic: only treat a working directory as the Context repo if it has the expected
    // workspace layout. This avoids "stealing" bootstrap away from global caches when the daemon
    // is started while working in an unrelated repo.
    path.join("Cargo.toml").exists()
        && path.join("crates").join("mcp-server").exists()
        && path.join("crates").join("cli").exists()
}

fn find_context_finder_repo_root_from(start: &Path) -> Option<PathBuf> {
    start
        .ancestors()
        .find(|candidate| is_context_finder_repo_root(candidate))
        .map(PathBuf::from)
}

fn bootstrap_from_repo_root(repo_root: Option<&Path>) -> BootstrapReport {
    let mut applied_env = Vec::new();
    let mut warnings = Vec::new();

    let repo_root = repo_root.map(Path::to_path_buf);

    // Model dir: optional, but helps avoid surprises when the MCP server is launched
    // from an arbitrary working directory.
    let model_dir = if env::var_os("CONTEXT_MODEL_DIR").is_none() {
        repo_root.as_deref().and_then(|root| {
            let candidate = root.join("models");
            if candidate.join("manifest.json").exists() {
                env::set_var("CONTEXT_MODEL_DIR", &candidate);
                applied_env.push("CONTEXT_MODEL_DIR".to_string());
                Some(candidate)
            } else {
                None
            }
        })
    } else {
        env::var("CONTEXT_MODEL_DIR").ok().map(PathBuf::from)
    };

    // GPU env: do best-effort bootstrap, but never fail server startup.
    if !is_cuda_disabled() {
        let before = diagnose_gpu_env();
        // `diagnose_gpu_env()` includes several "known places" (ORT cache, ~/.context deps)
        // even when the user didn't export env vars yet. That is great for diagnostics, but it
        // must not prevent us from actually setting `ORT_LIB_LOCATION` / `LD_LIBRARY_PATH` when
        // they're missing.
        let needs_env_bootstrap = !env_var_has_provider("ORT_LIB_LOCATION")
            || !before.provider_present
            || !before.cublas_present;

        if needs_env_bootstrap {
            if let Some(root) = repo_root.as_deref() {
                if let Err(err) = try_bootstrap_gpu_env_from_repo(root, &mut applied_env) {
                    warnings.push(err);
                }
            }
            // If repo-local bootstrap didn't apply (or we're not in the Context repo),
            // fall back to the global CUDA deps cache (and then ORT's own download cache).
            let after_repo = diagnose_gpu_env();
            let still_needs_env_bootstrap = !env_var_has_provider("ORT_LIB_LOCATION")
                || !after_repo.provider_present
                || !after_repo.cublas_present;
            if still_needs_env_bootstrap {
                if let Err(err) = try_bootstrap_gpu_env_from_global_cache(&mut applied_env) {
                    warnings.push(err);
                }
            }
        }
    }

    let gpu = diagnose_gpu_env();
    if !is_cuda_disabled() && (!gpu.provider_present || !gpu.cublas_present) {
        warnings.push("CUDA libraries are not fully configured (provider/cublas missing). Run `bash scripts/setup_cuda_deps.sh` in the Context repo or set ORT_LIB_LOCATION/LD_LIBRARY_PATH. If you want CPU fallback, set CONTEXT_ALLOW_CPU=1.".to_string());
    }

    BootstrapReport {
        repo_root: repo_root.as_ref().map(|p| display_path(p)),
        model_dir: model_dir.as_ref().map(|p| display_path(p)),
        applied_env,
        warnings,
        gpu,
    }
}

pub fn is_cuda_disabled() -> bool {
    env::var("ORT_DISABLE_CUDA")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
        || env::var("ORT_USE_CUDA")
            .map(|v| v == "0" || v.eq_ignore_ascii_case("false"))
            .unwrap_or(false)
}

fn infer_repo_root_from_exe(exe_path: &Path) -> Option<PathBuf> {
    let exe = exe_path
        .canonicalize()
        .unwrap_or_else(|_| exe_path.to_path_buf());
    let release_or_debug = exe.parent()?;
    let name = release_or_debug.file_name()?.to_string_lossy();
    if name != "release" && name != "debug" {
        return None;
    }
    let target = release_or_debug.parent()?;
    if target.file_name() != Some(OsStr::new("target")) {
        return None;
    }
    Some(target.parent()?.to_path_buf())
}

fn display_path(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

fn collect_env_paths() -> Vec<PathBuf> {
    let mut paths: Vec<PathBuf> = Vec::new();
    if let Ok(path) = env::var("ORT_LIB_LOCATION") {
        paths.push(PathBuf::from(path));
    }
    if let Ok(ld) = env::var("LD_LIBRARY_PATH") {
        paths.extend(ld.split(':').filter(|p| !p.is_empty()).map(PathBuf::from));
    }

    // Context installs may ship a self-contained ORT+CUDA deps bundle under ~/.context.
    // Include it in diagnostics even when the user didn't export env vars yet.
    if let Some(dir) = find_context_global_cuda_deps_dir() {
        paths.push(dir);
    }

    // ORT's own download cache (common on dev machines).
    if let Some(dir) = find_global_ort_cache_dir() {
        paths.push(dir);
    }

    // Probe common CUDA runtime locations even when the user doesn't explicitly export
    // LD_LIBRARY_PATH (helps keep doctor/diagnostics accurate).
    paths.extend(vector_gpu_env::non_system_cuda_lib_dirs());
    paths.extend(system_ld_default_dirs());

    dedup_existing_paths(paths)
}

fn find_first_with_file(paths: &[PathBuf], name: &str) -> Option<PathBuf> {
    paths.iter().find(|dir| dir.join(name).exists()).cloned()
}

fn find_first_with_any(paths: &[PathBuf], candidates: &[&str]) -> Option<PathBuf> {
    for name in candidates {
        if let Some(dir) = find_first_with_file(paths, name) {
            return Some(dir);
        }
    }
    None
}

pub fn diagnose_gpu_env() -> GpuEnvReport {
    let ort_lib_location = env::var("ORT_LIB_LOCATION").ok();
    let ld_library_path = env::var("LD_LIBRARY_PATH").ok();

    let paths = collect_env_paths();
    let provider_dir = find_first_with_file(&paths, ORT_PROVIDER_SO);
    let cublas_dir = find_first_with_any(&paths, CUBLAS_LT_CANDIDATES)
        .or_else(|| find_first_with_any(&paths, CUBLAS_CANDIDATES));
    let nvrtc_dir = find_first_with_any(&paths, NVRTC_CANDIDATES);

    GpuEnvReport {
        ort_lib_location,
        ld_library_path,
        provider_present: provider_dir.is_some(),
        cublas_present: cublas_dir.is_some(),
        nvrtc_present: nvrtc_dir.is_some(),
        provider_dir: provider_dir.as_ref().map(|p| display_path(p)),
        cublas_dir: cublas_dir.as_ref().map(|p| display_path(p)),
        nvrtc_dir: nvrtc_dir.as_ref().map(|p| display_path(p)),
        searched_paths: paths.iter().map(|p| display_path(p)).collect(),
    }
}

fn has_cuda_provider(dir: &Path) -> bool {
    dir.join(ORT_PROVIDER_SO).exists()
}

fn try_bootstrap_gpu_env_from_repo(
    root: &Path,
    applied_env: &mut Vec<String>,
) -> Result<(), String> {
    let deps = root.join(".deps").join("ort_cuda");
    let provider_dir = vector_gpu_env::repo_cuda_provider_dir(root);

    apply_gpu_env(provider_dir.as_deref(), Some(&deps), applied_env)
}

fn try_bootstrap_gpu_env_from_global_cache(applied_env: &mut Vec<String>) -> Result<(), String> {
    if let Some(dir) = find_context_global_cuda_deps_dir() {
        return apply_gpu_env(Some(&dir), Some(&dir), applied_env);
    }

    let dir = find_global_ort_cache_dir();
    apply_gpu_env(dir.as_deref(), None, applied_env)
}

fn find_context_global_cuda_deps_dir() -> Option<PathBuf> {
    let home = env::var("HOME").ok()?;
    let home = Path::new(&home);
    let preferred = home.join(".context").join("deps").join("ort_cuda");
    if preferred.join(ORT_PROVIDER_SO).exists() {
        return Some(preferred);
    }
    let legacy = home.join(".context-finder").join("deps").join("ort_cuda");
    if legacy.join(ORT_PROVIDER_SO).exists() {
        return Some(legacy);
    }
    None
}

fn find_global_ort_cache_dir() -> Option<PathBuf> {
    let cache_root = xdg_cache_home().or_else(|| {
        env::var("HOME")
            .ok()
            .map(|home| Path::new(&home).join(".cache"))
    })?;
    let root = cache_root
        .join("ort.pyke.io")
        .join("dfbin")
        .join("x86_64-unknown-linux-gnu");

    let entries = std::fs::read_dir(root).ok()?;
    for entry in entries.flatten() {
        let candidate = entry.path().join("onnxruntime").join("lib");
        if has_cuda_provider(&candidate) {
            return Some(candidate);
        }
    }
    None
}

fn apply_gpu_env(
    provider_dir: Option<&Path>,
    cuda_deps_dir: Option<&Path>,
    applied_env: &mut Vec<String>,
) -> Result<(), String> {
    let mut paths_to_prepend: Vec<PathBuf> = Vec::new();

    if let Some(dir) = cuda_deps_dir {
        if dir.exists() {
            paths_to_prepend.push(dir.to_path_buf());
        }
    }
    let provider_dir = match provider_dir {
        Some(dir) if dir.exists() => Some(dir),
        Some(dir) => {
            return Err(format!(
                "CUDA provider directory does not exist: {}",
                display_path(dir)
            ));
        }
        None => None,
    };

    if let Some(dir) = provider_dir {
        if !env_var_has_provider("ORT_LIB_LOCATION") {
            env::set_var("ORT_LIB_LOCATION", dir);
            applied_env.push("ORT_LIB_LOCATION".to_string());
        }
        if env::var_os("ORT_DYLIB_PATH").is_none() {
            env::set_var("ORT_DYLIB_PATH", dir);
            applied_env.push("ORT_DYLIB_PATH".to_string());
        }

        // Mirror the CLI behavior: prefer having the ORT provider dir on the dynamic loader path.
        paths_to_prepend.push(dir.to_path_buf());
    }

    // Add CUDA runtime library locations (pip wheels / toolkit) when present.
    paths_to_prepend.extend(vector_gpu_env::non_system_cuda_lib_dirs());

    if !paths_to_prepend.is_empty() {
        prepend_ld_library_path(&paths_to_prepend);
        applied_env.push("LD_LIBRARY_PATH".to_string());
    }

    if env::var_os("ORT_DISABLE_TENSORRT").is_none() {
        env::set_var("ORT_DISABLE_TENSORRT", "1");
        applied_env.push("ORT_DISABLE_TENSORRT".to_string());
    }
    if env::var_os("ORT_STRATEGY").is_none() {
        env::set_var("ORT_STRATEGY", "system");
        applied_env.push("ORT_STRATEGY".to_string());
    }
    if env::var_os("ORT_USE_CUDA").is_none() && env::var_os("ORT_DISABLE_CUDA").is_none() {
        env::set_var("ORT_USE_CUDA", "1");
        applied_env.push("ORT_USE_CUDA".to_string());
    }

    if provider_dir.is_none() {
        return Err(
            "CUDA provider directory not found; cannot bootstrap GPU environment".to_string(),
        );
    }

    Ok(())
}

fn env_var_has_provider(key: &str) -> bool {
    env::var_os(key).is_some_and(|val| PathBuf::from(val).join(ORT_PROVIDER_SO).exists())
}

fn prepend_ld_library_path(paths: &[PathBuf]) {
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut ordered: Vec<String> = Vec::new();

    for path in paths {
        if !path.exists() {
            continue;
        }
        let value = path.to_string_lossy().into_owned();
        if seen.insert(value.clone()) {
            ordered.push(value);
        }
    }

    if let Ok(existing) = env::var("LD_LIBRARY_PATH") {
        for part in existing.split(':').filter(|p| !p.is_empty()) {
            if seen.insert(part.to_string()) {
                ordered.push(part.to_string());
            }
        }
    }

    if !ordered.is_empty() {
        env::set_var("LD_LIBRARY_PATH", ordered.join(":"));
    }
}

fn xdg_cache_home() -> Option<PathBuf> {
    let Ok(value) = env::var("XDG_CACHE_HOME") else {
        return None;
    };
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(PathBuf::from(trimmed))
}

fn system_ld_default_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    for dir in ["/lib/x86_64-linux-gnu", "/usr/lib/x86_64-linux-gnu"] {
        let path = Path::new(dir);
        if path.exists() {
            dirs.push(path.to_path_buf());
        }
    }
    dirs
}

fn dedup_existing_paths(paths: Vec<PathBuf>) -> Vec<PathBuf> {
    let mut seen: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();
    let mut out = Vec::new();
    for path in paths {
        if !path.exists() {
            continue;
        }
        if seen.insert(path.clone()) {
            out.push(path);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::ENV_MUTEX;

    struct EnvGuard {
        saved: Vec<(String, Option<std::ffi::OsString>)>,
    }

    impl EnvGuard {
        fn new(keys: &[&str]) -> Self {
            let mut saved = Vec::new();
            for &key in keys {
                saved.push((key.to_string(), env::var_os(key)));
                env::remove_var(key);
            }
            Self { saved }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            for (key, value) in self.saved.drain(..) {
                match value {
                    Some(v) => env::set_var(&key, v),
                    None => env::remove_var(&key),
                }
            }
        }
    }

    struct CwdGuard {
        saved: PathBuf,
    }

    impl CwdGuard {
        fn new(path: &Path) -> Self {
            let saved = env::current_dir().expect("current_dir");
            env::set_current_dir(path).expect("set_current_dir");
            Self { saved }
        }
    }

    impl Drop for CwdGuard {
        fn drop(&mut self) {
            let _ = env::set_current_dir(&self.saved);
        }
    }

    fn bootstrap_for_test_repo(root: &Path) -> BootstrapReport {
        bootstrap_from_repo_root(Some(root))
    }

    #[test]
    fn bootstrap_sets_model_and_gpu_env_when_repo_layout_present() {
        let _lock = ENV_MUTEX.lock().expect("ENV_MUTEX");
        let _guard = EnvGuard::new(&[
            "CONTEXT_MODEL_DIR",
            "ORT_LIB_LOCATION",
            "ORT_DYLIB_PATH",
            "LD_LIBRARY_PATH",
            "ORT_DISABLE_TENSORRT",
            "ORT_STRATEGY",
            "ORT_USE_CUDA",
        ]);

        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();

        // models/manifest.json
        std::fs::create_dir_all(root.join("models")).unwrap();
        std::fs::write(root.join("models").join("manifest.json"), "{}").unwrap();

        // .deps/ort_cuda with expected filenames (empty files are enough for presence checks)
        let deps = root.join(".deps").join("ort_cuda");
        std::fs::create_dir_all(&deps).unwrap();
        std::fs::write(deps.join(ORT_PROVIDER_SO), "").unwrap();
        std::fs::write(deps.join("libcublasLt.so.12"), "").unwrap();

        let report = bootstrap_for_test_repo(root);
        assert!(report.model_dir.as_deref().unwrap().ends_with("/models"));
        assert_eq!(
            env::var("CONTEXT_MODEL_DIR").unwrap(),
            root.join("models").to_string_lossy()
        );

        assert!(report.gpu.provider_present);
        assert!(report.gpu.cublas_present);
        assert_eq!(
            env::var("ORT_LIB_LOCATION").unwrap(),
            deps.to_string_lossy()
        );
        let ld = env::var("LD_LIBRARY_PATH").unwrap_or_default();
        assert!(
            ld.contains(deps.to_string_lossy().as_ref()),
            "LD_LIBRARY_PATH did not include deps dir (ld={ld})"
        );
    }

    #[test]
    fn bootstrap_best_effort_uses_cwd_repo_root_when_installed_binary() {
        let _lock = ENV_MUTEX.lock().expect("ENV_MUTEX");
        let _guard = EnvGuard::new(&[
            "CONTEXT_ROOT",
            "CONTEXT_PROJECT_ROOT",
            "CONTEXT_MODEL_DIR",
            "ORT_LIB_LOCATION",
            "ORT_DYLIB_PATH",
            "LD_LIBRARY_PATH",
            "ORT_DISABLE_TENSORRT",
            "ORT_DISABLE_CUDA",
            "ORT_STRATEGY",
            "ORT_USE_CUDA",
        ]);

        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();

        std::fs::create_dir_all(root.join("crates").join("mcp-server")).expect("mkdir crates");
        std::fs::create_dir_all(root.join("crates").join("cli")).expect("mkdir crates");
        std::fs::write(root.join("Cargo.toml"), "[workspace]\n").expect("write Cargo.toml");

        let deps = root.join(".deps").join("ort_cuda");
        std::fs::create_dir_all(&deps).expect("mkdir deps");
        std::fs::write(deps.join(ORT_PROVIDER_SO), b"").expect("write provider");
        std::fs::write(deps.join("libcublasLt.so.12"), b"").expect("write cublaslt");

        let _cwd = CwdGuard::new(root);
        let report = bootstrap_best_effort();

        assert!(report.gpu.provider_present);
        assert!(report.gpu.cublas_present);
        assert!(
            report
                .warnings
                .iter()
                .all(|w| !w.contains("CUDA libraries are not fully configured")),
            "bootstrap should not emit CUDA missing warnings when deps are present"
        );
    }
}
