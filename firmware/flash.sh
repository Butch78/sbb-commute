#!/usr/bin/env bash
# Build with secrets from ../.env, flash the ESP32, and open the serial monitor.
set -euo pipefail
cd "$(dirname "$0")"
set -a; . ../.env; set +a
for v in WIFI_SSID WIFI_PASS WORKER_URL WORKER_TOKEN; do
  [ -n "${!v:-}" ] || { echo "$v is empty in ../.env"; exit 1; }
done
. ~/.cache/export-esp.sh
cargo run --release
