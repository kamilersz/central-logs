#!/usr/bin/env bash
# Source the env file and restart the matching central-logs instance.
# Usage: ./scripts/restart-cl.sh <port>      e.g. ./scripts/restart-cl.sh 8087
set -euo pipefail
PORT="${1:?usage: restart-cl.sh <port>}"
ENV_FILE=".env-${PORT}"
ROOT="${HOME}/central-logs"
BIN="${ROOT}/target/release/central-logs"
DATA_DIR="${ROOT}/data-${PORT}"
LOG="/tmp/cl-${PORT}.log"

# Source the .env file (KEY=VALUE lines only) into the environment.
if [[ -f "${ENV_FILE}" ]]; then
  set -a
  # shellcheck disable=SC1090
  source "${ENV_FILE}"
  set +a
fi

# Stop the existing instance, if any.
pkill -f "${BIN} --http-port ${PORT}" || true
sleep 2

# Match the original launcher's args: simple data dir + no syslog.
nohup env CENTRAL_LOGS_HTTP_API_KEY="${CENTRAL_LOGS_HTTP_API_KEY:-}" \
  "${BIN}" --http-port "${PORT}" \
    --data-dir "${DATA_DIR}" \
    --no-syslog-udp --no-syslog-tcp \
  > "${LOG}" 2>&1 &
disown
sleep 3

# Verify it's up + that the configured key actually works.
HEALTH=$(curl -s --max-time 3 -o /dev/null -w "%{http_code}" "http://127.0.0.1:${PORT}/health" || echo "000")
echo "${PORT} /health → HTTP ${HEALTH}"
if [[ -n "${CENTRAL_LOGS_HTTP_API_KEY:-}" ]]; then
  AUTH=$(curl -s --max-time 3 -o /dev/null -w "%{http_code}" \
    "http://127.0.0.1:${PORT}/v1/api-keys" \
    -H "Authorization: Bearer ${CENTRAL_LOGS_HTTP_API_KEY}" || echo "000")
  echo "${PORT} /v1/api-keys with .env key → HTTP ${AUTH}"
fi
