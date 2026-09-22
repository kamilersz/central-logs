#!/usr/bin/env bash
# One full stress pass against the :8088 runtime: ramp-up then bulk,
# with the monitor sampling in the background. Writes CSVs to ./results/.
# Usage: run_stress.sh <label> [duration_s] [max_workers]
set -eu
cd "$(dirname "$0")"
LABEL="${1:-baseline}"
DUR="${2:-180}"
WMAX="${3:-64}"
KEY=$(grep -oP '(?<=CENTRAL_LOGS_HTTP_API_KEY=).*' ../../.env-8088)
BASE=http://127.0.0.1:8088
mkdir -p results

PID=$(pgrep -f "central-logs --http-port 8088" | head -1)
[ -z "$PID" ] && { echo "8088 runtime not running"; exit 1; }
echo "== stress pass '$LABEL' (pid $PID, dur ${DUR}s, workers $WMAX) =="

./monitor.sh "$PID" 1 "results/monitor_$LABEL.csv" "$BASE" "$KEY" &
MON=$!
trap 'kill $MON 2>/dev/null || true' EXIT
sleep 2

drain_wait() {
    for i in $(seq 1 300); do
        LAG=$(curl -s --max-time 2 -H "Authorization: Bearer $KEY" "$BASE/metrics" \
            | awk '/^central_logs_ingest_lag_bytes/{printf "%.1f", $2/1048576}')
        [ -n "$LAG" ] && [ "${LAG%%.*}" -lt 1 ] 2>/dev/null && { echo "lag drained (<1MB) after ${i}s"; return; }
        sleep 1
    done
    echo "lag still high after 300s"
}

python3 loadgen.py ramp --url "$BASE" --key "$KEY" --duration "$DUR" \
    --workers "$WMAX" --ramp-start 8 --ramp-step 8 --ramp-interval 20 \
    --batch-size 5 --out "results/ramp_$LABEL.csv"
echo "-- draining ingest lag --"
T_DRAIN0=$(date +%s)
drain_wait
echo "drain took $(( $(date +%s) - T_DRAIN0 ))s"
curl -s -H "Authorization: Bearer $KEY" "$BASE/metrics" > "results/metrics_$LABEL.txt"

python3 loadgen.py bulk --url "$BASE" --key "$KEY" \
    --bulk-requests 20 --bulk-rows 5000 --bulk-concurrency 4
T_DRAIN0=$(date +%s)
drain_wait
echo "bulk drain took $(( $(date +%s) - T_DRAIN0 ))s"
curl -s -H "Authorization: Bearer $KEY" "$BASE/metrics" > "results/metrics_${LABEL}_after_bulk.txt"
kill $MON 2>/dev/null || true
trap - EXIT
echo "== pass '$LABEL' complete =="
