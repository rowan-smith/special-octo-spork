#!/usr/bin/env bash
# Restore drill (REL-06): follow the backup and restore procedure in
# docs/configuration.md#backup-and-restore against a real Vyrtel binary and
# check that every documented verification query answers as it did before
# the backup.
#
#   scripts/backup/restore-drill.sh [path/to/vyrtel]     (default: target/release/vyrtel)
#
# Linux and macOS (needs bash, curl, tar, GNU or BSD date). Run from the
# repository root; it uses tests/fixtures. Exits non-zero on the first
# failed check and keeps its working directory for inspection.
set -euo pipefail

VYRTEL=${1:-target/release/vyrtel}
FIXTURES=tests/fixtures
PASSWORD=drill-password
TRACE=4bf92f3577b34da6a3ce929d0e0e4736
WORK=$(mktemp -d "${TMPDIR:-/tmp}/vyrtel-restore-drill.XXXXXX")
# Below the ephemeral ranges (Linux 32768+, macOS 49152+), so no outgoing
# connection, such as one an earlier CI step left in TIME_WAIT, holds it.
PORT=$((20000 + RANDOM % 12000))
URL="http://127.0.0.1:$PORT"
PID=

step() { printf '\n== %s\n' "$*"; }
fail() { printf 'FAIL: %s\n(working directory kept: %s)\n' "$*" "$WORK" >&2; exit 1; }
cleanup() { if [ -n "$PID" ]; then kill -9 "$PID" 2>/dev/null || true; fi; }
trap cleanup EXIT

[ -x "$VYRTEL" ] || fail "no executable at $VYRTEL (build it with: cargo build --release -p server)"

# Start Vyrtel on a data directory and wait until it answers /health.
# Arguments after the directory: VAR=value environment settings and
# --flags for vyrtel.
start() {
  local dir=$1 a envs=() flags=(); shift
  for a in "$@"; do case $a in --*) flags+=("$a") ;; *) envs+=("$a") ;; esac; done
  env ${envs[@]+"${envs[@]}"} "$VYRTEL" --data-dir "$dir" --bind "127.0.0.1:$PORT" \
    --log-level warn ${flags[@]+"${flags[@]}"} >"$WORK/stdout.log" 2>>"$WORK/stderr.log" &
  PID=$!
  for _ in $(seq 1 100); do
    curl -fs "$URL/health" >/dev/null 2>&1 && return 0
    kill -0 "$PID" 2>/dev/null || fail "vyrtel exited during startup: $(cat "$WORK/stderr.log")"
    sleep 0.1
  done
  fail "vyrtel did not become healthy"
}

# Step 1 of the procedure: stop with SIGTERM and require exit status 0.
stop() {
  kill -TERM "$PID"
  local status=0
  wait "$PID" || status=$?
  PID=
  [ "$status" -eq 0 ] || fail "vyrtel exited with status $status: $(tail -5 "$WORK/stderr.log")"
}

# Replace {{now-<n><unit>}} placeholders with Unix nanoseconds (tests/fixtures/README.md).
fixture() {
  local text now ph n ns
  text=$(cat "$FIXTURES/$1")
  now=$(( $(date +%s) * 1000000000 ))
  for ph in $(grep -o '{{now-[0-9]*m\{0,1\}s\{0,1\}}}' "$FIXTURES/$1" | sort -u); do
    n=${ph#\{\{now-}; n=${n%\}\}}
    case $n in
      *ms) ns=$(( ${n%ms} * 1000000 )) ;;
      *s) ns=$(( ${n%s} * 1000000000 )) ;;
      *m) ns=$(( ${n%m} * 60000000000 )) ;;
    esac
    text=${text//"$ph"/$(( now - ns ))}
  done
  printf '%s' "$text"
}

# curl as the logged-in admin; prints the body, fails on a non-2xx status.
admin() { curl -fsS -b "$WORK/cookies" -H 'content-type: application/json' "$@"; }
status() { curl -s -o /dev/null -w '%{http_code}' "$@"; }

login() {
  [ "$(status -c "$WORK/cookies" -H 'content-type: application/json' \
      -d "{\"username\":\"admin\",\"password\":\"$PASSWORD\"}" "$URL/api/v1/auth/login")" = 200 ] \
    || fail "admin login refused"
}

# The documented verification queries. Writes one line per answer to $1,
# keeping only the fields that do not change between runs (no timings).
verify() {
  local out=$1 signal
  : >"$out"
  for signal in logs traces metrics; do
    echo "$signal $(admin -d '{"query":""}' "$URL/api/v1/query/$signal/count" | grep -o '"count":[0-9]*')" >>"$out"
  done
  admin -d '{"query":"level = Error","limit":100}' "$URL/api/v1/query/logs" \
    | grep -o '"message":"[^"]*"' | sort >>"$out"
  admin "$URL/api/v1/traces/$TRACE" | grep -o '"spanId":"[^"]*"' | sort >>"$out"
  admin "$URL/api/v1/metrics" >>"$out"; echo >>"$out"
  admin "$URL/api/v1/dashboards" >>"$out"; echo >>"$out"
  admin "$URL/api/v1/saved-queries" >>"$out"; echo >>"$out"
  admin "$URL/api/v1/alerts" | grep -Eo '"(name|query|op|threshold|enabled)":[^,}]*' >>"$out"
  admin "$URL/api/v1/api-keys" | grep -Eo '"(name|prefix|scope)":"[^"]*"' >>"$out"
}

step "Populate an instance ($VYRTEL, $("$VYRTEL" --version))"
# A short segment age, so the backup holds sealed segments as well as WALs.
start "$WORK/data" VYRTEL_AUTH_ADMIN_PASSWORD="$PASSWORD" VYRTEL_STORAGE_SEGMENT_MAX_AGE=1s
login
KEY=$(admin -d '{"name":"producers"}' "$URL/api/v1/api-keys" | grep -o '"key":"vyr_[0-9a-f]*"' | cut -d'"' -f4)
[ -n "$KEY" ] || fail "no API key created"
ingest() { curl -fsS -o /dev/null -H "authorization: Bearer $KEY" -H "content-type: $2" --data-binary "$3" "$URL$1"; }
ingest /api/v1/events application/x-ndjson "$(cat "$FIXTURES/logs.ndjson")"
for _ in $(seq 1 150); do
  ls "$WORK"/data/segments/logs/*.seg >/dev/null 2>&1 && break
  sleep 0.1
done
ls "$WORK"/data/segments/logs/*.seg >/dev/null 2>&1 || fail "no logs segment was sealed"
ingest /v1/logs application/json "$(fixture otlp-logs.json)"
ingest /v1/traces application/json "$(fixture otlp-traces.json)"
ingest /v1/metrics application/json "$(fixture otlp-metrics.json)"
DASH=$(admin -d '{"name":"Checkout","description":"restore drill"}' "$URL/api/v1/dashboards" | grep -o '"id":[0-9]*' | head -1 | cut -d: -f2)
admin -o /dev/null -d '{"title":"Errors","kind":"log_count","config":{"query":"level = Error"}}' "$URL/api/v1/dashboards/$DASH/panels"
admin -o /dev/null -d '{"name":"Error spike","query":"level = Error","op":">","threshold":5,"windowSecs":300,"intervalSecs":60,"enabled":false}' "$URL/api/v1/alerts"
admin -o /dev/null -d '{"name":"Inventory warnings","signal":"logs","query":"level = Warning"}' "$URL/api/v1/saved-queries"
verify "$WORK/before.txt"
grep -q '^logs "count":[1-9]' "$WORK/before.txt" || fail "logs were not stored: $(cat "$WORK/before.txt")"
grep -q '^traces "count":[1-9]' "$WORK/before.txt" || fail "traces were not stored"
grep -q '^metrics "count":[1-9]' "$WORK/before.txt" || fail "metrics were not stored"

step "1. Stop Vyrtel (SIGTERM, exit status 0)"
stop

step "2. Back up the whole data directory"
tar -C "$WORK/data" -czf "$WORK/backup.tar.gz" .
# Read the whole listing before matching: with pipefail, `tar | grep -q`
# fails when grep exits early and tar gets a broken pipe.
tar -tzf "$WORK/backup.tar.gz" >"$WORK/backup-list.txt"
grep -q '^\./vyrtel.json$' "$WORK/backup-list.txt" || fail "the backup has no manifest"
grep -q '"inventory"' "$WORK/data/vyrtel.json" || fail "the clean shutdown recorded no inventory"
rm -rf "$WORK/data"   # the original host is gone

step "3. Restore on a clean host and start (no admin password configured)"
mkdir "$WORK/restored"
tar -C "$WORK/restored" -xzf "$WORK/backup.tar.gz"
# The restored ingest key protects ingestion again; the admin password is
# not configured, so the restored user must be used.
start "$WORK/restored"
grep -q 'Admin login' "$WORK/stdout.log" && fail "a new admin password was generated: the restored user was not used"

step "4. Verification queries"
login
verify "$WORK/after.txt"
diff -u "$WORK/before.txt" "$WORK/after.txt" || fail "verification answers differ after restore"
[ "$(admin "$URL/api/v1/system/storage" | grep -o '"quarantined":\[\]' | wc -l | tr -d ' ')" = 3 ] \
  || fail "recovery quarantined files from a stopped backup"
[ "$(status -H "authorization: Bearer $KEY" -H 'content-type: application/json' -d '{"message":"after restore"}' "$URL/api/v1/events")" = 200 ] \
  || fail "the restored API key was refused"
[ "$(status -H 'content-type: application/json' -d '{}' "$URL/api/v1/events")" = 401 ] \
  || fail "ingest without a key was accepted"
[ "$(status "$URL/api/v1/dashboards")" = 401 ] || fail "configuration readable without a login"
echo "verification queries match; key, login and anonymous refusal behave as before"
stop

# Unpack a fresh copy of the backup into $WORK/$1.
unpack() { rm -rf "${WORK:?}/$1"; mkdir "$WORK/$1"; tar -C "$WORK/$1" -xzf "$WORK/backup.tar.gz"; }

# Start the copy in $WORK/$1 and require exit status 1, a message containing
# $2, and every file and directory left as it was.
refused() {
  local dir=$WORK/$1 before code=0
  before=$(cd "$dir" && find . -print | sort)
  "$VYRTEL" --data-dir "$dir" --bind "127.0.0.1:$PORT" >/dev/null 2>"$WORK/refused.log" || code=$?
  [ "$code" = 1 ] || fail "$1: expected exit status 1, got $code"
  grep -qF "$2" "$WORK/refused.log" || fail "$1: unexpected message: $(cat "$WORK/refused.log")"
  [ "$(cd "$dir" && find . -print | sort)" = "$before" ] || fail "$1: the refused directory was changed"
  cat "$WORK/refused.log"
}

step "5. Incomplete backups are refused without changes"
unpack no-metadata
rm "$WORK/no-metadata/metadata/metadata.db"
refused no-metadata 'is incomplete: metadata/metadata.db is missing'

unpack empty-metadata
: >"$WORK/empty-metadata/metadata/metadata.db"
refused empty-metadata 'is incomplete: metadata/metadata.db is empty (it has no schema)'

unpack no-wal-file
WAL=$(cd "$WORK/no-wal-file" && ls wal/logs/*.wal | head -1)
rm "$WORK/no-wal-file/$WAL"
refused no-wal-file "is incomplete: $WAL is missing"

unpack no-segment-file
SEG=$(cd "$WORK/no-segment-file" && ls segments/logs/*.seg | head -1)
rm "$WORK/no-segment-file/$SEG"
refused no-segment-file "is incomplete: $SEG is missing"

step "6. Continuing without the missing file, on purpose"
start "$WORK/no-wal-file" --accept-incomplete-restore
grep -q 'starting without part of the data directory' "$WORK/stderr.log" || fail "the accepted problem was not logged"
stop
start "$WORK/no-wal-file"   # accepted once; later starts need no flag
stop

rm -rf "$WORK"
printf '\nrestore drill passed\n'
