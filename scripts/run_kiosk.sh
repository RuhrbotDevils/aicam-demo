#!/usr/bin/env bash
# Launch AICam UI in Chromium kiosk mode for the integrated 800x480 touchscreen.
#
# Usage:  ./scripts/run_kiosk.sh [URL]
#   URL defaults to http://localhost:8000/?kiosk=1
#
# Prerequisites:
#   - chromium-browser (or chromium) installed
#   - X11 or Wayland display server running
#   - Optional: unclutter (for cursor hiding)

set -euo pipefail

URL="${1:-http://localhost:8000/?kiosk=1}"
API_URL="${URL%%\?*}"  # strip query params for health check
API_URL="${API_URL%/}"  # strip trailing slash

# --- Default DISPLAY for Pi touchscreen ---
export DISPLAY="${DISPLAY:-:0}"

# --- Find Chromium binary ---
CHROMIUM=""
for bin in chromium-browser chromium; do
  if command -v "$bin" &>/dev/null; then
    CHROMIUM="$bin"
    break
  fi
done

if [ -z "$CHROMIUM" ]; then
  echo "ERROR: chromium-browser not found" >&2
  exit 1
fi

# --- Wait for control API to be reachable ---
HEALTH_URL="${API_URL}/api/v1/health"
echo "Waiting for API at ${HEALTH_URL} ..."
API_READY=false
for i in $(seq 1 30); do
  if curl -sf "$HEALTH_URL" >/dev/null 2>&1 || wget -q -O /dev/null "$HEALTH_URL" 2>/dev/null; then
    echo "API ready."
    API_READY=true
    break
  fi
  sleep 1
done
if [ "$API_READY" = false ]; then
  echo "WARNING: API not reachable after 30s, launching browser anyway."
fi

# --- Disable screen blanking ---
if command -v xset &>/dev/null; then
  xset s off -dpms 2>/dev/null || true
fi

# --- Hide cursor ---
if command -v unclutter &>/dev/null; then
  unclutter -idle 0.5 -root &
  UNCLUTTER_PID=$!
  trap 'kill $UNCLUTTER_PID 2>/dev/null' EXIT
fi

# --- Remove stale Chromium profile locks ---
# Cloned SD cards or unclean shutdowns can leave lock files that prevent
# Chromium from starting ("profile appears to be in use by another process").
rm -f ~/.config/chromium/SingletonLock ~/.config/chromium/SingletonSocket ~/.config/chromium/SingletonCookie 2>/dev/null

# --- Launch Chromium in kiosk mode ---
echo "Launching kiosk: ${URL}"
# Detect Wayland vs X11
OZONE_FLAG=""
if [ -n "${WAYLAND_DISPLAY:-}" ]; then
  OZONE_FLAG="--ozone-platform=wayland --enable-features=UseOzonePlatform"
fi

exec "$CHROMIUM" \
  --kiosk \
  --noerrdialogs \
  --disable-infobars \
  --disable-translate \
  --disable-features=TranslateUI \
  --no-first-run \
  --fast \
  --fast-start \
  --disable-pinch \
  --overscroll-history-navigation=0 \
  --password-store=basic \
  --touch-events=enabled \
  $OZONE_FLAG \
  "$URL"
