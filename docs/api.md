# HTTP API

Base URL: `http://<host>:8080`. All bodies are JSON unless stated otherwise.
Times in responses are RFC 3339 strings with up to nanosecond precision.

## Conventions

### Errors

Every error has the same shape:

```json
{ "error": { "code": "invalid_query", "message": "Expected expression after 'and'", "position": 19 } }
```

| Status | `code`                                                                                                     | When                                                                                                                                                                                                                                               |
|-------:|------------------------------------------------------------------------------------------------------------|----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
|    400 | `invalid_request`, `invalid_json`, `invalid_payload`, `invalid_event`, `invalid_encoding`, `invalid_query` | malformed input (`position` for queries, `index` for batch events); `invalid_encoding` for a gzip body that cannot be decoded; `invalid_payload` with `"limit": "maxDepth"` for ingested values nested too deeply ([details](#decoding-limits))                                                                                                                      |
|    401 | `unauthorized`                                                                                             | missing/invalid API key or session                                                                                                                                                                                                                 |
|    403 | `forbidden`                                                                                                | an ingest-only key used on a read/admin endpoint                                                                                                                                                                                                   |
|    404 | `not_found`                                                                                                | unknown resource or endpoint                                                                                                                                                                                                                       |
|    408 | `request_timeout`                                                                                          | the request body did not arrive in full within `server.body_timeout` of its head; the connection closes ([details](#connections))                                                                                                                |
|    413 | `payload_too_large`                                                                                        | ingest body over `ingest.max_request_size` after gzip decoding, a batch with more events than `ingest.queue_capacity`, or one that would decode into too much memory; `limit` names which (`maxRequestSize`, `maxEvents`, `maxDecodedBytes`) ([details](#request-size-and-encoding), [decoding limits](#decoding-limits))                                                                            |
|    415 | `unsupported_media_type`                                                                                   | content type or `Content-Encoding` not accepted                                                                                                                                                                                                    |
|    422 | `query_too_expensive`                                                                                      | the query would read more than `query.max_scan_bytes` (`limitBytes` gives the limit)                                                                                                                                                               |
|    422 | `query_result_limit`                                                                                       | the result would exceed a result budget: more spans in one trace than `query.max_trace_spans`, or more than 10,000 points per metric series (`limit` gives the limit) ([details](query-language.md#execution-limits-and-cancellation))           |
|    429 | `rate_limited`                                                                                             | every ingest slot busy (`ingest.max_concurrent`) or the ingest queue full — retry after `Retry-After` seconds ([details](#ingest-limits-and-retries))                                                                                              |
|    503 | `unavailable`                                                                                              | storage unavailable, or every query slot (`query.max_concurrent`) stayed busy for `query.queue_timeout` — retry after `Retry-After` seconds                                                                                                       |
|    503 | `storage_write_failed`                                                                                     | storage could not write the request (`reason`: `noSpace`, `permissionDenied`, `readOnly`, `shortWrite`, `ioError` or `other`). `outcome` is `notStored`, or `unknown` if it may reappear after a power loss ([details](#status-codes-and-retries)) |
|    504 | `query_timeout`                                                                                            | the query exceeded `query.timeout` (`limitMs` gives the limit)                                                                                                                                                                                     |
|    500 | `internal`, `storage_error`                                                                                | details are logged server-side, never returned                                                                                                                                                                                                     |

Query limit errors never come with partial results. A query whose client
disconnects is cancelled at its next checkpoint and nothing is sent. See
[Execution limits and cancellation](query-language.md#execution-limits-and-cancellation).

### Connections

Vyrtel serves HTTP/1.1 only, one request at a time per connection
(pipelined requests are answered in order). HTTP/2, `h2c` upgrades and
WebSockets are not served. The limits below apply to every endpoint before
routing, authentication or ingest admission; they are configured under
[`[server]`](configuration.md#connections-and-transport-limits).

| What happens                                                                      | The client sees                                                                                                                                                                                                                   |
|-----------------------------------------------------------------------------------|-----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| All `server.max_connections` slots are taken                                      | The connection is accepted and waits up to 2 s for a slot (at most 64 wait). If none frees, or 64 already wait, Vyrtel writes `503 server_busy` with `Retry-After: 1` and `Connection: close` if the socket takes it at once, then closes. Clients may see only a closed or reset connection: treat it like the 503. |
| No complete request head within `server.header_timeout` (also: idle keep-alive) | The connection closes without a response.                                                                                                                                                                                         |
| Request head larger than `server.connection_buffer`, or more than 100 headers    | `431 Request Header Fields Too Large`, then the connection closes.                                                                                                                                                              |
| Body not complete within `server.body_timeout` of its head                        | `408 request_timeout`, then the connection closes. Endpoints that take no body are unaffected.                                                                                                                              |
| A write stays blocked for `server.write_timeout` (the client stopped reading)    | The connection closes; the rest of the response is not sent.                                                                                                                                                                      |
| Malformed request line or headers, or an HTTP/2 preface                          | `400 Bad Request` or a close.                                                                                                                                                                                                     |

The refusal is written before any of the request is read:

```
HTTP/1.1 503 Service Unavailable
content-type: application/json
retry-after: 1
connection: close

{"error":{"code":"server_busy","message":"too many open connections; retry shortly"}}
```

Clients that keep connections alive (the OpenTelemetry SDKs and Collector,
browsers, `reqwest`, `HttpClient`) reuse a few connections and are not
affected unless many of them share one server. Retry a `server_busy` 503, a
refused or reset connection, and a 408 with backoff; nothing was stored.

### Time inputs

`from` / `to` accept RFC 3339 (`"2026-10-06T10:00:00Z"`), Unix timestamps
(seconds, milliseconds, microseconds or nanoseconds, as numbers or strings),
`"now"`, and offsets from now such as `"-15m"` or `"now-1h"`. Ranges are
half-open: `from <= t < to`. Omitted bounds are unbounded (aggregations
default to the last hour).

## Authentication

Always on for the UI and API. Ingestion follows the API keys:

* **Ingestion** (`/api/v1/events`, `/v1/*`) is open while no ingest-scoped
  API key exists, so producers can send without setup. Creating the first
  ingest key protects it at once: from then on it requires a valid API key
  (`Authorization: Bearer <key>` or `X-Api-Key: <key>`; ingest or admin
  scope). Revoking the last ingest key opens it again. Admin keys never
  turn protection on or off. `GET /api/v1/api-keys` and
  `GET /api/v1/system/info` report the current state as
  `ingestKeyRequired`.
* **Everything else under `/api/v1`** requires either the admin session
  cookie (from `/api/v1/auth/login`) or an API key with scope `admin`.
  A valid session is used first; a missing, empty or unknown cookie falls
  back to the API key, if any. A session is accepted until its expiry
  (`auth.session_ttl`) and refused as soon as logout returns. Starting the
  server with a different `auth.admin_password` ends every existing
  session of the admin user; restarting with the same one keeps them. Valid
  sessions are cached in memory after their first request
  ([design](architecture.md#session-authentication)); this changes no
  outcome. If an uncached session cannot be looked up (a database error)
  the request fails with `500`, even when a key is also sent.
* `/health`, `/ready`, `/api/v1/auth/*` and the web UI assets are public.

```
POST /api/v1/auth/login    {"username": "admin", "password": "..."}  → sets HttpOnly cookie
POST /api/v1/auth/logout
GET  /api/v1/auth/me       → {"authEnabled": true, "user": "admin" | null}   (authEnabled is always true since 0.1.2)
```

### API keys

```
GET    /api/v1/api-keys
POST   /api/v1/api-keys        {"name": "checkout", "scope": "ingest" | "admin"}
DELETE /api/v1/api-keys/{id}
```

The create response contains the plaintext `key` — it is shown once; only a
SHA-256 hash is stored.

Key changes take effect when their response is sent:

* A `201` create or `204` revoke has been committed to the metadata store,
  and every request that starts afterwards is authenticated against it
  (including the first-key/last-key change to ingest protection). Requests
  already admitted before the change finish as admitted; nothing is
  cancelled retroactively.
* A failed change (`4xx`/`5xx`) changes nothing: a create that fails
  accepts no key and does not protect ingestion; a revoke that fails leaves
  the key working. Retry it.
* Changes run one at a time. While 16 are already waiting or running,
  another gets `503 unavailable` with `Retry-After: 1`. A change whose
  client disconnects before its turn is dropped; once its write has started
  it completes.
* Writing a change does not delay requests authenticated by an API key,
  or ingestion while it is open: they are checked against the keys in
  force until the change commits. A session-cookie request whose session
  is not cached yet (its first after login or a restart) reads the same
  metadata database, so it can wait while a key change is being written;
  later requests of that session do not.

`lastUsedAt` in the key list is approximate: it is written in the
background, at most once a minute per key, so it can lag the latest use by
up to a minute (or more while the metadata database is failing or very
busy, or while over 4096 keys were each used within the last minute).
Uses not yet written when the server stops are not recorded. No request
waits for it and none is refused because of it; a revoked key's time is
never updated. `/api/v1/system/info` reports the bookkeeping behind it,
counted since the process started (`apiKeyTouch`, no key ids):

```json
"apiKeyTouch": { "tracked": 12, "trackedCapacity": 4096, "trackedBytes": 576,
                 "trackedByteCapacity": 196608, "pending": 0, "pendingCapacity": 1024,
                 "peakPending": 9, "writers": 0, "writerCapacity": 1,
                 "queued": 340, "written": 331, "batches": 120, "failedBatches": 0,
                 "abandoned": 0, "dropped": 0, "deferred": 0, "retired": 2, "forgotten": 7 }
```

`tracked` keys are remembered as written recently (at most
`trackedCapacity`, counted at 48 bytes each); `pending` keys wait for the
one `writers` thread, which writes them in `batches` (`failedBatches`
failed; their keys are tried again after a minute). `queued` uses were
due a write and `written` rows were updated. Not written: `dropped` (all
`pendingCapacity` slots waiting), `deferred` (every tracked key written
within the last minute), `retired` (the key was revoked after the request
was admitted) and `abandoned` (the writer stopped without writing them).
`forgotten` entries were dropped because their key was revoked.

## Native event ingestion

```
POST /api/v1/events
Content-Type: application/json | application/x-ndjson
Content-Encoding: gzip (optional)
```

Body: a single event object, an array of events, `{"events": [...]}`, or
NDJSON (one object per line). Response: `{"accepted": <n>}`.

```json
{
  "timestamp": "2026-10-06T10:20:13.485Z",
  "level": "Error",
  "service": "payments",
  "environment": "production",
  "message": "Payment failed",
  "messageTemplate": "Payment failed for {customerId}",
  "traceId": "abc123",
  "spanId": "00f067aa0ba902b7",
  "properties": { "customerId": 481, "paymentProvider": "stripe", "amount": 71.50 },
  "resource": { "host.name": "web-1" },
  "exception": { "type": "TimeoutException", "message": "Payment provider timed out", "stackTrace": "..." }
}
```

| Field | Aliases | Notes |
|-------|---------|-------|
| `timestamp` | `@t`, `time`, `ts` | RFC 3339 or Unix number; default: arrival time |
| `level` | `@l`, `severity`, `severityText` | default `Information`; unknown spellings are kept in `severityText` |
| `message` | `@m`, `msg` | rendered from the template if absent |
| `messageTemplate` | `@mt`, `template` | `{Name}` holes are filled from properties |
| `service` | `serviceName`, `service.name` | falls back to `resource["service.name"]` |
| `environment` | `env` | falls back to `resource["deployment.environment[.name]"]` |
| `traceId`, `spanId` | `@tr`, `@sp`, `trace_id`, `span_id` | any string; hex is lowercased |
| `properties` | `attributes` | object |
| `exception` | `@x`, `error` | object or a stack-trace string |
| *anything else* | | becomes a property (Serilog CLEF style) |

Property values may be any JSON, nested up to 32 levels deep. The same
property can change type between events; nothing is rejected for that.
Batches are all-or-nothing: if one event is invalid the request fails with
`invalid_event` and its `index`, and a request rejected for any other
reason (size, encoding, malformed JSON, a [decoding limit](#decoding-limits))
stores nothing either. Size and encoding rules are under
[request size and encoding](#request-size-and-encoding).
The response is sent only after the batch is in the write-ahead log (and
fsynced with `durability = "strict"`) and visible to queries.

## OTLP/HTTP

```
POST /v1/logs
POST /v1/traces
POST /v1/metrics
Content-Type: application/x-protobuf | application/json
Content-Encoding: gzip (optional)
```

Point any OpenTelemetry SDK or Collector `otlphttp` exporter at
`http://<host>:8080`. Responses use the request's encoding; records that
cannot be stored (e.g. spans without ids) are dropped and reported via
`partialSuccess`, and the rest of the request is stored. That is the only
partial outcome: a request rejected with a 4xx stores nothing, including
one over a [decoding limit](#decoding-limits), which is refused as a whole
rather than partly accepted. Error responses (4xx/5xx) are the JSON error
object above for both encodings.

Mapping:

* Resource attributes are kept as `resource`; `service.name` and
  `deployment.environment(.name)` also populate `service` / `environment`.
* Logs: severity number (or text) → `level`; string body → `message`
  (structured bodies are kept in the `body` property); `exception.*`
  attributes → `exception`; `{OriginalFormat}` → `messageTemplate`;
  instrumentation scope name → `otel.scope.name` property.
* Spans: start time → `timestamp`; `durationMs`, kind, status, events and
  links are preserved.
* Metrics: every data point is one event. Gauges and sums store `value`;
  histograms store count/sum/min/max/buckets; exponential histograms and
  summaries keep count/sum (bucket detail dropped).

gRPC is not supported in v0.1.

## Ingest limits and retries

These apply to `POST /api/v1/events` and `POST /v1/{logs,traces,metrics}`
alike.

**Admission.** At most `ingest.max_concurrent` ingest requests are in
progress at once (default: derived from the memory budget, 3 at the default
512MB / 1MB settings; see
[ingest memory](configuration.md#ingest-memory)). The limit is the same
for native and OTLP endpoints, which share one pool. A request holds its slot from
before its body is read, through decompression and parsing, until its
write is acknowledged and the response is ready. Requests that finish, fail
(any 4xx/5xx) or are abandoned by the client (connection closed at any
point) free their slot; one abandoned mid-parse frees it when the parse
ends, because the parse still holds memory until then.

**Rejection.** When every slot is busy the request is rejected at once,
before any of its body is read:

```
HTTP/1.1 429 Too Many Requests
Retry-After: 1
Content-Type: application/json

{"error":{"code":"rate_limited","message":"too many concurrent ingest requests; retry shortly"}}
```

The server then reads and discards the unread body to its end (for at
most 10 s, never buffering it) so that clients still uploading receive this
response instead of a reset connection or broken pipe. At most 32 bodies
are read at a time and at most 256 are kept waiting; beyond that, or past
the time bound, the server drops the body and closes the connection. Treat a connection reset like a 429. `discarding` in
`GET /api/v1/system/info` shows how many rejected bodies are being kept.

### Request size and encoding

`ingest.max_request_size` (default 1 MB; 16 MB before 0.1.2) bounds every ingest body, both
the bytes after gzip decoding and the bytes on the wire. A body of exactly
the limit is accepted; one byte more is rejected.

* Send bodies uncompressed (no `Content-Encoding`, or `identity`) or
  gzip-compressed (`Content-Encoding: gzip`; `x-gzip` and any letter case
  work too). Any other encoding (`br`, `deflate`, `zstd`, or several
  stacked such as `gzip, br`) gets `415 unsupported_media_type` before the
  body is read.
* gzip is decoded as it arrives. Decoding stops as soon as the decoded size
  passes the limit and the request gets `413 payload_too_large`, so a small
  compressed "bomb" never makes the server inflate more than the limit (plus
  one 64 KB read).
* An uncompressed body whose `Content-Length` is over the limit gets the 413
  from its headers, before any of it is read.
* Concatenated gzip members (`cat a.gz b.gz`) form one stream and every
  member is decoded. A truncated stream, a CRC mismatch, anything other than
  gzip after the last member, or an empty body labelled gzip gets
  `400 invalid_encoding` with the decoder's reason.
* After a 413 or `invalid_encoding`, the unread rest of the upload, however
  large, is read and discarded within the same bounds as after a 429
  (above), so a client still sending receives the error rather than a reset
  connection or broken pipe. These responses
  carry `Connection: close`; send the next request on a new connection (HTTP
  clients do this automatically).
* A rejected request stores nothing. The whole body is read and decoded
  before any of it is parsed, and native batches are all-or-nothing. OTLP
  `partialSuccess` (see [OTLP/HTTP](#otlphttp)) is the only partial outcome.

```
HTTP/1.1 413 Payload Too Large
Content-Type: application/json

{"error":{"code":"payload_too_large","message":"request body exceeds the 1 MB limit (measured after gzip decoding); split the batch into smaller requests","limit":"maxRequestSize","limitBytes":1048576}}
```

```
HTTP/1.1 400 Bad Request
Content-Type: application/json

{"error":{"code":"invalid_encoding","message":"gzip body could not be decoded (unexpected end of file); send complete gzip data: one or more members and nothing after the last"}}
```

A 413 is not retryable as is: split the batch and send the parts. With
NDJSON, splitting on line boundaries keeps every event whole:

```bash
split -n l/4 -d big.ndjson part-          # four files, whole lines each
for f in part-*; do
  gzip -c "$f" | curl -s -X POST http://localhost:8080/api/v1/events \
    -H 'Content-Type: application/x-ndjson' -H 'Content-Encoding: gzip' \
    --data-binary @-
done
```

### Decoding limits

A body within `max_request_size` is then decoded within three limits,
checked while it is parsed, before the batch is built
([why and how](configuration.md#decoding-limits)):

| Limit (`limit`)   | Value                                      | Response                                       |
|-------------------|--------------------------------------------|------------------------------------------------|
| `maxEvents`       | `ingest.queue_capacity` (default 10,000)   | `413 payload_too_large`, with `maxEvents`      |
| `maxDecodedBytes` | 17 × `max_request_size` (default 17 MB)    | `413 payload_too_large`, with `limitBytes`     |
| `maxDepth`        | 32 levels of nesting                       | `400 invalid_payload`, with `maxDepth`         |

Events are native events, or OTLP log records, spans and metric data
points. Decoded bytes are the server's estimate of the memory the parsed
batch takes, which ordinary batches keep far below the limit: it is
reached by bodies of very many tiny or empty events, OTLP records of a few
bytes each, or many small nested values. Nesting counts objects and arrays
within one native event, or OTLP `arrayValue` / `kvlistValue` levels
within one attribute or body (key/value lists may stop at about 30, where
the JSON and protobuf parsers' own limits are, with the same error).

```
HTTP/1.1 413 Payload Too Large
Content-Type: application/json

{"error":{"code":"payload_too_large","message":"batch has more than 10000 events (ingest.queue_capacity); split it into requests of at most 10000 events","limit":"maxEvents","maxEvents":10000}}
```

```
HTTP/1.1 413 Payload Too Large
Content-Type: application/json

{"error":{"code":"payload_too_large","message":"batch would take more than 17 MB of memory to decode: it has too many events or values for its size. Split it into smaller requests, or send fewer, larger events","limit":"maxDecodedBytes","limitBytes":17825792}}
```

```
HTTP/1.1 400 Bad Request
Content-Type: application/json

{"error":{"code":"invalid_payload","message":"values are nested more than 32 levels deep; flatten them","limit":"maxDepth","maxDepth":32}}
```

Nothing of a refused request is stored, native or OTLP: limits never cause
an OTLP `partialSuccess`. None of these is retryable as is. For
`maxEvents`, send at most `maxEvents` events per request; for
`maxDecodedBytes`, split the batch into smaller requests (half the events
per request halves the decoded size; the split NDJSON example above works
for both); for `maxDepth`, flatten the values. OpenTelemetry exporters do
not retry a 413 and drop the batch, so keep their batch size below the
limits: for the Collector, `send_batch_max_size` at most `queue_capacity`
and batches within `max_request_size`. The SDKs' default batches (512
records) and the Collector's (8,192) are within the default limits.

### Status codes and retries

| Status | `code`                                                  | Stage                                                                                                          | Retry?                                                                                                                                                                                                              |
|-------:|---------------------------------------------------------|----------------------------------------------------------------------------------------------------------------|---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
|    415 | `unsupported_media_type`                                | content type or `Content-Encoding`, checked before admission                                                   | no                                                                                                                                                                                                                  |
|    429 | `rate_limited`                                          | admission: all `max_concurrent` slots busy                                                                     | yes, after `Retry-After`                                                                                                                                                                                            |
|    413 | `payload_too_large`                                     | body over `max_request_size`, decoded or on the wire                                                           | no — split the batch                                                                                                                                                                                                |
|    400 | `invalid_encoding`                                      | gzip body truncated, corrupt or followed by other data                                                         | no — resend a complete gzip body                                                                                                                                                                                    |
|    400 | `invalid_request` / `invalid_payload` / `invalid_event` | body unreadable (client went away) or malformed                                                                | no — fix the payload                                                                                                                                                                                                |
|    413 | `payload_too_large`                                     | decoding: more events than `queue_capacity` (`maxEvents`) or too much decoded data (`maxDecodedBytes`)         | no — split the batch                                                                                                                                                                                                |
|    400 | `invalid_payload`                                       | decoding: values nested more than 32 levels (`maxDepth`)                                                       | no — flatten the values                                                                                                                                                                                             |
|    429 | `rate_limited`                                          | submission: the signal's queue is full                                                                         | yes, after `Retry-After`                                                                                                                                                                                            |
|    503 | `unavailable`                                           | no acknowledgement within `ack_timeout`, or storage stopping                                                   | yes, after `Retry-After`; the batch may already be stored                                                                                                                                                           |
|    503 | `shutting_down`                                         | the server is stopping; checked before admission, the body is not read and the connection closes             | yes, after `Retry-After`, against the restarted or another instance; nothing was stored                                                                                                                             |
|    503 | `storage_write_failed`                                  | the WAL write or its fsync failed, or sealing is failing (full disk, read-only or inaccessible data directory) | yes, with backoff. `outcome: "notStored"`: nothing was stored. `outcome: "unknown"`: it may reappear after a power loss, so a retry can duplicate it. Fix storage ([what to do](configuration.md#storage-failures)) |

Batches are all-or-nothing, so retrying a request rejected with 429 cannot
duplicate events. The OpenTelemetry SDKs and Collector already retry 429
and 503 with backoff. For your own clients, retry with capped exponential
backoff that waits at least `Retry-After`:

```bash
for attempt in 1 2 3 4 5; do
  status=$(curl -s -o /dev/null -w '%{http_code}' -X POST http://localhost:8080/api/v1/events     -H 'Content-Type: application/x-ndjson' --data-binary @tests/fixtures/logs.ndjson)
  [ "$status" != 429 ] && [ "$status" != 503 ] && break
  sleep $((attempt * attempt))   # Retry-After is 1 s; back off further on repeats
done
echo "$status"
```

`GET /api/v1/system/info` reports the effective limit, how it was chosen,
current use and rejections since start under `ingest`:

```json
{
  "maxConcurrent": 3,
  "concurrencySource": "derived",
  "maxRequestBytes": 1048576,
  "budgetBytes": 80530635,
  "estimatedPeakBytes": 76283904,
  "inFlight": 1,
  "rejected": 0,
  "discarding": 0
}
```

`concurrencySource` is `derived` (`ingest.max_concurrent = 0`) or
`explicit`. `budgetBytes` is the ingest share of `storage.max_memory` and
`estimatedPeakBytes` the estimate for `maxConcurrent` maximum-size requests
plus rejected-body drains; with an explicit limit it can exceed the budget
(see [explicit limits](configuration.md#ingest-memory)). If 429s are
frequent while memory is plentiful, raise `storage.max_memory` or lower
`ingest.max_request_size` rather than sending larger batches.

## Querying logs

```
POST /api/v1/query/logs
{
  "query": "level = \"Error\" and service = \"payments\"",
  "from": "-1h",
  "to": null,
  "limit": 200,
  "direction": "backward",
  "continuationToken": null
}
```

Response:

```json
{
  "events": [ { "id": "000000000000002a", "signal": "log", "timestamp": "...", "level": "Error", "message": "...", "properties": {}, "resource": {} } ],
  "diagnostics": {
    "elapsedMs": 41.2, "segmentsConsidered": 23, "segmentsSkipped": 19,
    "segmentsSkippedByTime": 12, "segmentsSkippedByIndex": 7, "segmentsNotNeeded": 0,
    "blocksConsidered": 400, "blocksSkipped": 383, "blocksRead": 17,
    "eventsExamined": 4812, "eventsMatched": 200, "bytesRead": 1843210,
    "unsealedEventsScanned": 950,
    "indexes": [ { "field": "timestamp", "kind": "time" }, { "field": "level", "kind": "bitmap" }, { "field": "customerId", "kind": "none" } ]
  },
  "continuationToken": "v1.18bd3c…"
}
```

* `direction`: `backward` (newest first, default) or `forward`.
* `limit`: 1–`query.max_results` (default max 1000).
* Pass `continuationToken` back to get the next page. Pagination is
  cursor-based (`(timestamp, id)`), never offset-based.

Aggregations (all take `query`, `from`, `to`):

```
POST /api/v1/query/{signal}/count       → {"count": n, "diagnostics": {...}}
POST /api/v1/query/{signal}/histogram   {"buckets": 60} → {"stepMs", "total", "buckets": [{"start", "count", "levels"}]}
POST /api/v1/query/{signal}/facets      {"sample": 2000, "fields": ["level"]} → {"sampled", "fields": [{"field", "count", "values": [{"value", "count"}]}]}
POST /api/v1/query/{signal}/events      same body/response as /query/logs
```

`{signal}` is `logs`, `traces` (spans) or `metrics` (data points).
`levels` in histogram buckets counts `[none, Trace, Debug, Information,
Warning, Error, Fatal]`. Facets are computed over the most recent `sample`
matching events. `fields` (optional, at most 64) counts only the fields
named; without it every well-known field and the most common properties are
counted. Naming only well-known fields (`level`, `service`, `environment`,
`name`, `status`, `exception.type`) skips the walk over event properties.

### Live tail

```
GET /api/v1/live/logs?query=level%20%3D%20Error     (Server-Sent Events)
```

Events: `ready` once subscribed, then `event` (data: one event as JSON) for
each new matching event. `dropped` (data: count) when more than 200 events
matched in one batch, `lagged` (data: skipped batches) when the client fell
behind — the server never buffers unboundedly for a slow client. Use
`?signal=traces` for spans.

When the server shuts down, each stream sends the events it has already
matched, then `shutdown` (data: `{}`) with `retry: 2000`, and ends the
response normally. `EventSource` reconnects after two seconds; until the
server is back, reconnects fail and should be retried. A new stream opened
while the server is stopping gets `503 shutting_down`.

## Traces

```
POST /api/v1/query/traces   {"query": "status = Error", "from": "-1h", "limit": 50}
GET  /api/v1/traces?query=...&from=...&limit=...
```

Returns recent traces with at least one span matching the query:

```json
{ "traces": [ { "traceId": "4bf9…", "rootName": "POST /checkout", "rootService": "checkout",
                "start": "...", "durationMs": 800.0, "spanCount": 3,
                "services": ["checkout", "payments"], "errorCount": 2, "truncated": false } ],
  "diagnostics": {} }
```

A summary keeps at most 10,000 spans of a trace. With more, `truncated` is
`true` and `spanCount`, `services`, `errorCount` and `durationMs` cover the
kept spans only.

```
GET /api/v1/traces/{traceId}[?from=...&to=...]
→ {"traceId", "summary": {...}, "spans": [...], "diagnostics": {...}}
```

Spans are ordered by start time. Correlated logs: query logs with
`traceId = "<id>"`. A trace with more than `query.max_trace_spans` spans
(default 50,000) in the range gets `422 query_result_limit` rather than an
incomplete waterfall; narrow it with `from`/`to`.

## Metrics

```
GET  /api/v1/metrics      → {"metrics": [{"name": "http.server.requests"}, ...], "truncated": false}
POST /api/v1/query/metrics
{
  "name": "http.server.requests",
  "query": "route = \"/checkout\"",
  "from": "-1h",
  "agg": "sum",
  "stepMs": 60000,
  "groupBy": "route"
}
```

`GET /api/v1/metrics` lists distinct names in name order, at most
`query.max_metric_names` (default 10,000) of them: with more, it returns
the first ones and `truncated: true`. It is a query like the others (a
query slot, `query.timeout`, cancelled on disconnect).

`agg`: `sum`, `count`, `min`, `max`, `avg` (default), `last`. For histogram
points, `sum`/`count`/`avg` use the histogram's sum and count. Response:
`{"name", "agg", "stepMs", "unit", "kind", "series": [{"group", "points": [{"ts", "value"}]}]}`
(at most 20 groups; the rest are merged into `(other)`). A series has at
most 10,000 points: a range and `stepMs` needing more get `422
query_result_limit` before anything is read.

## Dashboards

```
GET    /api/v1/dashboards
POST   /api/v1/dashboards                     {"name": "Payments", "description": null}
GET    /api/v1/dashboards/{id}
PUT    /api/v1/dashboards/{id}                {"name": "Payments (prod)"}
DELETE /api/v1/dashboards/{id}
POST   /api/v1/dashboards/{id}/panels         {"title", "kind", "config", "width", "position"}
PUT    /api/v1/dashboards/{id}/panels/{panelId}
DELETE /api/v1/dashboards/{id}/panels/{panelId}
```

Panel kinds and their `config`:

| `kind` | `config` |
|--------|----------|
| `log_count` | `{"query": "level = Error"}` — count over time |
| `metric_line` | `{"metric": "...", "agg": "avg", "groupBy"?: "...", "query"?: "..."}` |
| `single_stat` | `{"query": "..."}` (log count) or `{"metric": "...", "agg": "..."}` (latest value) |
| `saved_query` | `{"query": "..."}` — latest matching logs |

`width` is in grid columns (1–12). Dashboards take the time range from the
UI; panels have no hidden state.

## Saved queries

```
GET    /api/v1/saved-queries
POST   /api/v1/saved-queries   {"name": "Payment errors", "signal": "logs", "query": "..."}
DELETE /api/v1/saved-queries/{id}
```

## Alerts

```
GET    /api/v1/alerts
POST   /api/v1/alerts
GET    /api/v1/alerts/{id}            (includes recent "history")
PUT    /api/v1/alerts/{id}
DELETE /api/v1/alerts/{id}
POST   /api/v1/alerts/{id}/evaluate   evaluate immediately
```

```json
{
  "name": "Payment error spike",
  "query": "level = \"Error\" and service = \"payments\"",
  "op": ">",
  "threshold": 100,
  "windowSecs": 300,
  "intervalSecs": 60,
  "webhookUrl": "https://example.com/hooks/vyrtel",
  "enabled": true
}
```

v0.1 supports log-count threshold alerts: count matching logs over the last
`windowSecs` and compare with `op threshold` (`>`, `>=`, `<`, `<=`, `=`,
`!=`). Status is `OK`, `FIRING`, or `ERROR` (query failed). Each status
change is recorded and POSTed to the webhook (3 attempts, 10 s timeout):

```json
{
  "source": "vyrtel",
  "alert": { "id": 1, "name": "Payment error spike", "query": "...", "condition": "count > 100", "windowSeconds": 300 },
  "status": "FIRING",
  "previousStatus": "OK",
  "value": 150,
  "threshold": 100,
  "error": null,
  "evaluatedAt": "2026-10-06T10:21:00Z"
}
```

## System

```
GET /health                      → 200 {"status": "ok"} while the process runs
GET /ready                       → 200 {"status": "ready", "storageWritable": true}, 503 {"status": "degraded", ...} or, while shutting down, 503 {"status": "draining"}
GET /api/v1/system/info          version, uptime, event counts, queue depths, connection, ingest and query admission, session-auth timings, API key last-used bookkeeping, active segments, memory budgets, recovery status
GET /api/v1/system/storage       raw/stored bytes, compression ratio, index overhead, recovery status, per-signal detail
GET /api/v1/system/query-stats   per-field query statistics (input for future adaptive indexing)
GET /api/v1/system/config        effective configuration (secrets omitted)
```

`/api/v1/system/info` includes the connection limits and what happened to
connections since the process started:

```json
"connections": { "maxConnections": 256, "active": 3, "pending": 0, "maxPending": 64,
                 "connectionBufferBytes": 65536, "estimatedPeakBytes": 53608448,
                 "admitted": 1250, "queued": 4, "refused": 0, "headerTimeouts": 1190,
                 "bodyTimeouts": 0, "writeTimeouts": 0, "protocolErrors": 2, "acceptErrors": 0 }
```

It also includes how session-cookie authentication was done, counted
since the process started (`sessionAuth`):

```json
"sessionAuth": { "cacheHits": 182310, "cached": 3, "cacheCapacity": 4096,
                 "cachedBytes": 495, "cacheByteCapacity": 1048576,
                 "lookups": 41, "found": 37, "failed": 0, "inFlight": 0, "peakInFlight": 4,
                 "queueMicros": 1720, "lockMicros": 380, "queryMicros": 3120,
                 "maxQueueMicros": 210, "maxLockMicros": 95, "maxQueryMicros": 410 }
```

Each request that sends a non-empty session cookie to an authenticated
`/api/v1` endpoint is either a cache hit or one lookup in the metadata
database (`lookups`, of which `found` were valid). `cached` is the number
of sessions held, at most `cacheCapacity`, and `cachedBytes` their size as
counted against `cacheByteCapacity` (key and user name plus 96 bytes
each). For lookups, `queueMicros` is
time waiting for a turn (at most 16 lookups at once) and a blocking
thread, `lockMicros` time waiting for the metadata connection (held by any
other metadata read or write, such as an API key change or a dashboard
save), `queryMicros` the lookup itself; the `max…` fields are the longest
single wait. Divide a total by `lookups` for the mean. `failed` counts
lookups that hit a database error (answered `500`). Requests authenticated
by API key alone, `/api/v1/auth/*` and ingestion use no session and are
not counted.

`active` connections hold a slot (this request's own included); `pending`
ones wait for one. `refused` counts connections closed for lack of a slot,
at once or after waiting. `headerTimeouts` includes idle keep-alive
connections closed normally, so it grows on a healthy server.
`estimatedPeakBytes` is the
[transport estimate](configuration.md#connections-and-transport-limits) at
these limits. `acceptErrors` above zero usually means the process ran out of
file descriptors.

It also includes query admission and how queries that did not complete
ended, counted since the process started:

```json
"query": { "maxConcurrent": 4, "inFlight": 1, "rejected": 0, "cancelled": 2,
           "timedOut": 1, "tooExpensive": 0, "resultLimited": 0 }
```

`inFlight` counts queries holding a slot, including cancelled ones that
have not reached their next checkpoint yet. `rejected` counts 503s for busy
slots; the others count the matching errors.

`/ready` is unauthenticated, so it says which signals are affected but
never why. It is 503 when a 4 KiB write-and-fsync probe in the data directory's `tmp/` fails (`storageWritable: false`,
every signal listed), or while a signal's WAL or sealing is failing. The probe runs at most once per second; other
requests reuse its result:

```json
{ "status": "degraded", "storageWritable": true, "signals": ["logs"] }
```

`/api/v1/system/storage`:

```json
{
  "rawBytes": 13743895347, "storedBytes": 2576980378, "segmentBytes": 2280000000,
  "indexBytes": 210000000, "walBytes": 40000000, "metadataBytes": 98304,
  "eventCount": 18400000, "segmentCount": 241, "compressionRatio": 5.3, "indexOverhead": 0.092,
  "signals": { "logs": { ... }, "traces": { ... }, "metrics": { ... } },
  "indexCache": { "budgetBytes": 80530636, "usedBytes": 1234567, "entries": 12, "hits": 900, "misses": 12 }
}
```

`rawBytes` is the uncompressed size of events in Vyrtel's binary encoding
(sealed segments plus WAL); `compressionRatio` is raw ÷ (data + index) for
sealed segments.

Each `signals.<signal>` object includes `recovery`: what startup crash
recovery did for that signal in this process's lifetime, and what is in its
quarantine directory now.

```json
"recovery": {
  "status": "damaged",
  "segmentsLoaded": 12, "walFilesReplayed": 1, "walRecordsReplayed": 340, "walEventsReplayed": 5100,
  "segmentsSealed": 0, "staleWalsRemoved": 1, "replacedSegmentsRemoved": 0,
  "walBytesDiscarded": 61,
  "segmentsQuarantined": 0, "walFilesQuarantined": 0, "walTailsQuarantined": 1,
  "quarantinedBytes": 61,
  "quarantineDir": { "files": 1, "bytes": 61, "error": null },
  "quarantined": [
    { "signal": "logs", "kind": "walTail", "source": "000000000014.wal",
      "file": "quarantine/logs/000000000014.wal.tail-48213.1791622413000",
      "bytes": 61, "reason": "incomplete record at offset 48213 (torn write)" }
  ]
}
```

`kind` is `segment`, `wal` or `walTail`; `file` is relative to the data
directory and `reason` never contains host paths. See
[Quarantine](storage-format.md#quarantine) for what each kind means and what
was lost. An empty `quarantined` list means this start found no damage.

`status` is `damaged` when this start quarantined something or discarded
WAL bytes. Otherwise it is `unknown` when `quarantine/<signal>/` could not
be read, `quarantineRetained` when it still holds files (from an earlier
start, or a recovery that was interrupted after setting a file aside), and
`clean` when it is empty or missing. Replaying the WAL is normal and leaves
it `clean`. The `*Quarantined` counts and `quarantinedBytes` cover this
start; `quarantineDir` counts the files on disk now, whichever start put
them there. When the directory or anything in it cannot be listed or
examined, `files` and `bytes` are `null` and `error` says why (`message` is
the OS error text, without a path):

```json
"quarantineDir": { "files": null, "bytes": null,
  "error": { "reason": "permissionDenied", "message": "Permission denied (os error 13)" } }
```

`quarantineBytes` at the top level is the sum of the readable
`quarantine/<signal>/` directories.

The response, and `/api/v1/system/info`, also carry a summary: the worst
status of the three signals (`damaged`, `unknown`, `quarantineRetained`,
`clean`, in that order) and the signals that are not `clean`.

```json
"recovery": { "status": "damaged", "signals": ["logs"] }
```

None of this is in `/health` or `/ready`; quarantine does not make an
instance unready. See [Recovery health](configuration.md#recovery-health).

Storage write failures are reported per signal under `faults` (at most one
each for `wal`, `segment` and `maintenance`), along with `healthy` (no
faults) and `ingestHealthy` (no `wal` or `segment` fault). The top-level
`writeProbe` is the readiness write probe:

```json
"writeProbe": { "writable": true, "fault": null },
"signals": { "logs": {
  "healthy": false, "ingestHealthy": false,
  "faults": [
    { "area": "wal", "operation": "walAppend", "reason": "noSpace",
      "path": "wal/logs/000000000007.wal",
      "message": "No space left on device (os error 28)",
      "firstSeen": "2026-10-11T03:12:09.112Z", "lastSeen": "2026-10-11T03:12:41.870Z",
      "occurrences": 37 }
  ], ... } }
```

`operation` is `walAppend`, `walSync`, `walCreate`, `segmentWrite`,
`maintenance` or `probe`. `path` is relative to the data directory and
`message` is the operating system's error text, so neither contains host
paths. A fault disappears once that part of the write path works again (see
[Storage write failures](storage-format.md#storage-write-failures)).
