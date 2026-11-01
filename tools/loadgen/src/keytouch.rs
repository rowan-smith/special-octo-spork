//! API key last-used bookkeeping under many keys and key churn (AUTH-PERF-03).
//!
//! ```text
//! cargo run --release -p loadgen --bin keytouch -- --target http://127.0.0.1:8080 \
//!     --admin-password secret --duration 70s --active-keys 5000 --concurrency 16 \
//!     --churn-interval 2ms --out results/cap-churn
//! ```
//!
//! Logs in as the admin, creates `--active-keys` ingest keys and, for
//! `--duration`, runs:
//!
//! * `--concurrency` closed-loop producers, each request with the next key
//!   in turn (round robin over all of them), so every key is used and, past
//!   4096 keys, the server's last-used bookkeeping is at its entry cap;
//! * an auth probe: a keyless ingest request every `--probe-interval`,
//!   answered 401 by the authentication layer alone;
//! * unless `--churn-interval` is 0, one churner that creates an ingest key,
//!   ingests one event with it, revokes it and waits `--churn-interval`, so
//!   the number of revoked keys grows with the run;
//! * a sampler: `/api/v1/system/info` every `--sample-interval`, recording
//!   the server's RSS and `apiKeyTouch` (absent on servers without it).
//!
//! After the load it waits `--settle`, samples once more (post-load
//! cleanup), counts the acknowledged events back with a query and writes
//! `summary.json`. Process CPU and peak memory are measured outside, by
//! `scripts/bench/keytouch.sh`.

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use clap::Parser;
use serde_json::{Value, json};

#[derive(Parser, Clone)]
#[command(name = "keytouch", about = "API key last-used bookkeeping under many keys and key churn")]
struct Args {
    /// Vyrtel base URL.
    #[arg(long, default_value = "http://127.0.0.1:8080")]
    target: String,
    /// The admin password (`auth.admin_password`).
    #[arg(long, env = "VYRTEL_AUTH_ADMIN_PASSWORD")]
    admin_password: String,
    /// The admin user name.
    #[arg(long, default_value = "admin")]
    admin_username: String,
    /// How long to run the load, e.g. 30s, 2m.
    #[arg(long, default_value = "70s", value_parser = parse_duration)]
    duration: Duration,
    /// Ingest keys used round robin by the producers.
    #[arg(long, default_value_t = 8)]
    active_keys: usize,
    /// Concurrent closed-loop producers.
    #[arg(long, default_value_t = 16)]
    concurrency: usize,
    /// Events per ingest request.
    #[arg(long, default_value_t = 1)]
    batch: usize,
    /// Pause between one churner's create+use+revoke rounds; 0 disables churn.
    #[arg(long, default_value = "0ms", value_parser = parse_duration)]
    churn_interval: Duration,
    /// Time between auth probes.
    #[arg(long, default_value = "10ms", value_parser = parse_duration)]
    probe_interval: Duration,
    /// Time between system/info samples.
    #[arg(long, default_value = "1s", value_parser = parse_duration)]
    sample_interval: Duration,
    /// Wait after the load before the last sample.
    #[arg(long, default_value = "5s", value_parser = parse_duration)]
    settle: Duration,
    /// A request taking longer than this counts as a timeout.
    #[arg(long, default_value = "10s", value_parser = parse_duration)]
    timeout: Duration,
    /// Directory for summary.json.
    #[arg(long, default_value = "keytouch-out")]
    out: std::path::PathBuf,
    /// Label recorded in the summary and used to tag events.
    #[arg(long, default_value = "run")]
    label: String,
}

fn parse_duration(s: &str) -> Result<Duration, String> {
    let s = s.trim();
    let (num, mult) = if let Some(n) = s.strip_suffix("ms") {
        (n, 0.001)
    } else if let Some(n) = s.strip_suffix('s') {
        (n, 1.0)
    } else if let Some(n) = s.strip_suffix('m') {
        (n, 60.0)
    } else {
        (s, 1.0)
    };
    let v: f64 = num.parse().map_err(|_| format!("invalid duration '{s}'"))?;
    Ok(Duration::from_secs_f64(v * mult))
}

#[derive(Default)]
struct Latencies(Mutex<Vec<u32>>);

impl Latencies {
    fn record(&self, d: Duration) {
        self.0.lock().unwrap().push(d.as_micros().min(u32::MAX as u128) as u32);
    }

    fn summary(&self) -> Value {
        let mut v = self.0.lock().unwrap().clone();
        v.sort_unstable();
        let p = |q: f64| -> Value {
            if v.is_empty() {
                return Value::Null;
            }
            let i = ((v.len() as f64 - 1.0) * q).round() as usize;
            json!(v[i] as f64 / 1000.0)
        };
        json!({ "samples": v.len(), "p50Ms": p(0.50), "p95Ms": p(0.95), "p99Ms": p(0.99), "maxMs": p(1.0) })
    }
}

/// Responses by outcome.
#[derive(Default)]
struct Statuses {
    ok: AtomicU64,
    s401: AtomicU64,
    s429: AtomicU64,
    s5xx: AtomicU64,
    other: AtomicU64,
    timeouts: AtomicU64,
    errors: AtomicU64,
}

impl Statuses {
    fn count(&self, r: &Result<reqwest::Response, reqwest::Error>, ok: u16) {
        let c = match r {
            Ok(r) if r.status().as_u16() == ok => &self.ok,
            Ok(r) => match r.status().as_u16() {
                401 => &self.s401,
                429 => &self.s429,
                500..=599 => &self.s5xx,
                _ => &self.other,
            },
            Err(e) if e.is_timeout() => &self.timeouts,
            Err(_) => &self.errors,
        };
        c.fetch_add(1, Ordering::Relaxed);
    }

    fn summary(&self) -> Value {
        let n = |c: &AtomicU64| c.load(Ordering::Relaxed);
        json!({
            "ok": n(&self.ok), "unauthorized401": n(&self.s401), "rejected429": n(&self.s429),
            "serverError5xx": n(&self.s5xx), "otherStatus": n(&self.other),
            "timeouts": n(&self.timeouts), "transportErrors": n(&self.errors),
        })
    }
}

#[derive(Default)]
struct Totals {
    acknowledged: AtomicU64,
    ingest: Statuses,
    ack_latency: Latencies,
    auth: Statuses,
    auth_latency: Latencies,
    create: Statuses,
    used: Statuses,
    revoke: Statuses,
    churn_latency: Latencies,
    samples: Mutex<Vec<Value>>,
}

fn event_lines(args: &Args, id: usize, seq: &mut u64) -> String {
    let service = format!("keytouch-{}", args.label);
    (0..args.batch)
        .map(|_| {
            *seq += 1;
            format!(
                "{{\"message\":\"keytouch {id}-{seq}\",\"service\":\"{service}\",\"level\":\"Information\",\"properties\":{{\"producer\":{id},\"seq\":{seq}}}}}\n"
            )
        })
        .collect()
}

async fn ingest(client: &reqwest::Client, args: &Args, key: &str, body: String) -> reqwest::Result<reqwest::Response> {
    client
        .post(format!("{}/api/v1/events", args.target))
        .bearer_auth(key)
        .header("content-type", "application/x-ndjson")
        .body(body)
        .send()
        .await
}

async fn accepted(r: reqwest::Response) -> u64 {
    r.json::<Value>().await.ok().and_then(|v| v["accepted"].as_u64()).unwrap_or(0)
}

#[allow(clippy::too_many_arguments)]
async fn producer(
    args: Args,
    client: reqwest::Client,
    keys: Arc<Vec<String>>,
    next: Arc<AtomicUsize>,
    totals: Arc<Totals>,
    until: Instant,
    id: usize,
) {
    let mut seq = 0u64;
    while Instant::now() < until {
        let key = &keys[next.fetch_add(1, Ordering::Relaxed) % keys.len()];
        let body = event_lines(&args, id, &mut seq);
        let started = Instant::now();
        let r = ingest(&client, &args, key, body).await;
        totals.ingest.count(&r, 200);
        if let Ok(r) = r
            && r.status().as_u16() == 200
        {
            let n = accepted(r).await;
            totals.ack_latency.record(started.elapsed());
            totals.acknowledged.fetch_add(n, Ordering::Relaxed);
        }
    }
}

/// Keyless ingest requests: 401 from authentication alone.
async fn probe(args: Args, client: reqwest::Client, totals: Arc<Totals>, until: Instant) {
    let url = format!("{}/api/v1/events", args.target);
    let mut tick = tokio::time::interval(args.probe_interval);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    while Instant::now() < until {
        tick.tick().await;
        let started = Instant::now();
        let r = client.post(&url).header("content-type", "application/x-ndjson").body("{}\n").send().await;
        totals.auth.count(&r, 401);
        if r.as_ref().is_ok_and(|r| r.status().as_u16() == 401) {
            totals.auth_latency.record(started.elapsed());
        }
    }
}

/// Create a key, use it once, revoke it; returns the rounds completed.
async fn churner(
    args: Args,
    admin: reqwest::Client,
    client: reqwest::Client,
    totals: Arc<Totals>,
    until: Instant,
) -> u64 {
    let mut rounds = 0;
    let mut seq = 0u64;
    while Instant::now() < until {
        let started = Instant::now();
        let r = admin
            .post(format!("{}/api/v1/api-keys", args.target))
            .json(&json!({ "name": format!("keytouch-churn-{rounds}"), "scope": "ingest" }))
            .send()
            .await;
        totals.create.count(&r, 201);
        let Ok(r) = r else { continue };
        if r.status().as_u16() != 201 {
            continue;
        }
        let Some((id, key)) =
            r.json::<Value>().await.ok().and_then(|v| Some((v["id"].as_i64()?, v["key"].as_str()?.to_string())))
        else {
            continue;
        };
        let r = ingest(&client, &args, &key, event_lines(&args, usize::MAX, &mut seq)).await;
        totals.used.count(&r, 200);
        if let Ok(r) = r
            && r.status().as_u16() == 200
        {
            totals.acknowledged.fetch_add(accepted(r).await, Ordering::Relaxed);
        }
        let r = admin.delete(format!("{}/api/v1/api-keys/{id}", args.target)).send().await;
        totals.revoke.count(&r, 204);
        totals.churn_latency.record(started.elapsed());
        rounds += 1;
        tokio::time::sleep(args.churn_interval).await;
    }
    rounds
}

async fn info(admin: &reqwest::Client, args: &Args) -> anyhow::Result<Value> {
    Ok(admin.get(format!("{}/api/v1/system/info", args.target)).send().await?.json().await?)
}

fn sample(info: &Value, at: f64) -> Value {
    json!({ "t": at, "rssBytes": info["memory"]["rssBytes"], "apiKeyTouch": info["apiKeyTouch"] })
}

async fn sampler(args: Args, admin: reqwest::Client, totals: Arc<Totals>, start: Instant, until: Instant) {
    let mut tick = tokio::time::interval(args.sample_interval);
    while Instant::now() < until {
        tick.tick().await;
        if let Ok(i) = info(&admin, &args).await {
            totals.samples.lock().unwrap().push(sample(&i, start.elapsed().as_secs_f64()));
        }
    }
}

/// Least-squares slope of RSS over the samples in the second half of the
/// load, in MiB per minute.
fn rss_slope(samples: &[Value], duration: f64) -> Option<f64> {
    let pts: Vec<(f64, f64)> = samples
        .iter()
        .filter_map(|s| Some((s["t"].as_f64()?, s["rssBytes"].as_f64()? / 1048576.0)))
        .filter(|(t, _)| *t >= duration / 2.0 && *t <= duration)
        .collect();
    if pts.len() < 3 {
        return None;
    }
    let n = pts.len() as f64;
    let (mt, mr) = (pts.iter().map(|p| p.0).sum::<f64>() / n, pts.iter().map(|p| p.1).sum::<f64>() / n);
    let cov: f64 = pts.iter().map(|(t, r)| (t - mt) * (r - mr)).sum();
    let var: f64 = pts.iter().map(|(t, _)| (t - mt).powi(2)).sum();
    (var > 0.0).then(|| cov / var * 60.0)
}

/// The largest value of `field` of `apiKeyTouch` over the samples.
fn peak(samples: &[Value], field: &str) -> Value {
    samples.iter().filter_map(|s| s["apiKeyTouch"][field].as_u64()).max().map_or(Value::Null, Value::from)
}

/// A client that sends the admin session cookie.
async fn login(args: &Args) -> anyhow::Result<reqwest::Client> {
    let r = reqwest::Client::new()
        .post(format!("{}/api/v1/auth/login", args.target))
        .json(&json!({ "username": args.admin_username, "password": args.admin_password }))
        .send()
        .await
        .context("logging in")?;
    if !r.status().is_success() {
        bail!("login failed: {}", r.status());
    }
    let cookie = r.headers().get("set-cookie").context("no session cookie")?.to_str()?;
    let cookie = cookie.split(';').next().unwrap_or_default().to_string();
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert(reqwest::header::COOKIE, cookie.parse()?);
    Ok(reqwest::Client::builder().default_headers(headers).timeout(args.timeout).build()?)
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    if args.active_keys == 0 {
        bail!("--active-keys must be at least 1");
    }
    std::fs::create_dir_all(&args.out).with_context(|| format!("creating {}", args.out.display()))?;
    let admin = login(&args).await?;
    let mut keys = Vec::with_capacity(args.active_keys);
    for i in 0..args.active_keys {
        let created: Value = admin
            .post(format!("{}/api/v1/api-keys", args.target))
            .json(&json!({ "name": format!("keytouch-{}-{i}", args.label), "scope": "ingest" }))
            .send()
            .await?
            .json()
            .await?;
        keys.push(created["key"].as_str().context("creating an active key")?.to_string());
    }
    let keys = Arc::new(keys);
    let before = info(&admin, &args).await?;

    let start = Instant::now();
    let until = start + args.duration;
    let totals = Arc::new(Totals::default());
    let next = Arc::new(AtomicUsize::new(0));
    let mut load = Vec::new();
    for id in 0..args.concurrency {
        let client = reqwest::Client::builder().timeout(args.timeout).build()?;
        load.push(tokio::spawn(producer(args.clone(), client, keys.clone(), next.clone(), totals.clone(), until, id)));
    }
    let client = reqwest::Client::builder().timeout(args.timeout).build()?;
    load.push(tokio::spawn(probe(args.clone(), client, totals.clone(), until)));
    load.push(tokio::spawn(sampler(args.clone(), admin.clone(), totals.clone(), start, until)));
    let churn = (!args.churn_interval.is_zero()).then(|| {
        let client = reqwest::Client::builder().timeout(args.timeout).build().unwrap();
        tokio::spawn(churner(args.clone(), admin.clone(), client, totals.clone(), until))
    });
    for t in load {
        t.await?;
    }
    let rounds = match churn {
        Some(t) => t.await?,
        None => 0,
    };
    let elapsed = start.elapsed().as_secs_f64();

    tokio::time::sleep(args.settle).await;
    let after = info(&admin, &args).await?;
    let persisted = admin
        .post(format!("{}/api/v1/query/logs/count", args.target))
        .json(&json!({ "query": format!("service = keytouch-{}", args.label) }))
        .send()
        .await?
        .json::<Value>()
        .await?["count"]
        .as_u64();
    let t = &totals;
    let acknowledged = t.acknowledged.load(Ordering::Relaxed);
    let samples = t.samples.lock().unwrap().clone();
    let (b, a) = (&before["apiKeyTouch"], &after["apiKeyTouch"]);
    let delta = |k: &str| match (b[k].as_u64(), a[k].as_u64()) {
        (Some(x), Some(y)) => json!(y.saturating_sub(x)),
        _ => Value::Null,
    };
    let summary = json!({
        "label": args.label,
        "durationS": elapsed,
        "args": {
            "activeKeys": args.active_keys, "concurrency": args.concurrency, "batch": args.batch,
            "churnIntervalMs": args.churn_interval.as_millis() as u64,
            "probeIntervalMs": args.probe_interval.as_millis() as u64,
            "settleMs": args.settle.as_millis() as u64,
            "timeoutMs": args.timeout.as_millis() as u64,
        },
        "server": {
            "version": before["version"],
            "rssBytesBefore": before["memory"]["rssBytes"],
            "rssBytesAfterSettle": after["memory"]["rssBytes"],
            "rssSlopeMiBPerMin": rss_slope(&samples, elapsed),
        },
        "ingest": {
            "statuses": t.ingest.summary(),
            "acknowledgedEvents": acknowledged, "persistedEvents": persisted,
            "eventsPerSecond": acknowledged as f64 / elapsed,
            "ackLatency": t.ack_latency.summary(),
        },
        "auth": { "statuses": t.auth.summary(), "latency": t.auth_latency.summary() },
        "churn": {
            "rounds": rounds,
            "create": t.create.summary(), "use": t.used.summary(), "revoke": t.revoke.summary(),
            "roundLatency": t.churn_latency.summary(),
        },
        "touch": {
            "reported": a.is_object(),
            "peakTracked": peak(&samples, "tracked"), "peakPending": peak(&samples, "peakPending"),
            "peakWriters": peak(&samples, "writers"),
            "afterSettle": a,
            "during": {
                "queued": delta("queued"), "written": delta("written"), "batches": delta("batches"),
                "failedBatches": delta("failedBatches"), "dropped": delta("dropped"),
                "deferred": delta("deferred"), "retired": delta("retired"), "forgotten": delta("forgotten"),
            },
        },
        "samples": samples,
        "reconciled": persisted == Some(acknowledged),
    });
    std::fs::write(args.out.join("summary.json"), serde_json::to_string_pretty(&summary)?)?;
    if persisted != Some(acknowledged) {
        bail!("{acknowledged} events acknowledged but {persisted:?} stored");
    }
    Ok(())
}
