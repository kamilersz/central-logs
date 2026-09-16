#!/usr/bin/env bash
# Deploy a central-logs update to EVERY instance.
#
# The four instances are separate per-client data silos that share one binary,
# so they must all be rebuilt and restarted together.
#
# Typical use on the server, after pushing from your machine:
#
#   cd ~/central-logs
#   git pull --ff-only
#   ./scripts/deploy.sh              # build + restart all instances
#   ./scripts/deploy.sh --web        # also rebuild the SPA (when web/ changed)
#   ./scripts/deploy.sh --pull       # do `git pull --ff-only` as part of deploy
#   ./scripts/deploy.sh --rollback   # restore the previous binary + restart
#
# Exit code is non-zero if any instance fails its health check.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

UNITS=(central-logs central-logs-8085 central-logs-8086 central-logs-8087)
PORTS=(8084 8085 8086 8087)
BIN="target/release/central-logs"
PREV="${BIN}.prev"

DO_PULL=0
DO_WEB=0
DO_ROLLBACK=0
for arg in "$@"; do
  case "$arg" in
    --pull)     DO_PULL=1 ;;
    --web)      DO_WEB=1 ;;
    --rollback) DO_ROLLBACK=1 ;;
    -h|--help)  sed -n '2,16p' "$0"; exit 0 ;;
    *)          printf 'unknown flag: %s\n' "$arg" >&2; exit 2 ;;
  esac
done

restart_all() {
  for u in "${UNITS[@]}"; do
    systemctl --user restart "$u"
  done
}

# Poll /health on every port. Returns non-zero if any instance is not 200.
health_all() {
  local i code failed=0
  for i in "${!UNITS[@]}"; do
    sleep 2
    code=$(curl -s -o /dev/null -w '%{http_code}' --max-time 5 \
      "http://127.0.0.1:${PORTS[$i]}/health" 2>/dev/null || true)
    printf '  %-22s :%s /health → %s\n' "${UNITS[$i]}" "${PORTS[$i]}" "${code:-000}"
    if [[ "${code:-000}" != "200" ]]; then
      failed=1
    fi
  done
  return "$failed"
}

if [[ "$DO_ROLLBACK" == 1 ]]; then
  if [[ ! -f "$PREV" ]]; then
    printf 'no %s snapshot to roll back to — nothing has been deployed by this script yet\n' "$PREV" >&2
    exit 1
  fi
  cp -f "$PREV" "$BIN"
  printf 'restored %s → %s; restarting %d instances\n' "$PREV" "$BIN" "${#UNITS[@]}"
  restart_all
  if health_all; then
    echo "rollback OK"
    exit 0
  fi
  echo "rollback health check FAILED" >&2
  exit 1
fi

if [[ "$DO_PULL" == 1 ]]; then
  echo "+ git pull --ff-only"
  git pull --ff-only
fi

if [[ "$DO_WEB" == 1 ]]; then
  echo "+ building SPA (web/) — required when anything under web/ changed"
  ( cd web && npm ci && npm run build )
fi

# Snapshot the current binary BEFORE building, so a bad build or a bad restart
# can be undone with `--rollback`.
if [[ -f "$BIN" ]]; then
  cp -f "$BIN" "$PREV"
  echo "saved rollback binary → $PREV"
fi

echo "+ cargo build --release --locked"
cargo build --release --locked

echo "+ restarting ${#UNITS[@]} instances"
restart_all

if health_all; then
  echo "deploy OK — all ${#UNITS[@]} instances healthy"
else
  echo "WARNING: one or more instances failed the health check." >&2
  echo "Roll back with: ./scripts/deploy.sh --rollback" >&2
  exit 1
fi
