#!/usr/bin/env bash
# status_all.sh - Check status of all AICam services via systemd.
#
# Usage:
#   scripts/status_all.sh

set -euo pipefail

SERVICES=(
    ai-cam-cpu-detector
    ai-cam-control-api
    ai-cam-media
    ai-cam-zmq-broker
)

echo "==> Checking AICam service status..."

echo ""
echo "==> Service status:"
for svc in "${SERVICES[@]}"; do
    status=$(systemctl is-active "${svc}.service" 2>/dev/null || echo "inactive")
    printf "  %-25s %s\n" "$svc" "$status"
done
echo ""
echo "==> Done."
