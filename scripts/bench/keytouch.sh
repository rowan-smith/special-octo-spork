#!/usr/bin/env bash
# API key last-used bookkeeping under many keys and key churn (AUTH-PERF-03):
# start each given Vyrtel binary on a fresh data directory and run `keytouch`
# against it for every cell of
#
#   active keys {few, cap} × churn (create → use → revoke) {on}
#
# RUNS times (default 3), alternating the binaries within each repetition so
# drift affects them alike. Each run records keytouch's summary.json plus
# the server's CPU seconds, peak memory and RSS every 5 s in server.json; then
# scripts/bench/keytouch-report.mjs compares them against the predeclared
# budgets.
#
#   scripts/bench/keytouch.sh BASELINE CANDIDATE
#   node scripts/bench/keytouch-report.mjs "$OUT"
#
# Linux (reads /proc) or Git Bash on Windows (asks PowerShell). Needs curl,
# and target/release/keytouch (cargo build --release -p loadgen).
# Environment: RUNS, DURATION (keytouch --duration), FEW/CAP (active keys),
# PRODUCERS, CHURN (pause between create+use+revoke rounds), CELLS (a regex
# of cells to run), OUT (results directory), KEYTOUCH (the keytouch binary).
set -euo pipefail

[ $# -eq 2 ] || { echo "usage: $0 BASELINE CANDIDATE" >&2; exit 2; }
RUNS=${RUNS:-3}
DURATION=${DURATION:-70s}
FEW=${FEW:-8}
CAP=${CAP:-5000}
PRODUCERS=${PRODUCERS:-16}
CHURN=${CHURN:-2ms}
CELLS=${CELLS:-.}
OUT=${OUT:-target/keytouch}
KEYTOUCH=${KEYTOUCH:-target/release/keytouch}
PASSWORD="keytouch-$RANDOM$RANDOM"
WORK=$(mktemp -d "${TMPDIR:-/tmp}/vyrtel-keytouch.XXXXXX")
trap 'kill -9 $(jobs -p) 2>/dev/null || true' EXIT
mkdir -p "$OUT"

# CPU seconds and peak memory (bytes) of a process, as "cpu peak".
usage() {
  local pid=$1
  if [ -r "/proc/$pid/winpid" ]; then
    powershell.exe -NoProfile -Command \
      "\$p = Get-Process -Id $(cat "/proc/$pid/winpid"); '{0} {1}' -f \$p.TotalProcessorTime.TotalSeconds, \$p.PeakWorkingSet64" |
      tr -d '\r'
  else
    local tck; tck=$(getconf CLK_TCK)
    awk -v t="$tck" '{ printf "%.2f ", ($14 + $15) / t }' "/proc/$pid/stat"
    awk '/^VmHWM:/ { print $2 * 1024 }' "/proc/$pid/status"
  fi
}

# Resident memory (bytes) of a process now.
rss() {
  local pid=$1
  if [ -r "/proc/$pid/winpid" ]; then
    powershell.exe -NoProfile -Command "(Get-Process -Id $(cat "/proc/$pid/winpid")).WorkingSet64" | tr -d '\r'
  else
    awk '/^VmRSS:/ { print $2 * 1024 }' "/proc/$pid/status"
  fi
}

run() {
  local name=$1 binary=$2 cell=$3 keys=$4 churn=$5 rep=$6
  local dir="$OUT/$name/$cell/run$rep" data="$WORK/data-$name-$cell-$rep"
  local port=$((20000 + RANDOM % 20000))
  mkdir -p "$dir"
  VYRTEL_AUTH_ADMIN_PASSWORD=$PASSWORD VYRTEL_ALERTS_ENABLED=false \
    "$binary" --data-dir "$data" --bind "127.0.0.1:$port" --log-level error >"$dir/server.log" 2>&1 &
  local pid=$!
  until curl -fs "http://127.0.0.1:$port/health" >/dev/null 2>&1; do sleep 0.05; done
  # CPU is measured from here, so it includes creating the active keys.
  read -r cpu0 _ < <(usage "$pid")
  # RSS every 5 s as "seconds bytes", for the slope and post-load cleanup.
  (t0=$SECONDS; while kill -0 "$pid" 2>/dev/null; do
     echo "$((SECONDS - t0)) $(rss "$pid")"; sleep 5; done) >"$dir/rss.txt" 2>/dev/null &
  local sampler=$!
  VYRTEL_AUTH_ADMIN_PASSWORD=$PASSWORD "$KEYTOUCH" --target "http://127.0.0.1:$port" --duration "$DURATION" \
    --active-keys "$keys" --concurrency "$PRODUCERS" --churn-interval "$churn" \
    --label "$name-$cell-$rep" --out "$dir" >/dev/null
  read -r cpu1 peak < <(usage "$pid")
  kill "$sampler" 2>/dev/null; wait "$sampler" 2>/dev/null || true
  kill "$pid"; wait "$pid" 2>/dev/null || true
  rm -rf "$data"
  local samples
  samples=$(awk 'NF == 2 && $2 ~ /^[0-9]+$/ { printf "%s[%s,%s]", n++ ? "," : "", $1, $2 }' "$dir/rss.txt")
  printf '{"binary":"%s","cpuSeconds":%s,"peakMemoryBytes":%s,"rssSamples":[%s]}\n' \
    "$binary" "$(awk -v a="$cpu0" -v b="$cpu1" 'BEGIN { printf "%.2f", b - a }')" "$peak" "$samples" >"$dir/server.json"
  echo "keytouch: $name $cell run $rep done" >&2
}

for rep in $(seq 1 "$RUNS"); do
  for keys in "few:$FEW" "cap:$CAP"; do
    cell="${keys%%:*}-churn"
    [[ $cell =~ $CELLS ]] || continue
    run baseline "$1" "$cell" "${keys#*:}" "$CHURN" "$rep"
    run candidate "$2" "$cell" "${keys#*:}" "$CHURN" "$rep"
  done
done
echo "results in $OUT; summarise with: node scripts/bench/keytouch-report.mjs $OUT" >&2
