use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use metadata::Metadata;
use query::{CancelToken, Engine};
use storage::{Batch, Storage};
use telemetry::{Signal, Timestamp};
use tokio::sync::{Semaphore, broadcast, watch};

use crate::config::{Config, MemoryBudgets};
use crate::error::{ApiError, ApiResult};
use crate::transport::Transport;

/// A committed batch, fanned out to live-tail subscribers.
#[derive(Clone)]
pub struct LiveBatch {
    pub signal: Signal,
    pub batch: Arc<Batch>,
}

pub struct AppState {
    pub config: Config,
    pub budgets: MemoryBudgets,
    pub storage: Arc<Storage>,
    pub engine: Arc<Engine>,
    pub metadata: Arc<Metadata>,
    pub live: broadcast::Sender<LiveBatch>,
    pub live_permits: Arc<Semaphore>,
    /// Query admission: one permit per executing query, held until the
    /// blocking work has returned (see [`AppState::query`]).
    pub query_permits: Arc<Semaphore>,
    /// How queries ended, for `/api/v1/system/info`.
    pub query_outcomes: Arc<QueryOutcomes>,
    /// Ingest admission: one permit per request from before its body is
    /// read until its write is acknowledged (see `routes::ingest`).
    pub ingest_permits: Arc<Semaphore>,
    /// Total permits in `ingest_permits`.
    pub ingest_max_concurrent: usize,
    /// How `ingest_max_concurrent` was chosen and its memory estimate.
    pub ingest_admission: crate::config::IngestAdmission,
    /// Ingest requests turned away because every permit was taken.
    pub ingest_rejected: AtomicU64,
    /// Bounds rejected ingest bodies being discarded at once.
    pub discard_permits: Arc<Semaphore>,
    /// Bounds rejected ingest bodies held at all (discarding or waiting).
    pub discard_pending: Arc<Semaphore>,
    /// Connection admission and transport limits (REL-18), shared by
    /// every listener this process serves.
    pub transport: Arc<Transport>,
    pub started_at: Instant,
    pub started_ts: Timestamp,
    pub received_bytes: AtomicU64,
    pub received_requests: AtomicU64,
    pub http: reqwest::Client,
    /// Every active API key; decides whether ingestion is protected. Also
    /// tracks their last-used writes (AUTH-PERF-03).
    pub api_keys: Arc<crate::auth::ApiKeys>,
    /// Session cache and lookup timings (AUTH-PERF-02).
    pub sessions: crate::auth::Sessions,
    /// Flips to `true` when shutdown starts: new ingest, queries and live
    /// streams are refused and live-tail streams end, so the drain only
    /// waits for work that was already accepted.
    pub shutdown: watch::Receiver<bool>,
}

pub type SharedState = Arc<AppState>;

/// Queries that did not complete, by reason (REL-12).
#[derive(Debug, Default)]
pub struct QueryOutcomes {
    /// Turned away: every slot stayed busy for `query.queue_timeout`.
    pub rejected: AtomicU64,
    /// Stopped because the client went away.
    pub cancelled: AtomicU64,
    /// Over `query.timeout`.
    pub timed_out: AtomicU64,
    /// Over `query.max_scan_bytes`.
    pub too_expensive: AtomicU64,
    /// Over a result budget (`query.max_trace_spans`, metric points).
    pub result_limited: AtomicU64,
}

impl QueryOutcomes {
    fn record(&self, e: &ApiError) {
        let counter = match e.code {
            "query_cancelled" => &self.cancelled,
            "query_timeout" => &self.timed_out,
            "query_too_expensive" => &self.too_expensive,
            "query_result_limit" => &self.result_limited,
            _ => return,
        };
        counter.fetch_add(1, Ordering::Relaxed);
    }
}

/// Cancels a query when the request handler is dropped: hyper drops it when
/// the client disconnects, and the query stops at its next checkpoint.
struct CancelOnDrop(CancelToken);

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

impl AppState {
    /// Shutdown has started; refuse new work.
    pub fn draining(&self) -> bool {
        *self.shutdown.borrow()
    }

    /// Run blocking work (SQLite, filesystem) off the async runtime.
    pub async fn blocking<T, F>(&self, f: F) -> ApiResult<T>
    where
        T: Send + 'static,
        F: FnOnce() -> ApiResult<T> + Send + 'static,
    {
        tokio::task::spawn_blocking(f).await?
    }

    pub async fn db<T, F>(&self, f: F) -> ApiResult<T>
    where
        T: Send + 'static,
        F: FnOnce(&Metadata) -> metadata::Result<T> + Send + 'static,
    {
        let m = self.metadata.clone();
        tokio::task::spawn_blocking(move || f(&m).map_err(ApiError::from)).await?
    }

    /// Run a query on the blocking pool under the concurrency limit. When
    /// all query slots stay busy for `query.queue_timeout`, fail with 503
    /// instead of queueing forever.
    ///
    /// `f` gets the request's [`CancelToken`]. It is cancelled if this
    /// future is dropped (the client disconnected), and the query then
    /// stops at its next checkpoint. Its slot stays taken until the blocking
    /// work has returned, so abandoned queries still count against
    /// `query.max_concurrent` and can never pile up beyond it.
    pub async fn query<T, F>(&self, f: F) -> ApiResult<T>
    where
        T: Send + 'static,
        F: FnOnce(&Engine, &CancelToken) -> ApiResult<T> + Send + 'static,
    {
        if self.draining() {
            return Err(ApiError::shutting_down());
        }
        let wait = self.config.query.queue_timeout;
        let permit = match tokio::time::timeout(wait, self.query_permits.clone().acquire_owned()).await {
            Ok(Ok(permit)) => permit,
            Ok(Err(_)) => return Err(ApiError::unavailable("shutting down")),
            Err(_) => {
                self.query_outcomes.rejected.fetch_add(1, Ordering::Relaxed);
                return Err(ApiError::unavailable(format!(
                    "all {} query slots are busy; try again shortly",
                    self.config.query.max_concurrent
                )));
            }
        };
        let engine = self.engine.clone();
        let outcomes = self.query_outcomes.clone();
        let guard = CancelOnDrop(CancelToken::new());
        let cancel = guard.0.clone();
        let r = tokio::task::spawn_blocking(move || {
            let _permit = permit;
            let r = f(&engine, &cancel);
            if let Err(e) = &r {
                outcomes.record(e);
                if e.code == "query_cancelled" {
                    tracing::debug!("query cancelled: the client disconnected");
                }
            }
            r
        })
        .await?;
        drop(guard);
        r
    }
}
