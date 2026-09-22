#!/usr/bin/env bash
# Burst-drain microbenchmark: send N rows as fast as the WAL accepts them,
# then time how long the DuckDB ingest pipeline takes to drain the lag to 0.
# Usage: burst_drain.sh [rows_per_worker] [workers] [duration_s]
set -eu
cd "$(dirname "$0")"
RPW="${1:-20}"; W="${2:-48}"; DUR="${3:-60}"
KEY=$(grep -oP '(?<=KEY=).*' ../../.env-8088)
BASE=http://127.0.0.1:8088
OUT=/tmp/drain_$(date +%s).csv

(while :; do
    M=$(curl -s -H "Authorization: Bearer $KEY" "$BASE/metrics")
    L=$(awk '/^central_logs_ingest_lag_bytes/{printf "%.1f", $2/1048576}' <<< "$M")
    R=$(awk '/^central_logs_records_total/{print $2}' <<< "$M")
    echo "$(date +%s.%N),${L:-0},${R:-0}"; sleep 1
done > "$OUT") &
SAMPLER=$!
trap 'kill $SAMPLER 2>/dev/null || true' EXIT
sleep 1

python3 loadgen.py ramp --url "$BASE" --key "$KEY" --duration "$DUR" \
    --workers "$W" --ramp-start "$W" --ramp-step 0 --batch-size "$RPW" \
    --out /tmp/burst.csv | tail -1
echo "-- burst done, draining --"
wait $SAMPLER

python3 - "$OUT" <<'EOF'
import csv, sys
rows = [(float(t), float(l), float(r)) for t, l, r in (ln.split(',') for ln in open(sys.argv[1]))]
sent_total = max(r for _, _, r in rows)
pre_burst = next((r for t, l, r in rows if r > 0), 0)
burst = sent_total - pre_burst
peak = max(l for _, l, _ in rows)
nz = [(t, l) for t, l, _ in rows if l > 0]
if not nz:
    print(f"burst={burst:.0f} rows, lag never built (pipeline kept up)")
    raise SystemExit
t0 = nz[0][0]
t_end = next((t for t, l, r in rows if l < 0.5 and r >= sent_total - 1000 and t > t0), t0)
print(f"burst={burst:.0f} rows, peak lag={peak:.0f}MB, "
      f"drain={(t_end - t0):.0f}s -> {burst / max(t_end - t0, 1):.0f} rows/s end-to-end")
EOF