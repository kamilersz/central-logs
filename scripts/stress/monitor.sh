#!/usr/bin/env bash
# Sample CPU / RSS / disk IO / WAL + DB growth for a PID, plus scrape /metrics.
# Usage: monitor.sh <pid> <interval_s> <out_csv> [base_url] [api_key]
set -u
PID="$1"; IV="${2:-1}"; OUT="$3"; BASE="${4:-http://127.0.0.1:8088}"; KEY="${5:-}"
AUTH=(); [ -n "$KEY" ] && AUTH=(-H "Authorization: Bearer $KEY")
CLK=$(getconf CLK_TCK)
DATA_DIR="$(dirname "$0")/../../data-8088"

# resolve block device for the data dir
DEV=$(df --output=source "$DATA_DIR" 2>/dev/null | tail -1 | xargs basename)
[ -z "${DEV:-}" ] && DEV=nvme1n1

echo "ts,cpu_pct,rss_mb,threads,disk_read_mb_s,disk_write_mb_s,fs_free_gb,wal_mb,duckdb_mb,records_total,ingest_lag_mb,wal_depth" > "$OUT"

read_prev() { # utime stime sectors_read sectors_written
    local a=($(awk '{print $14, $15}' "/proc/$PID/stat" 2>/dev/null))
    local b=($(awk -v d="$DEV" '$3==d {print $6, $10}' /proc/diskstats))
    echo "${a[0]:-0} ${a[1]:-0} ${b[0]:-0} ${b[1]:-0}"
}

PREV=($(read_prev))
T0=$(date +%s.%N)
while kill -0 "$PID" 2>/dev/null; do
    sleep "$IV"
    NOW=($(read_prev))
    TS=$(date +%s.%N)
    DT=$(awk -v a="$TS" -v b="$T0" 'BEGIN{print a-b}')
    CPU=$(awk -v u="$((NOW[0]-PREV[0]))" -v s="$((NOW[1]-PREV[1]))" \
        -v dt="$DT" -v clk="$CLK" 'BEGIN{printf "%.1f", (u+s)/(clk*dt)*100}')
    RSS=$(awk '/VmRSS/{printf "%.0f", $2/1024}' "/proc/$PID/status" 2>/dev/null)
    THR=$(awk '/^Threads/{print $2}' "/proc/$PID/status" 2>/dev/null)
    R_MB=$(awk -v d="$((NOW[2]-PREV[2]))" -v dt="$DT" 'BEGIN{printf "%.2f", d*512/1048576/dt}')
    W_MB=$(awk -v d="$((NOW[3]-PREV[3]))" -v dt="$DT" 'BEGIN{printf "%.2f", d*512/1048576/dt}')
    FS_FREE_GB=$(df -BG --output=avail "$DATA_DIR" 2>/dev/null | tail -1 | tr -dc '0-9')
    WAL_MB=$(du -sm "$DATA_DIR/wal" 2>/dev/null | cut -f1)
    DUCK_MB=$(du -sm "$DATA_DIR"/central.duckdb 2>/dev/null | cut -f1)
    METRICS=$(curl -s --max-time 2 "${AUTH[@]}" "$BASE/metrics" 2>/dev/null)
    RECS=$(awk '/^central_logs_records_total/{print $2}' <<< "$METRICS")
    LAG=$(awk '/^central_logs_ingest_lag_bytes/{printf "%.1f", $2/1048576}' <<< "$METRICS")
    DEPTH=$(awk '/^central_logs_wal_channel_depth/{print $2}' <<< "$METRICS")
    echo "$TS,$CPU,${RSS:-0},${THR:-0},$R_MB,$W_MB,${FS_FREE_GB:-?},${WAL_MB:-0},${DUCK_MB:-0},${RECS:-0},${LAG:-0},${DEPTH:-0}" >> "$OUT"
    PREV=("${NOW[@]}")
    T0="$TS"
done
