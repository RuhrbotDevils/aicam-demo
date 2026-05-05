#!/usr/bin/env bash
# start_all.sh - Start (or restart) all AICam services via systemd.
#
# Usage:
#   scripts/start_all.sh          # start all
#   scripts/start_all.sh restart  # restart all

set -euo pipefail

ACTION="${1:-start}"
if [[ "$ACTION" != "start" && "$ACTION" != "restart" ]]; then
    echo "Usage: scripts/start_all.sh [start|restart]"
    exit 1
fi

# ai-cam-cpu-detector is intentionally absent. The CPU detector
# runs on demand from /api/v1/detection/cpu_snap, not as a long-
# running service. The unit is installed but disabled; an operator
# can `systemctl start ai-cam-cpu-detector` manually for the legacy
# long-running mode.
SERVICES=(
    ai-cam-zmq-broker
    ai-cam-media
    ai-cam-control-api
)

echo "==> ${ACTION^}ing AICam services..."

for svc in "${SERVICES[@]}"; do
    if sudo systemctl "$ACTION" "${svc}.service" 2>/dev/null; then
        echo "  ${ACTION^}ed: $svc"
    else
        echo "  Skipped: $svc (condition unmet or not installed)"
    fi
done

sleep 2
echo ""
echo "==> Service status:"
for svc in "${SERVICES[@]}"; do
    status=$(systemctl is-active "${svc}.service" 2>/dev/null || echo "inactive")
    printf "  %-25s %s\n" "$svc" "$status"
done
echo ""
echo "==> Done."
