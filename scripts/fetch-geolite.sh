#!/usr/bin/env bash
# Fetch GeoLite2-City.mmdb for web-analytics region/city attribution.
#
# Primary source (no key needed): the wp-statistics auto-updated mirror —
#   https://github.com/wp-statistics/GeoLite2-City
# Alternative: official MaxMind download (needs a free account + license key,
# https://www.maxmind.com/en/geolite2/signup):
#   ./scripts/fetch-geolite.sh --maxmind  (with GEOLITE_ACCOUNT_ID and
#   GEOLITE_LICENSE_KEY set)
#
# Installs into ./geoip/ (git-ignored), matching the `geoip_db_path` example
# in central-logs.toml. Country-only needs no database: behind Cloudflare,
# `CF-IPCountry` already fills `country`. Restart instances afterwards.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
DEST="${ROOT}/geoip"
mkdir -p "$DEST"

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

if [[ "${1:-}" == "--maxmind" ]]; then
  : "${GEOLITE_ACCOUNT_ID:?set GEOLITE_ACCOUNT_ID for --maxmind mode}"
  : "${GEOLITE_LICENSE_KEY:?set GEOLITE_LICENSE_KEY for --maxmind mode}"
  echo "+ downloading GeoLite2-City from MaxMind (account ${GEOLITE_ACCOUNT_ID})"
  curl -fsSL -o "${TMP}/city.tar.gz" \
    "https://download.maxmind.com/geoip/databases/GeoLite2-City/download?suffix=tar.gz" \
    --user "${GEOLITE_ACCOUNT_ID}:${GEOLITE_LICENSE_KEY}"
  tar -xzf "${TMP}/city.tar.gz" -C "$TMP"
  MMDB="$(find "$TMP" -name 'GeoLite2-City.mmdb' | head -1)"
  if [[ -z "$MMDB" ]]; then
    echo "archive did not contain GeoLite2-City.mmdb" >&2
    exit 1
  fi
  cp -f "$MMDB" "${DEST}/GeoLite2-City.mmdb"
else
  echo "+ downloading GeoLite2-City from the wp-statistics mirror (no key needed)"
  curl -fsSL -o "${TMP}/city.mmdb.gz" \
    "https://github.com/wp-statistics/GeoLite2-City/raw/master/GeoLite2-City.mmdb.gz"
  gunzip -f "${TMP}/city.mmdb.gz"
  cp -f "${TMP}/city.mmdb" "${DEST}/GeoLite2-City.mmdb"
fi

ls -la "${DEST}/GeoLite2-City.mmdb"
echo "done — set geoip_db_path = \"${DEST}/GeoLite2-City.mmdb\" (or uncomment it in central-logs.toml) and restart"
