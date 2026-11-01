//! Health, readiness, system information and storage statistics.

use std::sync::atomic::Ordering;

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::{Value, json};
use storage::{QuarantineKind, QuarantineScan, StorageFault, StorageStats, StreamStats};
use telemetry::Signal;

use crate::error::ApiResult;
use crate::state::SharedState;

pub async fn health() -> Json<Value> {
    Json(json!({ "status": "ok" }))
}

/// Ready when the data directory accepts writes and no signal's ingest
/// path (WAL, sealing) is failing. Unauthenticated, so it reports which
/// signals are affected but never reasons or paths; those are in
/// `/api/v1/system/storage`. Once shutdown starts it is 503 `draining`.
pub async fn ready(State(state): State<SharedState>) -> Response {
    // Take the instance out of rotation while it drains.
    if state.draining() {
        return (StatusCode::SERVICE_UNAVAILABLE, Json(json!({ "status": "draining" }))).into_response();
    }
    let st = state.clone();
    let Ok((stats, probe)) =
        tokio::task::spawn_blocking(move || (st.storage.stats(), st.storage.probe_writable())).await
    else {
        return (StatusCode::SERVICE_UNAVAILABLE, Json(json!({ "status": "degraded" }))).into_response();
    };
    let writable = probe.is_ok();
    let degraded: Vec<&str> = Signal::ALL
        .iter()
        .filter(|s| !writable || stats.get(**s).ingest_fault().is_some())
        .map(|s| s.as_str())
        .collect();
    if degraded.is_empty() {
        Json(json!({ "status": "ready", "storageWritable": true })).into_response()
    } else {
        let body = json!({ "status": "degraded", "storageWritable": writable, "signals": degraded });
        (StatusCode::SERVICE_UNAVAILABLE, Json(body)).into_response()
    }
}

/// A write fault as reported by the API. `path` is relative to the data
/// directory and `message` is the OS error text, so neither discloses host
/// paths or configuration.
fn fault_json(f: &StorageFault) -> Value {
    json!({
        "area": f.area().as_str(),
        "operation": f.op.as_str(),
        "reason": f.reason.as_str(),
        "path": f.path,
        "message": f.message,
        "firstSeen": f.first_seen,
        "lastSeen": f.last_seen,
        "occurrences": f.occurrences,
    })
}

fn stream_json(s: &StreamStats) -> Value {
    let compressed = s.segment_data_bytes + s.segment_index_bytes;
    json!({
        "events": s.segment_events + s.active_events + s.frozen_events,
        "segments": s.segments,
        "rawBytes": s.segment_raw_bytes + s.wal_bytes,
        "storedBytes": compressed + s.wal_bytes,
        "segmentBytes": s.segment_data_bytes,
        "indexBytes": s.segment_index_bytes,
        "walBytes": s.wal_bytes,
        "compressionRatio": ratio(s.segment_raw_bytes, compressed),
        "activeEvents": s.active_events + s.frozen_events,
        "activeBytes": s.active_bytes,
        "queueDepth": s.queue_depth,
        "queueCapacity": s.queue_capacity,
        "ingestedSinceStart": s.ingested_events,
        "rejectedBatches": s.rejected_batches,
        "oldest": s.oldest,
        "newest": s.newest,
        "healthy": s.faults.is_empty(),
        "ingestHealthy": s.ingest_fault().is_none(),
        "faults": s.faults.iter().map(fault_json).collect::<Vec<_>>(),
        "recovery": recovery_json(s),
    })
}

/// How a signal's storage looks after startup recovery, worst first so the
/// most severe status of several signals is the smallest.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum RecoveryStatus {
    /// This start quarantined files or dropped WAL bytes for the signal.
    Damaged,
    /// This start found nothing, but `quarantine/<signal>/` could not be
    /// read, so whether earlier evidence is there is not known. Never
    /// reported as `clean`: not being able to look is not proof.
    Unknown,
    /// This start found nothing, but `quarantine/<signal>/` still holds
    /// files from an earlier start (or a recovery that was interrupted
    /// after moving them there).
    QuarantineRetained,
    /// Nothing damaged and nothing in quarantine. WAL replay alone is
    /// normal and does not change this.
    Clean,
}

impl RecoveryStatus {
    fn of(s: &StreamStats) -> Self {
        if s.recovery.found_damage() {
            RecoveryStatus::Damaged
        } else {
            match s.quarantine.usage() {
                None => RecoveryStatus::Unknown,
                Some(u) if u.files > 0 => RecoveryStatus::QuarantineRetained,
                Some(_) => RecoveryStatus::Clean,
            }
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            RecoveryStatus::Damaged => "damaged",
            RecoveryStatus::Unknown => "unknown",
            RecoveryStatus::QuarantineRetained => "quarantineRetained",
            RecoveryStatus::Clean => "clean",
        }
    }
}

/// The overall recovery status and the signals that are not `clean`.
/// Counts only, never file names or reasons, for the summary views.
fn recovery_summary(stats: &StorageStats) -> Value {
    let per: Vec<(Signal, RecoveryStatus)> =
        Signal::ALL.iter().map(|s| (*s, RecoveryStatus::of(stats.get(*s)))).collect();
    let worst = per.iter().map(|(_, st)| *st).min().unwrap_or(RecoveryStatus::Clean);
    json!({
        "status": worst.as_str(),
        "signals": per.iter().filter(|(_, st)| *st != RecoveryStatus::Clean).map(|(s, _)| s.as_str()).collect::<Vec<_>>(),
    })
}

/// What startup recovery did for one signal, and what is in its quarantine
/// directory now. Quarantine paths are relative to the data directory and
/// reasons never contain host paths or file contents, so the response
/// discloses neither.
fn recovery_json(s: &StreamStats) -> Value {
    let r = &s.recovery;
    json!({
        "status": RecoveryStatus::of(s).as_str(),
        "segmentsLoaded": r.segments_loaded,
        "walRecordsReplayed": r.wal_records_replayed,
        "walEventsReplayed": r.wal_events_replayed,
        "segmentsSealed": r.segments_sealed,
        "staleWalsRemoved": r.stale_wals_removed,
        "replacedSegmentsRemoved": r.replaced_segments_removed,
        "walFilesReplayed": r.wal_files_replayed,
        "walBytesDiscarded": r.wal_bytes_discarded,
        "segmentsQuarantined": r.quarantined_count(QuarantineKind::Segment),
        "walFilesQuarantined": r.quarantined_count(QuarantineKind::Wal),
        "walTailsQuarantined": r.quarantined_count(QuarantineKind::WalTail),
        "quarantinedBytes": r.quarantined_bytes(),
        "quarantineDir": quarantine_dir_json(&s.quarantine),
        "quarantined": r.quarantined.iter().map(|q| json!({
            "signal": q.signal.as_str(),
            "kind": q.kind.as_str(),
            "source": q.source,
            "file": q.path.file_name().map(|n| format!("quarantine/{}/{}", q.signal.as_str(), n.to_string_lossy())),
            "bytes": q.bytes,
            "reason": q.reason,
        })).collect::<Vec<_>>(),
    })
}

/// `files` and `bytes` in `quarantine/<signal>/`, or `null` for both with
/// the reason the directory could not be read. The OS message has no path.
fn quarantine_dir_json(q: &QuarantineScan) -> Value {
    match q {
        QuarantineScan::Read(u) => json!({ "files": u.files, "bytes": u.bytes, "error": null }),
        QuarantineScan::Unreadable { reason, message } => json!({
            "files": null,
            "bytes": null,
            "error": { "reason": reason.as_str(), "message": message },
        }),
    }
}

fn ratio(raw: u64, stored: u64) -> Option<f64> {
    (stored > 0).then(|| (raw as f64 / stored as f64 * 100.0).round() / 100.0)
}

fn metadata_bytes(state: &SharedState) -> u64 {
    storage::fsutil::dir_size(&state.config.storage.path.join("metadata"))
}

pub async fn storage_stats(State(state): State<SharedState>) -> ApiResult<Json<Value>> {
    let st = state.clone();
    let (stats, meta, probe) =
        state.blocking(move || Ok((st.storage.stats(), metadata_bytes(&st), st.storage.probe_writable()))).await?;
    let all = [&stats.logs, &stats.traces, &stats.metrics];
    let sum = |f: fn(&StreamStats) -> u64| all.iter().map(|s| f(s)).sum::<u64>();
    let raw_sealed = sum(|s| s.segment_raw_bytes);
    let data = sum(|s| s.segment_data_bytes);
    let index = sum(|s| s.segment_index_bytes);
    let wal = sum(|s| s.wal_bytes);
    Ok(Json(json!({
        "rawBytes": raw_sealed + wal,
        "storedBytes": data + index + wal + meta,
        "segmentBytes": data,
        "indexBytes": index,
        "walBytes": wal,
        "metadataBytes": meta,
        "tmpBytes": stats.tmp_bytes,
        "quarantineBytes": stats.quarantine_bytes,
        "recovery": recovery_summary(&stats),
        "eventCount": sum(|s| s.segment_events + s.active_events + s.frozen_events),
        "segmentCount": all.iter().map(|s| s.segments).sum::<usize>(),
        "compressionRatio": ratio(raw_sealed, data + index),
        "indexOverhead": (data > 0).then(|| ((index as f64 / data as f64) * 1000.0).round() / 1000.0),
        "receivedBytes": state.received_bytes.load(Ordering::Relaxed),
        "writeProbe": { "writable": probe.is_ok(), "fault": probe.err().as_ref().map(fault_json) },
        "signals": {
            "logs": stream_json(&stats.logs),
            "traces": stream_json(&stats.traces),
            "metrics": stream_json(&stats.metrics),
        },
        "indexCache": {
            "budgetBytes": stats.cache.budget_bytes,
            "usedBytes": stats.cache.used_bytes,
            "entries": stats.cache.entries,
            "hits": stats.cache.hits,
            "misses": stats.cache.misses,
        },
        "notes": "rawBytes is the uncompressed size in Vyrtel's binary event encoding (sealed segments plus WAL).",
    })))
}

/// Resident set size of this process, where the OS makes it cheap to read.
pub fn process_rss() -> Option<u64> {
    #[cfg(target_os = "linux")]
    {
        let s = std::fs::read_to_string("/proc/self/status").ok()?;
        let line = s.lines().find(|l| l.starts_with("VmRSS:"))?;
        let kb: u64 = line.split_whitespace().nth(1)?.parse().ok()?;
        Some(kb * 1024)
    }
    #[cfg(not(target_os = "linux"))]
    {
        None
    }
}

pub async fn info(State(state): State<SharedState>) -> ApiResult<Json<Value>> {
    let st = state.clone();
    let stats = state.blocking(move || Ok(st.storage.stats())).await?;
    let queue = |s: &StreamStats| json!({ "depth": s.queue_depth, "capacity": s.queue_capacity });
    let active = |s: &StreamStats| json!({ "events": s.active_events + s.frozen_events, "bytes": s.active_bytes });
    let b = &state.budgets;
    let q = &state.query_outcomes;
    let storage_bytes: u64 = [&stats.logs, &stats.traces, &stats.metrics]
        .iter()
        .map(|s| s.segment_data_bytes + s.segment_index_bytes + s.wal_bytes)
        .sum();
    Ok(Json(json!({
        "name": "Vyrtel",
        "version": env!("CARGO_PKG_VERSION"),
        "startedAt": state.started_ts,
        "uptimeSeconds": state.started_at.elapsed().as_secs(),
        "dataDir": state.config.storage.path,
        "durability": state.config.storage.durability,
        "authEnabled": true,
        "ingestKeyRequired": state.api_keys.ingest_protected(),
        "sessionAuth": state.sessions.snapshot(),
        "apiKeyTouch": state.api_keys.touch_snapshot(),
        "eventCounts": {
            "logs": stats.logs.segment_events + stats.logs.active_events + stats.logs.frozen_events,
            "traces": stats.traces.segment_events + stats.traces.active_events + stats.traces.frozen_events,
            "metrics": stats.metrics.segment_events + stats.metrics.active_events + stats.metrics.frozen_events,
        },
        "storageBytes": storage_bytes,
        "queue": { "logs": queue(&stats.logs), "traces": queue(&stats.traces), "metrics": queue(&stats.metrics) },
        "ingest": {
            "maxConcurrent": state.ingest_max_concurrent,
            "concurrencySource": if state.ingest_admission.derived { "derived" } else { "explicit" },
            "maxRequestBytes": state.ingest_admission.max_request_bytes,
            "budgetBytes": state.ingest_admission.budget,
            "estimatedPeakBytes": state.ingest_admission.estimated_peak,
            "inFlight": state.ingest_max_concurrent.saturating_sub(state.ingest_permits.available_permits()),
            "rejected": state.ingest_rejected.load(Ordering::Relaxed),
            "discarding": crate::routes::ingest::MAX_DISCARD_PENDING
                .saturating_sub(state.discard_pending.available_permits()),
        },
        "connections": state.transport.snapshot(),
        "query": {
            "maxConcurrent": state.config.query.max_concurrent,
            "inFlight": state.config.query.max_concurrent.saturating_sub(state.query_permits.available_permits()),
            "rejected": q.rejected.load(Ordering::Relaxed),
            "cancelled": q.cancelled.load(Ordering::Relaxed),
            "timedOut": q.timed_out.load(Ordering::Relaxed),
            "tooExpensive": q.too_expensive.load(Ordering::Relaxed),
            "resultLimited": q.result_limited.load(Ordering::Relaxed),
        },
        "activeSegment": { "logs": active(&stats.logs), "traces": active(&stats.traces), "metrics": active(&stats.metrics) },
        "memory": {
            "limitBytes": b.total,
            "rssBytes": process_rss(),
            "budgets": b,
            "indexCacheUsedBytes": stats.cache.used_bytes,
        },
        "retention": state.config.retention,
        "receivedRequests": state.received_requests.load(Ordering::Relaxed),
        "liveSubscribers": state.live.receiver_count(),
        "recovery": recovery_summary(&stats),
    })))
}

pub async fn query_stats(State(state): State<SharedState>) -> ApiResult<Json<Value>> {
    let stored = state.db(|m| m.query_stats()).await?;
    let pending = state.engine.stats().pending();
    // Merge persisted totals with not-yet-flushed increments.
    let mut rows: Vec<metadata::StoredQueryStats> = stored;
    for p in pending {
        if let Some(r) = rows.iter_mut().find(|r| r.signal == p.signal && r.field == p.field) {
            r.queries += p.queries;
            r.bytes_read += p.bytes_read;
            r.segments_scanned += p.segments_scanned;
            r.segments_skipped += p.segments_skipped;
            r.total_ms += p.total_ms;
            r.index = p.index;
        } else {
            rows.push(crate::app::stored_query_stats(p));
        }
    }
    rows.sort_by(|a, b| b.queries.cmp(&a.queries).then(a.field.cmp(&b.field)));
    Ok(Json(json!({ "fields": rows })))
}

/// Effective configuration (secrets omitted).
pub async fn config(State(state): State<SharedState>) -> Json<Value> {
    Json(json!({ "config": state.config, "budgets": state.budgets }))
}

#[cfg(test)]
mod tests {
    use storage::fsutil::DirUsage;
    use storage::{FaultReason, QuarantinedFile, RecoveryReport};

    use super::*;

    fn stream(recovery: RecoveryReport, quarantine: DirUsage) -> StreamStats {
        StreamStats { recovery, quarantine: QuarantineScan::Read(quarantine), ..Default::default() }
    }

    fn unreadable(recovery: RecoveryReport) -> StreamStats {
        let quarantine = QuarantineScan::Unreadable {
            reason: FaultReason::PermissionDenied,
            message: "Permission denied (os error 13)".into(),
        };
        StreamStats { recovery, quarantine, ..Default::default() }
    }

    fn torn_tail() -> RecoveryReport {
        RecoveryReport {
            wal_files_replayed: 1,
            wal_records_replayed: 3,
            wal_bytes_discarded: 61,
            quarantined: vec![QuarantinedFile {
                signal: Signal::Logs,
                kind: QuarantineKind::WalTail,
                source: "000000000002.wal".into(),
                path: "/srv/vyrtel/data/quarantine/logs/000000000002.wal.tail-80.1791622413000".into(),
                bytes: 61,
                reason: "incomplete record at offset 80 (torn write)".into(),
            }],
            ..Default::default()
        }
    }

    #[test]
    fn replay_alone_is_clean() {
        let replayed = RecoveryReport {
            wal_files_replayed: 1,
            wal_records_replayed: 40,
            wal_events_replayed: 400,
            ..Default::default()
        };
        let s = stream(replayed, DirUsage::default());
        assert_eq!(RecoveryStatus::of(&s), RecoveryStatus::Clean);
        let v = recovery_json(&s);
        assert_eq!(v["status"], "clean");
        assert_eq!(v["walBytesDiscarded"], 0);
        assert_eq!(v["quarantined"], json!([]));
        assert_eq!(v["quarantineDir"], json!({ "files": 0, "bytes": 0, "error": null }));
    }

    #[test]
    fn damage_this_start_is_counted_per_kind() {
        let s = stream(torn_tail(), DirUsage { files: 1, bytes: 61 });
        let v = recovery_json(&s);
        assert_eq!(v["status"], "damaged");
        assert_eq!(v["walBytesDiscarded"], 61);
        assert_eq!(v["walTailsQuarantined"], 1);
        assert_eq!(v["segmentsQuarantined"], 0);
        assert_eq!(v["walFilesQuarantined"], 0);
        assert_eq!(v["quarantinedBytes"], 61);
        assert_eq!(v["quarantined"][0]["file"], "quarantine/logs/000000000002.wal.tail-80.1791622413000");
        assert!(!v.to_string().contains("/srv/vyrtel"), "no host paths: {v}");
    }

    #[test]
    fn files_left_in_quarantine_are_reported_after_a_clean_start() {
        // An earlier start (or an interrupted recovery) moved them there.
        let s = stream(RecoveryReport::default(), DirUsage { files: 2, bytes: 4096 });
        let v = recovery_json(&s);
        assert_eq!(v["status"], "quarantineRetained");
        assert_eq!(v["quarantined"], json!([]));
        assert_eq!(v["quarantineDir"], json!({ "files": 2, "bytes": 4096, "error": null }));
    }

    #[test]
    fn an_unreadable_quarantine_directory_is_unknown_not_clean() {
        let v = recovery_json(&unreadable(RecoveryReport::default()));
        assert_eq!(v["status"], "unknown");
        assert_eq!(
            v["quarantineDir"],
            json!({
                "files": null,
                "bytes": null,
                "error": { "reason": "permissionDenied", "message": "Permission denied (os error 13)" },
            })
        );
        // Damage found this start is still the more specific answer.
        assert_eq!(recovery_json(&unreadable(torn_tail()))["status"], "damaged");
    }

    #[test]
    fn summary_reports_the_worst_status_and_affected_signals() {
        let mut stats = StorageStats::default();
        assert_eq!(recovery_summary(&stats), json!({ "status": "clean", "signals": [] }));
        stats.metrics = stream(RecoveryReport::default(), DirUsage { files: 1, bytes: 10 });
        assert_eq!(recovery_summary(&stats), json!({ "status": "quarantineRetained", "signals": ["metrics"] }));
        stats.traces = unreadable(RecoveryReport::default());
        assert_eq!(recovery_summary(&stats), json!({ "status": "unknown", "signals": ["traces", "metrics"] }));
        stats.logs = stream(torn_tail(), DirUsage { files: 1, bytes: 61 });
        assert_eq!(recovery_summary(&stats), json!({ "status": "damaged", "signals": ["logs", "traces", "metrics"] }));
    }
}
