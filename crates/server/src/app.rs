//! Process wiring: open storage and metadata, start background jobs, serve,
//! and drain accepted work on shutdown.
//!
//! Shutdown (REL-05): the first stop request flips `AppState::shutdown`,
//! which closes the listener, refuses new ingest, queries and live streams
//! with 503, and ends live-tail streams; idle connections close and waiting
//! ones are refused (see [`crate::transport`]). Requests already admitted run to
//! their acknowledgement, then every storage stream writes out its queue and
//! flushes its WAL. All of it shares one deadline,
//! `server.shutdown_timeout`; a second stop request ends the wait at once.
//! Giving up returns [`IncompleteShutdown`], which says what was unfinished.

use std::future::Future;
use std::pin::{Pin, pin};
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::time::{Duration, Instant};

use anyhow::Context;
use axum::Router;
use metadata::Metadata;
use query::{Engine, QueryLimits};
use storage::{CommitHook, Storage, StorageConfig, StorageFault};
use telemetry::{Signal, Timestamp};
use tokio::net::TcpListener;
use tokio::sync::{Semaphore, broadcast, oneshot, watch};
use tokio::task::JoinHandle;

use crate::config::{Config, Durability, format_duration, human_bytes};
use crate::datadir::{Layout, Manifest};
use crate::state::{AppState, LiveBatch, SharedState};

/// Batches retained for live-tail subscribers that fall behind.
const LIVE_CHANNEL_CAPACITY: usize = 64;

pub struct App {
    pub state: SharedState,
    pub router: Router,
    /// Admin password generated on first start (auth enabled, none set).
    pub generated_password: Option<String>,
    shutdown: Arc<watch::Sender<bool>>,
    tasks: Vec<JoinHandle<()>>,
}

/// Options for one start, given on the command line rather than in the
/// configuration, so they do not stay in effect by accident.
#[derive(Debug, Clone, Copy, Default)]
pub struct StartOptions {
    /// Start even though the data directory is missing parts recorded at
    /// its last clean shutdown, or its metadata database is empty
    /// (`--accept-incomplete-restore`). Format problems are never accepted.
    pub accept_incomplete_restore: bool,
}

fn storage_config(config: &Config) -> StorageConfig {
    let budgets = config.budgets();
    let mut sc = StorageConfig::new(&config.storage.path);
    sc.durability = match config.storage.durability {
        Durability::Normal => storage::Durability::Normal,
        Durability::Strict => storage::Durability::Strict,
    };
    sc.fsync_interval = config.storage.fsync_interval;
    sc.index_cache_bytes = budgets.index_cache as usize;
    sc.compaction.enabled = config.storage.compaction;
    // Compaction holds its inputs in memory; keep that inside the
    // background budget.
    sc.compaction.max_input_bytes = (budgets.background / 3).max(1 << 20);
    for (i, signal) in Signal::ALL.into_iter().enumerate() {
        let s = sc.stream_mut(signal);
        s.segment_target_bytes = budgets.segment_target[i];
        s.segment_max_age = config.storage.segment_max_age;
        s.queue_events = config.ingest.queue_capacity;
        s.retention = match signal {
            Signal::Logs => config.retention.logs,
            Signal::Traces => config.retention.traces,
            Signal::Metrics => config.retention.metrics,
        };
    }
    sc
}

impl App {
    /// Open everything and start background tasks. Must run inside a Tokio
    /// runtime.
    pub fn build(config: Config) -> anyhow::Result<App> {
        Self::build_with(config, StartOptions::default())
    }

    /// [`App::build`] with one-off start options.
    pub fn build_with(config: Config, options: StartOptions) -> anyhow::Result<App> {
        config.validate().map_err(anyhow::Error::msg)?;
        let budgets = config.budgets();
        let dir = &config.storage.path;
        let accept = options.accept_incomplete_restore;
        // Read-only: a directory that fails is left exactly as it was, not
        // even locked.
        crate::datadir::check(dir, accept)?;
        storage::fsutil::create_dir_all_durable(dir)
            .with_context(|| format!("creating data directory {}", dir.display()))?;
        // Nothing below changes the directory unless this process holds the
        // lock, which storage then keeps. Check again under it: another
        // process may have started, stopped or changed it in the meantime.
        let lock = storage::DataDirLock::acquire(dir).context("locking the data directory")?;
        let checked = crate::datadir::check(dir, accept)?;
        for problem in &checked.accepted {
            tracing::warn!(problem = %problem, "starting without part of the data directory (--accept-incomplete-restore)");
        }
        let layout = checked.layout.clone();
        if matches!(&layout, Layout::Unrecorded | Layout::Current(Manifest { inventory: None, .. })) {
            tracing::info!(
                "data directory has no inventory from a clean shutdown; missing WAL or segment files cannot be detected"
            );
        }
        crate::datadir::begin(dir, &checked).context("updating the data directory manifest")?;
        crate::datadir::prepare(dir).context("creating data directory")?;

        // The check refused a database of another layout: this one has
        // this layout's identity, or no schema yet and gets it.
        let metadata = Arc::new(
            Metadata::open(&dir.join(crate::datadir::METADATA_DB))
                .map_err(|e| anyhow::anyhow!("opening metadata store: {e}"))?,
        );
        let api_keys = crate::auth::ApiKeys::load(&metadata).map_err(|e| anyhow::anyhow!("loading API keys: {e}"))?;
        // Record the layout before storage recovery changes any telemetry
        // file, and before a new directory gets its first one.
        crate::datadir::record(dir, &layout).context("writing the data directory manifest")?;

        let (live_tx, _) = broadcast::channel::<LiveBatch>(LIVE_CHANNEL_CAPACITY);
        let hook_tx = live_tx.clone();
        let hook: CommitHook = Arc::new(move |signal, batch| {
            // Only pay for fan-out when someone is watching.
            if hook_tx.receiver_count() > 0 {
                let _ = hook_tx.send(LiveBatch { signal, batch: batch.clone() });
            }
        });
        let storage =
            Arc::new(Storage::open_locked(storage_config(&config), Some(hook), lock).context("opening storage")?);
        let engine = Arc::new(Engine::new(
            storage.clone(),
            QueryLimits {
                timeout: config.query.timeout,
                max_scan_bytes: config.query.max_scan_bytes.0,
                max_results: config.query.max_results,
                max_trace_spans: config.query.max_trace_spans,
                max_metric_names: config.query.max_metric_names,
            },
        ));

        let admission = config.ingest_admission();
        if let Some(warning) = admission.warning() {
            tracing::warn!(
                max_concurrent = admission.limit,
                max_request_bytes = admission.max_request_bytes,
                ingest_budget_bytes = admission.budget,
                estimated_peak_bytes = admission.estimated_peak,
                "{warning}"
            );
        }
        let ingest_slots = admission.limit;
        let transport = crate::transport::Transport::new(config.transport_limits());
        if config.query.max_live_streams >= config.server.max_connections {
            tracing::warn!(
                max_live_streams = config.query.max_live_streams,
                max_connections = config.server.max_connections,
                "query.max_live_streams is not below server.max_connections: open live-tail streams can take every \
                 connection slot and leave none for ingest or queries; raise server.max_connections or lower \
                 query.max_live_streams (see docs/configuration.md#connections-and-transport-limits)"
            );
        }
        let http = reqwest::Client::builder()
            .user_agent(concat!("vyrtel/", env!("CARGO_PKG_VERSION")))
            .build()
            .context("building HTTP client")?;

        let (shutdown, rx) = watch::channel(false);
        let state: SharedState = Arc::new(AppState {
            budgets,
            storage,
            engine,
            metadata,
            live: live_tx,
            live_permits: Arc::new(Semaphore::new(config.query.max_live_streams.max(1))),
            query_permits: Arc::new(Semaphore::new(config.query.max_concurrent)),
            query_outcomes: Default::default(),
            ingest_permits: Arc::new(Semaphore::new(ingest_slots)),
            ingest_max_concurrent: ingest_slots,
            ingest_admission: admission,
            ingest_rejected: AtomicU64::new(0),
            discard_permits: Arc::new(Semaphore::new(crate::routes::ingest::MAX_DISCARDING)),
            discard_pending: Arc::new(Semaphore::new(crate::routes::ingest::MAX_DISCARD_PENDING)),
            transport,
            started_at: Instant::now(),
            started_ts: Timestamp::now(),
            received_bytes: AtomicU64::new(0),
            received_requests: AtomicU64::new(0),
            http,
            sessions: Default::default(),
            api_keys: Arc::new(api_keys),
            shutdown: rx.clone(),
            config,
        });

        let generated_password = crate::auth::bootstrap_admin(&state).context("initialising admin user")?;
        let router = crate::routes::router(state.clone());

        let mut tasks = vec![tokio::spawn(flush_query_stats(state.clone(), rx.clone()))];
        tasks.push(tokio::spawn(purge_sessions(state.clone(), rx.clone())));
        if state.config.alerts.enabled {
            tasks.push(tokio::spawn(crate::alerts::run(state.clone(), rx)));
        }
        Ok(App { state, router, generated_password, shutdown: Arc::new(shutdown), tasks })
    }

    pub fn banner(&self, bound: std::net::SocketAddr) -> String {
        let c = &self.state.config;
        let s = self.state.storage.stats();
        let stored: u64 = [&s.logs, &s.traces, &s.metrics]
            .iter()
            .map(|x| x.segment_data_bytes + x.segment_index_bytes + x.wal_bytes)
            .sum();
        // SocketAddr's Display brackets IPv6 addresses. A wildcard bind (0.0.0.0 / [::]) isn't
        // something you can open in a browser, so the UI link points at localhost instead.
        let url = format!("http://{bound}");
        let web = if bound.ip().is_unspecified() { format!("http://localhost:{}", bound.port()) } else { url.clone() };
        let mut out = format!(
            "Vyrtel {}\n\n\
             Web UI              {web}\n\
             Data directory      {}\n\
             HTTP                {url}\n\
             OTLP HTTP           {url}/v1/*\n\
             Storage             {}\n\
             Memory limit        {}\n\
             Durability          {}\n\
             Connections         {} max (buffers up to {})\n\
             Authentication      {}\n",
            env!("CARGO_PKG_VERSION"),
            c.storage.path.display(),
            human_bytes(stored),
            c.storage.max_memory,
            match c.storage.durability {
                Durability::Normal => "normal",
                Durability::Strict => "strict",
            },
            c.server.max_connections,
            human_bytes(c.transport_limits().estimated_peak()),
            if self.state.api_keys.ingest_protected() {
                "login for the UI/API, API keys for ingestion"
            } else {
                "login for the UI/API; ingestion open until an ingest API key is created"
            },
        );
        if let Some(pw) = &self.generated_password {
            out.push_str(&format!(
                "\nAdmin login         {} / {pw}\n                    (generated once; set auth.admin_password to choose your own)\n",
                c.auth.admin_username
            ));
        }
        out.push_str("\nReady.\n");
        out
    }

    /// Serve until `shutdown_signal` resolves, then drain accepted work
    /// within `server.shutdown_timeout` (see [`App::serve_until`]).
    pub async fn serve(
        self,
        listener: TcpListener,
        shutdown_signal: impl Future<Output = ()> + Send + 'static,
    ) -> anyhow::Result<()> {
        self.serve_until(listener, shutdown_signal, std::future::pending()).await
    }

    /// Serve until `stop` resolves, then stop accepting connections, refuse
    /// new work and drain what was accepted: in-flight requests, queued
    /// writes and the final WAL flush, all within `server.shutdown_timeout`.
    /// If the deadline passes, or `force` resolves first (a second stop
    /// request), stop waiting and return [`IncompleteShutdown`]. The caller
    /// should then exit without running destructors, which could wait on
    /// the work that was left running.
    pub async fn serve_until(
        self,
        listener: TcpListener,
        stop: impl Future<Output = ()> + Send + 'static,
        force: impl Future<Output = ()>,
    ) -> anyhow::Result<()> {
        let timeout = self.state.config.server.shutdown_timeout;
        let (began_tx, mut began_rx) = oneshot::channel();
        let notify = self.shutdown.clone();
        let draining = self.state.clone();
        let transport = self.state.transport.clone();
        let serving = crate::transport::serve(listener, self.router.clone(), transport, async move {
            stop.await;
            let _ = began_tx.send(tokio::time::Instant::now());
            // Refuse new work and end live-tail streams so draining
            // connections can finish.
            let _ = notify.send(true);
            let work = Unfinished::now(&draining);
            tracing::info!(
                deadline = %format_duration(timeout),
                ingest_requests = work.ingest_requests,
                queued_events = work.queued_events(),
                live_streams = work.live_streams,
                "shutdown started; draining accepted work"
            );
        });
        let mut serving = pin!(serving);
        let mut force = pin!(force);

        // Until the stop request. Serving ends first only on an error, or
        // when the stop request and the drain both complete in one poll.
        let began = tokio::select! {
            r = &mut serving => {
                r.context("HTTP server error")?;
                None
            }
            Ok(at) = &mut began_rx => Some(at),
        };
        let deadline = began.unwrap_or_else(tokio::time::Instant::now) + timeout;
        if began.is_some() {
            // In-flight requests finish and live-tail streams end.
            match within(&mut serving, deadline, timeout, &mut force).await {
                Ok(r) => r.context("HTTP server error")?,
                Err(cause) => {
                    return Err(IncompleteShutdown::report(&self.state, cause, ShutdownPhase::Requests).into());
                }
            }
        }
        let started = deadline - timeout;
        self.finish(deadline, force).await?;
        tracing::info!(elapsed_ms = started.elapsed().as_millis() as u64, "shutdown complete");
        Ok(())
    }

    /// Flush storage and stop background jobs, waiting at most
    /// `server.shutdown_timeout`.
    pub async fn shutdown(self) -> Result<(), IncompleteShutdown> {
        let deadline = tokio::time::Instant::now() + self.state.config.server.shutdown_timeout;
        self.finish(deadline, pin!(std::future::pending())).await
    }

    /// Everything after the HTTP drain. Accepted work comes first: every
    /// stream writes out its queue and flushes its WAL. Then background
    /// jobs stop and query statistics are saved, then any seal or
    /// compaction in progress finishes. Each phase ends early on the
    /// deadline or a second stop request, reported the same way in every
    /// phase; only a shutdown that finished all of them is `Ok`.
    async fn finish(
        self,
        deadline: tokio::time::Instant,
        mut force: Pin<&mut impl Future<Output = ()>>,
    ) -> Result<(), IncompleteShutdown> {
        let _ = self.shutdown.send(true);
        let timeout = self.state.config.server.shutdown_timeout;
        let state = self.state;
        let fail = |cause, phase| Err(IncompleteShutdown::report(&state, cause, phase));

        // Unsealed data is replayed from the WAL on the next start.
        let storage = state.storage.clone();
        let flush = tokio::task::spawn_blocking(move || storage.stop_writers());
        match within(flush, deadline, timeout, &mut force).await {
            Ok(Ok(Ok(()))) => {}
            Ok(Ok(Err(faults))) => return fail(StopCause::WalFlushFailed(faults), ShutdownPhase::Storage),
            Ok(Err(panic)) => std::panic::resume_unwind(panic.into_panic()),
            Err(cause) => return fail(cause, ShutdownPhase::Storage),
        }

        let mut tasks = self.tasks;
        let background = async {
            for t in &mut tasks {
                let _ = t.await;
            }
            persist_query_stats(&state).await;
        };
        if let Err(cause) = within(background, deadline, timeout, &mut force).await {
            tasks.iter().for_each(JoinHandle::abort);
            return fail(cause, ShutdownPhase::Background);
        }

        // Interrupting a seal or compaction loses nothing (its input is in
        // the flushed WAL), but the shutdown did not finish, so it is
        // reported like any other phase.
        let storage = state.storage.clone();
        let maintenance = tokio::task::spawn_blocking(move || storage.shutdown());
        if let Err(cause) = within(maintenance, deadline, timeout, &mut force).await {
            return fail(cause, ShutdownPhase::Maintenance);
        }

        // Nothing writes telemetry files any more: record them, so a backup
        // of the stopped directory can be checked for completeness. Without
        // that, a backup loses the check, so the shutdown is not clean.
        let dir = state.config.storage.path.clone();
        let inventory = tokio::task::spawn_blocking(move || crate::datadir::record_stopped(&dir));
        match within(inventory, deadline, timeout, &mut force).await {
            Ok(Ok(Ok(()))) => {}
            Ok(Ok(Err(e))) => return fail(StopCause::InventoryFailed(e.to_string()), ShutdownPhase::Inventory),
            Ok(Err(panic)) => std::panic::resume_unwind(panic.into_panic()),
            Err(cause) => return fail(cause, ShutdownPhase::Inventory),
        }
        Ok(())
    }
}

/// Wait for `work` until `deadline` passes or `force` (a second stop
/// request) resolves. `force` must not be polled again once it has.
async fn within<T>(
    work: impl Future<Output = T>,
    deadline: tokio::time::Instant,
    timeout: Duration,
    force: &mut Pin<&mut impl Future<Output = ()>>,
) -> Result<T, StopCause> {
    tokio::select! {
        r = work => Ok(r),
        _ = tokio::time::sleep_until(deadline) => Err(StopCause::Deadline(timeout)),
        _ = force.as_mut() => Err(StopCause::Forced),
    }
}

/// Why shutdown did not finish.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StopCause {
    /// `server.shutdown_timeout` passed.
    Deadline(Duration),
    /// A second stop request arrived.
    Forced,
    /// The final WAL flush failed for these signals, so batches
    /// acknowledged since their last fsync may not be on disk. Each fault
    /// is also recorded on its stream.
    WalFlushFailed(Vec<(Signal, StorageFault)>),
    /// Everything was written and flushed, but the inventory of the stopped
    /// data directory could not be recorded (the I/O error).
    InventoryFailed(String),
}

/// What shutdown was doing when it stopped, in order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShutdownPhase {
    /// HTTP requests: ingest awaiting acknowledgement, queries, live tail.
    Requests,
    /// Storage writing out queued batches and flushing its WAL.
    Storage,
    /// Background jobs (alert evaluation) stopping and query statistics
    /// being saved. Every accepted request is already written and flushed.
    Background,
    /// A seal or compaction in progress. Every accepted request is already
    /// written and flushed; the next start redoes the seal from the WAL.
    Maintenance,
    /// Recording the WAL and segment files of the stopped directory in its
    /// manifest. Everything is already written, flushed and sealed.
    Inventory,
}

/// Accepted work still in progress.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unfinished {
    /// Ingest requests admitted and not yet answered.
    pub ingest_requests: usize,
    /// Events submitted to storage and not yet acknowledged, per signal.
    pub queued: Vec<(Signal, usize)>,
    /// Live-tail streams still open.
    pub live_streams: usize,
}

impl Unfinished {
    /// Reads lock-free counters only, so it works with a stuck writer.
    pub fn now(state: &AppState) -> Unfinished {
        Unfinished {
            ingest_requests: state.ingest_max_concurrent - state.ingest_permits.available_permits(),
            queued: Signal::ALL
                .into_iter()
                .map(|s| (s, state.storage.stream(s).queued_events()))
                .filter(|(_, n)| *n > 0)
                .collect(),
            live_streams: state.config.query.max_live_streams.max(1) - state.live_permits.available_permits(),
        }
    }

    pub fn queued_events(&self) -> usize {
        self.queued.iter().map(|(_, n)| n).sum()
    }
}

/// Shutdown did not finish: it stopped waiting (deadline, second stop
/// request) or the final WAL flush failed. `unfinished` is what was still
/// in progress. A process that gets this must exit with a failure status.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IncompleteShutdown {
    pub cause: StopCause,
    pub phase: ShutdownPhase,
    pub unfinished: Unfinished,
}

impl IncompleteShutdown {
    /// Capture the unfinished work and log it.
    fn report(state: &AppState, cause: StopCause, phase: ShutdownPhase) -> IncompleteShutdown {
        let e = IncompleteShutdown { cause, phase, unfinished: Unfinished::now(state) };
        let u = &e.unfinished;
        tracing::error!(
            ingest_requests = u.ingest_requests,
            queued_events = u.queued_events(),
            live_streams = u.live_streams,
            "{e}"
        );
        e
    }
}

fn plural(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

impl std::fmt::Display for IncompleteShutdown {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("shutdown incomplete: ")?;
        match &self.cause {
            StopCause::Deadline(d) => write!(f, "server.shutdown_timeout ({}) passed", format_duration(*d))?,
            StopCause::Forced => f.write_str("a second stop signal arrived")?,
            StopCause::WalFlushFailed(faults) => {
                let each: Vec<String> = faults
                    .iter()
                    .map(|(s, x)| {
                        format!("{}: {} {} on {}: {}", s.as_str(), x.op.as_str(), x.reason.as_str(), x.path, x.message)
                    })
                    .collect();
                return write!(
                    f,
                    "the final WAL flush failed ({}). Events acknowledged since the last successful fsync reached \
                     the operating system but may not be on disk: the next start replays them, but an OS crash or \
                     power loss before the OS writes them out can lose them; fix the storage problem \
                     (docs/configuration.md#storage-failures)",
                    each.join("; ")
                );
            }
            StopCause::InventoryFailed(e) => {
                return write!(
                    f,
                    "every accepted request was written and the WAL flushed, but the data directory inventory could \
                     not be recorded ({e}). The directory starts normally, but a backup of it cannot be checked for \
                     missing or truncated files when it is restored; fix the storage problem, then start and stop \
                     Vyrtel again before backing it up (docs/configuration.md#backup-and-restore)"
                );
            }
        }
        f.write_str(match self.phase {
            ShutdownPhase::Requests => " while waiting for in-flight requests",
            ShutdownPhase::Storage => " while storage was writing queued batches and flushing the WAL",
            ShutdownPhase::Background => {
                " while background jobs were stopping and query statistics were being saved, \
                 after every accepted request was written and the WAL flushed"
            }
            ShutdownPhase::Maintenance => {
                " while a seal or compaction was finishing, after every accepted request was written \
                 and the WAL flushed (the next start redoes it from the WAL)"
            }
            ShutdownPhase::Inventory => {
                " while the data directory inventory was being recorded, after every accepted request was \
                 written and the WAL flushed; a backup taken now cannot be checked for missing files, so start \
                 and stop Vyrtel again before backing it up"
            }
        })?;
        let u = &self.unfinished;
        let mut parts = Vec::new();
        if u.ingest_requests > 0 {
            parts.push(format!(
                "{} not acknowledged (the client gets no response and should retry)",
                plural(u.ingest_requests, "ingest request", "ingest requests")
            ));
        }
        if !u.queued.is_empty() {
            let per: Vec<String> = u.queued.iter().map(|(s, n)| format!("{} {n}", s.as_str())).collect();
            parts.push(format!(
                "{} not written ({})",
                plural(u.queued_events(), "queued event", "queued events"),
                per.join(", ")
            ));
        }
        if u.live_streams > 0 {
            parts.push(format!(
                "{} still open (client not reading)",
                plural(u.live_streams, "live-tail stream", "live-tail streams")
            ));
        }
        if parts.is_empty() {
            f.write_str("; no requests or queued events were left")?;
        } else {
            write!(f, "; unfinished: {}", parts.join("; "))?;
        }
        f.write_str(". Acknowledged events are already in the WAL and are replayed on the next start; ")?;
        f.write_str(match self.cause {
            StopCause::Forced => "send a single stop signal to let accepted work finish",
            _ => "raise server.shutdown_timeout if draining regularly takes longer",
        })
    }
}

impl std::error::Error for IncompleteShutdown {}

/// In-memory query statistics as the row that persists them.
pub(crate) fn stored_query_stats(s: query::stats::FieldQueryStats) -> metadata::StoredQueryStats {
    metadata::StoredQueryStats {
        signal: s.signal,
        field: s.field,
        index: s.index,
        queries: s.queries,
        bytes_read: s.bytes_read,
        segments_scanned: s.segments_scanned,
        segments_skipped: s.segments_skipped,
        total_ms: s.total_ms,
        last_seen: metadata::now_ms(),
    }
}

async fn persist_query_stats(state: &SharedState) {
    let rows: Vec<metadata::StoredQueryStats> =
        state.engine.stats().drain().into_iter().map(stored_query_stats).collect();
    if rows.is_empty() {
        return;
    }
    if let Err(e) = state.db(move |m| m.add_query_stats(&rows)).await {
        tracing::warn!(error = %e.message, "could not persist query statistics");
    }
}

async fn flush_query_stats(state: SharedState, mut shutdown: watch::Receiver<bool>) {
    let mut tick = tokio::time::interval(Duration::from_secs(60));
    tick.tick().await;
    loop {
        tokio::select! {
            _ = tick.tick() => persist_query_stats(&state).await,
            _ = shutdown.changed() => return,
        }
    }
}

async fn purge_sessions(state: SharedState, mut shutdown: watch::Receiver<bool>) {
    let mut tick = tokio::time::interval(Duration::from_secs(3600));
    loop {
        tokio::select! {
            _ = tick.tick() => {
                let _ = state.db(|m| m.purge_expired_sessions()).await;
            }
            _ = shutdown.changed() => return,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;
    use std::sync::{Arc, Mutex};

    use super::*;

    #[derive(Clone, Default)]
    struct Captured(Arc<Mutex<Vec<u8>>>);

    impl Write for Captured {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// Start an app with `max_concurrent` and return what it logged.
    async fn startup_log(max_concurrent: usize) -> String {
        let dir = tempfile::tempdir().unwrap();
        let mut config = Config::default();
        config.storage.path = dir.path().to_path_buf();
        config.alerts.enabled = false;
        config.ingest.max_concurrent = max_concurrent;
        let out = Captured::default();
        let writer = out.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(move || writer.clone())
            .with_max_level(tracing::Level::WARN)
            .with_ansi(false)
            .finish();
        let app = tracing::subscriber::with_default(subscriber, || App::build(config)).unwrap();
        app.shutdown().await.unwrap();
        String::from_utf8(out.0.lock().unwrap().clone()).unwrap()
    }

    // REL-14 AC4: one warning at startup for an over-budget explicit limit,
    // none within budget or when derived.
    #[tokio::test]
    async fn over_budget_explicit_ingest_limit_warns_once_at_startup() {
        let log = startup_log(8).await;
        assert_eq!(log.matches("the limit is honoured").count(), 1, "{log}");
        for part in
            ["max_concurrent=8", "max_request_bytes=1048576", "ingest_budget_bytes=80530635", "estimated_peak_bytes="]
        {
            assert!(log.contains(part), "{part} in {log}");
        }
        assert!(!startup_log(2).await.contains("honoured"));
        assert!(!startup_log(0).await.contains("honoured"));
    }

    // REL-05 AC1: once shutdown starts, new work gets 503 and the
    // connection closes; readiness takes the instance out of rotation.
    #[tokio::test]
    async fn draining_refuses_new_work() {
        use axum::body::Body;
        use axum::http::{Request, StatusCode, header};
        use tower::ServiceExt;

        let dir = tempfile::tempdir().unwrap();
        let mut config = Config::default();
        config.storage.path = dir.path().to_path_buf();
        config.alerts.enabled = false;
        let app = App::build(config).unwrap();
        let (_, key) = crate::auth::create_api_key(&app.state, "test", "admin").await.unwrap();
        let bearer = format!("Bearer {key}");
        let request = |method: &str, uri: &str, body: &'static str| {
            Request::builder()
                .method(method)
                .uri(uri)
                .header(header::AUTHORIZATION, &bearer)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(body))
                .unwrap()
        };
        let r = app.router.clone().oneshot(request("GET", "/ready", "")).await.unwrap();
        assert_eq!(r.status(), StatusCode::OK);

        app.shutdown.send(true).unwrap();
        for (method, uri, body) in [
            ("POST", "/api/v1/events", r#"{"message":"late"}"#),
            ("POST", "/v1/logs", "{}"),
            ("POST", "/api/v1/query/logs", r#"{"query":""}"#),
            ("GET", "/api/v1/live/logs", ""),
        ] {
            let r = app.router.clone().oneshot(request(method, uri, body)).await.unwrap();
            assert_eq!(r.status(), StatusCode::SERVICE_UNAVAILABLE, "{uri}");
            assert_eq!(r.headers()[header::RETRY_AFTER], "1", "{uri}");
            assert_eq!(r.headers()[header::CONNECTION], "close", "{uri}");
            let body = axum::body::to_bytes(r.into_body(), 1 << 16).await.unwrap();
            let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
            assert_eq!(v["error"]["code"], "shutting_down", "{uri}");
        }
        let r = app.router.clone().oneshot(request("GET", "/ready", "")).await.unwrap();
        assert_eq!(r.status(), StatusCode::SERVICE_UNAVAILABLE);
        let body = axum::body::to_bytes(r.into_body(), 1 << 16).await.unwrap();
        assert_eq!(serde_json::from_slice::<serde_json::Value>(&body).unwrap()["status"], "draining");
        // Liveness is unaffected: the process is still running.
        let r = app.router.clone().oneshot(request("GET", "/health", "")).await.unwrap();
        assert_eq!(r.status(), StatusCode::OK);
        assert_eq!(app.state.storage.stream(Signal::Logs).queued_events(), 0, "nothing reached storage");
        app.shutdown().await.unwrap();
    }

    // REL-05 AC3: the diagnostic names the cause, the phase and every kind
    // of unfinished work, and what happens to acknowledged data.
    #[test]
    fn incomplete_shutdown_describes_unfinished_work() {
        let e = IncompleteShutdown {
            cause: StopCause::Deadline(Duration::from_secs(8)),
            phase: ShutdownPhase::Requests,
            unfinished: Unfinished {
                ingest_requests: 2,
                queued: vec![(Signal::Logs, 150), (Signal::Traces, 1)],
                live_streams: 1,
            },
        };
        assert_eq!(
            e.to_string(),
            "shutdown incomplete: server.shutdown_timeout (8s) passed while waiting for in-flight requests; \
             unfinished: 2 ingest requests not acknowledged (the client gets no response and should retry); \
             151 queued events not written (logs 150, traces 1); 1 live-tail stream still open (client not reading). \
             Acknowledged events are already in the WAL and are replayed on the next start; \
             raise server.shutdown_timeout if draining regularly takes longer"
        );
        let e = IncompleteShutdown {
            cause: StopCause::Forced,
            phase: ShutdownPhase::Storage,
            unfinished: Unfinished { ingest_requests: 0, queued: vec![(Signal::Metrics, 1)], live_streams: 0 },
        };
        let text = e.to_string();
        assert!(
            text.starts_with(
                "shutdown incomplete: a second stop signal arrived while storage was writing queued batches \
                 and flushing the WAL; unfinished: 1 queued event not written (metrics 1)."
            ),
            "{text}"
        );
        assert!(text.ends_with("next start; send a single stop signal to let accepted work finish"), "{text}");
        let idle = IncompleteShutdown {
            cause: StopCause::Forced,
            phase: ShutdownPhase::Storage,
            unfinished: Unfinished { ingest_requests: 0, queued: vec![], live_streams: 0 },
        };
        assert!(idle.to_string().contains("; no requests or queued events were left."), "{idle}");

        let background = IncompleteShutdown { phase: ShutdownPhase::Background, ..idle.clone() };
        assert!(
            background.to_string().starts_with(
                "shutdown incomplete: a second stop signal arrived while background jobs were stopping and query \
                 statistics were being saved, after every accepted request was written and the WAL flushed;"
            ),
            "{background}"
        );
        let maintenance = IncompleteShutdown { phase: ShutdownPhase::Maintenance, ..idle };
        assert!(maintenance.to_string().contains("while a seal or compaction was finishing"), "{maintenance}");
    }

    // REL-05 review: a failed final flush names each signal's fault (paths
    // relative to the data directory) and what it means for acknowledged data.
    #[test]
    fn failed_final_flush_describes_the_fault() {
        let e = std::io::Error::new(std::io::ErrorKind::StorageFull, "No space left on device");
        let wal = std::path::Path::new("/data/wal/logs/000000000001.wal");
        let err = storage::StorageError::Io { path: wal.to_path_buf(), source: e };
        let fault = StorageFault::from_error(storage::FaultOp::WalSync, &err, wal, std::path::Path::new("/data"));
        let e = IncompleteShutdown {
            cause: StopCause::WalFlushFailed(vec![(Signal::Logs, fault)]),
            phase: ShutdownPhase::Storage,
            unfinished: Unfinished { ingest_requests: 0, queued: vec![], live_streams: 0 },
        };
        assert_eq!(
            e.to_string(),
            "shutdown incomplete: the final WAL flush failed (logs: walSync noSpace on wal/logs/000000000001.wal: \
             No space left on device). Events acknowledged since the last successful fsync reached the operating \
             system but may not be on disk: the next start replays them, but an OS crash or power loss before the \
             OS writes them out can lose them; fix the storage problem (docs/configuration.md#storage-failures)"
        );
    }

    /// Serve with a background job that ignores the shutdown signal, stop at
    /// once, and send a second stop request after `force_after` (if any).
    async fn stop_with_a_blocked_background_job(
        timeout: Duration,
        force_after: Option<Duration>,
    ) -> (IncompleteShutdown, Duration) {
        let dir = tempfile::tempdir().unwrap();
        let mut config = Config::default();
        config.storage.path = dir.path().to_path_buf();
        config.alerts.enabled = false;
        config.server.shutdown_timeout = timeout;
        let mut app = App::build(config).unwrap();
        app.tasks.push(tokio::spawn(std::future::pending::<()>()));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let force = async move {
            match force_after {
                Some(d) => tokio::time::sleep(d).await,
                None => std::future::pending().await,
            }
        };
        let began = std::time::Instant::now();
        let r = app.serve_until(listener, std::future::ready(()), force).await;
        let took = began.elapsed();
        (r.unwrap_err().downcast::<IncompleteShutdown>().unwrap(), took)
    }

    // REL-05 review: a second stop request is honoured while background jobs
    // are being waited for, not only during the HTTP drain and WAL flush.
    #[tokio::test(flavor = "multi_thread")]
    async fn second_stop_request_ends_a_blocked_background_job() {
        let (e, took) =
            stop_with_a_blocked_background_job(Duration::from_secs(60), Some(Duration::from_millis(200))).await;
        assert!(took < Duration::from_secs(2), "took {took:?}");
        assert_eq!((&e.cause, e.phase), (&StopCause::Forced, ShutdownPhase::Background));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn deadline_ends_a_blocked_background_job() {
        let (e, took) = stop_with_a_blocked_background_job(Duration::from_millis(300), None).await;
        assert!(took < Duration::from_secs(2), "took {took:?}");
        assert_eq!((&e.cause, e.phase), (&StopCause::Deadline(Duration::from_millis(300)), ShutdownPhase::Background));
    }
}
