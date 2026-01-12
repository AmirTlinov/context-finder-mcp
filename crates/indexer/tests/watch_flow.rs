use context_indexer::IndexUpdate;
use context_indexer::{ProjectIndexer, StreamingIndexer, StreamingIndexerConfig};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};
use tempfile::TempDir;
use tokio::sync::broadcast::error::TryRecvError;
use tokio::sync::broadcast::Receiver;

#[cfg_attr(
    not(target_os = "linux"),
    ignore = "watcher latency test is only reliable on Linux"
)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn streaming_indexer_latency_under_two_seconds() {
    if std::env::var("SKIP_WATCH_FLOW").is_ok() {
        eprintln!("skipping watch_flow due to SKIP_WATCH_FLOW");
        return;
    }
    if low_fd_limit() {
        warn_skip_fd();
        return;
    }
    ensure_ulimit();
    std::env::set_var("CONTEXT_FINDER_EMBEDDING_MODE", "stub");

    let temp = TempDir::new().expect("tempdir");
    let src_dir = temp.path().join("src");
    tokio::fs::create_dir_all(&src_dir)
        .await
        .expect("create src");
    let file_path = src_dir.join("lib.rs");
    tokio::fs::write(&file_path, "fn noop() {}\n")
        .await
        .expect("write initial file");

    let indexer = Arc::new(ProjectIndexer::new(temp.path()).await.expect("indexer"));
    indexer.index_full().await.expect("initial index");

    let cfg = StreamingIndexerConfig {
        debounce: Duration::from_millis(200),
        max_batch_wait: Duration::from_secs(1),
        notify_poll_interval: Duration::from_millis(100),
    };
    let streamer = match StreamingIndexer::start(indexer.clone(), cfg) {
        Ok(s) => s,
        Err(e) if e.to_string().contains("Too many open files") => {
            warn_skip_watcher(&e.to_string());
            return;
        }
        Err(e) => panic!("start streamer: {e}"),
    };
    if streamer.watch_count() == 0 {
        warn_skip_watcher("watch backend reported 0 active watches");
        return;
    }
    let mut updates = streamer.subscribe_updates();

    tokio::time::sleep(Duration::from_millis(250)).await;
    while matches!(updates.try_recv(), Ok(_) | Err(TryRecvError::Lagged(_))) {}

    let start = Instant::now();
    tokio::fs::write(
        &file_path,
        format!(
            "fn updated_{}() {{ println!(\"{}\"); }}",
            start.elapsed().as_nanos(),
            start.elapsed().as_millis()
        ),
    )
    .await
    .expect("update file");

    let update = wait_for_success(&mut updates, Duration::from_secs(4))
        .await
        .unwrap_or_else(|| {
            panic!(
                "timeout waiting for update (health={:?})",
                streamer.health_snapshot()
            )
        });

    let elapsed = start.elapsed();
    assert!(
        elapsed < Duration::from_secs(2),
        "streaming index latency too high: {elapsed:?} (update: {update:?})"
    );
}

#[cfg_attr(
    not(target_os = "linux"),
    ignore = "watcher latency test is only reliable on Linux"
)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn streaming_indexer_soak_keeps_alert_log_empty() {
    if std::env::var("SKIP_WATCH_FLOW").is_ok() {
        eprintln!("skipping watch_flow due to SKIP_WATCH_FLOW");
        return;
    }
    if low_fd_limit() {
        warn_skip_fd();
        return;
    }
    ensure_ulimit();
    std::env::set_var("CONTEXT_FINDER_EMBEDDING_MODE", "stub");

    let temp = TempDir::new().expect("tempdir");
    let src_dir = temp.path().join("src");
    tokio::fs::create_dir_all(&src_dir)
        .await
        .expect("create src");
    let file_path = src_dir.join("lib.rs");
    tokio::fs::write(&file_path, "fn noop() {}\n")
        .await
        .expect("write initial file");

    let indexer = Arc::new(ProjectIndexer::new(temp.path()).await.expect("indexer"));
    indexer.index_full().await.expect("initial index");

    let cfg = StreamingIndexerConfig {
        debounce: Duration::from_millis(100),
        max_batch_wait: Duration::from_millis(400),
        notify_poll_interval: Duration::from_millis(50),
    };
    let streamer = match StreamingIndexer::start(indexer.clone(), cfg) {
        Ok(s) => s,
        Err(e) if e.to_string().contains("Too many open files") => {
            warn_skip_watcher(&e.to_string());
            return;
        }
        Err(e) => panic!("start streamer: {e}"),
    };
    if streamer.watch_count() == 0 {
        warn_skip_watcher("watch backend reported 0 active watches");
        return;
    }
    let mut updates = streamer.subscribe_updates();

    tokio::time::sleep(Duration::from_millis(200)).await;
    while matches!(updates.try_recv(), Ok(_) | Err(TryRecvError::Lagged(_))) {}

    for idx in 0..30 {
        tokio::fs::write(
            &file_path,
            format!("fn updated_{idx}() {{ println!(\"{idx}\"); }}"),
        )
        .await
        .expect("update file");

        let update = wait_for_success(&mut updates, Duration::from_secs(4))
            .await
            .unwrap_or_else(|| {
                panic!(
                    "missing update for iteration {idx} (health={:?})",
                    streamer.health_snapshot()
                )
            });
        assert!(
            update.duration_ms < 2_000,
            "iteration {idx} took too long: {:?}",
            update.duration_ms
        );
    }

    let snapshot = streamer.health_snapshot();
    assert!(snapshot.last_error.is_none());
    assert_eq!(snapshot.alert_log_len, 0);
    assert_eq!(snapshot.alert_log_json, "[]");
}

async fn wait_for_success(
    updates: &mut Receiver<IndexUpdate>,
    timeout: Duration,
) -> Option<IndexUpdate> {
    tokio::time::timeout(timeout, async {
        loop {
            if let Ok(update) = updates.recv().await {
                if update.success {
                    break Some(update);
                }
            }
        }
    })
    .await
    .ok()
    .flatten()
}

#[cfg_attr(
    not(target_os = "linux"),
    ignore = "watcher latency test is only reliable on Linux"
)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn streaming_indexer_health_records_last_success() {
    if std::env::var("SKIP_WATCH_FLOW").is_ok() {
        eprintln!("skipping watch_flow due to SKIP_WATCH_FLOW");
        return;
    }
    if low_fd_limit() {
        warn_skip_fd();
        return;
    }
    ensure_ulimit();
    std::env::set_var("CONTEXT_FINDER_EMBEDDING_MODE", "stub");

    let temp = TempDir::new().expect("tempdir");
    let src_dir = temp.path().join("src");
    tokio::fs::create_dir_all(&src_dir)
        .await
        .expect("create src");
    let file_path = src_dir.join("lib.rs");
    tokio::fs::write(&file_path, "fn noop() {}\n")
        .await
        .expect("write initial file");

    let indexer = Arc::new(ProjectIndexer::new(temp.path()).await.expect("indexer"));
    indexer.index_full().await.expect("initial index");

    let cfg = StreamingIndexerConfig {
        debounce: Duration::from_millis(200),
        max_batch_wait: Duration::from_secs(1),
        notify_poll_interval: Duration::from_millis(100),
    };
    let streamer = match StreamingIndexer::start(indexer.clone(), cfg) {
        Ok(s) => s,
        Err(e) if e.to_string().contains("Too many open files") => {
            warn_skip_watcher(&e.to_string());
            return;
        }
        Err(e) => panic!("start streamer: {e}"),
    };
    if streamer.watch_count() == 0 {
        warn_skip_watcher("watch backend reported 0 active watches");
        return;
    }
    let mut updates = streamer.subscribe_updates();

    tokio::time::sleep(Duration::from_millis(250)).await;
    while matches!(updates.try_recv(), Ok(_) | Err(TryRecvError::Lagged(_))) {}

    tokio::fs::write(&file_path, "fn updated_health() {}")
        .await
        .expect("update file");

    wait_for_success(&mut updates, Duration::from_secs(4))
        .await
        .unwrap_or_else(|| panic!("health update (health={:?})", streamer.health_snapshot()));

    let snapshot = streamer.health_snapshot();
    let last_success = snapshot
        .last_success
        .expect("last_success should be recorded");
    let unix_ms = last_success
        .duration_since(SystemTime::UNIX_EPOCH)
        .expect("unix epoch conversion")
        .as_millis();
    assert!(unix_ms > 0, "unix timestamp must be positive");
    assert!(
        snapshot.last_duration_ms.unwrap_or(0) > 0,
        "duration must be captured"
    );
}

fn low_fd_limit() -> bool {
    rlimit::Resource::NOFILE
        .get()
        .map(|(soft, _)| soft < 1024)
        .unwrap_or(false)
}

fn ensure_ulimit() {
    if let Ok((_soft, hard)) = rlimit::Resource::NOFILE.get() {
        let target = 2048.min(hard);
        let _ = rlimit::Resource::NOFILE.set(target, hard);
        let _ = rlimit::Resource::NOFILE.set(target, hard);
    }
}

fn warn_skip_fd() {
    eprintln!("skipping watcher tests: NOFILE soft limit < 1024");
}

fn warn_skip_watcher(reason: &str) {
    eprintln!("skipping watcher tests: {reason}");
}
