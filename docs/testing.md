# Testing

Testing is part of the implementation. Everything below runs in CI
(`.github/workflows/ci.yml`).

```bash
cargo test --workspace --all-features    # all Rust tests (~220, many property-based; includes crash and overload tests)
cd web && npm test                       # frontend unit tests
cd web && npm run e2e                    # Playwright (needs a built server, see below)
cargo bench -p storage -p query          # Criterion benchmarks
node --test scripts/release/*.test.mjs        # release gate (see docs/releasing.md)
```

## Unit tests

Next to the code (`#[cfg(test)]` modules). Highlights:

| Area                                                                 | Where                                |
|----------------------------------------------------------------------|--------------------------------------|
| Query tokenizer, parser (precedence, errors, positions), AST display | `crates/query/src/{lexer,parser}.rs` |
| Comparison evaluation and type coercion rules                        | `crates/query/src/eval.rs`           |
| Field resolution                                                     | `crates/query/src/fields.rs`         |
| Timestamp parsing (RFC 3339, Unix units, precision)                  | `crates/telemetry/src/time.rs`       |
| Varints, zigzag, bounds checks                                       | `crates/storage/src/encoding.rs`     |
| Event codec, versioning (only version 1), garbage/truncation handling; resource tables (one entry per distinct resource, shared on decode) | `crates/storage/src/codec.rs`        |
| WAL encoding/decoding, CRC validation, torn records; streamed appends write the same bytes as whole ones; the layout identity in the header (a 0.1.1 header, another identity or one cut short refused, never replayed) | `crates/storage/src/wal.rs`          |
| Segment header and footer layout identity, checked before the version | `crates/storage/src/segment/format.rs` |
| Layout identity tag and its descriptions in errors                   | `crates/telemetry/src/layout.rs`     |
| Segment write/read, metadata, corruption detection                   | `crates/storage/src/segment/mod.rs`  |
| Bloom filters, bitmaps, HyperLogLog, canonical index keys            | `crates/storage/src/index/`          |
| Configuration parsing, env overrides, memory budgets                 | `crates/server/src/config.rs`        |
| OTLP mapping (protobuf + JSON), native/CLEF parsing, templates       | `crates/ingest/src/`                 |
| Decoding limits (REL-16): events, decoded bytes and depth at limit − 1 / limit / limit + 1 for every format; shared OTLP resources; the protobuf pre-scan covers what `prost` allocates and accepts what it accepts | `crates/ingest/tests/limits.rs`, `crates/ingest/src/{limits,jsontree,native}.rs` |
| Real heap of native decoding (a per-thread counting allocator): an escaped-key envelope over the event limit is refused without building its tree; `[]` allocates nothing and arrays of exactly 4, 8 or 16 elements no growth step beyond them | `crates/ingest/tests/alloc.rs` |
| Metadata store (schema and identity created in one transaction, an interrupted creation leaves nothing, CRUD, alert transitions, read-only identity inspection, 0.1.1-style, foreign, other-layout and other-schema databases refused with every byte unchanged) | `crates/metadata/tests/store.rs`, `crates/metadata/src/lib.rs` |
| Data-directory check: new and interrupted first-start directories accepted; missing parts, inventory files missing or truncated, an empty metadata database refused as incomplete; manifests, metadata and telemetry of another layout or without one, and unidentified telemetry, refused and never accepted; `accept_incomplete` returns the incomplete problems instead; the inventory is dropped on start; the `check-data-dir` report | `crates/server/src/datadir.rs` |
| Environment variables under a settings prefix that are not settings stop startup (TC2) | `crates/server/src/config.rs` |
| API key registry (AUTH-PERF-01): key writes off the request path, bounded serialised changes, failure/cancellation ordering, one admission decision per request, parity with the stored keys across restarts ([details](#api-key-changes-and-authentication-auth-perf-01)) | `crates/server/src/auth.rs` |
| API key last-used bookkeeping (AUTH-PERF-03): the one-minute throttle on a fake clock, revoked keys reclaimed and never re-added, entry/queue/writer caps, failure, panic, shutdown and restart ([details](#api-key-last-used-tracking-auth-perf-03)) | `crates/server/src/auth.rs`, `crates/metadata/tests/store.rs` |

## Property tests (proptest)

| Property                                                                                                          | Where                             |
|-------------------------------------------------------------------------------------------------------------------|-----------------------------------|
| `encode(events) → decode() == events` and a segment body's head + body round trip, for arbitrary logs, spans, metrics | `storage::codec`                  |
| arbitrary bytes never panic the decoder; truncation is always detected                                            | `storage::codec`                  |
| random WAL records → replay → identical records; a cut at any byte replays exactly the complete records before it | `storage::wal`                    |
| segment writing → reading → identical event set (random block sizes)                                              | `storage::segment`                |
| arbitrary valid AST → display → parse → identical AST; parser never panics                                        | `crates/query/tests/roundtrip.rs` |

Event generators live behind the `telemetry/testing` feature so all crates
share them.

## Crash-recovery tests

`crates/storage/tests/recovery.rs` simulates crashes by leaving files in the
states an interrupted process would:

* empty WAL; valid WAL replayed after restart (and ids continue);
* truncated final WAL record → dropped, tail preserved in quarantine,
  subsequent appends clean;
* invalid checksum → replay stops there, nothing after it is interpreted;
* temporary segment file left in `tmp/` → deleted;
* partial/damaged segment in `segments/` → quarantined, rebuilt from its WAL;
* restart after segment commit but before WAL deletion → no duplicates;
* restart during compaction (output committed, inputs still present) → no
  duplicates;
* several WALs at startup → older ones sealed;
* retention across restart, queue-full backpressure, strict durability,
  data-directory locking;
* quarantine evidence: a torn or corrupt final WAL record is preserved byte
  for byte under `quarantine/<signal>/`, the report names signal, kind and
  reason, and a second restart neither re-quarantines nor duplicates; damaged
  segments and WALs with bad or foreign headers are moved whole; reasons
  never contain host paths;
* an unreadable-but-undamaged WAL or segment (mode 000 on Unix, an exclusive
  share lock on Windows) stops startup with an I/O error naming the file,
  leaves it byte-for-byte in place and quarantines nothing; once readable,
  the acknowledged events come back (skipped when running as root, which
  bypasses permissions);
* a file in a newer format is not damage (REL-06): a segment whose header
  and checksummed footer say version 2, a WAL whose header says version 3,
  and a checksummed WAL record with an unknown record version or batch
  codec each stop startup with `UnsupportedFormat` naming the file, with
  every file in the data directory unchanged; `check_formats` finds the
  header cases without opening storage; a segment whose header alone
  disagrees with its footer, and an unknown record version without a valid
  checksum, are still quarantined as damage.

### Process-kill tests

`crates/storage/tests/crash.rs` kills a **real process** mid-write. The test
binary re-invokes itself as a child that ingests with several concurrent
producers and keeps a durable client-side ledger (one fsynced line per
acknowledged batch). The child is stopped either at a named crash point —
`VYRTEL_CRASH_AT=<point>[:<n>]` makes the `crash-points` build `abort()` the
n-th time it reaches `point`, which is as abrupt as `kill -9` — or by
`kill`/`TerminateProcess` after a random delay. The parent then recovers the
directory and checks, in both `strict` and `normal` durability:

* every ledgered batch is visible exactly once with the ids it was
  acknowledged with;
* batches are whole; per producer they are contiguous and exceed the ledger
  by at most the one batch in flight;
* sealing/compaction crashes leave visible counts equal to the acknowledged
  dataset, with nothing quarantined;
* a second recovery is identical and new ids do not collide.

| Test                              | Crash points                                                                                       |
|-----------------------------------|----------------------------------------------------------------------------------------------------|
| `kill_before_wal_sync`            | `wal.before_sync` — records written, not synced or acknowledged                                    |
| `kill_after_wal_sync_before_ack`  | `wal.before_ack` — records synced (strict), not acknowledged                                       |
| `kill_after_ack`                  | `wal.after_ack`                                                                                    |
| `kill_during_rotation_and_seal`   | `rotate.after_wal_create`, `segment.before_rename`, `segment.after_rename`, `seal.before_wal_delete` |
| `kill_during_compaction`          | `compact.before_input_delete`, `compact.mid_input_delete`, `segment.before_rename` (compaction output) |
| `kill_during_recovery_seal`       | a crash during rotation, then `recovery.before_wal_delete` while recovering from it                 |
| `random_kills_under_load`         | four kill/restart cycles on one directory at random moments (seed printed; `VYRTEL_CRASH_SEED` replays) |

The points are listed in `crates/storage/src/crash.rs`; without the
`crash-points` feature they compile to nothing. Run just these tests with:

```bash
cargo test -p storage --features crash-points --test crash
```

`tests/integration/crash.rs` does the same end to end: it kills the real
`vyrtel` binary after HTTP acknowledgements (both durability modes),
restarts it and checks every acknowledged event through the query API, plus
the `recovery` section of `/api/v1/system/storage`.

`tests/integration/recovery_health.rs` covers what operators see (REL-11):
a clean restart is `clean` although the WAL was replayed; a torn WAL record
is `damaged` for that signal only, with per-kind counts and discarded bytes,
and neither the storage nor the info response contains the record's bytes,
the admin password, the API key or the data directory, while `/health` and
`/ready` are unchanged; the next start is `quarantineRetained`; and after the
quarantine directory is deleted it is `clean` again. A segment left in
quarantine by an interrupted recovery is reported as `quarantineRetained`
for the right signal. A quarantine directory that cannot be listed (a file
in its place everywhere; mode `000` on Unix, skipped as root) is `unknown`
with the reason, never `clean`, and is reported again once readable.
`web/tests/unit/recovery-health.test.tsx` checks the
Settings → Storage warning, and `web/tests/e2e/storage.spec.ts` that a clean
start shows none.

## Storage write-failure tests

`crates/storage/tests/faults.rs` injects storage errors at named points in
the write path. With the test-only `fault-injection` feature,
`storage::fault::inject(dir, point, fault)` makes `point` fail for files
below `dir` until the returned guard is dropped. The points are
`wal.append`, `wal.sync`, `wal.create`, `segment.write`, `segment.sync` and
`probe`. The faults are full disk, permission denied, read-only and short
write (half the bytes, then an error). Without the feature the checks
compile to nothing. The tests check that:

* a failed WAL append (each fault, both durability modes) gets
  `WriteFailed`, is not visible, and leaves no torn record: after a restart
  exactly the acknowledged events are back and nothing is quarantined;
* a failed `strict` fsync rejects the batch, cuts it from the WAL and fsyncs that cut before replying (`notStored`). The
  order is checked with
  `storage::fault::inject::record(dir)`, which lists the fault points reached, because a process restart cannot show
  lost filesystem metadata the way a power loss would;
* if the cut's fsync fails too, the reply is `WriteUncertain` (`unknown`), the idle writer fsyncs the cut on its own
  once storage works (no request needed), and any later acknowledgement waits for that fsync;
* a WAL fault clears without new requests once storage works, but not while writes in the WAL directory still fail;
* after a failed background fsync, `normal` mode waits for an fsync before
  acknowledging, and rejects requests while it keeps failing;
* a failing seal (segment write or fsync) makes new batches fail fast once
  the writer has to wait, and every acknowledged batch is sealed after a
  restart; batches already queued when the writer starts waiting (a burst
  of 200 without pauses) are answered within 2 s, stored or `WriteFailed`
  with the seal's fault, instead of waiting for their acknowledgement
  timeout, and release their queue reservation;
* the same, deterministically (REL-17), in both durability modes for
  `segment.write` and `segment.sync` faults: a batch submitted from the
  previous batch's acknowledgement callback, which runs on the writer
  thread after the commit and before the writer checks rotation, passes the
  check in `submit` and still gets `WriteFailed` within 2 s; so does a batch
  the writer took in the same group as a rotation that has to wait for the
  seal. Queued rotations are kept: once sealing works they finish and
  ingest resumes with the faults cleared, and a shutdown during the stall
  takes under 5 s and answers them `Unavailable`;
* a failed WAL creation stalls rotation, and ingest resumes by itself once
  storage works;
* faults name the operation, the reason and a data-directory-relative path,
  clear once that part of the write path works again, and never contain the
  host path;
* the write probe reports full, read-only and permission-denied storage,
  and on Unix a real read-only directory (skipped as root).

```bash
cargo test -p storage --features fault-injection --test faults
```

## Storage correctness (indexed vs brute force)

`crates/query/tests/correctness.rs` generates messy datasets (the same field
as int/string/float, nested vs dotted keys, arrays, booleans as strings,
resource fallbacks), stores them across several segments with tiny blocks
and only a few zone-mapped fields, then runs **hundreds of random queries**
and asserts that:

* indexed `count` and `search` return exactly the brute-force result set;
* cursor pagination (both directions, random page sizes) walks exactly that
  set;
* histogram totals equal counts.

It also guards against vacuous passes: many queries must match something and
many must actually have been pruned by indexes. False positives are fine;
false negatives fail the test.

## Integration tests

`tests/integration/` (built as the `server` crate's `integration` test)
starts a real server on an ephemeral port with a temporary data directory and
talks HTTP to it:

* start → ingest → query → restart → query the same events;
* `kill -9` of the `vyrtel` binary after acknowledgements → restart → every
  acknowledged event exactly once; quarantined WAL tails reported by the
  storage API;
* ingest → rotate segment → query (diagnostics show segment pruning);
* OTLP logs (JSON and gzip protobuf) queried through the native API;
* OTLP spans → trace search → trace by id → correlated logs;
* OTLP metrics → names → sum/count/max/avg with group-by and filters;
* retention → restart → verify retained/deleted data;
* API errors: invalid JSON, invalid events, invalid queries (with position),
  oversized requests, unsupported media types, unknown endpoints;
* ingest size and encoding (`ingest_limits.rs`): decoded sizes of limit−1,
  limit and limit+1 bytes on native JSON, NDJSON, OTLP JSON and OTLP
  protobuf, plain and gzip (only the first two stored); a gzip bomb gets 413
  while its upload is still in progress; an oversized `Content-Length` gets
  413 before the body is read; clients that keep sending 8 MB against the
  64 KB limit (with `Content-Length`, chunked, and gzip past the wire limit)
  read the 413, not a broken pipe, and their next request succeeds; empty gzip members cannot exceed the limit on
  the wire; truncated, corrupt and trailing-garbage gzip get
  `invalid_encoding`, unsupported encodings a structured 415; concatenated
  members are all stored; rejected native batches (malformed final record,
  truncated gzip, over the limit) store nothing; OTLP `partialSuccess` is the
  documented partial outcome. Unit tests in `routes/ingest.rs` show decoding
  stops after a small fraction of a 256 MB bomb;
* ingest admission (`admission.rs`): a request holds its slot while parsed
  and queued, excess requests (plain and gzip) get 429 + `Retry-After`
  promptly without their bodies being read, every error frees the slot, and
  so does a client disconnecting while the body is read, while it is parsed
  and while the write awaits acknowledgement (the writer thread is parked
  with `stall_writer` to hold a request there); with
  `max_concurrent = 0` the derived limit (3 at the defaults) is what is
  enforced and what `system/info` reports, and an explicit limit over the
  estimated budget is honoured (REL-14);
* decoding limits (`decode_limits.rs`, REL-16): for native JSON, the
  `{"events":[...]}` envelope, NDJSON, CLEF and OTLP logs, traces and
  metrics in protobuf and JSON, each plain and gzip: `queue_capacity − 1`
  and `queue_capacity` events stored, one more a 413 `maxEvents` before
  parsing; a 64 KB body that would decode past 17× (tiny native events, OTLP
  records of empty values) a 413 `maxDecodedBytes` while an ordinary one of
  the same size is stored; values nested 32 levels stored, 33 a 400
  `maxDepth`. Refused requests store nothing, free their slot and the next
  request succeeds; OpenTelemetry SDK (512) and Collector (8,192) default
  batches are accepted at the default limits;
* storage write failures (`storage_faults.rs`, needs `--features
  fault-injection`): native, NDJSON and OTLP logs, traces and metrics get
  `503 storage_write_failed` with a reason and store nothing while the WAL cannot write; `/ready` is 503, names the
  signals and recovers without a restart, and also with no new traffic at all;
  `outcome` is `notStored`, or `unknown` when the rollback's fsync is denied too; a failing write probe (full or
  read-only) makes `/ready` report
  `storageWritable: false`; `/api/v1/system/storage` shows the fault without
  the host path; after a short write and a denied fsync, a restart brings
  back exactly the pre-fault dataset (`logs.ndjson` plus one event) with
  nothing quarantined;
* query budgets and cancellation (`query_budgets.rs`, REL-12; see
  [below](#query-budget-and-cancellation-tests));
* missing auth, bad auth, ingest-only keys, sessions, logout, revocation;
  a key write stalled in SQLite holds up no request (AUTH-PERF-01, see
  [below](#api-key-changes-and-authentication-auth-perf-01)); last-used
  times recorded and revoked keys' bookkeeping reclaimed (AUTH-PERF-03);
* pagination, time ranges, empty results, facets, histograms;
* live stream (SSE) delivery and filtering, and that open streams do not
  block shutdown;
* graceful shutdown (`shutdown.rs`, REL-05): with the queue full and the
  writer parked, a stop request waits for all 4 admitted requests, refuses
  new work (closed listener or `503 shutting_down`) and exits cleanly; all
  43 acknowledged events are back exactly once after a restart; an idle
  stop takes well under 2 s; when `server.shutdown_timeout` (500 ms) passes
  with a request unacknowledged, or a second stop request arrives, shutdown
  gives up on time and `IncompleteShutdown` names the request and its 5
  queued events; the deadline also bounds a stuck final storage flush;
  a seal blocked at the deadline, or ended by a second stop request, is
  reported as the unfinished phase and its events are back after a
  restart; a second stop request or the deadline also ends a blocked
  background job (`app.rs` unit tests); with `--features fault-injection`,
  a failed final WAL fsync in `normal` durability is reported with its
  fault instead of a clean shutdown (`storage_faults.rs`);
  live-tail streams, including one whose client has not read 300 events,
  end with `shutdown` and `retry: 2000` and a clean end of body, and a
  client reconnecting to the same address after a restart receives new
  events. On Unix the `process` tests send real SIGTERMs to the binary: an
  upload in flight is answered `200` before exit status 0; past a 1 s
  deadline it exits with status 1 within about a second and prints the
  diagnostic; a second SIGTERM exits at once. Unit tests in `app.rs` check
  each refused route and the diagnostic text, and `web/tests/unit/live-tail.test.ts`
  the UI's reconnection and backoff;
* dashboards, saved queries, alerts firing/resolving with webhook delivery,
  the background evaluator;
* backup and restore (`backup.rs`, REL-06): a stopped instance with all
  three signals (in segments and in the WAL), a dashboard with a panel, an
  alert, a saved query, an active and a revoked API key and an admin
  session is copied, the original deleted, and the copy started elsewhere
  without `auth.admin_password`; every
  [verification query](configuration.md#verification-queries) answers as
  before, the restored key ingests, the revoked key and anonymous requests
  get 401, the old session is valid, and nothing is quarantined. Backups
  without `metadata/metadata.db` (with and without a manifest), with an
  empty one, without a signal's WAL directory, without one populated
  `.wal` and one `.seg` file listed in the clean-shutdown inventory, with a
  truncated `.seg`, with a newer or unreadable manifest, a newer segment or
  WAL format, or a newer metadata schema are refused by `App::build` with
  the documented message, and every file and directory is unchanged (no
  lock file, no `tmp/`). The real binary exits with status 1 and prints
  the reason. `--accept-incomplete-restore` (`StartOptions`) starts a copy
  without a WAL file, with the segment events and the configuration kept,
  and the next start needs no flag; a running directory has no inventory.
  A directory without a manifest (0.1.1) starts and gets one; a new volume
  holding only `lost+found` starts as new. A start refused because another
  process holds the lock changes nothing, so the inventory still refuses a
  later start that is missing a WAL file. With `--features
  fault-injection` (`storage_faults.rs`): a failed manifest write at
  shutdown is `IncompleteShutdown` (phase `Inventory`, the I/O error, no
  host path, no partial inventory) and the next clean shutdown records it;
  a manifest write stalled past a 500 ms `shutdown_timeout` ends the
  shutdown within 3 s, reported as the deadline in phase `Inventory`;
* layouts (`layouts.rs`, COMPAT-GATE-0.1.2), against the data directories
  the 0.1.0 (Observer) and 0.1.1 releases wrote (`tests/fixtures/datadir/`,
  restored byte for byte from the sizes and SHA-256 sums in each
  `fixture.json`). Neither has this build's layout identity, so for each,
  `vyrtel check-data-dir` exits 1, a start and a start with
  `--accept-incomplete-restore` are refused, all with what to do, and the
  SHA-256 of every file is the same before and after (nothing locked,
  created, migrated, quarantined or truncated, so every event the release
  acknowledged is still where it put it). The documented way forward runs
  on that disposable copy: a new data directory beside it starts, takes the
  same telemetry and answers the same counts after a restart, and the old
  copy is still byte for byte unchanged. 0.1.1's WAL, segment and metadata
  files mixed into a directory of this layout are each refused before any
  change, named; taken out again, the directory starts with every event it
  acknowledged. This layout's own directories restart and restore from a
  stopped copy as they are (`check-data-dir`: no format changes). A fixture
  of this layout (from 0.1.2 on) must answer every verification query as
  its release did. With `--features fault-injection`, every write point of
  a first start fails once in turn: a failed `vyrtel.json` write leaves no
  telemetry file and a directory `check-data-dir` accepts as an
  interrupted first start, and in every case the next start completes.
  Backups (`backup.rs`): a manifest of another layout or without one, a
  segment, WAL or metadata database in a format version this layout does
  not have, and a WAL of another layout are refused with every file
  unchanged; telemetry with neither a manifest nor a metadata database of
  this layout is refused even with `--accept-incomplete-restore`; a backup
  missing only `vyrtel.json` is incomplete and starts on request. Storage
  (`crates/storage/tests/recovery.rs`): 0.1.1's WAL and segment files, also
  cut inside the identity, and files with another identity under a valid
  checksum stop `check_formats` and `Storage::open` with nothing changed,
  while a header identity the checksummed footer contradicts is damage
  (quarantined). A segment whose header and footer say version 1 but whose
  checksummed summary says 2 is refused before anything changes
  (`segment_summary_in_a_newer_format_is_found_before_startup`, where a
  summary failing its checksum is still quarantined as damage). The WAL
  scan believes an unknown record or codec version only when the record's
  checksum, streamed over a 200 KB record, holds (`wal.rs`
  `record_format_scan_reads_prefixes_and_believes_only_checksums`). New
  fixtures are written by the ignored `layouts::make_datadir_fixture` test
  with a released binary
  ([tests/fixtures/datadir/README.md](../tests/fixtures/datadir/README.md)).
* removed aliases (`api.rs` `removed_aliases_answer_actionably`, TC2):
  `GET /api/v1/live`, the `spans` signal in `/api/v1/query/{signal}/…` and
  `?signal=spans` answer `404`/`400` with what to use; unknown API paths
  point to `docs/api.md`. The web UI's stored columns recover from stale or
  unreadable values (`web/tests/unit/lib.test.ts`).

Fixtures shared with E2E live in `tests/fixtures/`.

### Query budget and cancellation tests

```bash
cargo test -p query --test budgets                        # engine level
cargo test -p server --test integration query_budgets     # over HTTP
```

Slow queries come from the engine's checkpoint hook
(`Engine::set_checkpoint_hook`), not from big datasets, so the tests are
deterministic and take under a second. The engine fixture is 2,000 logs
(every seventh an `Error`, event `i` at `BASE + i s`) and 30 traces of 10
spans, in 16-event blocks, sealed into three segments per signal plus an
unsealed tail. The HTTP tests ingest a few events (and
`tests/fixtures/otlp-traces.json`, one trace of three spans) into a real
server.

| Acceptance criterion / case | Tests |
|---|---|
| AC1 runtime or result budget → explicit limit error | `budgets.rs`: `timeout_then_a_small_query_is_complete` (every request kind gets `Timeout(20)`), `scan_byte_budget_boundary`, `trace_span_budget_boundary`, `metric_point_budget_boundary`; `query_budgets.rs`: `timeout_is_an_explicit_error_and_the_server_stays_usable` (504 `query_timeout`, `limitMs`), `scan_and_result_budgets_are_explicit_errors` (422 `query_too_expensive`/`query_result_limit`, 400 for `limit` over `max_results`) |
| AC2 client disconnect stops work at checkpoints | `budgets.rs`: `cancelled_requests_return_only_the_error`, `cancel_during_scan_stops_at_the_next_block`, `cancel_during_aggregation_stops_within_one_check_interval`, `cancel_at_a_row_checkpoint_stops_every_aggregation`, `cancel_between_trace_search_phases`, `summary_only_counts_still_check_segments`, `facet_counting_after_the_scan_checks_the_budget`, `facet_counting_checks_within_one_event`, `metric_name_discovery_stops_when_cancelled_during_enumeration`; `query_budgets.rs`: `client_disconnect_cancels_the_query_and_frees_its_slot` (raw socket closed mid-query), `metric_name_discovery_is_cancelled_on_disconnect_and_frees_its_slot` |
| AC3 concurrent queries and ingestion stay bounded | `query_budgets.rs`: `query_slots_bound_blocking_work_while_ingest_continues` — 2 slots held, 8 more queries get 503 promptly, 20 concurrent ingests all 200, never more than 2 queries executing; then both held queries complete |
| AC4 resources released, no partial result as complete | `budgets.rs`: `later_queries_are_complete_after_cancellation_and_limits`, `hook_can_be_removed`; `query_budgets.rs`: the disconnect test (slot back, next answer `["a", "b", "c"]`), `trace search` summaries report `truncated` |
| TC-02 cancel at scan and aggregation, budgets at boundaries | the AC2 rows above; boundaries: exactly `max_scan_bytes` succeeds and one byte less fails (count, facets, trace search across its pages and assembly); exactly `max_trace_spans` and 10,000 metric points succeed, one more fails; `metric_name_budget_boundary` (`max_metric_names` of 301, 300, 299 and 1 over 300 names: the first names by name, `truncated` only below 300); `budget.rs` unit tests (zero timeout, cancellation wins over other limits) |
| TC-03 small query right after a rejection | `timeout_then_a_small_query_is_complete`, `timeout_is_an_explicit_error_and_the_server_stays_usable`, `client_disconnect_cancels_the_query_and_frees_its_slot` |
| TC-04 regression | `cargo test --workspace --all-features`, `query/tests/correctness.rs` (indexed results equal brute force) unchanged |

Config validation (`config.rs` `query_budgets`) covers defaults, parsing
and refusing zero `timeout`, `max_results`, `max_trace_spans` and
`max_metric_names`. The engine fixture also holds 600 metric points (two
each of 300 names, `m-000` to `m-299`).

### Transport limit tests

```bash
cargo test -p server --test integration transport          # behaviour over real sockets
cargo test -p server --lib transport config::tests::transport_limits config::tests::durations_that_overflow_are_errors
cargo test -p server --release --test overload transport -- --nocapture   # heap, see below
```

`tests/integration/transport.rs` drives a real server with raw TCP sockets,
so a test controls exactly which bytes arrive and when. Limits are set
small (2 to 16 connections, deadlines of 300 to 500 ms) and every wait is on
the server's own counters (`AppState::transport`), not a sleep; the only
time that passes is the deadline under test.

| Acceptance criterion / case | Tests |
|---|---|
| AC1, TC1 finite admission: C−1, C, C+1, waiting cap, refusal | `connections_beyond_the_limit_wait_briefly_then_are_refused` (C = 4: four idle connections admitted one by one, the fifth waits and gets the first freed slot and is served; with 64 waiting the next is refused at once with the fixed 503 or a close; the 64 are refused after 2 s; the server then ingests and queries) |
| AC1, AC6, TC1 ≥1,000 attempts and reconnect waves | `a_thousand_simultaneous_attempts_never_exceed_the_limits` (C = 16: a wave of 1,000 simultaneous attempts, half idle and half keep-alive `GET`s, then three of 300; a 1 ms sampler never sees more than 16 active or 64 waiting; every attempt is admitted or refused; each wave returns to 0 active) |
| AC1, TC5 listeners share one budget | `listeners_share_one_connection_budget` (a second listener on the same `Transport`: 2 + 1 connections fill C = 3, the next on the second listener waits) |
| AC2, AC4, TC2 nothing routed before a head; deadlines; parser bounds | `incomplete_idle_and_oversized_request_heads_are_closed` (silent socket closed after `header_timeout`; a head sent one byte every 20 ms closed, never routed; a keep-alive connection reused twice then closed when idle; a 16 KB head with an 8 KB buffer and 101 headers get `431`) |
| AC4, TC3 slow bodies | `slow_request_bodies_get_408_and_release_their_slots` (ingest and query bodies stalled after 10 bytes get `408 request_timeout`; the ingest slot is released); `transport.rs` `body_frames_available_after_the_deadline_are_refused` (paused clock: a frame already buffered when the deadline passes is refused, not read) and `a_body_that_already_ended_does_not_time_out` |
| AC4, TC3 clients that stop reading | `clients_that_stop_reading_are_closed_on_the_write_timeout` (pipelined 1 MB query responses to a socket with a 4 KB receive buffer that never reads; closed on `write_timeout`) |
| AC2, TC3 pipelining and protocols | `pipelined_requests_are_served_in_order_and_http2_is_not_served` (20 pipelined requests answered in order on one connection; an HTTP/2 preface is a protocol error; an `h2c` upgrade is answered as HTTP/1.1) |
| AC5, TC4 every ending releases its slot | `every_way_a_connection_ends_releases_its_slot` (C = 2: resets with a waiter behind them, EOF mid-head, malformed input, a handler error on a kept-alive connection, abandoned responses; 0 active after each, then ingest and query work) |
| AC5, TC4 shutdown while saturated | `shutdown_while_saturated_is_bounded` (every slot held and one waiting: the stop finishes in under 2 s, idle connections just close, the waiting one is refused) |
| AC2 observability | `system_info_reports_connection_limits_and_outcomes` (`connections` object; `[server]` in `/api/v1/system/config`) |
| AC3, TC5 configuration | `config.rs` `transport_limits` (finite defaults; minimum and maximum of each setting accepted; zero, out of range, negative, fractional, non-numeric and overflowing values refused from TOML and the environment; an unrepresentable size saturates and fails validation); `durations_that_overflow_are_errors`; `transport.rs` unit tests (the fixed 503 is well formed; the estimate saturates; body timeouts are found through error wrappers). No reload exists, so none is tested |
| TC7 regression | `cargo test --workspace --all-features`; `shutdown.rs` `live_streams_end_cleanly_and_reconnect_after_restart` now asserts that a slow consumer gets an in-order prefix, then `shutdown`; the REL-02 flood test sets `max_connections` above its 600 connections so that it still exercises ingest admission |

#### Transport heap

`connection_heap_stays_within_the_transport_estimate`, in the `overload`
binary (counting allocator; clients on the uncounted runtime), holds all
200 slots of a server with the default 64 KB buffer: first idle, then with
request heads that fill each read buffer and never end, then floods the
full server with 1,000 more attempts, closes everything, and finally holds
every slot with clients that pipeline requests and never read. It asserts
heap per connection within `3 × connection_buffer + 16 KB`, peak growth
within `TransportLimits::estimated_peak`, never more than 200 active or 64
waiting, at most `C + 64 + 16` new runtime tasks, and at most 512 KB
retained after the connections close and after the stalled ones are closed
by `write_timeout`.

| Measured (release)                     | Linux x86_64 (WSL2, Rust 1.96) | Windows 11 x86_64 |
|----------------------------------------|-------------------------------:|------------------:|
| Heap per idle connection               |                        18.0 KB |           18.1 KB |
| Heap per full read buffer              |                       122.3 KB |          122.4 KB |
| Heap per stalled reader                |                        47.7 KB |           21.4 KB |
| Flood of 1,000: waiting / refused      |                       64 / 936 |          64 / 936 |
| New runtime tasks                      |                            264 |               264 |
| Peak heap growth / estimate            |               23.99 / 40.75 MB |  24.03 / 40.75 MB |
| Retained after close / after timeouts  |              63.1 KB / 63.0 KB |  65.1 KB / 65.1 KB |

On Linux the test needs several GB of memory: the 200 stalled readers'
kernel socket buffers (client and server sides, autotuned up to
`tcp_rmem`/`tcp_wmem`) peaked at about 1 GB in a container limited to
6 GB, against 24 MB of heap; with a 1 GB limit the container was killed.
That memory is the kernel's, which is why the
[configuration docs](configuration.md#memory-accounting) account for it
separately.

#### Transport soak on Linux

`scripts/bench/transport-soak.sh` runs a release `vyrtel` and
`connload` (`tools/loadgen/src/connload.rs`) in separate containers with
their own CPU and memory limits, samples the server process every second
from `/proc` and its cgroup (RSS, threads, file descriptors, sockets, CPU
time, the listen backlog and the kernel's `ListenOverflows`, established
sockets, cgroup memory and kernel socket buffers), then waits for idle, stops the server with `docker stop`
and records the shutdown time and exit status. `connload` keeps steady
ingest (4 keep-alive senders of 200-event NDJSON batches every 50 ms) and
count queries (2 senders, every second), opens waves of simultaneous
connections of four kinds (idle, a head trickled one byte every 500 ms, a
body that stalls after 10 bytes, a normal `GET /health`), samples
`/api/v1/system/info`, and at the end checks that every acknowledged event
is stored.

```bash
# Release binaries on a volume (any Linux host with Docker):
docker volume create rel18-target
docker run --rm -v "$PWD:/src:ro" -v rel18-target:/target -e CARGO_TARGET_DIR=/target -w /src \
  rust:1.96-bookworm cargo build --release --locked -p server -p loadgen

# Steady load only, for a baseline/candidate comparison (repeat 3×; for the
# baseline, build the prior commit onto another volume and set SERVER_VOLUME):
IMAGE=rust:1.96-bookworm scripts/bench/transport-soak.sh out/steady-1 --no-waves --duration 60s

# 1,000 simultaneous attempts every 30 s for 3 minutes, normal and constrained:
IMAGE=rust:1.96-bookworm scripts/bench/transport-soak.sh out/flood-normal-1 --duration 180s --attempts 1000
IMAGE=rust:1.96-bookworm SERVER_MEMORY=384m NOFILE=1024 \
  SERVER_ENV="-e VYRTEL_STORAGE_MAX_MEMORY=256MB -e VYRTEL_SERVER_MAX_CONNECTIONS=64 -e VYRTEL_SERVER_CONNECTION_BUFFER=16KB" \
  scripts/bench/transport-soak.sh out/flood-constrained-1 --duration 180s --attempts 1000

# The 30-minute soak:
IMAGE=rust:1.96-bookworm IDLE_AFTER=60 scripts/bench/transport-soak.sh out/soak --duration 30m --wave-interval 60s
```

Each run directory holds `resources.csv`, `resources.json`,
`samples.csv`, `summary.json`, `connload.log` and `server.log`. The budgets
were declared before the candidate was measured in
[`docs/benchmarks/transport/budgets.md`](benchmarks/transport/budgets.md); the
results, per budget with PASS/FAIL, are in
[`docs/benchmarks/transport/2026-10-17-wsl2`](benchmarks/transport/2026-10-17-wsl2/README.md).
They come from a developer laptop's WSL2 VM, not the release sign-off host:
[#170](https://github.com/rowan-smith/Vyrtel/issues/170) repeats them on the
final candidate.

### Restore drill

`scripts/backup/restore-drill.sh` runs the published
[backup and restore procedure](configuration.md#backup-and-restore) against
a built binary (Linux, macOS): it populates an instance from
`tests/fixtures`, stops it with SIGTERM, takes a `tar` backup, deletes the
original, restores into a new directory, starts without an admin password,
diffs every verification query against its answers before the backup,
checks the restored API key, anonymous refusal and login, and checks that
copies without `metadata/metadata.db`, with an empty one, without one WAL
file and without one segment file each exit with status 1 and are left
unchanged, and that `--accept-incomplete-restore` starts the copy without
the WAL file. CI runs it against the release build in the backend job.

```bash
cargo build --release -p server && scripts/backup/restore-drill.sh target/release/vyrtel
```

### Startup format check

The startup check reads every format version recovery reads before
anything changes (REL-07), and runs twice per start (before and after
taking the lock). It reads each WAL record's 5-byte prefix and first
payload byte and skips the rest, computes a record's checksum only when a
version is unknown, and holds at most 64 KiB of a file at once; a segment
costs its header, footer and a streamed checksum of its summary.
`scripts/bench/startup-preflight.sh` starts each binary on the same stopped
data directories, all telemetry in the WAL, and reports the median of 5
starts: time until `/health` answers, bytes read (`rchar`) and peak memory
(`VmHWM`) at that point, and `check-data-dir` time and peak memory.

Linux x86_64 (`rust:1-bookworm` in Docker on Windows 11, release builds,
warm page cache), `--memory-limit 4GB`:

| WAL | Build | Startup | Read | Peak memory | `check-data-dir` |
|-----|-------|--------:|-----:|------------:|-----------------:|
| 82 MB, 120 records of about 680 KB | before REL-07 (`98ba4be`) | 569 ms | 81 MiB | 376 MiB | n/a |
| | first REL-07 check, full replay (`15aabd4`) | 535 ms | 243 MiB | 377 MiB | 10 ms, 6 MiB |
| | prefix scan (this version) | 533 ms | 95 MiB | 377 MiB | <10 ms, 7 MiB |
| 15 MB, 20,000 records of about 750 B | before REL-07 | 124 ms | 14 MiB | 77 MiB | n/a |
| | first REL-07 check | 125 ms | 42 MiB | 77 MiB | <10 ms, 6 MiB |
| | prefix scan (this version) | 129 ms | 41 MiB | 77 MiB | <10 ms, 7 MiB |

With large records the check now reads about 7 MiB per pass instead of the
whole WAL. With small records the 64 KiB read buffer covers many records, so
each pass still reads the file once (from the page cache): about 5 ms on
15 MB here. Startup times vary by ±50 ms between runs, more than any
difference between builds, and peak memory is recovery's. The full replay
also allocated each record's payload (up to 256 MiB for one record); the
scan never holds more than 64 KiB. Run it with:

```bash
scripts/bench/startup-preflight.sh ./vyrtel-before ./vyrtel-after
```

### Overload memory test

`tests/overload/main.rs` (the `server` crate's `overload` test, its own
process because it installs a counting global allocator) holds the ingest
path to a memory budget under sustained overload. Fixed limits:
`max_memory = 64MB`, `max_request_size = 2MB`, `ingest.max_concurrent = 2`;
32 clients send 1.79 MB NDJSON batches (half gzip-compressed, 0.05 MB on the
wire) for 4 s, retrying 429s after 5 ms. It asserts that:

* peak heap growth stays below `max_memory`;
* no more than 2 requests are ever in flight and the server keeps accepting;
* every rejection is a 429 within 1 s and no client sees a reset connection.

```bash
cargo test -p server --test overload -- --nocapture
```

One run on the Windows 11 laptop used for the benchmarks below (test
profile), before and after REL-02 moved admission from "while the body is
read" to "until the write is acknowledged":

| Metric                      | Before                | After               |
|-----------------------------|-----------------------|---------------------|
| Peak heap growth            | 298.4 MB (4.7× budget) | 55.3 MB (< 64 MB)  |
| Slowest 429                 | 1.1 s                 | 161 ms              |
| Connection resets           | 223                   | 0                   |
| Batches accepted in 4 s     | 14                    | 40                  |

Five consecutive runs after the change peaked at 54.3–55.8 MB. Parsing one
1.79 MB body alone peaks at +8.3 MB, and one request end to end at +10.6 MB.

`flood_of_rejected_slow_uploads_is_bounded`, in the same binary, holds the
only ingest slot and opens 600 uploads that announce 1 MB, send 64 KB and
stall. All are rejected at admission. It asserts that rejected bodies kept
for discarding stop at the 256 cap (32 being read), that the runtime gains
no more than one task per connection plus that cap, that kept clients read
the 429, that peak heap growth stays below `max_memory`, and that everything
is released after the 10 s discard timeout. One run: 256 discard tasks,
512 new runtime tasks, 18.2 MB peak heap growth; without the cap, 1,200 new
tasks and 38.2 MB.

The two REL-02 tests set `ingest.max_concurrent` explicitly, so they are
unaffected by the derived sizing below (and log its over-budget warning).

### Ingest amplification and derived admission

REL-14 replaced the derived ingest limit with one budgeted from measured
parse amplification ([formula](configuration.md#ingest-memory)), and
REL-16 made it hold for every body shape by limiting decoding
([decoding limits](configuration.md#decoding-limits)). Three tests in the
same `overload` binary back them. Clients run on a separate
runtime whose allocations the counting allocator skips, so only the
server's heap is measured. Run them on a release build; the figures below
are from one Windows 11 x86_64 laptop (`cargo test --release`, Rust 1.96),
so expect other machines to differ somewhat.

```bash
cargo test -p server --release --test overload -- --nocapture --test-threads 1
# the pre-REL-14 comparison (needs ~2 GB of memory):
cargo test -p server --release --test overload pre_rel14 -- --nocapture --ignored
```

**Fixtures** (`tests/overload/fixtures.rs`). Every body is generated from
seed `0x5EED0014` (xorshift), so reruns are byte-identical. Each one is
filled to just under the target decoded size (2 MB by default;
`VYRTEL_AMPLIFICATION_TARGET=<bytes>` changes it) and sent plain and gzip.
OTLP fixtures are built once as protobuf structs and encoded both ways, so
both encodings carry the same records. Each fixture states whether the
server accepts it or refuses it at the request-size limit (REL-16), and the
test holds it to that.

| Fixture | Record shape | Events in 2 MB |
|---|---|---:|
| native JSON / NDJSON representative | app event, level, service, trace ids, 5 properties (~330 B) | 6,161 |
| CLEF representative | Serilog request log, template and 7 properties, 10% with `@x` | 5,977 |
| NDJSON string-heavy | 12 KB of strings per event | 173 |
| native JSON / NDJSON nested properties | 6 levels, ~40 small values | 7,973 |
| native JSON / NDJSON many small events | `{"message":"…"}` (~24 B) | 87,844 |
| NDJSON empty objects | `{}` | 699,050 |
| NDJSON wide arrays | 500 one-digit numbers in one property | 2,016 |
| OTLP logs representative | SDK resource (7 attributes), body, 4 attributes, trace ids | 9,271 protobuf / 4,089 JSON |
| OTLP logs many small records | body only (~20 B) | 137,999 / 13,775 |
| OTLP logs large shared resource | 200-attribute resource, tiny records | 183,361 / 14,055 |
| OTLP logs large resource, representative records | 200-attribute resource shared by SDK-like records | 9,238 / 4,064 |
| OTLP logs empty array values | 1,000 empty `AnyValue`s in one attribute | 1,039 / 661 |
| OTLP traces representative | 5 attributes, 1 in 8 with an event, 1 in 10 failed | 8,768 / 3,485 |
| OTLP metrics representative | gauges and sums (2 points), histograms (11 buckets) | 11,819 / 5,362 data points |

Before REL-16 the fixture builder sized fixtures from a 64-record probe,
so those dominated by a large resource came out far below the target
(0.08 MB of protobuf instead of 2 MB); it now searches for the largest
record count that fits.

**Per-format peaks** (`amplification_per_format_is_within_the_admission_estimate`).
For every fixture and encoding it measures, as multiples of the decoded
body: *parse*, decoding alone in-process over a body already in memory with
the server's decoding limits; *admission*, from sending the request until
its batch is in the storage queue with the writer parked, or until its 413
(body read and gzip-decoded, parse, parsed batch: what an admitted request
owns); and *end to end*, until the response, which adds the WAL record and
the active segment (storage's budget). A fresh server per request keeps
earlier writes out of the window. Requests are exactly as large as the
fixture and `queue_capacity` is effectively unlimited, so the
decoded-bytes limit (17 × `max_request_size`) is the one exercised. It
asserts, for **every** fixture with no exemptions, that the admission and
end-to-end peaks are at or under the 20× estimate and that the fixture is accepted (200) or
refused (413 `maxDecodedBytes`, nothing queued) as it expects.

| Fixture | Admission ×body, 2 MB plain / gzip | 1 MB plain / gzip | End to end ×body (2 MB) | Status |
|---|---:|---:|---:|:---:|
| native JSON representative | 3.83 / 3.83 | 3.89 / 3.89 | 3.83 | 200 |
| NDJSON representative | 3.83 / 3.83 | 3.89 / 3.89 | 3.83 | 200 |
| CLEF representative | 4.32 / 4.32 | 4.38 / 4.38 | 4.32 | 200 |
| NDJSON string-heavy | 2.10 / 2.12 | 2.16 / 2.24 | 2.10 | 200 |
| native JSON / NDJSON many small events | 15.51 / 15.51 | 15.64 / 15.64 | 15.51 | 200 |
| OTLP protobuf logs representative | 8.46 / 8.46 | 8.52 / 8.52 | 8.46 | 200 |
| OTLP JSON logs representative | 9.64 / 9.64 | 9.70 / 9.70 | 9.64 | 200 |
| OTLP JSON logs many small records | 13.14 / 13.14 | 13.22 / 13.19 | 13.14 | 200 |
| OTLP JSON logs large shared resource | 13.35 / 13.34 | 13.43 / 13.40 | 13.35 | 200 |
| OTLP protobuf logs large resource, representative records | 8.40 / 8.40 | 8.45 / 8.45 | 8.40 | 200 |
| OTLP JSON logs large resource, representative records | 9.65 / 9.65 | 9.72 / 9.72 | 9.65 | 200 |
| OTLP protobuf traces representative | 10.57 / 10.57 | 10.63 / 10.63 | 10.57 | 200 |
| OTLP JSON traces representative | 11.06 / 11.06 | 11.12 / 11.12 | 11.06 | 200 |
| OTLP protobuf metrics representative | 9.35 / 9.35 | 9.41 / 9.41 | 9.35 | 200 |
| OTLP JSON metrics representative | 11.82 / 11.82 | 11.88 / 11.88 | 11.82 | 200 |
| native JSON / NDJSON nested properties | 18.06 / 18.06 | 18.11 / 18.11 | 18.06 | 413 |
| NDJSON wide arrays | 18.05 / 18.05 | 18.11 / 18.11 | 18.05 | 413 |
| OTLP JSON logs empty array values | 18.06 / 18.03 | 18.12 / 18.06 | 18.06 | 413 |
| NDJSON empty objects | 1.12 / 2.05 | 1.24 / 2.11 | 1.12 | 413 |
| OTLP protobuf logs many small records | 1.12 / 2.11 | 1.24 / 2.29 | 1.12 | 413 |
| OTLP protobuf logs large shared resource | 1.12 / 2.11 | 1.24 / 2.22 | 1.12 | 413 |
| OTLP protobuf logs empty array values | 1.12 / 2.05 | 1.24 / 2.11 | 1.12 | 413 |

The highest accepted fixture is 15.6× (tiny native events), the highest
representative one 11.9× (OTLP/JSON metrics), and no refused request goes
past 18.1×: 20× keeps a margin of at least 10% over every shape.
Representative batches use far less than before REL-16 (native 9.4× → 3.9×,
OTLP 12.5–16.0× → 8.5–11.9×): native events are converted as they are
parsed instead of after the whole body is, the batch is allocated once at
its counted size, and OTLP records share their resource. Shapes that
cannot fit are refused while they decode: native JSON and OTLP/JSON when
the charged tree and events reach the limit (about 17× plus the body);
NDJSON of empty objects when its counted events alone cannot fit, and
OTLP/protobuf by the pre-scan, both before anything is decoded (1.1×, the
body; 2.1× with gzip, the body joined from its parts). Before REL-16 these
shapes were accepted at 30–1,700×, 213 MB for one 1 MB request of empty
objects.

Writing adds nothing measurable: the end-to-end column equals admission.
The WAL record is streamed to the file in 64 KiB pieces as it is encoded,
and a batch stores each distinct resource once
([storage format](storage-format.md#batch)). Before, the writer encoded
the whole batch, every event with its own copy of the resource, and then
copied it into the record, holding the encoding twice: up to 105× end to
end for a 200-attribute resource over tiny records, 72× over SDK-like
records, and 26.6× for tiny native events.

**WAL size** (batches as 0.1.1 wrote them, every resource inline, against
the current batch with its resource table; measured before the layout reset
renumbered the current codec to 1). Each accepted 1 MB fixture was decoded
and encoded both ways in-process (release build, median of 11 encodings):

| Fixture | 0.1.1 MB | Current MB | Current / 0.1.1 | Encode ms, 0.1.1 / current |
|---|---:|---:|---:|---:|
| native JSON / NDJSON representative | 0.63 | 0.63 | 1.00 | 0.26–0.46 / 0.25–0.30 |
| CLEF representative | 1.05 | 1.05 | 1.00 | 0.62 / 0.53 |
| NDJSON string-heavy | 0.99 | 0.99 | 1.00 | 0.36 / 0.44 |
| native JSON / NDJSON many small events | 1.25 | 1.25 | 1.00 | 1.35–2.28 / 1.98–2.95 |
| OTLP protobuf logs representative | 1.93 | 1.04 | 0.54 | 1.17 / 0.95 |
| OTLP JSON logs representative | 0.85 | 0.46 | 0.54 | 0.98 / 0.42 |
| OTLP JSON logs many small records | 1.78 | 0.45 | 0.26 | 2.32 / 0.81 |
| OTLP JSON logs large shared resource | 46.32 | 0.35 | 0.01 | 49.11 / 0.79 |
| OTLP protobuf logs large resource, representative records | 31.22 | 0.98 | 0.03 | 30.45 / 1.16 |
| OTLP JSON logs large resource, representative records | 13.69 | 0.43 | 0.03 | 12.45 / 0.30 |
| OTLP protobuf traces representative | 2.04 | 1.20 | 0.59 | 2.17 / 1.56 |
| OTLP JSON traces representative | 0.81 | 0.48 | 0.59 | 0.72 / 0.59 |
| OTLP protobuf metrics representative | 2.27 | 1.14 | 0.50 | 2.44 / 1.43 |
| OTLP JSON metrics representative | 1.03 | 0.51 | 0.50 | 0.92 / 0.39 |

OTLP batches repeat an SDK resource (7 attributes) on every record in
0.1.1's batches, so the resource table halves their WAL and its write time; a large shared resource
goes from dominating the WAL to costing once per batch. Native events have
no resource and encode the same, about 0.6 ms per MB slower for tiny events
(the per-event resource index). Segments are unchanged: their bodies keep
the resource inline and are compressed per block.

**Sustained overload** (`derived_admission_holds_the_ingest_budget_under_overload`).
Default and constrained derived configurations, three consecutive runs
each. `4 × limit + 16` clients cycle through every fixture at the
request-size limit, the high-amplification shapes among them, plain and
gzip, retrying 429s after 5 ms. A body over the configuration's decoding
limits (decided in-process with the same limits) must get 413, any other
body 200 or 429; with the default `queue_capacity` most high-amplification
bodies are refused by event count, the rest by decoded size. Each run has
three phases: 1 s of warm-up with every slot held by the test, so all
clients are rejected and their connections' read buffers reach working
size (reported, not counted: per-connection HTTP overhead); 2 s with every
writer parked, so admitted requests hold their slots with their batches
queued and unacknowledged while everyone else is rejected and drained; and
2 s with the writers running. In the parked phase storage allocates
nothing, so all heap growth is attributed to ingest; it includes the
server's connection and runtime buffers, which over-counts. It must stay
within the ingest budget. Process heap growth over both phases includes the
WAL and active segments and must stay within `max_memory`.

| Config | Run | Limit | Clients | Ingest budget | Conn. buffers | Ingest peak (parked) | 429s parked | 200s / 429s / 413s | Process heap peak | RSS before / parked / after |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| default (512MB, 1MB) | 1 | 3 | 28 | 76.8 MB | 2.2 MB | 30.1 MB | 3,703 | 39 / 6,678 / 21 | 126.1 MB | 61.5 / 84.6 / 199.2 MB |
| default (512MB, 1MB) | 2 | 3 | 28 | 76.8 MB | 2.2 MB | 40.7 MB | 3,679 | 17 / 6,681 / 32 | 70.3 MB | 54.9 / 101.8 / 144.0 MB |
| default (512MB, 1MB) | 3 | 3 | 28 | 76.8 MB | 2.2 MB | 29.7 MB | 3,678 | 15 / 6,482 / 13 | 61.4 MB | 129.5 / 133.7 / 102.7 MB |
| constrained (128MB, 256KB) | 1 | 1 | 20 | 19.2 MB | 1.5 MB | 4.5 MB | 2,771 | 42 / 5,188 / 66 | 19.4 MB | 69.4 / 70.2 / 75.0 MB |
| constrained (128MB, 256KB) | 2 | 1 | 20 | 19.2 MB | 1.5 MB | 4.6 MB | 2,792 | 30 / 5,091 / 55 | 19.1 MB | 32.2 / 41.4 / 67.8 MB |
| constrained (128MB, 256KB) | 3 | 1 | 20 | 19.2 MB | 1.6 MB | 3.3 MB | 2,735 | 50 / 5,262 / 55 | 23.6 MB | 66.7 / 66.9 / 74.8 MB |

Every run saturated at exactly the derived limit, returned only 200s, 429s
and the expected 413s (no resets) and accepted a request afterwards. With
the high-amplification shapes in the mix, the default runs peak at 30–41 MB
of ingest heap, under both the 72.75 MB estimate and the 76.8 MB budget
(36–43 MB before REL-16 with representative bodies only), and process heap
at 61–126 MB (102–142 MB before; streaming the WAL record removed the
encoded batch's two copies). RSS is the whole
test process (clients, fixtures and earlier runs included) and is reported
for context only.

**Decoding throughput** (AC4). The tolerance declared for REL-16 is a
regression of at most 10% in median decode time per accepted fixture. Each
accepted 1 MB fixture was decoded 21 times in-process by `main` (090669d)
and by REL-16, three alternating rounds, release build, same laptop; the
medians:

| Fixture | Events | `main` ms | REL-16 ms | Change |
|---|---:|---:|---:|---:|
| native JSON representative | 3,075 | 15.88 | 14.50 | −8.7% |
| NDJSON representative | 3,075 | 15.14 | 13.06 | −13.7% |
| CLEF representative | 2,970 | 12.64 | 11.96 | −5.3% |
| NDJSON string-heavy | 86 | 0.39 | 0.41 | +5.7% |
| NDJSON many small events | 43,810 | 34.05 | 27.96 | −17.9% |
| native JSON many small events | 43,810 | 31.11 | 27.24 | −12.4% |
| OTLP protobuf logs representative | 4,539 | 17.32 | 18.23 | +5.2% |
| OTLP JSON logs representative | 2,008 | 20.71 | 21.58 | +4.2% |
| OTLP JSON logs many small records | 6,594 | 22.15 | 18.99 | −14.3% |
| OTLP JSON logs large shared resource | 2,872 | 49.57 | 7.79 | −84.3% |
| OTLP protobuf traces representative | 4,297 | 21.69 | 22.40 | +3.3% |
| OTLP JSON traces representative | 1,716 | 23.17 | 22.65 | −2.2% |
| OTLP protobuf metrics representative | 5,825 | 12.21 | 10.08 | −17.4% |
| OTLP JSON metrics representative | 2,644 | 20.59 | 19.05 | −7.5% |
| total | | 296.61 | 235.88 | −20.5% |

Every fixture is within the tolerance. The OTLP/protobuf pre-scan costs a
few percent; streaming native parsing and shared resources save more. (The
large-shared-resource row is the pre-REL-16 fixture size, 0.42 MB.)

**Before and after.** `pre_rel14_defaults_under_the_same_overload` runs the
same harness with the old defaults (16MB requests, 4 slots) and
`queue_capacity = 200000`, so that 16MB batches are parsed and queued rather
than refused for exceeding the queue.

| | Before (512MB, 16MB, 4 slots) | After (512MB, 1MB, 3 slots) |
|---|---:|---:|
| Ingest heap peak, writers parked (76.8 MB budget) | 494.7–714.7 MB | 36.2–43.4 MB |
| Process heap peak (512 MB `max_memory`) | 540.0–825.5 MB | 102.3–142.4 MB |
| Requests / MB accepted in 2 s of writing | 4–8 / 64–128 MB | 22–26 / 22–26 MB |
| 429s in 2 s of writing | 692–709 | 6,385–6,673 |

The trade-off: with 1MB requests and 3 slots, the same overload accepts
fewer bytes per second of writing and rejects far more requests, and
clients that sent 16MB batches must send smaller ones. In return ingest
stays inside its budget instead of exceeding `max_memory`.

**Linux release run** (REL-14, before the decoding limits). The same suite in a `rust:1-bookworm` container
(Docker Desktop, Linux x86_64, 22 cores, 15 GB, Rust 1.99.0, `--release
--locked`), with the repository mounted read-only:

```bash
docker run --rm -v "$PWD:/src:ro" -v vyrtel-target:/target -e CARGO_TARGET_DIR=/target -w /src \
  rust:1-bookworm cargo test -p server --release --locked --test overload -- --nocapture --test-threads 1
```

| | Linux x86_64 | Windows x86_64 |
|---|---:|---:|
| Admission ×body, every fixture | within 0.15 of Windows | table above |
| Highest supported fixture | 17.00× | 17.00× |
| Default ingest peak, writers parked (3 runs) | 21.2–28.5 MB | 36.2–43.4 MB |
| Constrained ingest peak (3 runs) | 1.6–4.4 MB | 3.5–4.1 MB |
| Drain peak (12.75 MB allowance) | 2.87 MB | 2.9–6.7 MB |
| Pre-REL-14 ingest peak (3 runs) | 458.7–773.8 MB | 494.7–714.7 MB |

Linux RSS climbed to about 507 MB during the run and stayed there: glibc
keeps freed memory, and the amplification test that runs first allocates
over 400 MB for its out-of-envelope fixtures. RSS from this harness is
therefore a process high-water mark, not a per-run figure. Measure RSS for
release sign-off on a dedicated server process, as PERF-GATE-1 (#170)
requires.

**Rejected-body drain** (`rejected_body_drain_overhead_is_within_the_fixed_allowance`).
One slot, held by a stalled upload; 64 clients send 1 MB bodies (half gzip)
as fast as they are rejected, for 3 s. After 1 s of warm-up the
connections' read buffers are at working size (about 470 KB each, about
29 MB in all, not counted). The remaining 2 s measure the drain itself
against the 12.75 MB fixed overhead (32 drains × hyper's 408 KB read
buffer). Measured drain peaks: 2.9–6.7 MB in release runs, 6.8 MB in a debug
run.

### API key changes and authentication (AUTH-PERF-01)

Creating and revoking API keys writes SQLite on the blocking pool and then
publishes a new immutable key set; requests authenticate against the
current set without waiting for those writes
([design](architecture.md#api-keys-and-request-authentication),
[API semantics](api.md#api-keys)).

```bash
cargo test -p server --lib auth::                         # registry: slow, failed, cancelled, concurrent changes
cargo test -p server --test integration auth::            # over HTTP, including a write stalled in SQLite
```

| Acceptance criterion (AUTH-PERF-01) | Tests and evidence |
|---|---|
| AC1: key writes on blocking execution; requests never wait behind SQLite; mutations serialised and bounded | `auth::tests::slow_key_writes_block_neither_requests_nor_the_runtime` (a runtime with 1 worker and 2 blocking threads: while one key write is stalled behind a barrier, 15 more changes queue without taking a blocking thread, the 17th is refused `503 unavailable`, a heartbeat keeps ticking, another `spawn_blocking` runs at once, and requests are decided from the published set); integration `auth::stalled_key_writes_do_not_hold_up_ingestion` (a second SQLite connection holds the write lock: 40 ingest requests are decided by the keys in force while the revoke waits, which fails on the pre-change code because requests waited for the revoke) |
| AC2: linearisation, failure and cancellation ordering | `auth::tests::failed_key_writes_publish_nothing` (failed, invalid and panicking writes publish nothing and release their turn; a failed first ingest key leaves ingestion open; a failed revoke keeps the key accepted and ingestion protected); `cancelled_key_changes_finish_or_never_start` (a create whose caller is gone mid-write still commits and is published; a change cancelled while waiting never writes; every permit returns); `no_admission_with_a_key_after_its_revoke_returns` (50 first/last ingest-key rounds against 3 concurrent checkers); integration `stalled_key_writes_do_not_hold_up_ingestion` (a create that cannot commit within the store's 5 s busy timeout returns 500 and accepts nothing) |
| AC3: open/protected policy unchanged; one admission decision | `auth::tests::only_ingest_keys_protect_ingestion`, `concurrent_key_changes_match_the_store_across_restarts` (only ingest keys toggle protection), `no_admission_with_a_key_after_its_revoke_returns` (policy and key from one set: keyless is admitted exactly when the set is open, a revoked key never yields a principal, a valid admin key is always admitted); integration `ingest_protection_follows_the_ingest_keys`, `ingest_protection_changes_are_immediate_under_concurrent_requests`, `ui_api_requires_session_or_admin_key`, `ingestion_without_keys_leaves_the_rest_of_the_api_closed` (unchanged) |
| AC4: concurrent create/create/revoke, first/last transitions, slow/failed writes, restart parity | `concurrent_key_changes_match_the_store_across_restarts` (16 concurrent creates, then 8 revokes with 8 creates at once; scopes and the refused revoked keys checked; the published set equals what a reload of the database gives, before and after reopening it); every registry test ends with `assert_matches_store`; integration `ingest_protection_survives_restarts` |
| AC5: matched steady vs churn benchmark | `scripts/bench/keychurn.sh` with `tools/loadgen` `keychurn`, below; `scripts/bench/keychurn-report.test.mjs` (empty, partial, short, unmatched and non-finite evidence fails; each budget at and past its limit) |

No per-request database read was added: authentication still only reads
the in-memory set, and last-used is still persisted at most once a minute
per key (bounded since AUTH-PERF-03, [below](#api-key-last-used-tracking-auth-perf-03)).

#### API key churn benchmark

`keychurn` logs in as the admin, creates one ingest key so every ingest
request is authenticated, and for the duration runs closed-loop producers
with that key, an auth probe (a keyless ingest request every 10 ms,
answered 401 by the authentication layer alone, so its latency is the time
to an authentication decision including scheduler wait), a `/health`
heartbeat every 10 ms, and, in churn cells, one admin that creates and
revokes an ingest key every 20 ms. At the end it counts the acknowledged
events back with a query. `scripts/bench/keychurn.sh` runs a matched
baseline and candidate on fresh data directories for every cell of
producers {4, 64} × events per request {1, 100} × churn {off, on},
alternating the binaries within each repetition, and records the server's
CPU seconds and peak memory around each run (Linux `/proc`, or PowerShell
`Get-Process` under Git Bash on Windows):

```bash
git worktree add ../vyrtel-baseline <baseline-sha> && (cd ../vyrtel-baseline && cargo build --release -p server)
cargo build --release -p server -p loadgen
RUNS=3 DURATION=20s scripts/bench/keychurn.sh ../vyrtel-baseline/target/release/vyrtel target/release/vyrtel
node scripts/bench/keychurn-report.mjs target/keychurn --json target/keychurn/report.json
# a subset, e.g. a rerun of two cells with 5 repetitions:
CELLS='^high-batched' RUNS=5 OUT=target/keychurn-rerun scripts/bench/keychurn.sh BASELINE CANDIDATE
node scripts/bench/keychurn-report.mjs target/keychurn-rerun --cells '^high-batched' --min-runs 5
```

Settings: `RUNS`, `DURATION`, `LOW`/`HIGH` (producers), `SMALL`/`BATCHED`
(events per request), `CHURN` (pause between create/revoke pairs), `CELLS`
(a regex of cells to run), `OUT`, `KEYCHURN`. The server runs with its default configuration plus a random
admin password and alerts off; the generator runs on the same host.

**Evidence completeness**, checked by `keychurn-report.mjs` before any
budget (exit 1 when incomplete): every expected cell (all 8, or those
matching `--cells`) has results for both binaries and no other cell is
present; each has at least `--min-runs` (default 3) usable runs per binary,
the same number for both, with the same workload settings; a run is usable
when both its files parse, its gated values are finite, it acknowledged
events and measured the auth probe, and it changed keys exactly when its
cell is a churn cell. An empty or partial results directory therefore
fails rather than reporting “All budgets met”. The report's own tests:
`node --test scripts/bench/*.test.mjs` (CI job **Release gate tests**).

**Budgets**, fixed before the first run and checked by
`keychurn-report.mjs` (exit 1 when missed):

* every run, both binaries: every acknowledged event persisted; no ingest
  401, 5xx, timeout or transport error; every key create and revoke
  succeeded; every auth probe answered 401 (429s from ingest admission are
  reported, not failures: the high-concurrency cells exceed
  `ingest.max_concurrent` by design);
* per cell, candidate median against baseline median: accepted events/s at
  least 90%; server CPU per accepted event at most 110%; auth probe p99 at
  most max(110%, +1 ms); peak memory at most 110%.

**Results, 2026-10-26** (raw per-run values in
[`docs/benchmarks/auth-perf-01/`](benchmarks/auth-perf-01/)). Baseline
`ad346b6` (main before this change), candidate: this change on top of it;
both `cargo build --release` with Rust 1.96.1 on Windows 11, Intel Core
Ultra 7 155H (16 cores, 22 threads), 31.5 GB, generator on the same host,
20 s per run, uninstrumented (no profiler attached). Medians of 3 runs:

| Cell (producers-size-churn) | events/s base → cand | auth p99 ms base → cand | ACK p99 ms base → cand | CPU µs/event base → cand | peak MiB base → cand |
|---|---|---|---|---|---|
| low-small-steady | 9,920 → 10,058 | 0.62 → 0.61 | 0.84 → 0.73 | 271 → 261 | 101 → 102 |
| low-small-churn | 9,958 → 9,503 | 0.66 → 0.72 | 0.76 → 0.84 | 271 → 286 | 102 → 104 |
| low-batched-steady | 255,818 → 242,473 | 1.17 → 1.52 | 3.65 → 4.62 | 13.3 → 14.2 | 149 → 149 |
| low-batched-churn | 236,535 → 276,152 | 1.39 → 1.09 | 4.63 → 3.31 | 14.5 → 12.8 | 150 → 146 |
| high-small-steady | 5,747 → 6,241 | 4.64 → 4.40 | 5.74 → 4.95 | 1,107 → 1,031 | 110 → 111 |
| high-small-churn | 6,095 → 6,858 | 4.65 → 3.78 | 4.76 → 4.17 | 1,071 → 1,000 | 110 → 111 |
| high-batched-steady | 163,115 → 145,317 | 9.97 → 11.26 | 13.96 → 17.30 | 38.2 → 42.2 | 179 → 179 |
| high-batched-churn | 143,294 → 119,352 | 14.41 → 16.98 | 20.23 → 22.52 | 41.8 → 44.9 | 175 → 183 |

Every run of both binaries passed the correctness budgets: all
acknowledged events persisted; no ingest 401, 5xx, timeout or transport
error; all 19,171 create/revoke pairs succeeded. The first pass missed
five throughput/latency budgets, all in the two `high-batched` cells, one
of which has no key changes at all. Those cells were rerun with 5 more
repetitions each:

| Rerun (5 runs) | events/s base → cand | auth p99 ms base → cand | CPU µs/event base → cand | Budgets |
|---|---|---|---|---|
| high-batched-steady | 154,778 → 159,617 | 7.32 → 6.22 | 42.9 → 43.9 | met |
| high-batched-churn | 166,843 → 159,848 | 8.45 → 9.80 | 39.7 → 40.5 | auth p99 missed (+1.35 ms; allowed +1 ms) |

and the baseline was run against an identical copy of itself (A/A, 3 runs
of `high-batched-steady`): events/s 182,262 vs 193,662 (+6.3%), auth p99
5.64 vs 4.13 ms (−27%). On this host the run-to-run spread of the
high-concurrency cells is larger than the budgets, so these runs neither
show nor rule out a difference there; in the six other cells the candidate
is within budget, and lower-latency in the churn cells for small requests.
The budgets stay as declared; the missed one is recorded, not waived.

**Open before 0.1.2 performance sign-off:** the `high-batched-churn` auth
p99 budget is neither met nor shown to be a regression. It is to be rerun
on the dedicated Linux benchmark host, with the generator on a separate
machine, comparing a quiet baseline with the exact candidate build
(`CELLS='^high-batched-churn' RUNS=5`, `--min-runs 5`), and must pass or be
explicitly dispositioned by the reviewer as part of
[PERF-GATE-1](https://github.com/rowan-smith/Vyrtel/issues/170).

**Limitations.** One laptop, generator and server sharing 22 threads, no
CPU pinning; the A/A spread above bounds what these numbers can resolve.
The closed-loop producers retry immediately, so in every cell most requests
are 429s from ingest admission (even 4 producers exceed the derived
`ingest.max_concurrent` on this host): the throughput figures are of
an admission-saturated server. Lock and scheduler wait are not measured
directly: the auth probe and heartbeat latencies stand in for them; a
profile, taken separately from these runs, would be needed to attribute
time. CPU is process time from `Get-Process` and memory is the Windows peak
working set. This is not the release profile: as a candidate-affecting
change, it invalidates earlier
[PERF-GATE-1](https://github.com/rowan-smith/Vyrtel/issues/170)
evidence, which must be taken again on the final candidate, preferably on a
quiet Linux host with this harness included.

### Session authentication (AUTH-PERF-02)

Session-cookie requests are authenticated from a bounded in-memory cache
of valid sessions in front of the SQLite lookup
([design](architecture.md#session-authentication),
[API semantics](api.md#authentication)); `/api/v1/system/info` reports
where session authentication spends its time (`sessionAuth`).

```bash
cargo test -p server --lib auth::                         # cache: expiry, logout vs fill races, bounds, failures
cargo test -p server --test integration auth::            # over HTTP: parity, isolation, fallback, logout, expiry, restart
node --test scripts/bench/*.test.mjs                      # the benchmark report's evidence and budget checks
```

| Acceptance criterion (AUTH-PERF-02) | Tests and evidence |
|---|---|
| AC1: matched session-only vs admin-key-only load: auth-only, searches, facets, dashboards, concurrent ingestion and metadata writes | `tools/loadgen` `sessionauth` and `scripts/bench/sessionauth.sh`, below (each credential client is built with only its header and no cookie store; `GET /api/v1/auth/me` confirms before each run that the key clients carry no session; the report fails a key run that used any session); integration `auth::session_and_admin_key_authenticate_alike_in_isolation` |
| AC2: end-to-end, auth, queue and lock time, lookup rate, concurrency, latency percentiles, throughput, CPU, memory, errors; predeclared budgets; ≥3 matched runs, cold and warm | `sessionAuth` counters (queue, lock, query time, lookups, cache hits, in-flight); `sessionauth` summaries (per-request p50/p95/p99/max, cold and warm, the auth probe apart from query work, ingest and write outcomes); `sessionauth-report.mjs` budgets below, fixed before the first run; integration `auth::session_lookups_wait_for_metadata_writes_and_fail_closed` (a stalled metadata write shows up as lock time) |
| AC3: budgets met, or the smallest evidenced fix | The first matrix missed the budgets (below); the fix is the session cache, and the rerun on it is the evidence |
| AC4: cache entry and byte caps, eviction, bounded fills, no raw tokens, expiry, immediate invalidation (logout and password change), fill vs revoke, no unbounded negative cache | `auth::tests::cached_sessions_are_used_until_their_expiry_only`, `the_cache_is_bounded_in_bytes_too` (256-byte names: the 1 MiB byte cap binds before the entry cap; an 8 KiB name is authenticated but never cached; the byte count stays exact through expiry, logout and re-fills), `clearing_forgets_everything_including_fills_in_flight`, metadata `rotating_a_password_ends_only_that_users_sessions`, integration `a_changed_admin_password_ends_existing_sessions` (fails before this change: a session from before the rotation was still accepted), `invalidation_wins_over_a_fill_in_flight`, `the_cache_is_bounded_and_unknown_tokens_never_grow_it` (10,000 unknown tokens cache nothing; 100 expired and 4,596 live sessions leave exactly 4,096 live ones), `concurrent_fills_are_bounded` (64 misses: 16 read at once, waiters hold no blocking thread, cancelled waiters release their turn); integration `logout_is_immediate_under_concurrent_cached_requests` |
| AC5: expiry, invalid cookie with key fallback, concurrent invalidation, DB failure, restart, missing users; no bypass | `auth::tests::lookup_failures_fail_closed`, `sessions_follow_the_store` (login, never-issued token, expiry while cached, logout, restart with an empty cache); integration `invalid_cookies_fall_back_to_keys_and_logout_is_immediate`, `sessions_end_at_their_expiry_and_survive_restarts`, `session_lookups_wait_for_metadata_writes_and_fail_closed` (an unreadable session table is a 500 even with a valid key alongside), `logout_is_immediate_under_concurrent_cached_requests`; existing `ui_api_requires_session_or_admin_key` and the ingest-policy tests unchanged |

The ingest policy (no ingest key = open, ingest key = protected) is not
touched: ingestion never uses sessions, and the existing tests above
(`ingest_protection_*`, `ingestion_without_keys_leaves_the_rest_of_the_api_closed`)
pass unchanged.

#### Session authentication benchmark

`sessionauth` (in `tools/loadgen`) seeds 20,000 events, creates an admin
key, an ingest key and a dashboard with one log-count panel, then runs
the load with **one credential only**: the session cookie, or the admin key
(`Authorization: Bearer`). Closed-loop clients run the cell's workload:

* `auth`: `GET /api/v1/system/config` (no SQLite, no query: authentication
  plus routing);
* `query`: in turn a search (`/query/logs`, 50 events), facets (`level`,
  `service`) and a dashboard view (`GET /dashboards/{id}` then its panel's
  histogram);
* `mixed`: `query`, plus 4 producers sending 100 events per request with
  the ingest key and one writer creating and deleting a saved query every
  50 ms with the load credential.

Alongside, an auth probe (`GET /api/v1/system/config` every 10 ms with the
load credential) measures the authentication decision apart from query
work, and a `/health` heartbeat the runtime. The first 3 s are reported as
cold, the rest as warm. `scripts/bench/sessionauth.sh` runs every cell of
workload {auth, query, mixed} × clients {4, 32} with each credential on
the same binary, a fresh data directory per run, alternating the
credentials (and which goes first, by repetition), and records server CPU
seconds and peak memory around each run:

```bash
cargo build --release -p server -p loadgen
RUNS=3 DURATION=20s scripts/bench/sessionauth.sh target/release/vyrtel
node scripts/bench/sessionauth-report.mjs target/sessionauth --json target/sessionauth/report.json
# one cell, 5 repetitions:
CELLS='^mixed-low$' RUNS=5 OUT=target/sessionauth-rerun scripts/bench/sessionauth.sh target/release/vyrtel
node scripts/bench/sessionauth-report.mjs target/sessionauth-rerun --cells '^mixed-low$' --min-runs 5
# a longer run of the heaviest cell (soak):
CELLS='^mixed-high$' RUNS=1 DURATION=4m OUT=target/sessionauth-soak scripts/bench/sessionauth.sh target/release/vyrtel
```

Settings: `RUNS`, `DURATION`, `LOW`/`HIGH` (clients), `CELLS`, `OUT`,
`SESSIONAUTH`, `DURABILITY` (`storage.durability`, default `normal`). The
server runs with its default configuration plus a random admin password and
alerts off; the generator runs on the same host. Credentials are synthetic
(a random password per invocation; the keys and the session created for the
run) and are never written to the results.

**Evidence completeness**, checked by `sessionauth-report.mjs` before any
budget (exit 1 when incomplete): every expected cell has results for both
credentials and no other cell is present; each has at least `--min-runs`
(default 3) usable runs per credential, the same number for both, with the
same workload; a run is usable when both files parse, it used the
credential and workload of its directory, its gated values are finite, and
it completed load requests and warm probes.

**Budgets**, fixed before the first run and checked by
`sessionauth-report.mjs` (exit 1 when missed):

* every run, both credentials: no load, probe, write or ingest request
  answered 401, 403, 5xx other than 503, another error status, a timeout
  or a transport error (503s from busy query slots and ingest 429s are
  reported, not failures); every probe succeeded; no session lookup
  failed; in mixed cells every acknowledged event persisted and the writer
  made changes; key runs used no session (no cookie was sent); session runs
  authenticated every successful request by session (cache hit or SQLite
  lookup);
* session runs, absolute: mean SQLite session lookup (queue + metadata
  lock + query) at most 1 ms; warm probe p99 at most 25 ms;
* per cell, session median against key median: successful requests/s at
  least 90%; warm load p99 and warm probe p99 at most max(110%, +1 ms);
  server CPU per successful request at most 110%; peak memory at most 110%.

**Results, 2026-10-27** (raw per-run values in
[`docs/benchmarks/auth-perf-02/`](benchmarks/auth-perf-02/)). Baseline
`308d74b` (main before this change, plus only the `sessionAuth` counters,
which time each lookup and change nothing else); candidate: this change
with the session cache. Both `cargo build --release` with Rust 1.96.1 on
Windows 11, Intel Core Ultra 7 155H (16 cores, 22 threads), 31.5 GB,
generator on the same host, 20 s per run, uninstrumented (no profiler).
Medians of 3 runs, key → session:

*Without the cache* (`2026-10-27-windows-laptop-before-cache`): budgets
missed in every cell.

| Cell (workload-clients) | requests/s | load p99 ms | probe p99 ms | CPU µs/request | SQLite lookups/s | mean lock wait µs |
|---|---|---|---|---|---|---|
| auth-low | 18,910 → 12,351 | 0.52 → 0.71 | 0.58 → 0.88 | 98 → 210 | 12,451 | 38 |
| auth-high | 37,616 → 11,413 | 1.65 → 10.34 | 1.73 → 11.31 | 162 → 304 | 11,512 | 2,401 |
| query-low | 241 → 252 | 36.4 → 38.4 | 0.95 → 1.33 | 15,050 → 14,200 | 419 | 20 |
| query-high | 274 → 243 | 183 → 238 | 1.06 → 1.87 | 15,152 → 14,685 | 404 | 28 |
| mixed-low | 14.4 → 12.1 | 1,980 → 1,872 | 1.08 → 1.91 | 294,233 → 324,561 | 144 | 7 |
| mixed-high | 16.2 → 14.2 | 6,339 → 6,521 | 1.04 → 1.71 | 274,045 → 283,731 | 144 | 21 |

Every session request read SQLite through the one metadata connection; at
32 clients requests queued for it (mean lock wait 2.4 ms, over the 1 ms
absolute budget) and throughput fell 70%. That decided AC3: the fix is the
cache.

*With the cache* (`2026-10-27-windows-laptop-matrix`):

| Cell (workload-clients) | requests/s | load p99 ms | probe p99 ms | CPU µs/request | cache hits per run | SQLite lookups in the run |
|---|---|---|---|---|---|---|
| auth-low | 12,902 → 14,223 | 0.85 → 0.77 | 0.91 → 0.87 | 156 → 141 | 286,633 | 0 |
| auth-high | 22,584 → 24,933 | 5.41 → 5.25 | 7.94 → 8.39 | 229 → 220 | 500,719 | 0 |
| query-low | 163 → 164 | 69.1 → 64.2 | 1.26 → 1.17 | 23,583 → 23,626 | 5,878 | 0 |
| query-high | 209 → 193 | 256 → 245 | 1.11 → 1.24 | 19,699 → 21,181 | 6,714 | 0 |
| mixed-low | 13.2 → 11.8 | 2,019 → 2,262 | 1.51 → 1.50 | 347,411 → 396,639 | 3,001 | 0 |
| mixed-high | 12.9 → 13.8 | 7,740 → 6,542 | 1.69 → 1.51 | 323,185 → 323,738 | 3,145 | 0 |

Five cells meet every budget. Session requests no longer read SQLite once
cached (the one lookup per session happens during setup), and in the
auth-only cells they are as fast as key requests. Every run of both
credentials passed the correctness checks: no 401, 403, 5xx, timeout or
transport error; all acknowledged events persisted; key runs used no
session.

`mixed-low` missed throughput, load p99 and CPU. Rerun with 5 repetitions
(`2026-10-27-windows-laptop-mixed-low-rerun`): requests/s 12.26 → 12.57
(met), CPU µs/request 378,828 → 358,238 (met), probe p99 1.40 → 1.59 ms
(met), peak memory met; **load p99 2,275 → 2,672 ms (+17%) missed**, from
the dashboard histogram queries (2,423 → 2,904 ms) while ingestion runs.
Those session runs did no SQLite lookup (only about 3,000 cache hits each,
sub-microsecond), and their auth probe is within budget, so the extra time
is in query execution under ingestion, not authentication; the key runs
alone ranged 1,588–2,561 ms, wider than the budget.

*Soak* (`2026-10-27-windows-laptop-soak`, `mixed-high`, 4 minutes per
credential): authentication stayed flat (probe p99 1.51 → 1.90 ms, max
11.8 → 54.5 ms, 33,741 cache hits and no SQLite lookup) while the data
grew and the query clients saturated the 4 query slots: both credentials
had the same share of 10 s client timeouts (494 of 1,295 key, 469 of 1,248
session), which fails the correctness budget for both. That is query-engine
load, independent of the credential, and belongs to
[PERF-GATE-1](https://github.com/rowan-smith/Vyrtel/issues/170).

**Open before 0.1.2 performance sign-off**, both retained under
[PERF-GATE-1](https://github.com/rowan-smith/Vyrtel/issues/170); neither
is shown to come from session authentication, and neither is waived:

1. the `mixed-low` load p99 budget (+17.4% session against key over 5
   runs, with the auth probe within budget and the difference in histogram
   query time);
2. the `mixed-high` soak correctness budget (10 s client timeouts for both
   credentials alike: 494 of 1,295 key, 469 of 1,248 session).

Both are to be rerun on the dedicated Linux benchmark host, with the
generator on a separate machine (`CELLS='^mixed-low$' RUNS=5`,
`--min-runs 5`; the soak at the #170 duration), and must pass or be
explicitly dispositioned by the reviewer.

**Limitations.** One laptop, generator and server sharing 22 threads, no
CPU pinning. Key-only throughput in the auth cells moved by up to 40%
between the two matrices, which use the same key path, so compare session
with key only within one matrix. Queue and lock time are measured directly
(`sessionAuth`), but runtime scheduling is not; no profile was taken. CPU
is process time from `Get-Process`, memory the Windows peak working set.
The soak is 4 minutes, not the #170 soak. Query workloads use one service
over the last hour, so their time grows with ingested data. As a
candidate-affecting change, this invalidates earlier
[PERF-GATE-1](https://github.com/rowan-smith/Vyrtel/issues/170) evidence,
which must be taken again on the final candidate with this harness
included.

### API key last-used tracking (AUTH-PERF-03)

Requests authenticated by API key record the key's last-used time in
bounded bookkeeping: at most once a minute per key, at most 4096 keys
remembered (only active ones), at most 1024 waiting and one writer thread
([design](architecture.md#api-key-last-used-tracking),
[API semantics](api.md#api-keys)); `/api/v1/system/info` reports it
(`apiKeyTouch`).

```bash
cargo test -p server --lib auth::                         # throttle, revoke race, churn, caps, failures, restart
cargo test -p server --test integration auth::            # over HTTP: last-used written, revoked keys reclaimed
cargo test -p metadata --test store touching_api_keys     # the batched write skips revoked keys
node --test scripts/bench/*.test.mjs                      # the benchmark report's evidence and budget checks
```

| Acceptance criterion (AUTH-PERF-03) | Tests and evidence |
|---|---|
| AC1: revoked keys' entries reclaimed; a request admitted before a revoke cannot re-add them; numeric entry/byte and queue/worker bounds | `auth::tests::revoked_keys_leave_no_last_used_state` (a revoke drops its key's entry; touches with the set from before the revoke are `Retired` and add nothing), `a_touch_racing_a_revoke_is_dropped_when_the_revoke_publishes` (deterministic: a touch queued while the revoke's write is held behind a barrier loses its entry when the revoke publishes, and later touches with the old set are refused), `concurrent_touches_and_revokes_leave_only_active_keys` (4 touchers against 32 revokes); metadata `touching_api_keys_in_a_batch_skips_revoked_keys`; integration `auth::api_key_last_used_is_recorded_and_revoked_keys_are_reclaimed`. Bounds: `KEY_TOUCH_ENTRIES` 4096 × `KEY_TOUCH_ENTRY_BYTES` 48 = 192 KiB, `MAX_PENDING_KEY_TOUCHES` 1024, 1 writer; every test ends with `assert_bounded` |
| AC2: coalesced, rate-limited writes; no unbounded `spawn_blocking`; at capacity defer/drop with counters, never reject ingest or bypass auth; no write storm on eviction | `a_stalled_writer_bounds_waiting_keys_and_threads` (a runtime with 2 blocking threads: while the writer is stalled, 4,999 more keys give exactly 1,024 waiting and 3,975 dropped, decided without waiting; a waiting key used again is not queued twice; the other blocking thread stays free; on release the waiting keys are written in one batch, and a dropped key is written on its next use); `high_cardinality_keys_at_the_cap_are_deferred_not_written` (5,096 keys round robin for 58 fake-clock seconds: exactly 4,096 writes, the other 1,000 deferred 30 times each without any write; once the entries are a minute old the 1,000 get in, written once each). The touch result never affects the request: `require_ingest` and `require_user` ignore it |
| AC3: approximate last-used and the one-minute throttle kept; failure, cancellation and shutdown release everything; no keys or hashes in logs or metrics | `last_used_writes_keep_the_one_minute_throttle` (fake clock: not written again at 0 s, 1 s or exactly 60 s, written again at 60 s + 1 ns; per key); `failed_and_panicking_writes_release_everything` (a failing database: the writer is released, the key is retried after the interval and not before; a panicking writer gives back its waiting keys, which queue again on next use); `a_writer_that_never_runs_releases_everything` (a runtime already shut down); integration test checks `system/info` contains no key or hash. Counters are aggregate (`apiKeyTouch`); the one log line names a count and the SQLite error only |
| AC4: ≥10,000 sequential create → use → revoke with bounded active keys; concurrent touch/revoke; saturated worker/queue; slow/failed DB; restart | `ten_thousand_revoked_keys_leave_nothing_behind` (10,000 rounds through the real registry and SQLite with at most 8 keys active: at most 9 entries at any time, 8 at the end, 9,992 forgotten, every active key's `last_used_at` written); the stalled, failing and concurrent tests above; `last_used_survives_a_restart_and_bookkeeping_starts_empty` |
| AC5: before/after entries, queue, workers, heap/RSS slope, CPU, write rate, p99 ingest/auth latency, post-load cleanup; predeclared budgets | `tools/loadgen` `keytouch`, `scripts/bench/keytouch.sh` and `keytouch-report.mjs`, below |

The ingest policy (no ingest key = open, an ingest key = protected) and
query/UI authentication are unchanged: `ingest_protection_*`,
`ingestion_without_keys_leaves_the_rest_of_the_api_closed` and
`ui_api_requires_session_or_admin_key` pass unchanged, and the new
integration test checks that revoking the last ingest key reopens
ingestion and that revoked keys are then refused once another ingest key
exists. No configuration was added and the schema is unchanged.

#### API key last-used benchmark

`keytouch` logs in as the admin, creates the cell's active ingest keys and
for the duration runs 16 closed-loop producers that use those keys in turn
(each request the next key), an auth probe (a keyless ingest request every
10 ms, answered 401) and one churner that creates an ingest key, ingests
one event with it, revokes it and waits 2 ms, so revoked keys accumulate
with the run. It samples `/api/v1/system/info` every second, waits 5 s
after the load, samples again (post-load cleanup) and counts the
acknowledged events back. `scripts/bench/keytouch.sh` runs a matched
baseline and candidate on fresh data directories for each cell, alternating
the binaries within each repetition, and records the server's CPU seconds
(from just before the keys are created), peak memory and RSS every 5 s:

* `few-churn`: 8 active keys, churn on;
* `cap-churn`: 5,000 active keys (past the 4,096-entry cap), churn on.

```bash
git worktree add ../vyrtel-baseline <baseline-sha> && (cd ../vyrtel-baseline && cargo build --release -p server)
cargo build --release -p server -p loadgen
RUNS=3 DURATION=70s scripts/bench/keytouch.sh ../vyrtel-baseline/target/release/vyrtel target/release/vyrtel
node scripts/bench/keytouch-report.mjs target/keytouch --json target/keytouch/report.json
```

Settings: `RUNS`, `DURATION`, `FEW`/`CAP` (active keys), `PRODUCERS`,
`CHURN` (pause between rounds), `CELLS`, `OUT`, `KEYTOUCH`. The server runs
with its default configuration plus a random admin password and alerts off;
the generator runs on the same host. Runs are 70 s so each key's one-minute
throttle expires within the run.

**Evidence completeness**, checked by `keytouch-report.mjs` before any
budget (exit 1 when incomplete): both cells (or those matching `--cells`)
for both binaries and no others; at least `--min-runs` (default 3) usable
runs per binary, the same number for both, with the same workload; a run is
usable when its files parse, its gated values are finite, it acknowledged
events, measured the auth probe and completed churn rounds; a candidate run
must also report `apiKeyTouch`.

**Budgets**, fixed before the first run and checked by
`keytouch-report.mjs` (exit 1 when missed):

* every run, both binaries: every acknowledged event persisted; no ingest
  401, 5xx, timeout or transport error; every key create and revoke
  succeeded and no churn ingest got a 401, 5xx, timeout or transport error
  (429s from ingest admission are reported, not failures); every auth probe
  answered 401;
* every candidate run: at most 4,096 keys tracked and 1,024 waiting at
  every sample and at most 1 writer; after the settle period nothing
  waiting, no writer, at most as many keys tracked as are active (not
  growing with the revoked ones), and no failed write batch;
* per cell, candidate median against baseline median: accepted events/s at
  least 90%; server CPU per accepted event at most 110%; auth probe p99 and
  ingest ACK p99 at most max(110%, +1 ms); peak memory at most 110%.

**Results, 2026-10-28** (report and raw per-run values in
[`docs/benchmarks/auth-perf-03/`](benchmarks/auth-perf-03/)). Baseline
`c232a6f` (main before this change), candidate: this change on top of it;
both `cargo build --release` with Rust 1.96.1 on Windows 11, Intel Core
Ultra 7 155H (16 cores, 22 threads), 31.5 GB, generator on the same host,
70 s per run plus 5 s settle, uninstrumented (no profiler attached).
Medians of 3 runs; **all budgets met** on the first pass:

| Cell | events/s base → cand | ACK p99 ms | auth p99 ms | CPU µs/event | peak MiB |
|---|---|---|---|---|---|
| few-churn | 13,162 → 13,808 | 1.05 → 0.99 | 0.90 → 0.83 | 497 → 480 | 117 → 116 |
| cap-churn | 8,559 → 10,143 | 3.37 → 1.87 | 1.55 → 1.41 | 953 → 640 | 140 → 128 |

Every run of both binaries persisted every acknowledged event, with no
ingest 401, 5xx, timeout or transport error and every key create and
revoke succeeding (about 15,400–16,000 churn rounds per `few-churn` run and
7,300–8,900 per `cap-churn` run).

Bookkeeping, candidate (medians; `apiKeyTouch`):

| Cell | peak tracked | tracked after settle | peak waiting | writer threads | rows written (per s) | write batches | deferred | dropped | revoked keys forgotten |
|---|---|---|---|---|---|---|---|---|---|
| few-churn | 9 | 8 | 3 | ≤ 1 | 15,856 (≈ 227) | 15,851 | 0 | 0 | 15,840 |
| cap-churn | 4,096 | 4,096 | 1,024 | ≤ 1 | 8,222 (≈ 117) | 1,828 | 326,087 | 172 | 30 |

The baseline does not report its bookkeeping. From its code, it keeps one
map entry for every key ever authenticated and never removes one, and
spawns one blocking task per due write: over these runs that is about
15,400 retained entries and as many spawned writes per `few-churn` run, and
about 12,900 of each per `cap-churn` run (the churned keys plus the 5,000
active keys), growing for as long as keys are churned. The candidate keeps
at most the active keys (8) or the cap (4,096) and at most one writer
thread, independent of how many keys were revoked, and nothing was left
waiting after the load. In `cap-churn` the 4,096 tracked keys were all
written within the minute, so the other active keys and every churned key
were deferred (not written) rather than evicting anything: 8,222 rows were
written there against roughly 12,900 baseline writes. Touches count
authenticated requests, including those then refused `429` by ingest
admission, as before.

Retained heap: at about 50 bytes per baseline entry, its growth here is
under 1 MB per run, below this host's RSS noise. The RSS slopes over the
second half of the load (sampled every 5 s from the Windows working set)
range from −62 to +51 MiB/min for both binaries alike, so they neither
show nor rule out a slope difference at these run lengths; RSS after the
5 s settle was lower for the candidate (median 58 vs 87 MiB `few-churn`,
97 vs 133 MiB `cap-churn`), within the same spread.

**Open before 0.1.2 performance sign-off:** none of these budgets. The
retained-memory comparison needs a longer authenticated soak on the
dedicated Linux benchmark host (where `system/info` reports RSS itself),
with the generator on a separate machine: `CELLS='^cap-churn$'` at the
[PERF-GATE-1](https://github.com/rowan-smith/Vyrtel/issues/170) soak
duration, comparing the RSS slope and the number of revoked keys.

**Limitations.** One laptop, generator and server sharing 22 threads, no
CPU pinning, 3 runs per cell. Most ingest requests are 429s from ingest
admission (16 producers exceed the derived `ingest.max_concurrent` here),
so throughput is of an admission-saturated server. Write rate and queue
depth are the candidate's own counters; the baseline's are derived from
its code, not measured. CPU is process time from `Get-Process` (including
creating the active keys), memory the Windows peak and current working set.
No profile was taken. As a candidate-affecting change, this invalidates
earlier [PERF-GATE-1](https://github.com/rowan-smith/Vyrtel/issues/170)
evidence, which must be taken again on the final candidate with this
harness included.

## Frontend tests

Vitest + React Testing Library in `web/tests/unit/`:

* live toggle states (paused red / live green) and toggling;
* query form submission and external updates;
* log row expansion and the vertical property tree (nested objects, arrays,
  null, include/exclude actions);
* filter panel selection;
* error rendering with a caret under the query position;
* trace navigation from a log;
* the Logs page with a mocked API and `EventSource`: results, diagnostics,
  filter clicks updating the query, live streaming, and paused mode keeping
  results stationary;
* helpers (query building, formatting, routing, span tree ordering,
  nanosecond math).

## Browser E2E (Playwright)

`web/tests/e2e/logs.spec.ts` runs against the real binary:

1. launch the server with an empty data directory;
2. ingest the NDJSON and OTLP trace fixtures;
3. open Logs, search `level = Error`, verify the results;
4. expand an event and verify structured properties and the stack trace;
5. confirm Live is paused, ingest an event, verify it does **not** appear;
6. enable Live, ingest events, verify the matching one appears (and the
   non-matching one does not);
7. pause again — results stay;
8. open the related trace and verify the span hierarchy and related logs.

`web/tests/e2e/storage.spec.ts` checks the storage-failure contract against
the same binary, through HTTP only:

1. `/ready` is `{"status": "ready", "storageWritable": true}` and every
   signal in `/api/v1/system/storage` is healthy with no `faults`;
2. on Linux and macOS (skipped on Windows and as root), `data/tmp/` is made
   read-only for real: `/ready` turns 503 with `storageWritable: false` and
   all signals listed, the `writeProbe` fault says `permissionDenied` on
   `tmp/.write-probe` and the response never contains the data directory;
   once permissions are restored `/ready` is 200 again and an event ingested
   before the fault is still queryable.

The data directory is exported to the specs as `VYRTEL_E2E_DATA`.

```bash
cd web && npm ci && npm run build && cd ..
cargo build -p server
cd web && npx playwright install chromium && npm run e2e
```

Set `VYRTEL_BIN` to test a different binary (e.g. a release build).

## Release gate tests

`scripts/release/release.test.mjs`, `scripts/release/performance.test.mjs` and
`scripts/release/image.test.mjs` (`node --test`,
CI job **Release gate tests**) cover the checks in
[`scripts/release/release.mjs`](../scripts/release/release.mjs) and
[`scripts/release/performance.mjs`](../scripts/release/performance.mjs) and
[`scripts/release/image.mjs`](../scripts/release/image.mjs) that
[`release.yml`](../.github/workflows/release.yml) runs before publishing (see
[releasing.md](releasing.md)):

| Acceptance criterion | Tests |
|---|---|
| A tag whose version differs from the workspace fails | stable and prerelease tags against temporary workspaces; stale `Cargo.toml`, `Cargo.lock` or `web/package.json`; malformed tags; the repository's own version is consistent |
| Failed checks publish nothing | the workflow's job graph: `image` and `release` transitively need `preflight`, `performance`, `ci` (the whole callable CI workflow), `binaries` and `verify`; only a tag push publishes (a manual run is a rehearsal); release builds set `VYRTEL_REQUIRE_WEB_UI` |
| Embedded UI, version output and checksums are verified | artifact sets that are complete, missing a binary, tampered, missing `SHA256SUMS` or an entry, holding unexpected or empty files; the artifact list matches the build matrix; a fake binary (`testdata/fake-vyrtel.mjs`) that is correct, prints or serves the wrong version, serves the placeholder UI, misses its UI assets or exits on startup |
| A prerelease does not replace the stable latest channel | stable plans are Latest and tag `X.Y.Z`, `X.Y`, `latest`; prereleases (`-rc.1`, `-beta`, `-alpha.3`) are not Latest and tag only `X.Y.Z-pre`; image names are lowercased |
| TC-PERF-01: missing or failed performance evidence blocks tagging and publication | in throwaway git repositories: no record → `pretag --create` (function and CLI) exits 1 and creates no tag; FAIL verdict, missing reviewer, failing or mis-marked metric rows, missing limit/baseline/category/reports, artifact hashes missing for a published binary or listed twice, N/A without reason, budgets declared after measuring; a version with no declared gates; a review after the tag or a lightweight tag is refused by the workflow check; uncommitted changes and existing tags |
| TC-PERF-02: stale evidence is rejected until revalidated | a commit after PASS makes the record stale (no tag) until a PASS for the new commit; same-version gates (0.2.0) on different commits, or one missing; a record for another gate or version; rebuilt artifacts whose hash differs without documented `equivalence` |
| COMPAT-GATE TC5: missing, failed or stale compatibility evidence blocks tagging and publication (`compat.test.mjs`) | no record → `pretag --create` refuses even with a performance PASS, and `check-compat` exits 1; FAIL verdict, missing reviewer or date, a failed, missing, unknown or evidence-less checklist item, no inventory, artifact hashes missing for a published binary or listed twice, a wrong issue, exceptions not approved by Rowan or without owner, removal release or issue; a commit after PASS is stale until a renewed review; a record for another layout identity than the candidate's `telemetry::LAYOUT`; a review after a performance measurement of the same candidate, or for another candidate than the performance gate; a review after the tag or a lightweight tag; built artifacts that differ from the reviewed hashes |
| COMPAT-GATE AC7: every tagged release needs its gate | a prerelease needs its own compatibility PASS (but no performance sign-off) and its tag records it; a version without a declared gate cannot be tagged, and declaring one in `compat-gates.json` (the setup for a newly assigned release) makes it require and accept its record; the manifest, the record template and the issue template are complete; the workflow's `compat` job blocks `binaries` and `verify` |
| TC-PERF-03: a complete passing matrix for the exact candidate passes | the documented template covers every metric category and passes; `pretag --create` makes an annotated tag on the candidate with the PASS lines, which `check-performance --tag-date` (the workflow's check) accepts; matching or documented-equivalent artifacts; prereleases need no sign-off; the manifest declares unique gates with issues; the workflow wiring |
| REL-09 AC1: on each architecture the image becomes ready and serves the UI | the workflow runs `verify-image` in an `image-smoke` matrix on native `linux/amd64` and `linux/arm64` runners; each platform is pushed by digest only after its smoke test and checked to have the tested layers; `image` tags exactly those digests without building, and the published image is checked against the tested records (`checkManifest`: a missing platform, or a platform whose layers differ from the tested image, fails; `imagesByPlatform`, `testedRecord`); CI runs the same gate on amd64; an image for the wrong platform fails |
| REL-09 AC2: events in a named volume survive recreating the container | `checkPersisted`: the same events and settings pass; a lost or changed event, an extra event, or a changed `dataDir` or `durability` fail; `docker port` parsing |
| REL-09 AC3: an unwritable bind mount explains the uid requirement | `checkBindMountRefusal`: a refusal with the hint passes; a start that succeeds, or a refusal without the hint, fails; the strings it expects are the ones in `crates/server/src/access.rs`. Server side: `access::tests` (unit) and `data_dir_access` (integration, Unix, real binary on a `555` directory and on a missing directory under one). `awaitRefusal` with a fake `docker`: the expected refusal is returned and the container removed; a server that keeps running fails at the deadline (0.3 s) instead of hanging; one that prints `Ready.` fails at once; the container is removed every time |
| REL-09 AC4: the image runs as non-root and advertises its version | `checkImageConfig`: `nonroot:nonroot`/`65532` pass; root, unset, `0:0` and a root group fail; a missing or `dev` version label fails; missing `/data` volume, storage path or healthcheck fail; `processUid` requires uid 65532 for the running process; the Dockerfile's `USER`, label and `/data` owner |

```bash
node --test scripts/release/*.test.mjs
VYRTEL_BIN=target/debug/vyrtel node --test scripts/release/*.test.mjs   # also checks a real binary
VYRTEL_IMAGE=vyrtel:rc node --test scripts/release/image.test.mjs       # also runs verify-image (Docker)
```

The CI **End-to-end** job also builds the server with `VYRTEL_REQUIRE_WEB_UI=1` and runs
`verify-binary` against it, so the gate is exercised against a real binary on every pull request. The performance tests
run git with an empty global configuration (`GIT_CONFIG_GLOBAL`), so local signing or hook
settings do not affect them.

## Benchmarks

Criterion benchmarks (`cargo bench -p storage -p query`). Results from one
run on a Windows 11 laptop, release profile — treat them as relative, not
absolute:

| Benchmark                                                       | Result                    |
|-----------------------------------------------------------------|---------------------------|
| WAL append, 1-event batch                                       | 5.0 µs (no fsync)         |
| WAL append, 100-event batch                                     | 34 µs (~2.9 M events/s)   |
| Encode 1,000 events                                             | 213 µs                    |
| Decode 1,000 events                                             | 2.8 ms (allocation-bound) |
| Create 20k-event segment (sort, encode, compress, index, fsync) | 183 ms (~110k events/s)   |
| Decode a whole 20k-event segment                                | 71 ms                     |
| Decompress one block, all columns                               | 556 µs (~470 MiB/s)       |
| Decompress one block, meta column only                          | 34 µs                     |
| Parse a 5-clause query                                          | 3.2 µs                    |
| 200k events / 4 segments: `message contains` (scan)             | 39 ms                     |
| … `traceId = …` (Bloom filter)                                  | 1.6 ms                    |
| … `level = Error` (bitmap)                                      | 4.9 ms                    |
| … `durationMs > 1990` (zone map)                                | 14 ms                     |
| … latest 200 events                                             | 0.66 ms                   |
| … count all (block counts, no decode)                           | 2.9 µs                    |

The benchmark dataset is synthetic and very regular (13.6× compression;
its unique-per-event `requestId` makes Bloom filters a large share of a tiny
segment). The load generator's data is more realistic: see below.

### End-to-end benchmarks

The Criterion benchmarks above measure library code in-process. For HTTP-level
numbers (ingest throughput and query latency against a real release server; the
[baseline](benchmarks.md#reproducible-baselines) adds a mixed-load p50/p90/p95/p99
profile), use the .NET harness under `dotnet/`. Results are published at [benchmarks](benchmarks.md):

```bash
cargo build --release -p server
dotnet run --project dotnet/Vyrtel.Benchmark -c Release -- --job short
```

### Reproducible baselines

`dotnet/Vyrtel.Baseline` records repeated runs of a seeded workload against a server with a
pinned configuration and proposes regression budgets from the run-to-run variance (see
[Reproducible baselines](benchmarks.md#reproducible-baselines)):

```bash
cargo build --release -p server
dotnet run --project dotnet/Vyrtel.Baseline -c Release -- --runs 5
dotnet run --project dotnet/Vyrtel.Baseline -c Release -- compare old.json target/baseline/report.json
```

Its logic is unit-tested without a server in `dotnet/Vyrtel.Baseline.Tests` (xUnit;
`dotnet test dotnet/Vyrtel.slnx -c Release`, CI job **.NET**). The tests check arithmetic, report
contents and determinism; they never assert a machine-specific timing. Measured runs keep their
raw numbers in the report instead.

| Acceptance criterion (REL-10) | Tests and evidence |
|---|---|
| AC1: a seeded workload with a pinned configuration records accepted/rejected counts and latency percentiles | `DatasetCheckTests` (spans and data points counted from the fixture payloads; a complete dataset passes; events acknowledged but not stored, under-acknowledged events, and a shortfall or surplus in any signal fail the run with both counts); `RequestTallyTests` (each status classified: 2xx accepted, 429/503 backpressure, 413 too large, other rejected, 0 transport error; counts by outcome and status, exact under concurrency, summed across operations); `BaselineReportWriterTests.States_accepted_and_rejected_counts_and_percentiles`; `WorkloadTests.Each_worker_issues_the_same_sequence_every_run`, `The_sequence_is_fixed_across_processes`, `Seeded_events_are_identical_apart_from_timestamps`; `HostAndOptionsTests.Finds_the_checked_in_pinned_configuration`; Rust `config::tests::baseline_config_file_is_valid_and_pins_the_derived_ingest_concurrency` (the pinned file loads, keeps compaction off, and its ingest concurrency equals the derived value); measured: the reference baseline's accepted/rejected table |
| AC2: selective queries and message scans include cold/warm behaviour and diagnostics | `QueryMeasurerTests` (a healthy endpoint is measured cold and 20× warm; every warm request returning 500, a 503 on the cold request, a single 500 among warm requests, and repeated timeouts each stop the run with the query, request and status named, so a broken endpoint cannot record 0 ms; through the real HTTP client, a local server answering 500 yields no measurement); `WorkloadTests.Queries_cover_selective_lookups_and_message_scans`, `Reads_the_servers_diagnostics` (every diagnostics field, including indexes), `Missing_or_malformed_diagnostics_are_null`; `BaselineReportWriterTests.States_cold_and_warm_timings_with_diagnostics`; measured: the reference baseline's cold/warm table |
| AC3: the report states hardware, duration, peak RSS, binary size and compression method | `BaselineReportWriterTests.States_hardware_duration_peak_rss_binary_size_and_compression`; `HostAndOptionsTests` (`/proc/cpuinfo` on x86 and arm64, this host's CPU, memory and disk, peak RSS source: OS peak vs sampling, never under-reporting) |
| AC4: budgets are justified by measured variance | `BaselineStatisticsTests` (sample standard deviation and CV; the 90% upper-bound factor k(n) for 3, 5, 10, 31 and 41 runs; `3 × CV × k(n)` rounded up, wider for fewer runs; timing and size floors; `Same_commit_noise_measured_on_the_reference_laptop_is_not_a_regression` replays the false regression two same-commit baselines produced before k(n); fewer than 3 runs, zero mean and a budget over 50% are not gated, with the reason); `BaselineComparerTests` (regression, improvement and within-budget verdicts in both directions; throughput is higher-is-better; peak RSS growth; `Collapsing_successful_throughput_behind_fast_429s_fails`: attempted requests/s rises while successful requests/s, accepted events/s and the unsuccessful share regress, so the comparison fails with exit status 2; a rise in rejections from a 0% baseline is caught by the 1-point floor; a missing gated metric, a report missing every metric, a 2-run baseline that gates nothing, and configuration, CPU or seed changes all fail; a missing ungated metric and a different number of runs do not); `BaselineStatisticsTests.Successful_throughput_counts_only_accepted_requests`, `Attempted_throughput_is_informational_and_never_gated`, `Ratio_budgets_are_absolute_points`; `BaselineReportWriterTests.States_budgets_with_their_justification`, `Json_round_trips_and_samples_go_to_csv`, `Refuses_a_report_with_another_schema`; measured: two independent baselines compared in [Run-to-run evidence](benchmarks.md#run-to-run-evidence) |

## Load generator

`tools/loadgen` sends realistic structured logs (services, error rate,
high-cardinality customer and request ids, nested HTTP properties,
exceptions with stack traces) and optionally OTLP traces.

```bash
cargo run --release -p loadgen -- \
  --target http://localhost:8080 --rate 10000 --duration 60s \
  --services 8 --error-rate 0.02 --cardinality 10000 --traces-every 200
```

Options: `--rate`, `--duration`, `--services`, `--error-rate`,
`--cardinality`, `--batch`, `--concurrency`, `--traces-every`, `--api-key`,
`--seed`. It reports throughput, latency and backpressure (429/503).

The same package has `connload`, which keeps a steady mixed load while
opening waves of idle, slow and normal connections, for the
[transport soak](#transport-soak-on-linux):

```bash
cargo run --release -p loadgen --bin connload -- --target http://localhost:8080 \
  --duration 3m --attempts 1000 --wave-interval 30s --out connload-out
```

Options: `--attempts`, `--wave-interval`, `--wave-patience`,
`--ingest-clients`, `--batch`, `--ingest-interval`, `--query-clients`,
`--query-interval`, `--sample-interval`, `--no-waves`, `--out`, `--label`.
It writes `summary.json` and `samples.csv`, and exits with status 1 if any
acknowledged event is missing afterwards.

`keychurn`, in the same package, measures authenticated ingest while an
admin creates and revokes API keys (see
[API key churn benchmark](#api-key-churn-benchmark)):

```bash
cargo run --release -p loadgen --bin keychurn -- --target http://localhost:8080   --duration 20s --concurrency 64 --batch 1 --churn-interval 20ms
```

Options: `--admin-password` (or `VYRTEL_AUTH_ADMIN_PASSWORD`),
`--admin-username`, `--duration`, `--concurrency`, `--batch`,
`--churn-interval` (0 = no churn), `--probe-interval`, `--timeout`,
`--out`, `--label`. It writes `summary.json` and exits with status 1 if any
acknowledged event is missing afterwards.

`sessionauth` runs the same load with only a session cookie or only an
admin key (see
[Session authentication benchmark](#session-authentication-benchmark)):

```bash
cargo run --release -p loadgen --bin sessionauth -- --target http://localhost:8080 \
  --auth session --workload mixed --concurrency 32 --duration 20s
```

Options: `--auth session|key`, `--workload auth|query|mixed`,
`--admin-password` (or `VYRTEL_AUTH_ADMIN_PASSWORD`), `--admin-username`,
`--duration`, `--cold`, `--concurrency`, `--seed-events`,
`--ingest-concurrency`, `--batch`, `--write-interval`, `--probe-interval`,
`--timeout`, `--out`, `--label`. It writes `summary.json` (no credentials)
and exits with status 1 if any acknowledged event is missing afterwards.

`keytouch` uses many API keys in turn while one churner creates, uses and
revokes keys, sampling the server's RSS and last-used bookkeeping (see
[API key last-used benchmark](#api-key-last-used-benchmark)):

```bash
cargo run --release -p loadgen --bin keytouch -- --target http://localhost:8080   --active-keys 5000 --concurrency 16 --churn-interval 2ms --duration 70s
```

Options: `--admin-password` (or `VYRTEL_AUTH_ADMIN_PASSWORD`),
`--admin-username`, `--duration`, `--active-keys`, `--concurrency`,
`--batch`, `--churn-interval` (0 = no churn), `--probe-interval`,
`--sample-interval`, `--settle`, `--timeout`, `--out`, `--label`. It writes
`summary.json` (no keys) and exits with status 1 if any acknowledged event
is missing afterwards.

Observed on the same laptop with a release build (`--rate 10000 --duration
30s --traces-every 200`, default config):

| Metric                              | Value                                       |
|-------------------------------------|---------------------------------------------|
| Accepted                            | 302,000 events, 10,063 events/s, 0 rejected |
| Request latency (500-event batches) | 5.3 ms mean, 20.9 ms max                    |
| Process memory during the run       | 112 MB working set (126 MB peak)            |
| Idle memory before the run          | 20 MB working set                           |
| Compression (sealed segments)       | 4.9× raw → stored                           |
| Index overhead                      | 9.2% of compressed data                     |
| Query latency on that data          | 2–105 ms (see diagnostics in the UI)        |
