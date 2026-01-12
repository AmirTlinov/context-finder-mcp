use std::sync::Mutex;

/// Cross-test synchronization for process-wide state (env vars, cwd, etc.).
///
/// Rust tests run in parallel by default, but env vars are shared per-process.
/// Any test that mutates or depends on process-wide env should lock this mutex.
#[cfg(test)]
pub(crate) static ENV_MUTEX: Mutex<()> = Mutex::new(());
