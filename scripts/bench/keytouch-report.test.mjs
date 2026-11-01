// Tests for the AUTH-PERF-03 benchmark report (keytouch-report.mjs): incomplete evidence fails
// before any budget is checked, the candidate's absolute bounds, and the comparative budgets.
//
//   node --test scripts/bench/*.test.mjs
//
// Each test writes a results directory laid out like scripts/bench/keytouch.sh output.

import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { afterEach, test } from 'node:test';

import { CELLS, evaluate, slope } from './keytouch-report.mjs';

const SCRIPT = path.join(import.meta.dirname, 'keytouch-report.mjs');
const dirs = [];
afterEach(() => {
    for (const d of dirs.splice(0)) fs.rmSync(d, { recursive: true, force: true });
});

const statuses = (ok) => ({
    ok,
    unauthorized401: 0,
    rejected429: 0,
    serverError5xx: 0,
    otherStatus: 0,
    timeouts: 0,
    transportErrors: 0,
});

/** A passing run of `cell` by `binary`; `edit` changes its summary and server record. */
function run(binary, cell, edit = () => {}) {
    const activeKeys = cell.startsWith('cap') ? 5000 : 8;
    const reported = binary === 'candidate';
    const summary = {
        args: { activeKeys, concurrency: 16, batch: 1, churnIntervalMs: 2, timeoutMs: 10000 },
        server: { rssBytesAfterSettle: 90 * 2 ** 20, rssSlopeMiBPerMin: 0.1 },
        ingest: {
            statuses: statuses(1000),
            acknowledgedEvents: 100000,
            eventsPerSecond: 5000,
            ackLatency: { samples: 1000, p50Ms: 1, p99Ms: 3, maxMs: 9 },
        },
        auth: { statuses: { ...statuses(0), unauthorized401: 100 }, latency: { samples: 100, p50Ms: 0.5, p99Ms: 2, maxMs: 4 } },
        churn: {
            rounds: 500,
            create: statuses(500),
            use: statuses(500),
            revoke: statuses(500),
            roundLatency: { samples: 500, p99Ms: 4 },
        },
        touch: reported
            ? {
                  reported: true,
                  peakTracked: Math.min(4096, activeKeys + 1),
                  peakPending: 600,
                  peakWriters: 1,
                  afterSettle: { tracked: Math.min(4096, activeKeys), pending: 0, writers: 0, failedBatches: 0 },
                  during: { queued: 5000, batches: 40, deferred: 0, dropped: 0, forgotten: 500 },
              }
            : { reported: false, peakTracked: null, peakPending: null, peakWriters: null, afterSettle: null, during: {} },
        reconciled: true,
    };
    const server = { cpuSeconds: 10, peakMemoryBytes: 100 * 2 ** 20 };
    edit(summary, server);
    return { summary, server };
}

/** Writes `runs` runs of every cell for both binaries; `change(binary, cell, i)` may edit or drop one (return null). */
function results({ cells = CELLS, runs = 3, change = () => undefined } = {}) {
    const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'keytouch-report-'));
    dirs.push(dir);
    for (const binary of ['baseline', 'candidate']) {
        for (const cell of cells) {
            for (let i = 1; i <= runs; i++) {
                if (change(binary, cell, i) === null) continue;
                const r = run(binary, cell, (s, p) => change(binary, cell, i, s, p));
                const d = path.join(dir, binary, cell, `run${i}`);
                fs.mkdirSync(d, { recursive: true });
                fs.writeFileSync(path.join(d, 'summary.json'), JSON.stringify(r.summary));
                fs.writeFileSync(path.join(d, 'server.json'), JSON.stringify(r.server));
            }
        }
    }
    return dir;
}

const failures = (dir, opts) => evaluate(dir, opts).failures;

test('complete matched evidence within budget passes', () => {
    assert.deepEqual(failures(results()), []);
    const cli = spawnSync(process.execPath, [SCRIPT, results()], { encoding: 'utf8' });
    assert.equal(cli.status, 0, cli.stderr);
    assert.match(cli.stdout, /All budgets met\./);
});

test('an empty or missing results directory fails', () => {
    const empty = fs.mkdtempSync(path.join(os.tmpdir(), 'keytouch-report-'));
    dirs.push(empty);
    const f = failures(empty);
    assert.equal(f.filter((x) => x.endsWith(': no results')).length, 4, f.join('\n'));
    assert.ok(f.includes('baseline few-churn: 0 usable runs, at least 3 required'));
    const cli = spawnSync(process.execPath, [SCRIPT, path.join(empty, 'nothing-here')], { encoding: 'utf8' });
    assert.equal(cli.status, 1);
    assert.doesNotMatch(cli.stdout, /All budgets met/);
});

test('missing, short or unequal evidence fails', () => {
    const f = failures(results({ change: (b, c) => (b === 'baseline' && c === 'cap-churn' ? null : undefined) }));
    assert.deepEqual(f, [
        'baseline cap-churn: no results',
        'baseline cap-churn: 0 usable runs, at least 3 required',
        'cap-churn: 0 baseline runs but 3 candidate runs',
    ]);
    assert.ok(failures(results({ runs: 2 })).includes('baseline few-churn: 2 usable runs, at least 3 required'));
    assert.deepEqual(failures(results({ runs: 2 }), { minRuns: 2 }), []);
    const g = failures(results({ change: (b, c, i, s) => s && b === 'candidate' && c === 'few-churn' && (s.args.activeKeys = 9) }));
    assert.ok(g.some((x) => x.startsWith('few-churn: runs used different workloads')), g.join('\n'));
});

test('unusable runs: missing sections, values, churn or bookkeeping', () => {
    const cases = [
        [(s) => delete s.touch, 'summary.json is missing sections'],
        [(s) => (s.auth.latency.p99Ms = null), 'auth p99 is null'],
        [(s) => (s.ingest.ackLatency.p99Ms = undefined), 'ACK p99 is undefined'],
        [(s, p) => (p.peakMemoryBytes = 0), 'peak memory not measured'],
        [(s) => (s.churn.rounds = 0), 'no churn rounds'],
        [(s) => (s.touch.reported = false), 'no apiKeyTouch reported'],
        [(s) => (s.touch.peakPending = null), 'peakPending is null'],
    ];
    for (const [edit, reason] of cases) {
        const f = failures(results({ change: (b, c, i, s, p) => s && b === 'candidate' && c === 'few-churn' && i === 1 && edit(s, p) }));
        assert.ok(f.includes(`candidate few-churn run1: unusable, ${reason}`), `${reason}:\n${f.join('\n')}`);
    }
    // The baseline has no bookkeeping to report; that is expected.
    assert.deepEqual(failures(results()).filter((x) => x.startsWith('baseline')), []);
    const dir = results();
    fs.writeFileSync(path.join(dir, 'baseline', 'cap-churn', 'run2', 'summary.json'), '{ truncated');
    assert.ok(failures(dir).some((x) => x.startsWith('baseline cap-churn run2: unusable, unreadable')));
});

test('candidate bounds: caps, post-load cleanup and failed writes', () => {
    const cand = (edit) =>
        failures(results({ change: (b, c, i, s) => s && b === 'candidate' && c === 'cap-churn' && i === 2 && edit(s.touch) }));
    assert.deepEqual(cand((t) => (t.peakTracked = 4096)), []);
    assert.deepEqual(cand((t) => (t.peakTracked = 4097)), ['candidate cap-churn run2: tracked 4097 > 4096']);
    assert.deepEqual(cand((t) => (t.peakPending = 1025)), ['candidate cap-churn run2: waiting 1025 > 1024']);
    assert.deepEqual(cand((t) => (t.peakWriters = 2)), ['candidate cap-churn run2: 2 writers']);
    assert.deepEqual(cand((t) => (t.afterSettle.pending = 3)), ['candidate cap-churn run2: after settle: 3 waiting, 0 writers']);
    assert.deepEqual(cand((t) => (t.afterSettle.failedBatches = 1)), ['candidate cap-churn run2: 1 failed write batches']);
    const few = failures(
        results({ change: (b, c, i, s) => s && b === 'candidate' && c === 'few-churn' && i === 1 && (s.touch.afterSettle.tracked = 9) }),
    );
    assert.deepEqual(few, ['candidate few-churn run1: after settle: 9 keys tracked, more than the 8 active']);
});

test('budgets: throughput, CPU, latency and memory', () => {
    const cand = (edit) => results({ change: (b, c, i, s, p) => s && b === 'candidate' && c === 'cap-churn' && edit(s, p) });
    assert.deepEqual(failures(cand((s) => (s.ingest.eventsPerSecond = 4600))), []);
    assert.deepEqual(failures(cand((s) => (s.ingest.eventsPerSecond = 4400))), ['cap-churn: events/s 4400 < 90% of 5000']);
    assert.deepEqual(failures(cand((s, p) => (p.cpuSeconds = 11.5))), ['cap-churn: CPU µs/event 115 > 110% of 100']);
    assert.deepEqual(failures(cand((s) => (s.auth.latency.p99Ms = 3))), []);
    assert.deepEqual(failures(cand((s) => (s.auth.latency.p99Ms = 3.01))), ['cap-churn: auth p99 ms 3.01 > max(110%, +1 ms) of 2.00']);
    assert.deepEqual(failures(cand((s) => (s.ingest.ackLatency.p99Ms = 4.5))), ['cap-churn: ACK p99 ms 4.50 > max(110%, +1 ms) of 3.00']);
    assert.deepEqual(failures(cand((s, p) => (p.peakMemoryBytes = 120 * 2 ** 20))), ['cap-churn: peak MiB 120 > 110% of 100']);
});

test('correctness checks apply to every run of both binaries', () => {
    const f = failures(results({ change: (b, c, i, s) => s && b === 'baseline' && c === 'few-churn' && i === 2 && (s.reconciled = false) }));
    assert.deepEqual(f, ['baseline few-churn run2: acknowledged events not all persisted']);
    const g = failures(results({ change: (b, c, i, s) => s && b === 'candidate' && c === 'cap-churn' && i === 1 && (s.churn.revoke.serverError5xx = 1) }));
    assert.equal(g.length, 1);
    assert.match(g[0], /^candidate cap-churn run1: key revoke /);
    // 429s on the churner's one ingest are admission, not failures.
    assert.deepEqual(failures(results({ change: (b, c, i, s) => s && (s.churn.use.rejected429 = 7) })), []);
    const h = failures(results({ change: (b, c, i, s) => s && b === 'baseline' && c === 'few-churn' && i === 3 && (s.churn.use.unauthorized401 = 1) }));
    assert.match(h[0], /^baseline few-churn run3: churn ingest /);
});

test('RSS slope and post-load RSS come from the server or the script samples', () => {
    assert.equal(slope([[0, 0], [10, 0], [20, 2 ** 20], [30, 2 * 2 ** 20], [40, 3 * 2 ** 20]], 40), 6);
    assert.equal(slope([[30, 1], [40, 1]], 40), null);
    const dir = results({
        change: (b, c, i, s, p) => {
            if (!s || b !== 'baseline' || c !== 'few-churn') return;
            delete s.server.rssSlopeMiBPerMin;
            s.args.settleMs = 10000;
            p.rssSamples = [[0, 100 * 2 ** 20], [20, 100 * 2 ** 20], [30, 101 * 2 ** 20], [40, 102 * 2 ** 20], [50, 103 * 2 ** 20], [60, 90 * 2 ** 20]];
        },
    });
    const { report, failures: f } = evaluate(dir);
    assert.deepEqual(f, []);
    assert.equal(report['few-churn']['RSS slope MiB/min'].baselineMedian, 6);
    assert.equal(report['few-churn']['RSS after settle MiB'].baselineMedian, 90);
    assert.equal(report['few-churn']['RSS slope MiB/min'].candidateMedian, 0.1);
});
