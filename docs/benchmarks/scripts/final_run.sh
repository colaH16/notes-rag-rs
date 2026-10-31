#!/bin/sh
# Final measurement sequence (only after /tmp/rag-rs-ready-for-bench exists):
#   1. snapshot the Rust release binary named by the marker (or $RUST_BINARY)
#   2. wait for a quiet host (no compile processes) up to $QUIET_WAIT seconds
#   3. output parity (Python oracle vs Rust) BEFORE any timing
#   4. rotated timing rounds, 5. summaries (all trials and clean-only)
set -eu
HERE=$(cd "$(dirname "$0")" && pwd)
OUT=$(cd "$HERE/.." && pwd)/raw
BENCH=${BENCH:-/tmp/rag-bench}
PY=${BENCH_VENV:-/tmp/rag-bench-venv}/bin/python
ROUNDS=${ROUNDS:-6}
QUIET_WAIT=${QUIET_WAIT:-600}
MARKER=/tmp/rag-rs-ready-for-bench
[ -e "$MARKER" ] || { echo "marker missing: final measurements not allowed" >&2; exit 3; }
mkdir -p "$OUT" "$BENCH/bin" "$BENCH/results"

if [ -z "${RUST_BINARY:-}" ]; then
  RUST_BINARY=$("$PY" -I - "$MARKER" <<'EOF'
import json, os, sys
data = json.load(open(sys.argv[1]))
found = []
def walk(value, key=''):
    if isinstance(value, dict):
        for k, v in value.items(): walk(v, k)
    elif isinstance(value, list):
        for v in value: walk(v, key)
    elif isinstance(value, str) and value.startswith('/') and os.path.isfile(value) and os.access(value, os.X_OK):
        found.append((('binary' in key.lower() or 'release' in key.lower()), value))
found.sort(reverse=True)
print(found[0][1] if found else '')
EOF
)
fi
[ -n "$RUST_BINARY" ] && [ -x "$RUST_BINARY" ] || { echo "Rust binary not found from marker; set RUST_BINARY" >&2; exit 4; }
cp "$RUST_BINARY" "$BENCH/bin/notes-rag-rs"
sha256sum "$RUST_BINARY" "$BENCH/bin/notes-rag-rs"

waited=0
while ps -eo comm= | grep -Eqx 'rustc|cargo|cc1|cc1plus|ld|ld.lld|clang|gcc|make|go'; do
  [ "$waited" -ge "$QUIET_WAIT" ] && { echo "host still compiling after ${QUIET_WAIT}s; continuing, trials will be flagged"; break; }
  sleep 30; waited=$((waited + 30))
done

cd "$HERE"
"$PY" parity.py --rust-binary "$BENCH/bin/notes-rag-rs" --out "$OUT/parity.json"
"$PY" run_bench.py --rust-binary "$BENCH/bin/notes-rag-rs" --rounds "$ROUNDS" --out "$BENCH/results/bench-raw.json"
cp "$BENCH/results/bench-raw.json" "$OUT/bench-raw.json"
"$PY" summarize.py "$OUT/bench-raw.json" --out "$OUT/summary-all.json"
"$PY" summarize.py "$OUT/bench-raw.json" --clean-only --out "$OUT/summary-clean.json" > /dev/null
echo "final run complete: $OUT"
