#!/usr/bin/env bash
set -euo pipefail

# WS Lag Demo: drives high event rate and polls Prometheus metrics to observe lag drops.
# Usage: bash monitoring/scripts/ws_lag_demo.sh [duration_secs]
# Requires: server is already running. For best effect:
#   export FPN_WS_CHANNEL_CAP=10
#   make server-run
# In another terminal run this script. Optionally connect a WS client:
#   websocat ws://127.0.0.1:${FPN_PORT:-8080}/ws

DURATION="${1:-20}"
PORT="${FPN_PORT:-8080}"
PROM_URL="http://127.0.0.1:${PORT}/metrics"

# Start loader in insert mode to generate many WS events
( 
  echo "Starting loader (insert mode) for ${DURATION}s..." >&2
  FPN_LOAD_MODE=insert RUST_LOG=warn \
  cargo run --manifest-path rust/server/Cargo.toml --release --example loader \
    >/dev/null 2>&1 &
  echo $! > /tmp/fpn_loader_pid
) || true

LOADER_PID="$(cat /tmp/fpn_loader_pid 2>/dev/null || true)"

echo "Polling metrics from ${PROM_URL} (duration=${DURATION}s)" >&2
start_ts=$(date +%s)
while true; do
  now=$(date +%s)
  elapsed=$(( now - start_ts ))
  if [ "$elapsed" -ge "$DURATION" ]; then
    break
  fi
  metrics=$(curl -sf "$PROM_URL" || true)
  cap=$(echo "$metrics" | awk '/^fpn_ws_channel_capacity /{print $2}')
  clients=$(echo "$metrics" | awk '/^fpn_ws_clients /{print $2}')
  lagged=$(echo "$metrics" | awk '/^fpn_ws_lagged_total /{print $2}')
  rejects=$(echo "$metrics" | awk '/^fpn_rejects_total /{print $2}')
  intents=$(echo "$metrics" | awk '/^fpn_intents_total /{print $2}')
  printf "t=%2ss cap=%s clients=%s lagged_total=%s rejects_total=%s intents_total=%s\n" \
    "$elapsed" "${cap:-na}" "${clients:-na}" "${lagged:-0}" "${rejects:-0}" "${intents:-0}"
  sleep 1
done

# Stop loader if still running
if [ -n "${LOADER_PID}" ] && kill -0 "$LOADER_PID" 2>/dev/null; then
  kill "$LOADER_PID" 2>/dev/null || true
fi
rm -f /tmp/fpn_loader_pid 2>/dev/null || true

echo "Done. Consider increasing FPN_WS_CHANNEL_CAP and rerunning to compare lag." >&2
