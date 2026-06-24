#!/usr/bin/env bash
# stop_all.sh - Stop all AICam services via systemd.
#
# Usage:
#   scripts/stop_all.sh

set -euo pipefail

# Reverse order of start_all.sh - stop consumers before producers.
SERVICES=(
    ai-cam-cpu-detector
    ai-cam-control-api
    ai-cam-media
    ai-cam-zmq-broker
)

echo "==> Stopping AICam services..."

for svc in "${SERVICES[@]}"; do
    if sudo systemctl stop "${svc}.service" 2>/dev/null; then
        echo "  Stopped: $svc"
    else
        echo "  Skipped: $svc (not running or not installed)"
    fi
done

echo ""
echo "==> Service status:"
for svc in "${SERVICES[@]}"; do
    status=$(systemctl is-active "${svc}.service" 2>/dev/null || echo "inactive")
    printf "  %-25s %s\n" "$svc" "$status"
done
echo ""
echo "==> Done."
