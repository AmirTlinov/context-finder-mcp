use anyhow::{anyhow, Result};
use context_indexer::IndexerHealth;
use hyper::service::{make_service_fn, service_fn};
use hyper::{Body, Request, Response, Server};
use prometheus::{Encoder, Gauge, IntGauge, Opts, Registry, TextEncoder};
use std::convert::{Infallible, TryFrom};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::SystemTime;
use tokio::task::JoinHandle;

#[derive(Clone)]
struct DualIntGauge {
    primary: IntGauge,
    legacy: IntGauge,
}

impl DualIntGauge {
    fn new(name: &str, legacy: &str, help: &str) -> Result<Self> {
        Ok(Self {
            primary: IntGauge::with_opts(Opts::new(name, help))?,
            legacy: IntGauge::with_opts(Opts::new(legacy, help))?,
        })
    }

    fn register(&self, registry: &Registry) -> Result<()> {
        registry.register(Box::new(self.primary.clone()))?;
        registry.register(Box::new(self.legacy.clone()))?;
        Ok(())
    }

    fn set(&self, value: i64) {
        self.primary.set(value);
        self.legacy.set(value);
    }
}

#[derive(Clone)]
struct DualGauge {
    primary: Gauge,
    legacy: Gauge,
}

impl DualGauge {
    fn new(name: &str, legacy: &str, help: &str) -> Result<Self> {
        Ok(Self {
            primary: Gauge::with_opts(Opts::new(name, help))?,
            legacy: Gauge::with_opts(Opts::new(legacy, help))?,
        })
    }

    fn register(&self, registry: &Registry) -> Result<()> {
        registry.register(Box::new(self.primary.clone()))?;
        registry.register(Box::new(self.legacy.clone()))?;
        Ok(())
    }

    fn set(&self, value: f64) {
        self.primary.set(value);
        self.legacy.set(value);
    }
}

#[derive(Clone)]
pub struct MetricsExporter {
    _registry: Arc<Registry>,
    has_watcher: DualIntGauge,
    indexing: DualIntGauge,
    pending_events: DualIntGauge,
    consecutive_failures: DualIntGauge,
    last_duration_ms: DualIntGauge,
    last_success_unix_ms: DualIntGauge,
    files_per_second: DualGauge,
    index_size_bytes: DualIntGauge,
    duration_p95_ms: DualIntGauge,
    alert_log_len: DualIntGauge,
    _server_handle: Arc<JoinHandle<()>>,
}

impl MetricsExporter {
    pub async fn new(bind: &str) -> Result<Self> {
        let addr: SocketAddr = bind.parse()?;
        let registry = Arc::new(Registry::new());

        let has_watcher = DualIntGauge::new(
            "context_watcher_present",
            "contextfinder_watcher_present",
            "1 when StreamingIndexer is active",
        )?;
        let indexing = DualIntGauge::new(
            "context_indexing_active",
            "contextfinder_indexing_active",
            "1 when incremental indexing is running",
        )?;
        let pending_events = DualIntGauge::new(
            "context_pending_events",
            "contextfinder_pending_events",
            "Number of file events waiting to be processed",
        )?;
        let consecutive_failures = DualIntGauge::new(
            "context_consecutive_failures",
            "contextfinder_consecutive_failures",
            "Number of consecutive indexing failures",
        )?;
        let last_duration_ms = DualIntGauge::new(
            "context_last_index_duration_ms",
            "contextfinder_last_index_duration_ms",
            "Duration of the last indexing cycle",
        )?;
        let last_success_unix_ms = DualIntGauge::new(
            "context_last_success_unix_ms",
            "contextfinder_last_success_unix_ms",
            "Unix timestamp (ms) of the last successful cycle",
        )?;
        let files_per_second = DualGauge::new(
            "context_files_per_second",
            "contextfinder_files_per_second",
            "Indexing throughput (files/sec)",
        )?;
        let index_size_bytes = DualIntGauge::new(
            "context_index_size_bytes",
            "contextfinder_index_size_bytes",
            "Size of index.json (bytes)",
        )?;
        let duration_p95_ms = DualIntGauge::new(
            "context_duration_p95_ms",
            "contextfinder_duration_p95_ms",
            "P95 indexing duration",
        )?;
        let alert_log_len = DualIntGauge::new(
            "context_alert_log_len",
            "contextfinder_alert_log_len",
            "Number of entries in the alert log",
        )?;

        for metric in [
            &has_watcher,
            &indexing,
            &pending_events,
            &consecutive_failures,
            &last_duration_ms,
            &last_success_unix_ms,
            &index_size_bytes,
            &duration_p95_ms,
            &alert_log_len,
        ] {
            metric.register(&registry)?;
        }
        files_per_second.register(&registry)?;

        let server_registry = Arc::clone(&registry);
        let make_service = make_service_fn(move |_| {
            let registry = Arc::clone(&server_registry);
            async move {
                Ok::<_, Infallible>(service_fn(move |_req: Request<Body>| {
                    let registry = Arc::clone(&registry);
                    async move {
                        let encoder = TextEncoder::new();
                        let metric_families = registry.gather();
                        let mut buffer = Vec::new();
                        encoder.encode(&metric_families, &mut buffer).unwrap_or(());
                        Ok::<_, Infallible>(
                            Response::builder()
                                .status(200)
                                .header("Content-Type", encoder.format_type())
                                .body(Body::from(buffer))
                                .expect("valid HTTP response"),
                        )
                    }
                }))
            }
        });

        let server = Server::try_bind(&addr)
            .map_err(|err| anyhow!("failed to bind metrics endpoint on {addr}: {err}"))?
            .serve(make_service);

        let handle = tokio::spawn(async move {
            if let Err(err) = server.await {
                log::error!("Prometheus endpoint failed: {err}");
            }
        });

        let exporter = Self {
            _registry: registry,
            has_watcher,
            indexing,
            pending_events,
            consecutive_failures,
            last_duration_ms,
            last_success_unix_ms,
            files_per_second,
            index_size_bytes,
            duration_p95_ms,
            alert_log_len,
            _server_handle: Arc::new(handle),
        };

        exporter.update(None);
        Ok(exporter)
    }

    pub fn update(&self, snapshot: Option<&IndexerHealth>) {
        if let Some(health) = snapshot {
            self.has_watcher.set(1);
            self.indexing.set(if health.indexing { 1 } else { 0 });
            self.pending_events.set(health.pending_events as i64);
            self.consecutive_failures
                .set(health.consecutive_failures as i64);
            self.last_duration_ms
                .set(as_i64(health.last_duration_ms.unwrap_or(0)));
            self.last_success_unix_ms
                .set(as_i64(to_unix_ms(health.last_success)));
            self.index_size_bytes
                .set(as_i64(health.last_index_size_bytes.unwrap_or(0)));
            self.duration_p95_ms
                .set(as_i64(health.p95_duration_ms.unwrap_or(0)));
            self.alert_log_len.set(as_i64(health.alert_log_len as u64));
            self.files_per_second.set(f64::from(
                health.last_throughput_files_per_sec.unwrap_or(0.0),
            ));
        } else {
            self.has_watcher.set(0);
            self.indexing.set(0);
            self.pending_events.set(0);
            self.consecutive_failures.set(0);
            self.last_duration_ms.set(0);
            self.last_success_unix_ms.set(0);
            self.index_size_bytes.set(0);
            self.duration_p95_ms.set(0);
            self.alert_log_len.set(0);
            self.files_per_second.set(0.0);
        }
    }
}

fn as_i64(value: u64) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

fn to_unix_ms(ts: Option<SystemTime>) -> u64 {
    ts.and_then(|time| time.duration_since(SystemTime::UNIX_EPOCH).ok())
        .map(|dur| dur.as_millis() as u64)
        .unwrap_or(0)
}
