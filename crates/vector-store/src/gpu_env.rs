use std::env;
use crate::paths::{CONTEXT_DIR_NAME, LEGACY_CONTEXT_DIR_NAME};
use std::path::{Path, PathBuf};

const ORT_PROVIDER_SO: &str = "libonnxruntime_providers_cuda.so";

const CUBLAS_LT_CANDIDATES: &[&str] = &["libcublasLt.so.12", "libcublasLt.so.13"];
const CUBLAS_CANDIDATES: &[&str] = &["libcublas.so.12", "libcublas.so.13"];
#[derive(Debug, Clone)]
pub struct GpuEnvReport {
    pub provider_present: bool,
    pub cublas_present: bool,
}

pub fn bootstrap_cuda_env_best_effort() -> GpuEnvReport {
    // Keep behavior deterministic: do not override explicit user configuration.
    if env::var_os("ORT_DISABLE_TENSORRT").is_none() {
        env::set_var("ORT_DISABLE_TENSORRT", "1");
    }
    if env::var_os("ORT_STRATEGY").is_none() {
        env::set_var("ORT_STRATEGY", "system");
    }
    if env::var_os("ORT_USE_CUDA").is_none() && env::var_os("ORT_DISABLE_CUDA").is_none() {
        env::set_var("ORT_USE_CUDA", "1");
    }

    if !env_var_has_provider("ORT_LIB_LOCATION") {
        if let Some(dir) = find_ort_provider_dir() {
            env::set_var("ORT_LIB_LOCATION", &dir);
            if env::var_os("ORT_DYLIB_PATH").is_none() {
                env::set_var("ORT_DYLIB_PATH", &dir);
            }
        }
    }

    let mut prepend: Vec<PathBuf> = Vec::new();
    if let Ok(dir) = env::var("ORT_LIB_LOCATION") {
        prepend.push(PathBuf::from(dir));
    }
    prepend.extend(non_system_cuda_lib_dirs());
    if !prepend.is_empty() {
        prepend_ld_library_path(&prepend);
    }

    diagnose_gpu_env()
}

pub fn diagnose_gpu_env() -> GpuEnvReport {
    let dirs = collect_candidate_dirs();
    let provider_present = find_first_with_file(&dirs, ORT_PROVIDER_SO).is_some();
    let cublas_present = find_first_with_any(&dirs, CUBLAS_LT_CANDIDATES)
        .or_else(|| find_first_with_any(&dirs, CUBLAS_CANDIDATES))
        .is_some();

    GpuEnvReport {
        provider_present,
        cublas_present,
    }
}

fn collect_candidate_dirs() -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = Vec::new();

    if let Ok(path) = env::var("ORT_LIB_LOCATION") {
        dirs.push(PathBuf::from(path));
    }
    if let Ok(ld) = env::var("LD_LIBRARY_PATH") {
        dirs.extend(ld.split(':').filter(|p| !p.is_empty()).map(PathBuf::from));
    }

    // ORT's own downloaded binaries cache (most common "it works but env is empty" case).
    dirs.extend(ort_download_cache_dirs());

    // CUDA runtime libraries may live outside the default dynamic loader path.
    dirs.extend(non_system_cuda_lib_dirs());

    // These are typically in the default loader path, but including them avoids false negatives in
    // "presence" checks.
    dirs.extend(system_ld_default_dirs());

    dedup_existing_dirs(dirs)
}

fn find_first_with_file(dirs: &[PathBuf], name: &str) -> Option<PathBuf> {
    dirs.iter().find(|dir| dir.join(name).exists()).cloned()
}

fn find_first_with_any(dirs: &[PathBuf], candidates: &[&str]) -> Option<PathBuf> {
    for candidate in candidates {
        if let Some(dir) = find_first_with_file(dirs, candidate) {
            return Some(dir);
        }
    }
    None
}

fn env_var_has_provider(key: &str) -> bool {
    env::var_os(key).is_some_and(|val| PathBuf::from(val).join(ORT_PROVIDER_SO).exists())
}

fn find_ort_provider_dir() -> Option<PathBuf> {
    if let Ok(path) = env::var("ORT_LIB_LOCATION") {
        let candidate = PathBuf::from(path);
        if candidate.join(ORT_PROVIDER_SO).exists() {
            return Some(candidate);
        }
    }

    if let Some(root) = env_root_override() {
        if let Some(dir) = find_repo_cuda_deps_provider_dir(&root) {
            return Some(dir);
        }
    }

    // Global CUDA deps cache installed by `scripts/setup_cuda_deps.sh`.
    if let Some(dir) = global_cuda_deps_dir() {
        return Some(dir);
    }

    // Common case in dev: local deps bundle inside a repo checkout.
    if let Ok(cwd) = env::current_dir() {
        for ancestor in cwd.ancestors() {
            if let Some(dir) = find_repo_cuda_deps_provider_dir(ancestor) {
                return Some(dir);
            }
        }
    }

    // Fallback: ORT download cache.
    ort_download_cache_dirs()
        .into_iter()
        .find(|dir| dir.join(ORT_PROVIDER_SO).exists())
}

fn global_cuda_deps_dir() -> Option<PathBuf> {
    let Ok(home) = env::var("HOME") else {
        return None;
    };
    let home = Path::new(&home);
    let preferred = home.join(CONTEXT_DIR_NAME).join("deps").join("ort_cuda");
    if preferred.join(ORT_PROVIDER_SO).exists() {
        return Some(preferred);
    }
    let legacy = home
        .join(LEGACY_CONTEXT_DIR_NAME)
        .join("deps")
        .join("ort_cuda");
    if legacy.join(ORT_PROVIDER_SO).exists() {
        return Some(legacy);
    }
    None
}

/// Resolve a repo-scoped CUDA provider directory, preferring the local `.deps/ort_cuda` bundle
/// when available, otherwise falling back to `.deps/ort_cuda_official/<best>/lib`.
pub fn repo_cuda_provider_dir(root: &Path) -> Option<PathBuf> {
    find_repo_cuda_deps_provider_dir(root)
}

fn env_root_override() -> Option<PathBuf> {
    for key in [
        "CONTEXT_ROOT",
        "CONTEXT_PROJECT_ROOT",
        "CONTEXT_FINDER_ROOT",
        "CONTEXT_FINDER_PROJECT_ROOT",
    ] {
        if let Ok(value) = env::var(key) {
            let trimmed = value.trim();
            if !trimmed.is_empty() {
                return Some(PathBuf::from(trimmed));
            }
        }
    }
    None
}

fn find_repo_cuda_deps_provider_dir(root: &Path) -> Option<PathBuf> {
    let ort_cuda = root.join(".deps").join("ort_cuda");
    if ort_cuda.join(ORT_PROVIDER_SO).exists() {
        return Some(ort_cuda);
    }

    find_best_official_ort_dir(root)
}

fn find_best_official_ort_dir(root: &Path) -> Option<PathBuf> {
    let base = root.join(".deps").join("ort_cuda_official");
    let entries = std::fs::read_dir(&base).ok()?;

    let mut best: Option<(VersionTriple, PathBuf)> = None;
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let Some(version) = parse_version_triple(&name) else {
            continue;
        };
        let lib_dir = path.join("lib");
        if !lib_dir.join(ORT_PROVIDER_SO).exists() {
            continue;
        }
        match &best {
            None => best = Some((version, lib_dir)),
            Some((best_v, _)) if &version > best_v => best = Some((version, lib_dir)),
            _ => {}
        }
    }

    best.map(|(_, dir)| dir)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct VersionTriple(u32, u32, u32);

fn parse_version_triple(name: &str) -> Option<VersionTriple> {
    // e.g. "onnxruntime-linux-x64-gpu-1.22.0" -> "1.22.0"
    let ver = name.rsplit('-').next()?;
    let mut parts = ver.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = parts.next()?.parse().ok()?;
    Some(VersionTriple(major, minor, patch))
}

fn ort_download_cache_dirs() -> Vec<PathBuf> {
    let cache_root = xdg_cache_home().or_else(|| {
        env::var("HOME")
            .ok()
            .map(|home| Path::new(&home).join(".cache"))
    });
    let Some(cache_root) = cache_root else {
        return Vec::new();
    };

    let dfbin = cache_root
        .join("ort.pyke.io")
        .join("dfbin")
        .join("x86_64-unknown-linux-gnu");
    let entries = std::fs::read_dir(dfbin).ok();
    let Some(entries) = entries else {
        return Vec::new();
    };

    let mut dirs = Vec::new();
    for entry in entries.flatten() {
        let candidate = entry.path().join("onnxruntime").join("lib");
        if candidate.join(ORT_PROVIDER_SO).exists() {
            dirs.push(candidate);
        }
    }

    dirs
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

pub fn non_system_cuda_lib_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();

    // nvidia pip wheels (common on dev machines and CI).
    if let Ok(home) = env::var("HOME") {
        let base = Path::new(&home).join(".local").join("lib");
        if let Ok(entries) = std::fs::read_dir(&base) {
            for entry in entries.flatten() {
                let path = entry.path();
                if !path.is_dir() {
                    continue;
                }
                let name = entry.file_name().to_string_lossy().to_string();
                if !name.starts_with("python") {
                    continue;
                }
                let nvidia = path.join("site-packages").join("nvidia");
                if !nvidia.exists() {
                    continue;
                }
                for pkg in [
                    "cublas",
                    "cuda_runtime",
                    "cuda_nvrtc",
                    "cudnn",
                    "cufft",
                    "curand",
                    "cusolver",
                    "cusparse",
                ] {
                    let lib = nvidia.join(pkg).join("lib");
                    if lib.exists() {
                        dirs.push(lib);
                    }
                }
            }
        }
    }

    // CUDA toolkit installs (outside default loader path).
    for dir in [
        "/usr/local/cuda/lib64",
        "/usr/local/cuda/targets/x86_64-linux/lib",
    ] {
        let path = Path::new(dir);
        if path.exists() {
            dirs.push(path.to_path_buf());
        }
    }

    dirs
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

fn dedup_existing_dirs(dirs: Vec<PathBuf>) -> Vec<PathBuf> {
    let mut seen: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();
    let mut out = Vec::new();
    for dir in dirs {
        if !dir.exists() {
            continue;
        }
        if seen.insert(dir.clone()) {
            out.push(dir);
        }
    }
    out
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    static ENV_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());

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

    #[test]
    fn bootstrap_uses_ort_cache_when_env_is_empty() {
        let _lock = ENV_MUTEX.lock().expect("ENV_MUTEX");
        let _guard = EnvGuard::new(&[
            "HOME",
            "XDG_CACHE_HOME",
            "CONTEXT_ROOT",
            "CONTEXT_PROJECT_ROOT",
            "CONTEXT_FINDER_ROOT",
            "CONTEXT_FINDER_PROJECT_ROOT",
            "ORT_LIB_LOCATION",
            "ORT_DYLIB_PATH",
            "LD_LIBRARY_PATH",
            "ORT_DISABLE_TENSORRT",
            "ORT_STRATEGY",
            "ORT_USE_CUDA",
            "ORT_DISABLE_CUDA",
        ]);

        let tmp = tempfile::tempdir().expect("tempdir");
        env::set_var("HOME", tmp.path());
        let _cwd = CwdGuard::new(tmp.path());

        let lib_dir = tmp
            .path()
            .join(".cache/ort.pyke.io/dfbin/x86_64-unknown-linux-gnu/TEST/onnxruntime/lib");
        std::fs::create_dir_all(&lib_dir).expect("mkdir ort cache");
        std::fs::write(lib_dir.join(ORT_PROVIDER_SO), b"").expect("write provider stub");
        std::fs::write(lib_dir.join("libcublasLt.so.12"), b"").expect("write cublas stub");

        let report = bootstrap_cuda_env_best_effort();
        assert!(report.provider_present);
        assert!(report.cublas_present);
        assert_eq!(
            env::var("ORT_LIB_LOCATION").unwrap(),
            lib_dir.to_string_lossy()
        );
        assert!(env::var("LD_LIBRARY_PATH")
            .unwrap_or_default()
            .contains(lib_dir.to_string_lossy().as_ref()));
    }

    #[test]
    fn bootstrap_prefers_repo_deps_when_available() {
        let _lock = ENV_MUTEX.lock().expect("ENV_MUTEX");
        let _guard = EnvGuard::new(&[
            "HOME",
            "XDG_CACHE_HOME",
            "CONTEXT_ROOT",
            "CONTEXT_PROJECT_ROOT",
            "CONTEXT_FINDER_ROOT",
            "CONTEXT_FINDER_PROJECT_ROOT",
            "ORT_LIB_LOCATION",
            "ORT_DYLIB_PATH",
            "LD_LIBRARY_PATH",
            "ORT_DISABLE_TENSORRT",
            "ORT_STRATEGY",
            "ORT_USE_CUDA",
            "ORT_DISABLE_CUDA",
        ]);

        let tmp = tempfile::tempdir().expect("tempdir");
        env::set_var("CONTEXT_ROOT", tmp.path());
        let _cwd = CwdGuard::new(tmp.path());

        let deps = tmp.path().join(".deps").join("ort_cuda");
        std::fs::create_dir_all(&deps).expect("mkdir deps");
        std::fs::write(deps.join(ORT_PROVIDER_SO), b"").expect("write provider");
        std::fs::write(deps.join("libcublasLt.so.12"), b"").expect("write cublas");

        let report = bootstrap_cuda_env_best_effort();
        assert!(report.provider_present);
        assert!(report.cublas_present);
        assert_eq!(
            env::var("ORT_LIB_LOCATION").unwrap(),
            deps.to_string_lossy()
        );
    }

    #[test]
    fn bootstrap_falls_back_to_best_official_repo_dir() {
        let _lock = ENV_MUTEX.lock().expect("ENV_MUTEX");
        let _guard = EnvGuard::new(&[
            "HOME",
            "XDG_CACHE_HOME",
            "CONTEXT_ROOT",
            "CONTEXT_PROJECT_ROOT",
            "CONTEXT_FINDER_ROOT",
            "CONTEXT_FINDER_PROJECT_ROOT",
            "ORT_LIB_LOCATION",
            "ORT_DYLIB_PATH",
            "LD_LIBRARY_PATH",
            "ORT_DISABLE_TENSORRT",
            "ORT_STRATEGY",
            "ORT_USE_CUDA",
            "ORT_DISABLE_CUDA",
        ]);

        let tmp = tempfile::tempdir().expect("tempdir");
        env::set_var("CONTEXT_ROOT", tmp.path());
        let _cwd = CwdGuard::new(tmp.path());

        let base = tmp.path().join(".deps").join("ort_cuda_official");
        let v1 = base.join("onnxruntime-linux-x64-gpu-1.19.0").join("lib");
        let v2 = base.join("onnxruntime-linux-x64-gpu-1.22.0").join("lib");
        std::fs::create_dir_all(&v1).expect("mkdir v1");
        std::fs::create_dir_all(&v2).expect("mkdir v2");

        std::fs::write(v1.join(ORT_PROVIDER_SO), b"").expect("write provider v1");
        std::fs::write(v2.join(ORT_PROVIDER_SO), b"").expect("write provider v2");
        std::fs::write(v2.join("libcublasLt.so.12"), b"").expect("write cublas v2");

        let report = bootstrap_cuda_env_best_effort();
        assert!(report.provider_present);
        assert!(report.cublas_present);
        assert_eq!(env::var("ORT_LIB_LOCATION").unwrap(), v2.to_string_lossy());
    }
}
