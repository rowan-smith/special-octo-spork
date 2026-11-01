#!/usr/bin/env node
// Summarise scripts/bench/keytouch.sh results and check them against the
// AUTH-PERF-03 budgets, which were fixed before any run (docs/testing.md,
// "API key last-used benchmark").
//
// The evidence must be complete before any budget is checked; anything
// missing fails the report:
//
// * every expected cell (few-churn and cap-churn, or those matching
//   --cells) has results for both binaries and no other cell is present;
// * each cell has at least --min-runs (default 3) usable runs per binary,
//   the same number for both, run with the same workload settings;
// * a run is usable when its summary.json and server.json parse, its gated
//   values are finite, it acknowledged events, it measured the auth probe
//   and it completed churn rounds; a candidate run must also report its
//   last-used bookkeeping (`apiKeyTouch`).
//
// Budgets:
//
// * every run, both binaries: every acknowledged event persisted; no ingest
//   401, 5xx, timeout or transport error; every key create and revoke
//   succeeded and no churn request got a 401, 5xx, timeout or transport
//   error (429s from ingest admission are reported, not failures); every
//   auth probe answered 401;
// * every candidate run, absolute bounds (AC1/AC2): tracked keys at most
//   4096 and waiting keys at most 1024 at every sample, at most 1 writer;
//   after the settle period nothing waiting, no writer, and at most as many
//   keys tracked as there are active keys (not growing with revoked ones);
//   no failed write batch;
// * per cell, candidate median against baseline median:
//   - accepted events/s at least 90%;
//   - server CPU seconds per accepted event at most 110%;
//   - auth probe p99 and ingest ACK p99 at most max(110%, +1 ms);
//   - peak memory at most 110%.
//
//   node scripts/bench/keytouch-report.mjs RESULTS_DIR [--cells REGEX] [--min-runs N] [--json OUT]
//
// Prints a Markdown table of medians (and every usable run's raw value) and
// exits 1 if the evidence is incomplete or any budget is missed.

import { readdirSync, readFileSync, existsSync, writeFileSync } from 'node:fs';
import { join } from 'node:path';
import { pathToFileURL } from 'node:url';

export const CELLS = ['few-churn', 'cap-churn'];
const BINARIES = ['baseline', 'candidate'];
export const TRACKED_CAP = 4096;
export const PENDING_CAP = 1024;

const median = (xs) => {
  const v = [...xs].sort((a, b) => a - b);
  if (!v.length) return null;
  const m = Math.floor(v.length / 2);
  return v.length % 2 ? v[m] : (v[m - 1] + v[m]) / 2;
};

const finite = (x) => typeof x === 'number' && Number.isFinite(x);

const readJson = (path) => JSON.parse(readFileSync(path, 'utf8'));

/**
 * Least-squares slope (MiB/min) of `[seconds, bytes]` samples in the second
 * half of `[0, end]`, or null with fewer than 3.
 */
export function slope(samples, end) {
  const pts = samples.filter(([t]) => t >= end / 2 && t <= end).map(([t, b]) => [t, b / 2 ** 20]);
  if (pts.length < 3) return null;
  const mt = pts.reduce((a, [t]) => a + t, 0) / pts.length;
  const mr = pts.reduce((a, [, r]) => a + r, 0) / pts.length;
  const cov = pts.reduce((a, [t, r]) => a + (t - mt) * (r - mr), 0);
  const v = pts.reduce((a, [t]) => a + (t - mt) ** 2, 0);
  return v > 0 ? (cov / v) * 60 : null;
}

/**
 * RSS slope during the load and RSS after it, from the server's own report
 * (Linux) or else from the script's samples (any platform). Those span key
 * creation, the load and the settle, so the slope uses the second half of
 * all but the settle period.
 */
function rssFrom(s, p) {
  const own = s?.server ?? {};
  if (finite(own.rssSlopeMiBPerMin) && own.rssBytesAfterSettle > 0) {
    return { slope: own.rssSlopeMiBPerMin, afterSettleMiB: own.rssBytesAfterSettle / 2 ** 20 };
  }
  const samples = Array.isArray(p?.rssSamples)
    ? p.rssSamples.filter((x) => Array.isArray(x) && x.length === 2 && x.every(finite))
    : [];
  if (!samples.length) return { slope: null, afterSettleMiB: null };
  const last = samples[samples.length - 1];
  const loadEnd = last[0] - (s?.args?.settleMs ?? 0) / 1000;
  return { slope: slope(samples, loadEnd), afterSettleMiB: last[1] / 2 ** 20 };
}

/** One run's values, or the reason it is unusable. */
function loadRun(path, binary) {
  let s;
  let p;
  try {
    s = readJson(join(path, 'summary.json'));
    p = readJson(join(path, 'server.json'));
  } catch (e) {
    return { error: `unreadable: ${e.message}` };
  }
  const acked = s?.ingest?.acknowledgedEvents;
  const r = {
    eventsPerSecond: s?.ingest?.eventsPerSecond,
    ack: s?.ingest?.ackLatency ?? {},
    auth: s?.auth?.latency ?? {},
    ingest: s?.ingest?.statuses,
    authStatuses: s?.auth?.statuses,
    churn: s?.churn,
    touch: s?.touch,
    args: s?.args,
    reconciled: s?.reconciled,
    cpuSeconds: p?.cpuSeconds,
    cpuMicrosPerEvent: finite(acked) && acked > 0 ? (p?.cpuSeconds * 1e6) / acked : null,
    peakMemoryBytes: p?.peakMemoryBytes,
    rss: rssFrom(s, p),
  };
  if (!r.ingest || !r.authStatuses || !r.churn?.create || !r.churn?.use || !r.churn?.revoke || !r.args || !r.touch) {
    return { error: 'summary.json is missing sections' };
  }
  if (!finite(acked) || acked <= 0) return { error: 'no events acknowledged' };
  if (!finite(r.auth.samples) || r.auth.samples <= 0) return { error: 'no auth probe measured' };
  if (!(r.churn.rounds > 0)) return { error: 'no churn rounds' };
  for (const [name, v] of [
    ['events/s', r.eventsPerSecond],
    ['auth p99', r.auth.p99Ms],
    ['ACK p99', r.ack.p99Ms],
    ['CPU seconds', r.cpuSeconds],
    ['CPU per event', r.cpuMicrosPerEvent],
    ['peak memory', r.peakMemoryBytes],
  ]) {
    if (!finite(v) || v < 0) return { error: `${name} is ${JSON.stringify(v)}` };
  }
  if (r.peakMemoryBytes <= 0) return { error: 'peak memory not measured' };
  if (binary === 'candidate') {
    const t = r.touch;
    if (t.reported !== true || typeof t.afterSettle !== 'object' || t.afterSettle === null) {
      return { error: 'no apiKeyTouch reported' };
    }
    for (const k of ['peakTracked', 'peakPending', 'peakWriters']) {
      if (!finite(t[k])) return { error: `${k} is ${JSON.stringify(t[k])}` };
    }
  }
  return r;
}

const workload = (r) =>
  JSON.stringify([r.args.activeKeys, r.args.concurrency, r.args.batch, r.args.churnIntervalMs, r.args.timeoutMs]);

const metrics = [
  ['events/s', (r) => r.eventsPerSecond, 'higher'],
  ['CPU µs/event', (r) => r.cpuMicrosPerEvent, 'lower'],
  ['ACK p50 ms', (r) => r.ack.p50Ms, 'info'],
  ['ACK p99 ms', (r) => r.ack.p99Ms, 'latency'],
  ['ACK max ms', (r) => r.ack.maxMs, 'info'],
  ['auth p50 ms', (r) => r.auth.p50Ms, 'info'],
  ['auth p99 ms', (r) => r.auth.p99Ms, 'latency'],
  ['auth max ms', (r) => r.auth.maxMs, 'info'],
  ['429s', (r) => r.ingest.rejected429, 'info'],
  ['churn rounds', (r) => r.churn.rounds, 'info'],
  ['churn round p99 ms', (r) => r.churn.roundLatency?.p99Ms, 'info'],
  ['peak MiB', (r) => r.peakMemoryBytes / 2 ** 20, 'memory'],
  ['RSS after settle MiB', (r) => r.rss.afterSettleMiB, 'info'],
  ['RSS slope MiB/min', (r) => r.rss.slope, 'info'],
  ['peak tracked', (r) => r.touch.peakTracked, 'info'],
  ['tracked after settle', (r) => r.touch.afterSettle?.tracked, 'info'],
  ['peak waiting', (r) => r.touch.peakPending, 'info'],
  ['writes queued', (r) => r.touch.during?.queued, 'info'],
  ['write batches', (r) => r.touch.during?.batches, 'info'],
  ['deferred', (r) => r.touch.during?.deferred, 'info'],
  ['dropped', (r) => r.touch.during?.dropped, 'info'],
  ['forgotten', (r) => r.touch.during?.forgotten, 'info'],
];

/** Absolute AC1/AC2 bounds of one candidate run; returns failures. */
export function candidateBounds(r) {
  const f = [];
  const t = r.touch;
  const a = t.afterSettle;
  if (t.peakTracked > TRACKED_CAP) f.push(`tracked ${t.peakTracked} > ${TRACKED_CAP}`);
  if (t.peakPending > PENDING_CAP) f.push(`waiting ${t.peakPending} > ${PENDING_CAP}`);
  if (t.peakWriters > 1) f.push(`${t.peakWriters} writers`);
  if (a.pending !== 0 || a.writers !== 0) f.push(`after settle: ${a.pending} waiting, ${a.writers} writers`);
  if (!(a.tracked <= Math.min(TRACKED_CAP, r.args.activeKeys))) {
    f.push(`after settle: ${a.tracked} keys tracked, more than the ${r.args.activeKeys} active`);
  }
  if (a.failedBatches !== 0) f.push(`${a.failedBatches} failed write batches`);
  return f;
}

/** Checks the results in `dir`; returns the Markdown lines, per-cell values and every failure. */
export function evaluate(dir, { cells = '.', minRuns = 3 } = {}) {
  const failures = [];
  let pattern;
  try {
    pattern = new RegExp(cells);
  } catch (e) {
    return { lines: [], report: {}, failures: [`--cells: ${e.message}`] };
  }
  const expected = CELLS.filter((c) => pattern.test(c));
  if (!expected.length) failures.push(`--cells ${cells} matches none of ${CELLS.join(', ')}`);

  const runs = {};
  for (const binary of BINARIES) {
    runs[binary] = {};
    const root = join(dir, binary);
    const present = existsSync(root) ? readdirSync(root) : [];
    for (const cell of present.filter((c) => !expected.includes(c))) {
      failures.push(`${binary}: unexpected cell ${cell}`);
    }
    for (const cell of expected) {
      const path = join(root, cell);
      if (!existsSync(path)) {
        failures.push(`${binary} ${cell}: no results`);
        runs[binary][cell] = [];
        continue;
      }
      runs[binary][cell] = readdirSync(path)
        .sort()
        .flatMap((run) => {
          const r = loadRun(join(path, run), binary);
          if (r.error) {
            failures.push(`${binary} ${cell} ${run}: unusable, ${r.error}`);
            return [];
          }
          return [{ run, ...r }];
        });
    }
  }

  const bad = (s) => s.unauthorized401 || s.serverError5xx || s.otherStatus || s.timeouts || s.transportErrors;
  for (const binary of BINARIES) {
    for (const [cell, list] of Object.entries(runs[binary])) {
      for (const r of list) {
        const where = `${binary} ${cell} ${r.run}`;
        if (r.reconciled !== true) failures.push(`${where}: acknowledged events not all persisted`);
        if (bad(r.ingest)) failures.push(`${where}: ingest ${JSON.stringify(r.ingest)}`);
        const a = r.authStatuses;
        if (a.serverError5xx || a.timeouts || a.transportErrors || a.otherStatus || a.rejected429) {
          failures.push(`${where}: auth probe ${JSON.stringify(a)}`);
        }
        for (const op of ['create', 'revoke']) {
          const s = r.churn[op];
          if (bad(s) || s.rejected429) failures.push(`${where}: key ${op} ${JSON.stringify(s)}`);
        }
        if (bad(r.churn.use)) failures.push(`${where}: churn ingest ${JSON.stringify(r.churn.use)}`);
        if (binary === 'candidate') {
          for (const f of candidateBounds(r)) failures.push(`${where}: ${f}`);
        }
      }
    }
  }

  const fmt = (x) => (!finite(x) ? '–' : Math.abs(x) >= 100 ? x.toFixed(0) : x.toFixed(2));
  const lines = [];
  const report = {};
  for (const cell of expected) {
    const b = runs.baseline[cell];
    const c = runs.candidate[cell];
    let comparable = true;
    for (const [binary, list] of [
      ['baseline', b],
      ['candidate', c],
    ]) {
      if (list.length < minRuns) {
        failures.push(`${binary} ${cell}: ${list.length} usable runs, at least ${minRuns} required`);
        comparable = false;
      }
    }
    if (b.length !== c.length) {
      failures.push(`${cell}: ${b.length} baseline runs but ${c.length} candidate runs`);
      comparable = false;
    }
    const workloads = new Set([...b, ...c].map(workload));
    if (workloads.size > 1) {
      failures.push(`${cell}: runs used different workloads ${[...workloads].join(' / ')}`);
      comparable = false;
    }

    report[cell] = {};
    lines.push(`\n### ${cell}\n`);
    lines.push('| Metric | Baseline median | Candidate median | Change | Baseline runs | Candidate runs |');
    lines.push('|---|---:|---:|---:|---|---|');
    for (const [label, get, kind] of metrics) {
      const bv = b.map(get);
      const cv = c.map(get);
      const usable = (xs) => xs.length > 0 && xs.every(finite);
      const bm = usable(bv) ? median(bv) : null;
      const cm = usable(cv) ? median(cv) : null;
      const change = finite(bm) && finite(cm) && bm !== 0 ? ((cm - bm) / bm) * 100 : null;
      report[cell][label] = { baseline: bv, candidate: cv, baselineMedian: bm, candidateMedian: cm };
      lines.push(
        `| ${label} | ${fmt(bm)} | ${fmt(cm)} | ${change == null ? '–' : `${change >= 0 ? '+' : ''}${change.toFixed(1)}%`} | ${bv.map(fmt).join(', ')} | ${cv.map(fmt).join(', ')} |`,
      );
      if (kind === 'info' || !comparable) continue;
      if (!finite(bm) || !finite(cm)) {
        failures.push(`${cell}: ${label} has missing or non-finite values`);
        continue;
      }
      if (kind === 'higher' && cm < 0.9 * bm) failures.push(`${cell}: ${label} ${fmt(cm)} < 90% of ${fmt(bm)}`);
      if ((kind === 'lower' || kind === 'memory') && cm > 1.1 * bm) {
        failures.push(`${cell}: ${label} ${fmt(cm)} > 110% of ${fmt(bm)}`);
      }
      if (kind === 'latency' && cm > Math.max(1.1 * bm, bm + 1)) {
        failures.push(`${cell}: ${label} ${fmt(cm)} > max(110%, +1 ms) of ${fmt(bm)}`);
      }
    }
  }
  return { lines, report, failures };
}

function main(argv) {
  const opts = { cells: '.', minRuns: 3 };
  let dir;
  let jsonOut = null;
  for (let i = 0; i < argv.length; i++) {
    const a = argv[i];
    if (a === '--json') jsonOut = argv[++i];
    else if (a === '--cells') opts.cells = argv[++i];
    else if (a === '--min-runs') opts.minRuns = Number(argv[++i]);
    else if (!dir && !a.startsWith('--')) dir = a;
    else {
      console.error(`unknown argument ${a}`);
      return 2;
    }
  }
  if (!dir || !Number.isInteger(opts.minRuns) || opts.minRuns < 1 || jsonOut === undefined || opts.cells === undefined) {
    console.error('usage: keytouch-report.mjs RESULTS_DIR [--cells REGEX] [--min-runs N] [--json OUT]');
    return 2;
  }
  const { lines, report, failures } = evaluate(dir, opts);
  console.log(lines.join('\n'));
  console.log(failures.length ? `\nNot met:\n- ${failures.join('\n- ')}` : '\nAll budgets met.');
  if (jsonOut) writeFileSync(jsonOut, JSON.stringify({ options: opts, report, failures }, null, 2));
  return failures.length ? 1 : 0;
}

if (import.meta.url === pathToFileURL(process.argv[1]).href) process.exit(main(process.argv.slice(2)));
