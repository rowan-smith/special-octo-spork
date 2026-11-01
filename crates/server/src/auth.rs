//! Authentication: API keys for ingestion, admin sessions for the UI.
//!
//! * API keys are random 32-byte tokens shown once; only their SHA-256 is
//!   stored (keys are high-entropy, so a fast hash is appropriate).
//! * The admin password is hashed with Argon2id.
//! * Sessions are random tokens in an HttpOnly, SameSite=Lax cookie; only
//!   their SHA-256 is stored.
//!
//! The UI and API always need a session or an admin key. Ingestion needs an
//! API key while at least one ingest-scoped key exists, and is open while
//! none does (see [`ApiKeys`]). The boundaries are kept small so OIDC etc.
//! can be added later as another way of producing a [`Principal`].

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use argon2::password_hash::phc::PasswordHash;
use argon2::{Argon2, PasswordHasher, PasswordVerifier};
use axum::extract::{Request, State};
use axum::http::{HeaderMap, header};
use axum::middleware::Next;
use axum::response::Response;
use metadata::Metadata;
use rand::Rng;
use sha2::{Digest, Sha256};
use tokio::sync::{Mutex, Semaphore};

use crate::error::{ApiError, ApiResult};
use crate::state::{AppState, SharedState};

pub const SESSION_COOKIE: &str = "vyrtel_session";

#[derive(Debug, Clone)]
pub enum Principal {
    User(String),
    ApiKey { id: i64, scope: String },
}

pub fn sha256_hex(s: &str) -> String {
    hex::encode(Sha256::digest(s.as_bytes()))
}

pub fn random_token(bytes: usize) -> String {
    let mut b = vec![0u8; bytes];
    rand::rng().fill_bytes(&mut b);
    hex::encode(b)
}

/// Generate an API key: returns (plaintext, display prefix, hash).
pub fn new_api_key() -> (String, String, String) {
    let key = format!("vyr_{}", random_token(32));
    let prefix = key[..12].to_string();
    let hash = sha256_hex(&key);
    (key, prefix, hash)
}

pub fn hash_password(password: &str) -> Result<String, String> {
    PasswordHasher::<PasswordHash>::hash_password(&Argon2::default(), password.as_bytes())
        .map(|h| h.to_string())
        .map_err(|e| e.to_string())
}

pub fn verify_password(password: &str, hash: &str) -> bool {
    Argon2::default().verify_password(password.as_bytes(), hash).is_ok()
}

/// `Authorization: Bearer <key>` or `X-Api-Key: <key>`.
pub fn api_key_from(headers: &HeaderMap) -> Option<String> {
    if let Some(v) = headers.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()) {
        let v = v.trim();
        if let Some(k) = v.strip_prefix("Bearer ").or_else(|| v.strip_prefix("bearer ")) {
            return Some(k.trim().to_string());
        }
    }
    headers.get("x-api-key").and_then(|v| v.to_str().ok()).map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

pub fn session_from(headers: &HeaderMap) -> Option<String> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .filter_map(|c| c.trim().split_once('='))
        .find(|(k, _)| *k == SESSION_COOKIE)
        .map(|(_, v)| v.to_string())
        .filter(|v| !v.is_empty())
}

/// Most API key changes waiting for or running their SQLite write at once.
/// Further changes are refused with 503 instead of queueing without bound.
pub const MAX_PENDING_KEY_CHANGES: usize = 16;

/// The active API keys at one moment, by SHA-256 of the key with its id
/// and scope. Ingestion is protected while it holds an ingest-scoped key;
/// admin keys do not count. Never changed once published: a change builds
/// the next set (see [`ApiKeys`]).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct KeySet {
    by_hash: HashMap<String, (i64, String)>,
    ingest: usize,
}

impl KeySet {
    /// Whether ingestion needs an API key.
    pub fn ingest_protected(&self) -> bool {
        self.ingest > 0
    }

    /// The `(id, scope)` of an active key.
    fn find(&self, key: &str) -> Result<(i64, String), ApiError> {
        self.by_hash.get(&sha256_hex(key)).cloned().ok_or_else(|| ApiError::unauthorized("invalid API key"))
    }

    /// Whether an ingest request with `headers` is admitted: `None` while
    /// ingestion is open, else the key's `(id, scope)`. Policy and key come
    /// from this one set, so a key change between them cannot mix two.
    fn admit_ingest(&self, headers: &HeaderMap) -> Result<Option<(i64, String)>, ApiError> {
        if !self.ingest_protected() {
            return Ok(None);
        }
        let Some(key) = api_key_from(headers) else {
            return Err(ApiError::unauthorized(
                "missing API key; send 'Authorization: Bearer <key>' or 'X-Api-Key: <key>'",
            ));
        };
        self.find(&key).map(Some)
    }

    fn add(&mut self, hash: String, id: i64, scope: String) {
        self.ingest += usize::from(scope == "ingest");
        self.by_hash.insert(hash, (id, scope));
    }

    fn ids(&self) -> HashSet<i64> {
        self.by_hash.values().map(|(id, _)| *id).collect()
    }

    fn has_id(&self, id: i64) -> bool {
        self.by_hash.values().any(|(key_id, _)| *key_id == id)
    }

    fn remove(&mut self, id: i64) {
        let Self { by_hash, ingest } = self;
        by_hash.retain(|_, (key_id, scope)| {
            let revoked = *key_id == id;
            if revoked && scope == "ingest" {
                *ingest -= 1;
            }
            !revoked
        });
    }
}

/// The API key registry. Loaded at startup and changed only through
/// [`ApiKeys::change`], so authenticating a request by API key never reads
/// SQLite. (Sessions are cached, but a miss reads SQLite: see [`Sessions`].)
///
/// Requests read the current [`KeySet`] by cloning an `Arc` under a read
/// lock that is only ever held for a pointer copy or swap, so they never
/// wait for SQLite. Changes take turns on an async mutex (at most
/// [`MAX_PENDING_KEY_CHANGES`] waiting or running), write SQLite on the
/// blocking pool, and only after the write commits publish the next set.
/// Lock order: `changes`, then `current`; `touches`, then `current`;
/// `current` is never held across anything else. It also keeps the keys'
/// last-used bookkeeping (see [`ApiKeys::touch`]).
pub struct ApiKeys {
    current: RwLock<Arc<KeySet>>,
    changes: Arc<Mutex<()>>,
    pending: Arc<Semaphore>,
    touches: std::sync::Mutex<Touches>,
    touch_stats: TouchStats,
    write_touches: TouchWrite,
}

impl ApiKeys {
    /// A registry whose last-used writes go nowhere (for tests).
    pub fn new(keys: KeySet) -> Self {
        Self::with_touch_writer(keys, Arc::new(|ids: &[i64]| Ok(ids.len())))
    }

    /// A registry whose last-used writes are made by `write` (the ids to
    /// touch; returns how many were updated), on the blocking pool.
    pub fn with_touch_writer(keys: KeySet, write: TouchWrite) -> Self {
        Self {
            current: RwLock::new(Arc::new(keys)),
            changes: Arc::new(Mutex::new(())),
            pending: Arc::new(Semaphore::new(MAX_PENDING_KEY_CHANGES)),
            touches: Default::default(),
            touch_stats: Default::default(),
            write_touches: write,
        }
    }

    pub fn load(m: &Arc<Metadata>) -> metadata::Result<Self> {
        let mut keys = KeySet::default();
        for (id, hash, scope) in m.active_api_keys()? {
            keys.add(hash, id, scope);
        }
        let m = m.clone();
        Ok(Self::with_touch_writer(keys, Arc::new(move |ids: &[i64]| m.touch_api_keys(ids))))
    }

    /// The current keys. Hold the result only as long as one decision
    /// needs it, so superseded sets are freed.
    pub fn snapshot(&self) -> Arc<KeySet> {
        self.current.read().unwrap().clone()
    }

    /// Whether ingestion needs an API key.
    pub fn ingest_protected(&self) -> bool {
        self.snapshot().ingest_protected()
    }

    /// Run `write` (the SQLite change) on the blocking pool, then publish
    /// the current set changed by `apply`. Changes run one at a time, in
    /// the order they get their turn.
    ///
    /// A change is published only after its write commits and before this
    /// returns, so a request that starts after it returns sees it. A failed
    /// (or panicking) write publishes nothing. Dropping this future while
    /// it waits for its turn cancels the change; once the write has started
    /// it runs to the end and is published, and keeps its turn until then,
    /// so a later change can never be overtaken by it. Requests admitted
    /// before a change are not affected by it, except that the last-used
    /// bookkeeping of keys it removes is dropped in the same step.
    pub async fn change<T, W, A>(self: &Arc<Self>, write: W, apply: A) -> ApiResult<T>
    where
        T: Send + 'static,
        W: FnOnce() -> metadata::Result<T> + Send + 'static,
        A: FnOnce(&mut KeySet, &T) + Send + 'static,
    {
        let Ok(queued) = self.pending.clone().try_acquire_owned() else {
            return Err(ApiError::unavailable("too many API key changes in progress; try again shortly"));
        };
        let turn = self.changes.clone().lock_owned().await;
        let keys = self.clone();
        tokio::task::spawn_blocking(move || {
            let _held = (queued, turn);
            let out = write()?;
            let mut next = KeySet::clone(&keys.snapshot());
            apply(&mut next, &out);
            let next = Arc::new(next);
            *keys.current.write().unwrap() = next.clone();
            // After the swap: a touch that checks the current set from
            // here on finds a removed key gone (see `touch_at`).
            keys.forget_removed(&next);
            Ok(out)
        })
        .await?
    }

    /// See [`create_api_key`].
    pub async fn create(
        self: &Arc<Self>,
        m: &Arc<Metadata>,
        name: &str,
        scope: &str,
    ) -> ApiResult<(metadata::ApiKey, String)> {
        let (key, prefix, hash) = new_api_key();
        let (m, name, scope, stored) = (m.clone(), name.to_string(), scope.to_string(), hash.clone());
        let write = move || m.create_api_key(&name, &prefix, &stored, &scope);
        let rec = self.change(write, |keys, rec| keys.add(hash, rec.id, rec.scope.clone())).await?;
        Ok((rec, key))
    }

    /// See [`revoke_api_key`].
    pub async fn revoke(self: &Arc<Self>, m: &Arc<Metadata>, id: i64) -> ApiResult<()> {
        let m = m.clone();
        self.change(move || m.revoke_api_key(id), move |keys, ()| keys.remove(id)).await
    }

    /// Note that key `id`, admitted by `admitted` (it must be in that set),
    /// was used: see [`Touches`].
    /// Never fails and never waits for SQLite; the request goes ahead
    /// whatever this decides.
    pub fn touch(self: &Arc<Self>, id: i64, admitted: &Arc<KeySet>) -> Touch {
        self.touch_at(id, admitted, Instant::now())
    }

    /// [`ApiKeys::touch`] at `now` (a fake clock in tests).
    fn touch_at(self: &Arc<Self>, id: i64, admitted: &Arc<KeySet>, now: Instant) -> Touch {
        let s = &self.touch_stats;
        let mut t = self.touches.lock().unwrap();
        if t.last.get(&id).is_some_and(|&last| !t.stale(last, now)) {
            return Touch::Throttled;
        }
        // Checked under `touches`, which `forget_removed` takes only after
        // publishing: either this sees the key gone, or its entry is
        // removed after this inserts it.
        let current = self.snapshot();
        if !Arc::ptr_eq(&current, admitted) && !current.has_id(id) {
            s.retired.fetch_add(1, Ordering::Relaxed);
            return Touch::Retired;
        }
        if !t.pending.contains(&id) && t.pending.len() >= MAX_PENDING_KEY_TOUCHES {
            s.dropped.fetch_add(1, Ordering::Relaxed);
            return Touch::Dropped;
        }
        if !t.last.contains_key(&id) && t.last.len() >= KEY_TOUCH_ENTRIES && !t.sweep(now) {
            s.deferred.fetch_add(1, Ordering::Relaxed);
            return Touch::Deferred;
        }
        t.remember(id, now);
        t.pending.insert(id);
        s.queued.fetch_add(1, Ordering::Relaxed);
        s.peak_pending.fetch_max(t.pending.len() as u64, Ordering::Relaxed);
        let start = !t.writing;
        t.writing = true;
        drop(t);
        if start {
            let writer = TouchWriter::new(self.clone());
            tokio::task::spawn_blocking(move || writer.run());
        }
        Touch::Queued
    }

    /// Drop the bookkeeping of every key not in `keys` (just published).
    fn forget_removed(&self, keys: &KeySet) {
        let mut t = self.touches.lock().unwrap();
        if t.last.is_empty() && t.pending.is_empty() {
            return;
        }
        let ids = keys.ids();
        let before = t.last.len();
        t.last.retain(|id, _| ids.contains(id));
        t.pending.retain(|id| ids.contains(id));
        self.touch_stats.forgotten.fetch_add((before - t.last.len()) as u64, Ordering::Relaxed);
    }

    pub fn touch_snapshot(&self) -> serde_json::Value {
        let s = &self.touch_stats;
        let n = |c: &AtomicU64| c.load(Ordering::Relaxed);
        let (tracked, pending, writing) = {
            let t = self.touches.lock().unwrap();
            (t.last.len(), t.pending.len(), t.writing)
        };
        serde_json::json!({
            "tracked": tracked, "trackedCapacity": KEY_TOUCH_ENTRIES,
            "trackedBytes": tracked * KEY_TOUCH_ENTRY_BYTES,
            "trackedByteCapacity": KEY_TOUCH_ENTRIES * KEY_TOUCH_ENTRY_BYTES,
            "pending": pending, "pendingCapacity": MAX_PENDING_KEY_TOUCHES, "peakPending": n(&s.peak_pending),
            "writers": usize::from(writing), "writerCapacity": 1,
            "queued": n(&s.queued), "written": n(&s.written), "batches": n(&s.batches),
            "failedBatches": n(&s.failed_batches), "abandoned": n(&s.abandoned),
            "dropped": n(&s.dropped), "deferred": n(&s.deferred), "retired": n(&s.retired),
            "forgotten": n(&s.forgotten),
        })
    }
}

/// How often a key's last-used time is persisted at most: a key used again
/// within this of its last queued write is not written (the pre-existing
/// one-minute throttle).
pub const KEY_TOUCH_INTERVAL: Duration = Duration::from_secs(60);

/// Most keys whose last write time is remembered. Only entries younger than
/// [`KEY_TOUCH_INTERVAL`] matter (an older one would be written anyway), so
/// a full map evicts only those; while every entry is younger, a key not in
/// it is not written (`deferred`) until one ages out, so eviction can never
/// turn requests into writes.
pub const KEY_TOUCH_ENTRIES: usize = 4096;

/// What one remembered key costs, counted generously: the id and time and
/// the map's slot and control byte. 4096 entries: 192 KiB.
pub const KEY_TOUCH_ENTRY_BYTES: usize = 48;

/// Most keys waiting for their last-used write. Further keys are not
/// written (`dropped`) until the queue drains: they are tried again on
/// their next request.
pub const MAX_PENDING_KEY_TOUCHES: usize = 1024;

/// Writes last-used times: given the ids, sets them to now and returns how
/// many were updated.
pub type TouchWrite = Arc<dyn Fn(&[i64]) -> metadata::Result<usize> + Send + Sync>;

/// What [`ApiKeys::touch`] did. Only bookkeeping: the request goes ahead
/// in every case.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Touch {
    /// Queued for writing.
    Queued,
    /// Written or queued less than [`KEY_TOUCH_INTERVAL`] ago.
    Throttled,
    /// The key was revoked after this request was admitted.
    Retired,
    /// [`MAX_PENDING_KEY_TOUCHES`] keys are already waiting.
    Dropped,
    /// [`KEY_TOUCH_ENTRIES`] keys were all written too recently to forget.
    Deferred,
}

/// API key last-used bookkeeping (AUTH-PERF-03). `last_used_at` is
/// approximate and informational: nothing is decided by it.
///
/// * A key is written at most once per [`KEY_TOUCH_INTERVAL`] (from when it
///   was queued, whether or not the write then succeeds).
/// * At most [`KEY_TOUCH_ENTRIES`] keys are remembered (192 KiB), and only
///   active ones: a revoke drops its key's entry when it publishes, and a
///   request admitted before the revoke cannot add it back.
/// * At most [`MAX_PENDING_KEY_TOUCHES`] keys wait, each once however
///   often it is used, and one blocking thread writes them, as many as are
///   waiting in one transaction. Nothing else is spawned or waited on.
/// * A failed write keeps its keys throttled (retried after the interval).
///   A panicking writer, or one that never runs (shutdown), gives back its
///   slot, the waiting keys and the batch it was writing, unthrottled.
///   Waiting keys are not written at exit.
#[derive(Default)]
struct Touches {
    /// When each key was last queued for writing.
    last: HashMap<i64, Instant>,
    /// No entry of `last` is stale before this (a lower bound), so a full
    /// map is swept only when it can free something.
    stale_from: Option<Instant>,
    /// Keys waiting for the writer.
    pending: HashSet<i64>,
    /// The writer is running (there is at most one).
    writing: bool,
    /// The last write failed (to log failures once, not per batch).
    failing: bool,
}

impl Touches {
    fn stale(&self, last: Instant, now: Instant) -> bool {
        now.saturating_duration_since(last) > KEY_TOUCH_INTERVAL
    }

    fn remember(&mut self, id: i64, now: Instant) {
        self.last.insert(id, now);
        let at = now + KEY_TOUCH_INTERVAL;
        self.stale_from = Some(self.stale_from.map_or(at, |s| s.min(at)));
    }

    /// Forget stale entries; whether there is room now.
    fn sweep(&mut self, now: Instant) -> bool {
        if self.stale_from.is_some_and(|s| now <= s) {
            return false;
        }
        self.last.retain(|_, last| now.saturating_duration_since(*last) <= KEY_TOUCH_INTERVAL);
        self.stale_from = self.last.values().min().map(|&oldest| oldest + KEY_TOUCH_INTERVAL);
        self.last.len() < KEY_TOUCH_ENTRIES
    }
}

/// Last-used bookkeeping counted since the process started, reported by
/// `/api/v1/system/info` (`apiKeyTouch`). Only atomics; no key ids.
#[derive(Debug, Default)]
struct TouchStats {
    queued: AtomicU64,
    written: AtomicU64,
    batches: AtomicU64,
    failed_batches: AtomicU64,
    /// Keys, waiting or in the batch being written, given up because the
    /// writer panicked or never ran.
    abandoned: AtomicU64,
    dropped: AtomicU64,
    deferred: AtomicU64,
    retired: AtomicU64,
    forgotten: AtomicU64,
    peak_pending: AtomicU64,
}

/// The one writer: writes waiting keys until none are left. However it
/// ends (a panic, or never running because the runtime shut down), the
/// writer slot and the keys it did not write, waiting or in the batch it
/// was writing, are given back.
struct TouchWriter {
    keys: Arc<ApiKeys>,
    /// The batch being written: taken from `pending`, not yet answered by
    /// the write.
    batch: Vec<i64>,
    finished: bool,
}

impl TouchWriter {
    fn new(keys: Arc<ApiKeys>) -> Self {
        Self { keys, batch: Vec::new(), finished: false }
    }

    fn run(mut self) {
        loop {
            {
                let mut t = self.keys.touches.lock().unwrap();
                if t.pending.is_empty() {
                    t.writing = false;
                    self.finished = true;
                    return;
                }
                self.batch = t.pending.drain().collect();
            }
            let r = (self.keys.write_touches)(&self.batch);
            // Answered (even by an error): no longer the writer's to give back.
            let batch = std::mem::take(&mut self.batch);
            let s = &self.keys.touch_stats;
            s.batches.fetch_add(1, Ordering::Relaxed);
            let mut t = self.keys.touches.lock().unwrap();
            match r {
                Ok(n) => {
                    s.written.fetch_add(n as u64, Ordering::Relaxed);
                    if std::mem::take(&mut t.failing) {
                        tracing::info!("recording API key last-used times again");
                    }
                }
                Err(e) => {
                    s.failed_batches.fetch_add(1, Ordering::Relaxed);
                    // These keys are tried again after the interval, not at
                    // once, so a failing database gets no more writes.
                    if !std::mem::replace(&mut t.failing, true) {
                        tracing::warn!(keys = batch.len(), "could not record API key last-used times: {e}");
                    }
                }
            }
        }
    }
}

impl Drop for TouchWriter {
    fn drop(&mut self) {
        if self.finished {
            return;
        }
        let mut t = self.keys.touches.lock().unwrap_or_else(|p| p.into_inner());
        t.writing = false;
        // Unwritten, so not throttled: each is queued again on its next use.
        let mut abandoned = std::mem::take(&mut t.pending);
        abandoned.extend(self.batch.drain(..));
        for id in &abandoned {
            t.last.remove(id);
        }
        self.keys.touch_stats.abandoned.fetch_add(abandoned.len() as u64, Ordering::Relaxed);
    }
}

/// Store a new key and accept it; an ingest key also protects ingestion
/// from here on. Returns the record and the plaintext key.
pub async fn create_api_key(state: &AppState, name: &str, scope: &str) -> ApiResult<(metadata::ApiKey, String)> {
    state.api_keys.create(&state.metadata, name, scope).await
}

/// Revoke a key; revoking the last ingest key opens ingestion.
pub async fn revoke_api_key(state: &AppState, id: i64) -> ApiResult<()> {
    state.api_keys.revoke(&state.metadata, id).await
}

fn check_api_key(state: &AppState, key: &str) -> Result<Principal, ApiError> {
    let keys = state.api_keys.snapshot();
    let (id, scope) = keys.find(key)?;
    state.api_keys.touch(id, &keys);
    Ok(Principal::ApiKey { id, scope })
}

/// Most sessions [`Sessions`] keeps.
pub const SESSION_CACHE_ENTRIES: usize = 4096;

/// Most bytes [`Sessions`] keeps, counted per entry as its key and user name
/// plus [`SESSION_ENTRY_OVERHEAD`]. With the default `admin` user an entry
/// is about 165 bytes, so the entry cap is reached first (about 660 KiB).
pub const SESSION_CACHE_BYTES: usize = 1024 * 1024;

/// What an entry costs beyond its key and user name: the two `String`
/// headers, the expiry, and the map's slot and control byte, rounded up.
pub const SESSION_ENTRY_OVERHEAD: usize = 96;

/// Sessions of users with longer names are not cached (they are still
/// authenticated, by reading SQLite every time), so one entry is always a
/// small part of [`SESSION_CACHE_BYTES`] whatever `auth.admin_username` is.
pub const MAX_CACHED_USERNAME: usize = 256;

/// Most session lookups reading SQLite at once. Further lookups wait for a
/// turn without holding a thread; how many can wait is bounded by the
/// connection limit (`server.max_connections`).
pub const MAX_SESSION_FILLS: usize = 16;

/// Where request authentication by session cookie spends its time, counted
/// since the process started and reported by `/api/v1/system/info`
/// (AUTH-PERF-02). Each SQLite lookup is split into its wait for a turn and
/// a blocking thread (`queue`), its wait for the metadata connection
/// (`lock`, held by any other metadata read or write) and the lookup
/// itself (`query`). Only atomics.
#[derive(Debug, Default)]
struct SessionLookups {
    hits: AtomicU64,
    lookups: AtomicU64,
    found: AtomicU64,
    failed: AtomicU64,
    in_flight: AtomicU64,
    peak_in_flight: AtomicU64,
    queue_us: AtomicU64,
    lock_us: AtomicU64,
    query_us: AtomicU64,
    max_queue_us: AtomicU64,
    max_lock_us: AtomicU64,
    max_query_us: AtomicU64,
}

/// One lookup in flight; leaving (even by cancellation) ends it.
struct InFlight<'a>(&'a SessionLookups);

impl Drop for InFlight<'_> {
    fn drop(&mut self) {
        self.0.in_flight.fetch_sub(1, Ordering::Relaxed);
    }
}

impl SessionLookups {
    fn begin(&self) -> InFlight<'_> {
        let now = self.in_flight.fetch_add(1, Ordering::Relaxed) + 1;
        self.peak_in_flight.fetch_max(now, Ordering::Relaxed);
        InFlight(self)
    }

    fn record(&self, found: Option<bool>, queue: Duration, lock: Duration, query: Duration) {
        let add = |total: &AtomicU64, max: &AtomicU64, d: Duration| {
            let us = d.as_micros().min(u64::MAX as u128) as u64;
            total.fetch_add(us, Ordering::Relaxed);
            max.fetch_max(us, Ordering::Relaxed);
        };
        self.lookups.fetch_add(1, Ordering::Relaxed);
        match found {
            Some(true) => self.found.fetch_add(1, Ordering::Relaxed),
            Some(false) => 0,
            None => self.failed.fetch_add(1, Ordering::Relaxed),
        };
        add(&self.queue_us, &self.max_queue_us, queue);
        add(&self.lock_us, &self.max_lock_us, lock);
        add(&self.query_us, &self.max_query_us, query);
    }
}

/// A cached session: never used at or after `expires_at` (ms since the
/// epoch), the same boundary as the SQLite lookup.
struct Cached {
    username: String,
    expires_at: i64,
}

fn entry_bytes(hash: &str, username: &str) -> usize {
    hash.len() + username.len() + SESSION_ENTRY_OVERHEAD
}

#[derive(Default)]
struct Cache {
    by_hash: HashMap<String, Cached>,
    /// The [`entry_bytes`] of every entry, kept in step with `by_hash`.
    bytes: usize,
    /// Advanced by every invalidation. A lookup caches its result only if
    /// no invalidation happened since before it read SQLite, so a lookup
    /// racing a logout can never put the ended session back.
    epoch: u64,
}

impl Cache {
    fn remove(&mut self, hash: &str) {
        if let Some((hash, e)) = self.by_hash.remove_entry(hash) {
            self.bytes -= entry_bytes(&hash, &e.username);
        }
    }

    /// Make room for an entry of `size` bytes: expired entries go first,
    /// then arbitrary ones, until both caps allow it.
    fn make_room(&mut self, size: usize, now: i64) {
        let full = |c: &Self| c.by_hash.len() >= SESSION_CACHE_ENTRIES || c.bytes + size > SESSION_CACHE_BYTES;
        if !full(self) {
            return;
        }
        let expired: Vec<String> =
            self.by_hash.iter().filter(|(_, e)| e.expires_at <= now).map(|(h, _)| h.clone()).collect();
        for hash in expired {
            self.remove(&hash);
        }
        while full(self) {
            let Some(evict) = self.by_hash.keys().next().cloned() else { break };
            self.remove(&evict);
        }
    }
}

/// Sessions for request authentication (AUTH-PERF-02): a bounded cache of
/// valid sessions in front of the SQLite lookup, so repeat requests from
/// one session do not queue for the metadata connection.
///
/// * Keyed by the token's SHA-256, as stored; the token itself is never
///   kept.
/// * Only sessions found valid are cached (no negative entries), so unknown
///   tokens cannot grow it; at most [`SESSION_CACHE_ENTRIES`] entries and
///   [`SESSION_CACHE_BYTES`], evicting expired entries first, then
///   arbitrary ones; users with names over [`MAX_CACHED_USERNAME`] bytes
///   are never cached.
/// * An entry is used only before its session's expiry.
/// * Logout deletes the session in SQLite, then [`Sessions::invalidate`]s
///   it before responding: from then on no request is admitted with it,
///   and no lookup that started earlier can cache it again. A changed
///   admin password ends all of the user's sessions at startup
///   ([`bootstrap_admin`]).
/// * A miss reads SQLite as before, at most [`MAX_SESSION_FILLS`] at once;
///   a database error fails the request (`500`) and caches nothing.
/// * In memory only: a restart starts empty and reads SQLite again.
pub struct Sessions {
    cache: std::sync::Mutex<Cache>,
    fills: Arc<Semaphore>,
    stats: SessionLookups,
}

impl Default for Sessions {
    fn default() -> Self {
        Self {
            cache: Default::default(),
            fills: Arc::new(Semaphore::new(MAX_SESSION_FILLS)),
            stats: Default::default(),
        }
    }
}

impl Sessions {
    /// The user of an unexpired cached session.
    fn cached(&self, hash: &str, now: i64) -> Option<String> {
        let mut c = self.cache.lock().unwrap();
        match c.by_hash.get(hash) {
            Some(e) if e.expires_at > now => Some(e.username.clone()),
            Some(_) => {
                c.remove(hash);
                None
            }
            None => None,
        }
    }

    fn epoch(&self) -> u64 {
        self.cache.lock().unwrap().epoch
    }

    /// Cache a session found valid by a lookup that began at `epoch`.
    fn fill(&self, hash: String, username: String, expires_at: i64, epoch: u64) {
        let mut c = self.cache.lock().unwrap();
        if c.epoch != epoch || username.len() > MAX_CACHED_USERNAME {
            return;
        }
        c.remove(&hash);
        let size = entry_bytes(&hash, &username);
        c.make_room(size, metadata::now_ms());
        c.bytes += size;
        c.by_hash.insert(hash, Cached { username, expires_at });
    }

    /// Forget session `hash` (by its token's SHA-256), and stop every lookup
    /// already under way from caching what it read.
    pub fn invalidate(&self, hash: &str) {
        let mut c = self.cache.lock().unwrap();
        c.epoch += 1;
        c.remove(hash);
    }

    /// Forget every session, as [`Sessions::invalidate`] does one (for a
    /// password change, which ends all of a user's sessions).
    pub fn clear(&self) {
        let mut c = self.cache.lock().unwrap();
        c.epoch += 1;
        c.by_hash.clear();
        c.bytes = 0;
    }

    /// The user of session `hash`, from the cache or else from `lookup` (the
    /// SQLite read: the user and expiry if valid, and how long it waited for
    /// the connection), run on the blocking pool.
    pub async fn check<F>(&self, hash: String, lookup: F) -> ApiResult<Option<String>>
    where
        F: FnOnce(&str) -> metadata::Result<(Option<(String, i64)>, Duration)> + Send + 'static,
    {
        if let Some(user) = self.cached(&hash, metadata::now_ms()) {
            self.stats.hits.fetch_add(1, Ordering::Relaxed);
            return Ok(Some(user));
        }
        let _in_flight = self.stats.begin();
        let submitted = Instant::now();
        let Ok(turn) = self.fills.clone().acquire_owned().await else {
            return Err(ApiError::unavailable("shutting down"));
        };
        // Before the read: an invalidation from here on discards its result.
        let epoch = self.epoch();
        let key = hash.clone();
        let (r, queue, lock, query) = tokio::task::spawn_blocking(move || {
            let _turn = turn;
            let queue = submitted.elapsed();
            let started = Instant::now();
            let r = lookup(&key);
            let ran = started.elapsed();
            let lock = r.as_ref().map_or(Duration::ZERO, |(_, lock)| *lock);
            (r, queue, lock, ran.saturating_sub(lock))
        })
        .await?;
        self.stats.record(r.as_ref().ok().map(|(found, _)| found.is_some()), queue, lock, query);
        let (found, _) = r?;
        Ok(found.map(|(username, expires_at)| {
            self.fill(hash, username.clone(), expires_at, epoch);
            username
        }))
    }

    pub fn snapshot(&self) -> serde_json::Value {
        let s = &self.stats;
        let n = |c: &AtomicU64| c.load(Ordering::Relaxed);
        let (cached, cached_bytes) = {
            let c = self.cache.lock().unwrap();
            (c.by_hash.len(), c.bytes)
        };
        serde_json::json!({
            "cacheHits": n(&s.hits), "cached": cached, "cacheCapacity": SESSION_CACHE_ENTRIES,
            "cachedBytes": cached_bytes, "cacheByteCapacity": SESSION_CACHE_BYTES,
            "lookups": n(&s.lookups), "found": n(&s.found), "failed": n(&s.failed),
            "inFlight": n(&s.in_flight), "peakInFlight": n(&s.peak_in_flight),
            "queueMicros": n(&s.queue_us), "lockMicros": n(&s.lock_us), "queryMicros": n(&s.query_us),
            "maxQueueMicros": n(&s.max_queue_us), "maxLockMicros": n(&s.max_lock_us),
            "maxQueryMicros": n(&s.max_query_us),
        })
    }
}

/// The session's user, from [`Sessions`] or SQLite. A lookup reads through
/// the metadata store's one connection, so it can wait behind any metadata
/// write, a key change's included.
async fn check_session(state: &SharedState, token: String) -> Result<Option<Principal>, ApiError> {
    let m = state.metadata.clone();
    let lookup = move |hash: &str| {
        let (found, lock) = m.find_session_timed(hash)?;
        Ok((found.map(|(user, expires_at)| (user.username, expires_at)), lock))
    };
    Ok(state.sessions.check(sha256_hex(&token), lookup).await?.map(Principal::User))
}

/// Ingestion endpoints: open while no ingest-scoped key exists, otherwise
/// any valid API key. Both are decided from one [`KeySet`].
pub async fn require_ingest(
    State(state): State<SharedState>,
    mut req: Request,
    next: Next,
) -> Result<Response, ApiError> {
    let admitted = {
        let keys = state.api_keys.snapshot();
        let admitted = keys.admit_ingest(req.headers())?;
        if let Some((id, _)) = &admitted {
            state.api_keys.touch(*id, &keys);
        }
        admitted
    };
    if let Some((id, scope)) = admitted {
        req.extensions_mut().insert(Principal::ApiKey { id, scope });
    }
    Ok(next.run(req).await)
}

/// Everything else under /api/v1: an admin session or an admin-scoped key.
pub async fn require_user(
    State(state): State<SharedState>,
    mut req: Request,
    next: Next,
) -> Result<Response, ApiError> {
    if let Some(token) = session_from(req.headers())
        && let Some(p) = check_session(&state, token).await?
    {
        req.extensions_mut().insert(p);
        return Ok(next.run(req).await);
    }
    if let Some(key) = api_key_from(req.headers()) {
        let p = check_api_key(&state, &key)?;
        if let Principal::ApiKey { scope, .. } = &p
            && scope != "admin"
        {
            return Err(ApiError::forbidden("this API key may only ingest data"));
        }
        req.extensions_mut().insert(p);
        return Ok(next.run(req).await);
    }
    Err(ApiError::unauthorized("authentication required"))
}

/// Create/refresh the admin user at startup. Returns a generated password
/// when none was configured and no user exists yet.
///
/// A changed `auth.admin_password` ends every session of the admin user in
/// the same transaction as the new hash; an unchanged one keeps them.
pub fn bootstrap_admin(state: &SharedState) -> anyhow::Result<Option<String>> {
    let auth = &state.config.auth;
    let m = &state.metadata;
    if let Some(pw) = &auth.admin_password {
        let existing = m.find_user(&auth.admin_username)?;
        // Only re-hash when the configured password changed.
        if !existing.is_some_and(|u| verify_password(pw, &u.password_hash)) {
            let hash = hash_password(pw).map_err(anyhow::Error::msg)?;
            let (_, ended) = m.rotate_password(&auth.admin_username, &hash)?;
            // Nothing is cached before the server starts; cleared anyway so
            // the order can never matter.
            state.sessions.clear();
            if ended > 0 {
                tracing::info!(ended, "admin password changed; signed out its existing sessions");
            }
        }
        return Ok(None);
    }
    if m.count_users()? == 0 {
        let pw = random_token(12);
        m.upsert_user(&auth.admin_username, &hash_password(&pw).map_err(anyhow::Error::msg)?)?;
        return Ok(Some(pw));
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    #[test]
    fn passwords() {
        let h = hash_password("hunter2").unwrap();
        assert!(h.starts_with("$argon2id$"));
        assert!(verify_password("hunter2", &h));
        assert!(!verify_password("hunter3", &h));
        assert!(!verify_password("x", "not a hash"));
    }

    #[test]
    fn api_keys() {
        let (key, prefix, hash) = new_api_key();
        assert!(key.starts_with("vyr_") && key.len() == 68);
        assert!(key.starts_with(&prefix));
        assert_eq!(hash, sha256_hex(&key));
        assert_ne!(new_api_key().0, key);
    }

    #[test]
    fn header_parsing() {
        let mut h = HeaderMap::new();
        h.insert(header::AUTHORIZATION, HeaderValue::from_static("Bearer abc"));
        assert_eq!(api_key_from(&h).as_deref(), Some("abc"));
        let mut h = HeaderMap::new();
        h.insert("x-api-key", HeaderValue::from_static(" k1 "));
        assert_eq!(api_key_from(&h).as_deref(), Some("k1"));
        let mut h = HeaderMap::new();
        h.insert(header::AUTHORIZATION, HeaderValue::from_static("Basic Zm9v"));
        assert_eq!(api_key_from(&h), None);
        let mut h = HeaderMap::new();
        h.insert(header::COOKIE, HeaderValue::from_static("a=1; vyrtel_session=tok; b=2"));
        assert_eq!(session_from(&h).as_deref(), Some("tok"));
    }

    #[test]
    fn only_ingest_keys_protect_ingestion() {
        let mut k = KeySet::default();
        k.add("a".into(), 1, "admin".into());
        assert!(!k.ingest_protected());
        k.add("i1".into(), 2, "ingest".into());
        k.add("i2".into(), 3, "ingest".into());
        assert!(k.ingest_protected());
        k.remove(2);
        assert!(k.ingest_protected());
        k.remove(1);
        k.remove(99);
        assert!(k.ingest_protected());
        k.remove(3);
        assert!(!k.ingest_protected());
        assert!(k.by_hash.is_empty());
    }

    // ---- AUTH-PERF-01: key changes off the request path -------------------

    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc;

    use metadata::MetadataError;
    use tokio::sync::oneshot;

    fn bearer(key: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(header::AUTHORIZATION, HeaderValue::from_str(&format!("Bearer {key}")).unwrap());
        h
    }

    /// A key write that signals when it starts and returns `out` only once
    /// released (or once the release handle is dropped).
    fn gated<T: Send + 'static>(
        out: metadata::Result<T>,
    ) -> (impl FnOnce() -> metadata::Result<T> + Send + 'static, oneshot::Receiver<()>, mpsc::Sender<()>) {
        let (started_tx, started) = oneshot::channel();
        let (release, released) = mpsc::channel::<()>();
        let write = move || {
            let _ = started_tx.send(());
            let _ = released.recv();
            out
        };
        (write, started, release)
    }

    async fn until(what: &str, cond: impl Fn() -> bool) {
        tokio::time::timeout(Duration::from_secs(10), async {
            while !cond() {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for {what}"));
    }

    fn in_memory() -> (Arc<Metadata>, Arc<ApiKeys>) {
        let m = Arc::new(Metadata::open_in_memory().unwrap());
        let keys = Arc::new(ApiKeys::load(&m).unwrap());
        (m, keys)
    }

    /// The published set equals what a restart would load.
    fn assert_matches_store(keys: &ApiKeys, m: &Arc<Metadata>) {
        assert_eq!(*keys.snapshot(), *ApiKeys::load(m).unwrap().snapshot());
    }

    // AC1: while a key write is stalled, the runtime keeps running, requests
    // are decided from the published set without waiting, queued changes
    // hold no blocking thread, and changes beyond the bound are refused.
    #[test]
    fn slow_key_writes_block_neither_requests_nor_the_runtime() {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .max_blocking_threads(2)
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let keys = Arc::new(ApiKeys::new(KeySet::default()));
            let (write, started, release) = gated(Ok(()));
            let first = tokio::spawn({
                let keys = keys.clone();
                async move { keys.change(write, |k, ()| k.add(sha256_hex("vyr_first"), 1, "ingest".into())).await }
            });
            started.await.unwrap();
            let queued: Vec<_> = (2..=MAX_PENDING_KEY_CHANGES as i64)
                .map(|id| {
                    let keys = keys.clone();
                    tokio::spawn(async move {
                        keys.change(|| Ok(()), move |k, ()| k.add(format!("h{id}"), id, "admin".into())).await
                    })
                })
                .collect();
            until("every change to queue", || keys.pending.available_permits() == 0).await;

            let refused = keys.change(|| Ok(()), |_, ()| {}).await.unwrap_err();
            assert_eq!((refused.status.as_u16(), refused.code), (503, "unavailable"));
            let heartbeat = Instant::now();
            for _ in 0..20 {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
            assert!(heartbeat.elapsed() < Duration::from_secs(5), "the runtime kept ticking");
            // One blocking thread runs the stalled write; the other is free.
            tokio::time::timeout(Duration::from_secs(5), tokio::task::spawn_blocking(|| ())).await.unwrap().unwrap();
            let open = keys.snapshot();
            assert_eq!(open.admit_ingest(&HeaderMap::new()).unwrap(), None, "still open: nothing committed");
            assert!(!first.is_finished());

            release.send(()).unwrap();
            first.await.unwrap().unwrap();
            for q in queued {
                q.await.unwrap().unwrap();
            }
            let now = keys.snapshot();
            assert!(now.ingest_protected());
            assert_eq!(now.by_hash.len(), MAX_PENDING_KEY_CHANGES);
            assert!(!open.ingest_protected(), "a published set never changes");
            until("every turn to be returned", || keys.pending.available_permits() == MAX_PENDING_KEY_CHANGES).await;
        });
    }

    // AC2: a failed or panicking write publishes nothing and releases its
    // turn; a failed revoke keeps the key accepted.
    #[tokio::test]
    async fn failed_key_writes_publish_nothing() {
        let (m, keys) = in_memory();
        let before = keys.snapshot();
        assert_eq!(keys.create(&m, "ci", "root").await.unwrap_err().status.as_u16(), 400);
        let disk = || Err::<(), _>(MetadataError::Invalid("disk I/O error".into()));
        keys.change(disk, |k, ()| k.add("fictitious".into(), 1, "ingest".into())).await.unwrap_err();
        let panicked = keys
            .change(
                || -> metadata::Result<()> { panic!("injected key write panic") },
                |k, ()| k.add("fictitious".into(), 1, "ingest".into()),
            )
            .await
            .unwrap_err();
        assert_eq!(panicked.status.as_u16(), 500);
        assert!(Arc::ptr_eq(&before, &keys.snapshot()), "nothing published");
        assert!(!keys.ingest_protected(), "the failed first key did not change the policy");

        let (rec, key) = keys.create(&m, "ci", "ingest").await.unwrap();
        keys.change(disk, move |k, ()| k.remove(rec.id)).await.unwrap_err();
        assert_eq!(keys.snapshot().admit_ingest(&bearer(&key)).unwrap(), Some((rec.id, "ingest".into())));
        assert!(keys.snapshot().admit_ingest(&HeaderMap::new()).is_err(), "ingestion stays protected");
        assert_eq!(keys.revoke(&m, rec.id + 100).await.unwrap_err().status.as_u16(), 404);
        assert_matches_store(&keys, &m);

        keys.revoke(&m, rec.id).await.unwrap();
        assert!(!keys.ingest_protected());
        assert_eq!(keys.revoke(&m, rec.id).await.unwrap_err().status.as_u16(), 404, "revoked once");
        assert_matches_store(&keys, &m);
    }

    // AC2: a change whose caller goes away while it waits never runs; one
    // whose write has started commits and is published, in order.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancelled_key_changes_finish_or_never_start() {
        let (m, keys) = in_memory();
        let (key, prefix, hash) = new_api_key();
        let (gate, started, release) = gated(Ok(()));
        let created = tokio::spawn({
            let (keys, m, stored) = (keys.clone(), m.clone(), hash.clone());
            async move {
                let write = move || {
                    gate()?;
                    m.create_api_key("ci", &prefix, &stored, "ingest")
                };
                keys.change(write, move |k, rec| k.add(hash, rec.id, rec.scope.clone())).await
            }
        });
        started.await.unwrap();
        let ran = Arc::new(AtomicBool::new(false));
        let wiped = tokio::spawn({
            let (keys, ran) = (keys.clone(), ran.clone());
            async move {
                let write = move || {
                    ran.store(true, Ordering::SeqCst);
                    Ok(())
                };
                keys.change(write, |k, ()| *k = KeySet::default()).await
            }
        });
        until("the second change to queue", || keys.pending.available_permits() == MAX_PENDING_KEY_CHANGES - 2).await;
        created.abort();
        wiped.abort();
        assert!(wiped.await.unwrap_err().is_cancelled());
        assert!(created.await.unwrap_err().is_cancelled());
        assert!(!keys.ingest_protected(), "not published before it commits");

        release.send(()).unwrap();
        until("the abandoned create to publish", || keys.ingest_protected()).await;
        until("every turn to be returned", || keys.pending.available_permits() == MAX_PENDING_KEY_CHANGES).await;
        assert!(!ran.load(Ordering::SeqCst), "the cancelled change never wrote");
        assert!(matches!(keys.snapshot().admit_ingest(&bearer(&key)), Ok(Some(_))));
        assert_matches_store(&keys, &m);
    }

    // AC3/AC4: concurrent creates and revokes leave exactly the stored keys,
    // a restart loads the same set, and only ingest keys toggle protection.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_key_changes_match_the_store_across_restarts() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("metadata.db");
        let m = Arc::new(Metadata::open(&path).unwrap());
        let keys = Arc::new(ApiKeys::load(&m).unwrap());
        let create = |name: String, scope: &'static str| {
            let (keys, m) = (keys.clone(), m.clone());
            tokio::spawn(async move { keys.create(&m, &name, scope).await.unwrap() })
        };
        let revoke = |id: i64| {
            let (keys, m) = (keys.clone(), m.clone());
            tokio::spawn(async move { keys.revoke(&m, id).await.unwrap() })
        };
        let first: Vec<_> =
            (0..8).flat_map(|i| [create(format!("i{i}"), "ingest"), create(format!("a{i}"), "admin")]).collect();
        let mut made = Vec::new();
        for t in first {
            made.push(t.await.unwrap());
        }
        assert!(keys.ingest_protected());
        assert_matches_store(&keys, &m);

        // Revoke half of each scope while creating more, all at once.
        let (gone, kept): (Vec<_>, Vec<_>) = made.into_iter().enumerate().partition(|(i, _)| i % 4 < 2);
        let revokes: Vec<_> = gone.iter().map(|(_, (rec, _))| revoke(rec.id)).collect();
        let more: Vec<_> =
            (0..8).map(|i| create(format!("m{i}"), if i % 2 == 0 { "ingest" } else { "admin" })).collect();
        for t in revokes {
            t.await.unwrap();
        }
        let mut live: Vec<_> = kept.into_iter().map(|(_, k)| k).collect();
        for t in more {
            live.push(t.await.unwrap());
        }
        let now = keys.snapshot();
        for (_, (rec, key)) in &gone {
            assert!(now.find(key).is_err(), "revoked key {} is refused", rec.id);
        }
        for (rec, key) in &live {
            assert_eq!(now.find(key).unwrap(), (rec.id, rec.scope.clone()), "scope of {}", rec.name);
        }
        assert_eq!(now.by_hash.len(), live.len());
        assert_matches_store(&keys, &m);

        // A restart loads exactly the committed keys.
        let before = keys.snapshot();
        drop((keys, m));
        let m = Arc::new(Metadata::open(&path).unwrap());
        let keys = Arc::new(ApiKeys::load(&m).unwrap());
        assert_eq!(*keys.snapshot(), *before);

        // Revoking every ingest key but one keeps ingestion protected; the
        // last reopens it; the admin keys left never protect it.
        let ingest: Vec<_> = live.iter().filter(|(rec, _)| rec.scope == "ingest").map(|(rec, _)| rec.id).collect();
        let (last, rest) = ingest.split_last().unwrap();
        let revokes: Vec<_> = rest
            .iter()
            .map(|&id| {
                let (keys, m) = (keys.clone(), m.clone());
                tokio::spawn(async move { keys.revoke(&m, id).await.unwrap() })
            })
            .collect();
        for t in revokes {
            t.await.unwrap();
        }
        assert!(keys.ingest_protected(), "one ingest key is left");
        keys.revoke(&m, *last).await.unwrap();
        assert!(!keys.ingest_protected());
        assert!(!keys.snapshot().by_hash.is_empty(), "admin keys remain");
        let (_, admin) = keys.create(&m, "another admin", "admin").await.unwrap();
        assert!(!keys.ingest_protected(), "admin keys do not protect ingestion");
        assert!(keys.snapshot().find(&admin).is_ok());
        assert_matches_store(&keys, &m);
    }

    // AC2/AC3: under churn of the first/last ingest key, every admission is
    // one consistent decision, and none accepts a key whose revoke returned.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn no_admission_with_a_key_after_its_revoke_returns() {
        let (m, keys) = in_memory();
        let (admin_rec, admin) = keys.create(&m, "automation", "admin").await.unwrap();
        let revoked = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let done = Arc::new(AtomicBool::new(false));
        let checkers: Vec<_> = (0..3)
            .map(|_| {
                let (keys, revoked, done, admin) = (keys.clone(), revoked.clone(), done.clone(), admin.clone());
                tokio::spawn(async move {
                    let mut checks = 0u64;
                    while !done.load(Ordering::SeqCst) {
                        let gone = revoked.lock().unwrap().clone();
                        let set = keys.snapshot();
                        for key in &gone {
                            match set.admit_ingest(&bearer(key)) {
                                Ok(None) => assert!(!set.ingest_protected()),
                                Ok(Some(p)) => panic!("revoked key admitted as {p:?}"),
                                Err(e) => assert!(set.ingest_protected() && e.status.as_u16() == 401),
                            }
                        }
                        match set.admit_ingest(&HeaderMap::new()) {
                            Ok(None) => assert!(!set.ingest_protected()),
                            other => assert!(set.ingest_protected() && other.is_err()),
                        }
                        let any_valid = set.admit_ingest(&bearer(&admin)).unwrap();
                        assert!(any_valid.is_none() || any_valid == Some((admin_rec.id, "admin".into())));
                        checks += 1;
                        tokio::task::yield_now().await;
                    }
                    checks
                })
            })
            .collect();

        for round in 0..50 {
            let (rec, key) = keys.create(&m, &format!("k{round}"), "ingest").await.unwrap();
            assert!(keys.snapshot().admit_ingest(&HeaderMap::new()).is_err(), "protected at once");
            keys.revoke(&m, rec.id).await.unwrap();
            assert_eq!(keys.snapshot().admit_ingest(&bearer(&key)).unwrap(), None, "open again at once");
            revoked.lock().unwrap().push(key);
        }
        done.store(true, Ordering::SeqCst);
        for c in checkers {
            assert!(c.await.unwrap() > 0);
        }
        assert_matches_store(&keys, &m);
    }

    // ---- AUTH-PERF-02: the session cache -----------------------------------

    use std::sync::atomic::AtomicUsize;

    type Found = metadata::Result<(Option<(String, i64)>, Duration)>;

    /// A lookup that finds `user` valid until `expires_at`, counting calls.
    fn valid(calls: &Arc<AtomicUsize>, expires_at: i64) -> impl FnOnce(&str) -> Found + Send + 'static {
        let calls = calls.clone();
        move |_| {
            calls.fetch_add(1, Ordering::SeqCst);
            Ok((Some(("admin".into(), expires_at)), Duration::ZERO))
        }
    }

    fn later(ms: i64) -> i64 {
        metadata::now_ms() + ms
    }

    fn stat(s: &Sessions, k: &str) -> u64 {
        s.snapshot()[k].as_u64().unwrap()
    }

    // AC4/AC5: a valid session is read once, then served from memory until
    // its expiry and never after; the token itself is not kept.
    #[tokio::test]
    async fn cached_sessions_are_used_until_their_expiry_only() {
        let s = Sessions::default();
        let calls = Arc::new(AtomicUsize::new(0));
        let token = random_token(32);
        let hash = sha256_hex(&token);
        let expires = later(300);
        for _ in 0..5 {
            assert_eq!(s.check(hash.clone(), valid(&calls, expires)).await.unwrap().as_deref(), Some("admin"));
        }
        assert_eq!(calls.load(Ordering::SeqCst), 1, "one SQLite lookup");
        assert_eq!((stat(&s, "cacheHits"), stat(&s, "lookups"), stat(&s, "cached")), (4, 1, 1));
        {
            let c = s.cache.lock().unwrap();
            assert!(c.by_hash.contains_key(&hash) && !c.by_hash.contains_key(&token), "keyed by hash only");
        }

        // At the expiry the entry is not used; SQLite (which also refuses an
        // expired session) decides.
        while metadata::now_ms() < expires {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let none = |_: &str| -> Found { Ok((None, Duration::ZERO)) };
        assert_eq!(s.check(hash.clone(), none).await.unwrap(), None);
        assert_eq!(stat(&s, "cached"), 0, "the expired entry is dropped");
        assert_eq!(s.check(hash, none).await.unwrap(), None, "and nothing negative is cached");
        assert_eq!(stat(&s, "lookups"), 3);
    }

    // AC4: logout invalidates at once, and a lookup that read the session
    // before the logout's delete cannot cache it again afterwards.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn invalidation_wins_over_a_fill_in_flight() {
        let s = Arc::new(Sessions::default());
        let calls = Arc::new(AtomicUsize::new(0));
        let hash = sha256_hex("token");
        s.check(hash.clone(), valid(&calls, later(60_000))).await.unwrap();
        s.invalidate(&hash);
        assert_eq!(stat(&s, "cached"), 0);

        // A lookup reads the (not yet deleted) session; the logout then
        // deletes and invalidates; only then does the lookup finish.
        let (gate, started, release) = gated(Ok((Some(("admin".to_string(), later(60_000))), Duration::ZERO)));
        let racing = tokio::spawn({
            let (s, hash) = (s.clone(), hash.clone());
            async move { s.check(hash, move |_| gate()).await }
        });
        started.await.unwrap();
        s.invalidate(&hash);
        release.send(()).unwrap();
        // The racing request read SQLite before the logout, so it may be
        // admitted (it is ordered before the logout), but it is not cached.
        assert_eq!(racing.await.unwrap().unwrap().as_deref(), Some("admin"));
        assert_eq!(stat(&s, "cached"), 0, "the ended session was not put back");
        let none = |_: &str| -> Found { Ok((None, Duration::ZERO)) };
        assert_eq!(s.check(hash, none).await.unwrap(), None, "the next request reads SQLite and is refused");
    }

    // AC4: the entry cap holds, expired entries go first, and a flood of
    // unknown tokens caches nothing.
    #[tokio::test]
    async fn the_cache_is_bounded_and_unknown_tokens_never_grow_it() {
        let s = Sessions::default();
        let calls = Arc::new(AtomicUsize::new(0));
        let none = |_: &str| -> Found { Ok((None, Duration::ZERO)) };
        for i in 0..10_000 {
            assert_eq!(s.check(sha256_hex(&format!("unknown-{i}")), none).await.unwrap(), None);
        }
        assert_eq!(stat(&s, "cached"), 0);
        assert_eq!(stat(&s, "lookups"), 10_000, "every unknown token is checked in SQLite");

        // Fill past the cap: soon-expiring entries first, then live ones.
        for i in 0..100 {
            s.check(sha256_hex(&format!("short-{i}")), valid(&calls, later(50))).await.unwrap();
        }
        tokio::time::sleep(Duration::from_millis(60)).await;
        for i in 0..SESSION_CACHE_ENTRIES + 500 {
            s.check(sha256_hex(&format!("live-{i}")), valid(&calls, later(60_000))).await.unwrap();
            assert!(stat(&s, "cached") as usize <= SESSION_CACHE_ENTRIES);
        }
        let c = s.cache.lock().unwrap();
        assert_eq!(c.by_hash.len(), SESSION_CACHE_ENTRIES);
        assert!(c.by_hash.values().all(|e| e.expires_at > metadata::now_ms()), "expired entries were evicted first");
        assert_counted(&c);
    }

    /// `bytes` is the sum of the entries' sizes.
    fn assert_counted(c: &Cache) {
        assert_eq!(c.bytes, c.by_hash.iter().map(|(h, e)| entry_bytes(h, &e.username)).sum::<usize>());
        assert!(c.bytes <= SESSION_CACHE_BYTES && c.by_hash.len() <= SESSION_CACHE_ENTRIES);
    }

    /// A lookup that finds `user` valid for a minute.
    fn valid_as(user: String) -> impl FnOnce(&str) -> Found + Send + 'static {
        move |_| Ok((Some((user, later(60_000))), Duration::ZERO))
    }

    // AC4 (review): the byte cap holds whatever the user name's length;
    // names over MAX_CACHED_USERNAME are authenticated but never cached.
    #[tokio::test]
    async fn the_cache_is_bounded_in_bytes_too() {
        let s = Sessions::default();
        let longest = "u".repeat(MAX_CACHED_USERNAME);
        for i in 0..SESSION_CACHE_ENTRIES {
            let user = s.check(sha256_hex(&format!("t{i}")), valid_as(longest.clone())).await.unwrap();
            assert_eq!(user.as_deref(), Some(longest.as_str()));
            assert_counted(&s.cache.lock().unwrap());
        }
        let reported = stat(&s, "cachedBytes") as usize;
        {
            let c = s.cache.lock().unwrap();
            let per_entry = entry_bytes(&sha256_hex("t0"), &longest);
            assert_eq!(c.by_hash.len(), SESSION_CACHE_BYTES / per_entry, "the byte cap, not the count, binds");
            assert_eq!(reported, c.bytes);
        }

        // An 8 KiB name: still authenticated, every time from SQLite.
        let s = Sessions::default();
        let huge = "u".repeat(8 * 1024);
        for _ in 0..3 {
            let user = s.check(sha256_hex("huge"), valid_as(huge.clone())).await.unwrap();
            assert_eq!(user.as_deref(), Some(huge.as_str()));
        }
        assert_eq!((stat(&s, "cached"), stat(&s, "cachedBytes"), stat(&s, "lookups")), (0, 0, 3));

        // Entries are counted out as well as in: expiry, logout, re-fill.
        let s = Sessions::default();
        let calls = Arc::new(AtomicUsize::new(0));
        s.check(sha256_hex("a"), valid(&calls, later(30))).await.unwrap();
        s.check(sha256_hex("b"), valid(&calls, later(60_000))).await.unwrap();
        s.invalidate(&sha256_hex("b"));
        s.check(sha256_hex("b"), valid(&calls, later(60_000))).await.unwrap();
        s.check(sha256_hex("b"), valid(&calls, later(60_000))).await.unwrap();
        tokio::time::sleep(Duration::from_millis(40)).await;
        let none = |_: &str| -> Found { Ok((None, Duration::ZERO)) };
        s.check(sha256_hex("a"), none).await.unwrap();
        let c = s.cache.lock().unwrap();
        assert_eq!(c.by_hash.len(), 1);
        assert_counted(&c);
    }

    // AC4 (review): clearing (a password change) forgets every session and
    // stops fills already under way from caching.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn clearing_forgets_everything_including_fills_in_flight() {
        let s = Arc::new(Sessions::default());
        let calls = Arc::new(AtomicUsize::new(0));
        for i in 0..10 {
            s.check(sha256_hex(&format!("t{i}")), valid(&calls, later(60_000))).await.unwrap();
        }
        let (gate, started, release) = gated(Ok((Some(("admin".to_string(), later(60_000))), Duration::ZERO)));
        let racing = tokio::spawn({
            let s = s.clone();
            async move { s.check(sha256_hex("racing"), move |_| gate()).await }
        });
        started.await.unwrap();
        s.clear();
        release.send(()).unwrap();
        racing.await.unwrap().unwrap();
        assert_eq!((stat(&s, "cached"), stat(&s, "cachedBytes")), (0, 0));
        s.check(sha256_hex("t0"), valid(&calls, later(60_000))).await.unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 11, "read from SQLite again after clearing");
    }

    // AC4: at most MAX_SESSION_FILLS lookups read SQLite at once, waiting
    // ones hold no blocking thread, and cancelled ones release their turn.
    #[test]
    fn concurrent_fills_are_bounded() {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .max_blocking_threads(MAX_SESSION_FILLS + 1)
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let s = Arc::new(Sessions::default());
            let (running, peak) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
            let (release, released) = std::sync::mpsc::channel::<()>();
            let released = Arc::new(std::sync::Mutex::new(released));
            let lookups: Vec<_> = (0..64)
                .map(|i| {
                    let (s, running, peak, released) = (s.clone(), running.clone(), peak.clone(), released.clone());
                    tokio::spawn(async move {
                        let lookup = move |_: &str| -> Found {
                            let now = running.fetch_add(1, Ordering::SeqCst) + 1;
                            peak.fetch_max(now, Ordering::SeqCst);
                            let _ = released.lock().unwrap().recv();
                            running.fetch_sub(1, Ordering::SeqCst);
                            Ok((None, Duration::ZERO))
                        };
                        s.check(sha256_hex(&format!("t{i}")), lookup).await
                    })
                })
                .collect();
            until("the fills to start", || running.load(Ordering::SeqCst) == MAX_SESSION_FILLS).await;
            tokio::time::sleep(Duration::from_millis(50)).await;
            assert_eq!(peak.load(Ordering::SeqCst), MAX_SESSION_FILLS);
            assert_eq!(stat(&s, "inFlight"), 64);
            // The one spare blocking thread is free: waiters hold none.
            tokio::time::timeout(Duration::from_secs(5), tokio::task::spawn_blocking(|| ())).await.unwrap().unwrap();
            // Cancel half of the waiting ones; the rest finish.
            for t in &lookups[48..] {
                t.abort();
            }
            for _ in 0..64 {
                let _ = release.send(());
            }
            drop(release);
            for t in lookups {
                let _ = t.await;
            }
            assert_eq!(peak.load(Ordering::SeqCst), MAX_SESSION_FILLS);
            until("every turn to return", || s.fills.available_permits() == MAX_SESSION_FILLS).await;
            assert_eq!(stat(&s, "inFlight"), 0);
        });
    }

    // AC5: a database error fails the check (no fallback, no bypass) and
    // caches nothing; a panicking lookup fails too.
    #[tokio::test]
    async fn lookup_failures_fail_closed() {
        let s = Sessions::default();
        let hash = sha256_hex("token");
        let broken = |_: &str| -> Found { Err(MetadataError::Sqlite(rusqlite::Error::InvalidQuery)) };
        assert_eq!(s.check(hash.clone(), broken).await.unwrap_err().status.as_u16(), 500);
        let panics = |_: &str| -> Found { panic!("injected lookup panic") };
        assert_eq!(s.check(hash.clone(), panics).await.unwrap_err().status.as_u16(), 500);
        assert_eq!((stat(&s, "failed"), stat(&s, "cached"), stat(&s, "inFlight")), (1, 0, 0));
        assert_eq!(s.fills.available_permits(), MAX_SESSION_FILLS);
    }

    // AC5: against the real store, sessions come and go with SQLite: login,
    // logout, expiry, a missing user and a restart (an empty cache).
    #[tokio::test]
    async fn sessions_follow_the_store() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("metadata.db");
        let m = Arc::new(Metadata::open(&path).unwrap());
        let s = Sessions::default();
        async fn check(s: &Sessions, m: &Arc<Metadata>, token: &str) -> ApiResult<Option<String>> {
            let m = m.clone();
            let lookup = move |hash: &str| -> Found {
                let (found, lock) = m.find_session_timed(hash)?;
                Ok((found.map(|(u, expires)| (u.username, expires)), lock))
            };
            s.check(sha256_hex(token), lookup).await
        }
        let uid = m.upsert_user("admin", &hash_password("pw").unwrap()).unwrap();
        let (token, short) = (random_token(32), random_token(32));
        m.create_session(&sha256_hex(&token), uid, 60_000).unwrap();
        m.create_session(&sha256_hex(&short), uid, 150).unwrap();
        assert_eq!(check(&s, &m, &token).await.unwrap().as_deref(), Some("admin"));
        assert_eq!(check(&s, &m, &short).await.unwrap().as_deref(), Some("admin"));
        assert_eq!(check(&s, &m, "never-issued").await.unwrap(), None);
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(check(&s, &m, &short).await.unwrap(), None, "expired, though cached");

        // Logout as routes::session does it: delete, then invalidate.
        m.delete_session(&sha256_hex(&token)).unwrap();
        s.invalidate(&sha256_hex(&token));
        assert_eq!(check(&s, &m, &token).await.unwrap(), None);

        // A restart keeps stored sessions and starts with an empty cache.
        let other = random_token(32);
        m.create_session(&sha256_hex(&other), uid, 60_000).unwrap();
        drop(m);
        let m = Arc::new(Metadata::open(&path).unwrap());
        let s = Sessions::default();
        assert_eq!(check(&s, &m, &other).await.unwrap().as_deref(), Some("admin"));
        assert_eq!(check(&s, &m, &token).await.unwrap(), None);
        assert_eq!(stat(&s, "lookups"), 2);
    }

    // ---- AUTH-PERF-03: bounded last-used bookkeeping -----------------------

    fn tstat(keys: &ApiKeys, k: &str) -> u64 {
        keys.touch_snapshot()[k].as_u64().unwrap()
    }

    /// Ids written so far, in order, by a writer that records them.
    type Written = Arc<std::sync::Mutex<Vec<i64>>>;

    fn recording() -> (TouchWrite, Written) {
        let written = Written::default();
        let w = written.clone();
        (
            Arc::new(move |ids: &[i64]| {
                w.lock().unwrap().extend_from_slice(ids);
                Ok(ids.len())
            }),
            written,
        )
    }

    /// A registry of `n` active ingest keys, ids 1..=n.
    fn active(n: i64, write: TouchWrite) -> Arc<ApiKeys> {
        let mut set = KeySet::default();
        for id in 1..=n {
            set.add(format!("h{id}"), id, "ingest".into());
        }
        Arc::new(ApiKeys::with_touch_writer(set, write))
    }

    async fn idle(keys: &ApiKeys) {
        until("the writer to finish", || {
            let t = keys.touches.lock().unwrap();
            !t.writing && t.pending.is_empty()
        })
        .await;
    }

    /// Every bound holds, and only active keys are remembered or waiting.
    fn assert_bounded(keys: &ApiKeys) {
        let ids = keys.snapshot().ids();
        let t = keys.touches.lock().unwrap();
        assert!(t.last.len() <= KEY_TOUCH_ENTRIES && t.pending.len() <= MAX_PENDING_KEY_TOUCHES);
        assert!(t.last.keys().chain(&t.pending).all(|id| ids.contains(id)), "a removed key is remembered");
        assert!(t.pending.iter().all(|id| t.last.contains_key(id)));
    }

    // AC3/tests: on a fake clock, a key is written once per
    // KEY_TOUCH_INTERVAL: not at the boundary, and again just past it.
    #[tokio::test]
    async fn last_used_writes_keep_the_one_minute_throttle() {
        let (write, written) = recording();
        let keys = active(2, write);
        let set = keys.snapshot();
        let t0 = Instant::now();
        assert_eq!(keys.touch_at(1, &set, t0), Touch::Queued);
        for d in [Duration::ZERO, Duration::from_secs(1), KEY_TOUCH_INTERVAL] {
            assert_eq!(keys.touch_at(1, &set, t0 + d), Touch::Throttled, "{d:?} after");
        }
        assert_eq!(keys.touch_at(2, &set, t0 + KEY_TOUCH_INTERVAL), Touch::Queued, "per key");
        idle(&keys).await;
        assert_eq!(keys.touch_at(1, &set, t0 + KEY_TOUCH_INTERVAL + Duration::from_nanos(1)), Touch::Queued);
        assert_eq!(keys.touch_at(1, &set, t0 + KEY_TOUCH_INTERVAL * 2), Touch::Throttled);
        idle(&keys).await;
        let mut ids = written.lock().unwrap().clone();
        ids.sort();
        assert_eq!(ids, [1, 1, 2], "key 1 twice, key 2 once");
        assert_eq!((tstat(&keys, "queued"), tstat(&keys, "written"), tstat(&keys, "writers")), (3, 3, 0));
    }

    // AC1: a request admitted before a revoke cannot put the key back, and
    // a revoke drops what was remembered or waiting for it.
    #[tokio::test]
    async fn revoked_keys_leave_no_last_used_state() {
        let (m, keys) = in_memory();
        let (a, _) = keys.create(&m, "a", "ingest").await.unwrap();
        let (b, _) = keys.create(&m, "b", "ingest").await.unwrap();
        let before = keys.snapshot();
        let now = Instant::now();
        assert_eq!(keys.touch_at(a.id, &before, now), Touch::Queued);
        idle(&keys).await;
        assert_eq!(keys.touches.lock().unwrap().last.len(), 1);

        keys.revoke(&m, a.id).await.unwrap();
        keys.revoke(&m, b.id).await.unwrap();
        assert!(keys.touches.lock().unwrap().last.is_empty(), "the revoke dropped a's entry");
        assert_eq!(tstat(&keys, "forgotten"), 1);
        // Both were admitted by the set from before the revokes.
        for id in [a.id, b.id] {
            assert_eq!(keys.touch_at(id, &before, now + KEY_TOUCH_INTERVAL * 2), Touch::Retired);
        }
        assert_eq!((tstat(&keys, "tracked"), tstat(&keys, "retired")), (0, 2));
        assert!(m.list_api_keys().unwrap().is_empty());
        assert_bounded(&keys);
    }

    // AC1: the race itself, made deterministic: a touch that saw the key
    // active and queued it while the revoke was writing loses its entry
    // when the revoke publishes, and its write leaves the revoked row alone.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_touch_racing_a_revoke_is_dropped_when_the_revoke_publishes() {
        let (m, keys) = in_memory();
        let (rec, _) = keys.create(&m, "a", "ingest").await.unwrap();
        let admitted = keys.snapshot();
        let (gate, started, release) = gated(Ok(()));
        let revoke = tokio::spawn({
            let (keys, m) = (keys.clone(), m.clone());
            async move {
                let write = move || {
                    gate()?;
                    m.revoke_api_key(rec.id)
                };
                keys.change(write, move |k, ()| k.remove(rec.id)).await
            }
        });
        started.await.unwrap();
        // Not published yet, so the key is still active: queued.
        let now = Instant::now();
        assert_eq!(keys.touch_at(rec.id, &admitted, now), Touch::Queued);
        idle(&keys).await;
        release.send(()).unwrap();
        revoke.await.unwrap().unwrap();
        assert_eq!(keys.touch_at(rec.id, &admitted, now + KEY_TOUCH_INTERVAL * 2), Touch::Retired);
        assert_eq!((tstat(&keys, "tracked"), tstat(&keys, "pending")), (0, 0));
        assert_bounded(&keys);
    }

    // AC1/AC4: 10,000 sequential create → use → revoke operations, with at
    // most 8 keys active, leave state bounded by the active keys, not by
    // the revoked ones, and last-used is persisted for each key used.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn ten_thousand_revoked_keys_leave_nothing_behind() {
        let (m, keys) = in_memory();
        let mut live = std::collections::VecDeque::new();
        let mut peak = 0;
        for i in 0..10_000 {
            let (rec, key) = keys.create(&m, &format!("k{i}"), "ingest").await.unwrap();
            let set = keys.snapshot();
            let (id, _) = set.find(&key).unwrap();
            assert_eq!(keys.touch(id, &set), Touch::Queued);
            live.push_back(rec.id);
            if live.len() > 8 {
                keys.revoke(&m, live.pop_front().unwrap()).await.unwrap();
            }
            peak = peak.max(keys.touches.lock().unwrap().last.len());
            if i % 1000 == 0 {
                assert_bounded(&keys);
            }
        }
        idle(&keys).await;
        assert_bounded(&keys);
        assert!(peak <= 9, "at most the active keys and one being revoked are remembered (peak {peak})");
        assert_eq!(tstat(&keys, "tracked"), 8);
        assert_eq!(tstat(&keys, "forgotten"), 9_992);
        let listed = m.list_api_keys().unwrap();
        assert_eq!(listed.len(), 8);
        assert!(listed.iter().all(|k| k.last_used_at.is_some()), "the active keys' last-used was written");
        assert!(tstat(&keys, "written") >= 8 && tstat(&keys, "written") <= 10_000);
        assert!(tstat(&keys, "peakPending") <= MAX_PENDING_KEY_TOUCHES as u64);
    }

    // AC1/AC4: concurrent touches and revokes: no revoked key is remembered
    // afterwards, whatever the interleaving.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_touches_and_revokes_leave_only_active_keys() {
        let (m, keys) = in_memory();
        let mut made = Vec::new();
        for i in 0..64 {
            made.push(keys.create(&m, &format!("k{i}"), if i % 2 == 0 { "ingest" } else { "admin" }).await.unwrap());
        }
        let done = Arc::new(AtomicBool::new(false));
        let ids: Vec<i64> = made.iter().map(|(rec, _)| rec.id).collect();
        let touchers: Vec<_> = (0..4)
            .map(|w| {
                let (keys, done, ids) = (keys.clone(), done.clone(), ids.clone());
                tokio::spawn(async move {
                    // Each pass a minute later, so every touch is due.
                    let mut now = Instant::now();
                    while !done.load(Ordering::SeqCst) {
                        let set = keys.snapshot();
                        // As requests do: only keys this set admits.
                        for id in ids.iter().skip(w % 2).filter(|id| set.has_id(**id)) {
                            keys.touch_at(*id, &set, now);
                        }
                        now += KEY_TOUCH_INTERVAL * 2;
                        tokio::task::yield_now().await;
                    }
                })
            })
            .collect();
        for id in ids.iter().step_by(2) {
            keys.revoke(&m, *id).await.unwrap();
        }
        done.store(true, Ordering::SeqCst);
        for t in touchers {
            t.await.unwrap();
        }
        idle(&keys).await;
        assert_bounded(&keys);
        assert!(tstat(&keys, "tracked") <= 32);
        assert_matches_store(&keys, &m);
    }

    // AC2: a stalled database: one writer thread, at most
    // MAX_PENDING_KEY_TOUCHES keys waiting, the rest dropped at once
    // (requests never wait), nothing else spawned; afterwards the dropped
    // keys are written on their next use.
    #[test]
    fn a_stalled_writer_bounds_waiting_keys_and_threads() {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .max_blocking_threads(2)
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let (started_tx, started) = mpsc::channel::<()>();
            let (release, released) = mpsc::channel::<()>();
            let released = std::sync::Mutex::new(released);
            let started_tx = std::sync::Mutex::new(started_tx);
            let calls = Arc::new(AtomicUsize::new(0));
            let c = calls.clone();
            let write: TouchWrite = Arc::new(move |ids: &[i64]| {
                c.fetch_add(1, Ordering::SeqCst);
                let _ = started_tx.lock().unwrap().send(());
                let _ = released.lock().unwrap().recv();
                Ok(ids.len())
            });
            let n = 5_000;
            let keys = active(n, write);
            let set = keys.snapshot();
            let now = Instant::now();
            assert_eq!(keys.touch_at(1, &set, now), Touch::Queued);
            started.recv().unwrap();
            // The writer is stalled with key 1; everything else waits or is dropped.
            let decided = Instant::now();
            let outcomes: Vec<_> = (2..=n).map(|id| keys.touch_at(id, &set, now)).collect();
            assert!(decided.elapsed() < Duration::from_secs(5), "touches never wait for the writer");
            let count = |o: Touch| outcomes.iter().filter(|&&x| x == o).count();
            assert_eq!(count(Touch::Queued), MAX_PENDING_KEY_TOUCHES);
            assert_eq!(count(Touch::Dropped), n as usize - 1 - MAX_PENDING_KEY_TOUCHES);
            assert_eq!((tstat(&keys, "pending"), tstat(&keys, "writers")), (MAX_PENDING_KEY_TOUCHES as u64, 1));
            assert_eq!(tstat(&keys, "tracked"), 1 + MAX_PENDING_KEY_TOUCHES as u64, "dropped keys are not remembered");
            // A waiting key used again is not queued twice.
            assert_eq!(keys.touch_at(2, &set, now), Touch::Throttled);
            // The other blocking thread is free: waiting keys hold none.
            tokio::time::timeout(Duration::from_secs(5), tokio::task::spawn_blocking(|| ())).await.unwrap().unwrap();

            release.send(()).unwrap();
            started.recv().unwrap();
            release.send(()).unwrap();
            idle(&keys).await;
            assert_eq!(calls.load(Ordering::SeqCst), 2, "the waiting keys were written in one batch");
            assert_eq!(tstat(&keys, "written"), 1 + MAX_PENDING_KEY_TOUCHES as u64);
            // A dropped key is written on its next use.
            assert_eq!(keys.touch_at(n, &set, now), Touch::Queued);
            started.recv().unwrap();
            release.send(()).unwrap();
            idle(&keys).await;
            assert_bounded(&keys);
        });
    }

    // AC2: with KEY_TOUCH_ENTRIES keys all written recently, further keys
    // are deferred, not written and not evicting anything, so requests
    // cannot turn into a write storm; once entries age out they get in.
    #[tokio::test]
    async fn high_cardinality_keys_at_the_cap_are_deferred_not_written() {
        let (write, written) = recording();
        let extra = 1_000;
        let keys = active(KEY_TOUCH_ENTRIES as i64 + extra, write);
        let set = keys.snapshot();
        let t0 = Instant::now();
        for chunk in (1..=KEY_TOUCH_ENTRIES as i64).collect::<Vec<_>>().chunks(MAX_PENDING_KEY_TOUCHES) {
            for &id in chunk {
                assert_eq!(keys.touch_at(id, &set, t0), Touch::Queued);
            }
            idle(&keys).await;
        }
        assert_eq!(tstat(&keys, "tracked"), KEY_TOUCH_ENTRIES as u64);
        assert_eq!(tstat(&keys, "trackedBytes"), tstat(&keys, "trackedByteCapacity"));

        // Every key, round-robin, for most of a minute: only the extra keys
        // are refused, and nothing more is written.
        for step in 0..30 {
            let now = t0 + Duration::from_secs(2 * step);
            for id in 1..=KEY_TOUCH_ENTRIES as i64 + extra {
                let want = if id as usize > KEY_TOUCH_ENTRIES { Touch::Deferred } else { Touch::Throttled };
                assert_eq!(keys.touch_at(id, &set, now), want);
            }
        }
        assert_eq!(written.lock().unwrap().len(), KEY_TOUCH_ENTRIES, "no write storm");
        assert_eq!(tstat(&keys, "deferred"), 30 * extra as u64);

        // After the interval the old entries are stale: the extra keys get
        // in (and are written once), within the cap.
        let later = t0 + KEY_TOUCH_INTERVAL + Duration::from_secs(1);
        for id in KEY_TOUCH_ENTRIES as i64 + 1..=KEY_TOUCH_ENTRIES as i64 + extra {
            assert_eq!(keys.touch_at(id, &set, later), Touch::Queued);
        }
        idle(&keys).await;
        assert_eq!(written.lock().unwrap().len(), KEY_TOUCH_ENTRIES + extra as usize);
        assert_eq!(tstat(&keys, "tracked"), extra as u64, "the stale entries were swept");
        assert_bounded(&keys);
    }

    // AC3: a failed write releases the writer and keeps the throttle (no
    // retry storm against a failing database); a panicking writer gives
    // back the waiting keys, which are queued again on next use.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn failed_and_panicking_writes_release_everything() {
        let failing: TouchWrite = Arc::new(|_: &[i64]| Err(MetadataError::Invalid("disk I/O error".into())));
        let keys = active(2, failing);
        let set = keys.snapshot();
        let now = Instant::now();
        assert_eq!(keys.touch_at(1, &set, now), Touch::Queued);
        idle(&keys).await;
        assert_eq!((tstat(&keys, "failedBatches"), tstat(&keys, "writers"), tstat(&keys, "written")), (1, 0, 0));
        assert_eq!(keys.touch_at(1, &set, now + Duration::from_secs(1)), Touch::Throttled, "not retried at once");
        assert_eq!(keys.touch_at(1, &set, now + KEY_TOUCH_INTERVAL * 2), Touch::Queued, "retried after the interval");
        idle(&keys).await;
        assert_eq!(tstat(&keys, "failedBatches"), 2);

        // The writer panics on its first batch while key 2 waits.
        let (gate, started, release) = gated::<usize>(Ok(0));
        let gate = std::sync::Mutex::new(Some(gate));
        let panics: TouchWrite = Arc::new(move |_: &[i64]| {
            if let Some(g) = gate.lock().unwrap().take() {
                let _ = g();
            }
            panic!("injected last-used write panic")
        });
        let keys = active(2, panics);
        let set = keys.snapshot();
        assert_eq!(keys.touch_at(1, &set, now), Touch::Queued);
        started.await.unwrap();
        assert_eq!(keys.touch_at(2, &set, now), Touch::Queued);
        release.send(()).unwrap();
        idle(&keys).await;
        // Key 1 was in the batch being written, key 2 waiting: both given back.
        assert_eq!((tstat(&keys, "abandoned"), tstat(&keys, "writers"), tstat(&keys, "pending")), (2, 0, 0));
        assert_eq!(tstat(&keys, "tracked"), 0);
        for id in [1, 2] {
            assert_eq!(
                keys.touch_at(id, &set, now),
                Touch::Queued,
                "abandoned key {id} is queued again on its next use"
            );
        }
        idle(&keys).await;
        assert_bounded(&keys);
    }

    // AC3 (review): a writer that panics after taking a batch gives that
    // batch back, so its keys are not throttled for a minute without having
    // been written; the next use within the interval queues and writes them.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_panicking_writer_gives_back_the_batch_it_was_writing() {
        let (record, written) = recording();
        let panicked = Arc::new(AtomicBool::new(false));
        let p = panicked.clone();
        let write: TouchWrite = Arc::new(move |ids: &[i64]| {
            if !p.swap(true, Ordering::SeqCst) {
                panic!("injected last-used write panic after taking the batch");
            }
            record(ids)
        });
        let keys = active(1, write);
        let set = keys.snapshot();
        let now = Instant::now();
        assert_eq!(keys.touch_at(1, &set, now), Touch::Queued);
        idle(&keys).await;
        assert!(panicked.load(Ordering::SeqCst));
        assert_eq!((tstat(&keys, "abandoned"), tstat(&keys, "tracked"), tstat(&keys, "written")), (1, 0, 0));
        assert!(written.lock().unwrap().is_empty());

        // Within the interval: not throttled, since nothing was written.
        assert_eq!(keys.touch_at(1, &set, now + Duration::from_secs(1)), Touch::Queued);
        idle(&keys).await;
        assert_eq!(*written.lock().unwrap(), [1]);
        assert_eq!(keys.touch_at(1, &set, now + Duration::from_secs(2)), Touch::Throttled, "written: throttled again");
        assert_bounded(&keys);
    }

    // AC3: at shutdown a writer that never runs gives back its slot and the
    // waiting keys.
    #[test]
    fn a_writer_that_never_runs_releases_everything() {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let handle = rt.handle().clone();
        rt.shutdown_background();
        let keys = active(1, Arc::new(|ids: &[i64]| Ok(ids.len())));
        let set = keys.snapshot();
        let _enter = handle.enter();
        assert_eq!(keys.touch_at(1, &set, Instant::now()), Touch::Queued);
        let t = keys.touches.lock().unwrap();
        assert!(!t.writing && t.pending.is_empty() && t.last.is_empty());
        drop(t);
        assert_eq!(tstat(&keys, "abandoned"), 1);
    }

    // AC4: across a restart the stored last-used survives and the
    // bookkeeping starts empty: each key is written once more.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn last_used_survives_a_restart_and_bookkeeping_starts_empty() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("metadata.db");
        let m = Arc::new(Metadata::open(&path).unwrap());
        let keys = Arc::new(ApiKeys::load(&m).unwrap());
        let (_, key) = keys.create(&m, "ci", "ingest").await.unwrap();
        let set = keys.snapshot();
        let (id, _) = set.find(&key).unwrap();
        assert_eq!(keys.touch(id, &set), Touch::Queued);
        idle(&keys).await;
        let first = m.list_api_keys().unwrap()[0].last_used_at.unwrap();
        drop((keys, set, m));

        let m = Arc::new(Metadata::open(&path).unwrap());
        assert_eq!(m.list_api_keys().unwrap()[0].last_used_at, Some(first));
        let keys = Arc::new(ApiKeys::load(&m).unwrap());
        assert_eq!(tstat(&keys, "tracked"), 0);
        tokio::time::sleep(Duration::from_millis(5)).await;
        let set = keys.snapshot();
        assert_eq!(keys.touch(id, &set), Touch::Queued);
        assert_eq!(keys.touch(id, &set), Touch::Throttled);
        idle(&keys).await;
        assert!(m.list_api_keys().unwrap()[0].last_used_at.unwrap() > first);
    }
}
