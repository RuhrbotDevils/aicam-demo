#!/usr/bin/env bash
# install_locally.sh - set up the AICam.demo runtime on the host
# this script is run from. Use this when the repository has been
# cloned onto a Pi (or any Debian-based machine) and you want to
# install everything in place - apt deps, Python venv, Rust media
# service binary, Hailo postprocess libs, systemd units, config -
# from a single command.
#
# The deploy path is whatever directory contains this script's repo
# root, so nothing is fixed to /opt/<x>/. Re-running from a
# different checkout simply re-points systemd at that checkout.
#
# Usage:
#   scripts/install_locally.sh [--node-id ID] [--install-rust] [--no-build]
#
# Flags:
#   --node-id ID    Sets node.id in config.yaml (default: hostname)
#   --install-rust  If cargo isn't on PATH, install Rust via rustup
#   --no-build      Skip the cargo build of the media service
#                   (useful if you're iterating on Python only)
#
# Requirements:
#   - sudo / NOPASSWD sudo for apt + systemd writes
#   - Python 3, internet access for apt + pip + (optional) rustup

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

# shellcheck source=scripts/lib/install_steps.sh
. "$SCRIPT_DIR/lib/install_steps.sh"

NODE_ID="$(hostname)"
INSTALL_RUST=0
NO_BUILD=0
while [ $# -gt 0 ]; do
    case "$1" in
        --node-id)      NODE_ID="$2"; shift 2 ;;
        --install-rust) INSTALL_RUST=1; shift ;;
        --no-build)     NO_BUILD=1; shift ;;
        -h|--help)
            sed -n '1,30p' "$0" | sed 's/^# \{0,1\}//'
            exit 0
            ;;
        *) echo "Unknown flag: $1" >&2; exit 1 ;;
    esac
done

DEPLOY_PATH="$REPO_ROOT"
TARGET_USER="$(whoami)"

aicam_refuse_unsafe_install_root "$DEPLOY_PATH" || exit 1

echo "==> Installing AICam.demo at $DEPLOY_PATH"
echo "    user:    $TARGET_USER"
echo "    node id: $NODE_ID"
echo ""

# Need sudo for apt + systemd. Prompt once if we don't already have
# NOPASSWD; the rest of the install just continues.
if ! sudo -n true 2>/dev/null; then
    echo "    Caching sudo credentials (you may be prompted) ..."
    sudo -v
fi

_pass=0
_fail=0
_results=()

# ---------------------------------------------------------------------------
# Step 1 - System packages
# ---------------------------------------------------------------------------
echo "--- Step 1: System packages ---"
if aicam_apt_install_missing; then
    aicam_step_done "System packages installed / present"
else
    aicam_step_fail "apt install failed - check apt sources / Hailo repo"
fi
echo ""

# ---------------------------------------------------------------------------
# Step 2 - Python venv
# ---------------------------------------------------------------------------
echo "--- Step 2: Python venv ---"
if aicam_setup_python_venv "$DEPLOY_PATH"; then
    aicam_step_done "Python venv + deps installed"
else
    aicam_step_fail "Python venv setup failed - check requirements.txt"
fi
echo ""

# ---------------------------------------------------------------------------
# Step 3 - Rust toolchain
# ---------------------------------------------------------------------------
echo "--- Step 3: Rust/cargo ---"
if aicam_have_cargo; then
    aicam_step_skipped "Rust install (cargo already present: $(aicam_cargo_path))"
elif [ "$INSTALL_RUST" -eq 1 ]; then
    if aicam_install_rust_via_rustup; then
        aicam_step_done "Rust installed via rustup"
    else
        aicam_step_fail "Rust install failed"
    fi
else
    echo "    cargo not found. Re-run with --install-rust to install it."
    aicam_step_skipped "Rust install (--install-rust not given)"
fi
echo ""

# ---------------------------------------------------------------------------
# Step 4 - Media service build
# ---------------------------------------------------------------------------
echo "--- Step 4: Media service build ---"
if [ "$NO_BUILD" -eq 1 ]; then
    aicam_step_skipped "Media service build (--no-build)"
elif ! aicam_have_cargo; then
    aicam_step_skipped "Media service build (cargo not available)"
else
    if aicam_build_media_service "$DEPLOY_PATH"; then
        aicam_step_done "Media service release binary built"
    else
        aicam_step_fail "cargo build --release failed"
    fi
fi
echo ""

# ---------------------------------------------------------------------------
# Step 4b - NV12-native overlay plugin
# (before systemd install so the GST_PLUGIN_PATH drop-in is in place
# when the media service is restarted)
# ---------------------------------------------------------------------------
echo "--- Step 4b: NV12-native overlay plugin ---"
if [ "$NO_BUILD" -eq 1 ]; then
    aicam_step_skipped "Overlay plugin build (--no-build)"
elif ! aicam_have_cargo; then
    aicam_step_skipped "Overlay plugin build (cargo not available)"
else
    aicam_build_install_overlay_plugin "$DEPLOY_PATH"
    case "$?" in
        0) aicam_step_done    "broadcast_overlay plugin built + installed (GST_PLUGIN_PATH drop-in)" ;;
        2) aicam_step_skipped "Overlay plugin (crate not present)" ;;
        *) aicam_step_fail    "Overlay plugin build/install failed - streaming falls back to cairo" ;;
    esac
fi
echo ""

# ---------------------------------------------------------------------------
# Step 5 - Hailo postprocess libraries
# ---------------------------------------------------------------------------
echo "--- Step 5: Hailo postprocess libraries ---"
case "$(aicam_build_hailo_postprocess "$DEPLOY_PATH"; echo $?)" in
    0) aicam_step_done    "Hailo postprocess libraries built" ;;
    2) aicam_step_skipped "Hailo postprocess libraries (no Makefile)" ;;
    *) aicam_step_fail    "Hailo postprocess build failed" ;;
esac
echo ""

# ---------------------------------------------------------------------------
# Step 6 - Legacy systemd units
# ---------------------------------------------------------------------------
echo "--- Step 6: Legacy systemd unit cleanup ---"
read -r _legacy_present _legacy_removed < <(aicam_disable_legacy_units)
if [ "${_legacy_present:-0}" -gt 0 ]; then
    aicam_step_done "Legacy unit cleanup ($_legacy_removed of $_legacy_present units removed)"
else
    aicam_step_skipped "Legacy unit cleanup (no pre-demo units present)"
fi
echo ""

# ---------------------------------------------------------------------------
# Step 7 - /opt sidecar symlinks
# ---------------------------------------------------------------------------
echo "--- Step 7: /opt sidecar path symlinks ---"
aicam_setup_opt_symlinks "$DEPLOY_PATH"
aicam_step_done "/opt sidecar symlinks (created or already in place)"
echo ""

# ---------------------------------------------------------------------------
# Step 8 - Systemd unit files
# ---------------------------------------------------------------------------
echo "--- Step 8: Systemd unit files ---"
case "$(aicam_install_systemd_units "$DEPLOY_PATH" "$TARGET_USER"; echo $?)" in
    0) aicam_step_done    "Systemd unit files installed; cpu-detector disabled; main services restarted" ;;
    2) aicam_step_skipped "Systemd units (no templates found)" ;;
    *) aicam_step_fail    "Systemd unit installation failed" ;;
esac
echo ""

# ---------------------------------------------------------------------------
# Step 9 - config.yaml
# ---------------------------------------------------------------------------
echo "--- Step 9: config.yaml ---"
if aicam_bootstrap_config_yaml "$DEPLOY_PATH" "$NODE_ID"; then
    aicam_step_done "config.yaml node.id set to $NODE_ID"
else
    aicam_step_fail "config.yaml bootstrap failed"
fi
echo ""

# ---------------------------------------------------------------------------
# Step 10 - Inbound firewall (needs venv + config.yaml from above)
# ---------------------------------------------------------------------------
echo "--- Step 10: Inbound firewall ---"
if aicam_setup_firewall "$DEPLOY_PATH" "$TARGET_USER"; then
    aicam_step_done "Inbound firewall applied + sudoers drop-in installed"
else
    aicam_step_fail "Firewall setup failed - re-run scripts/apply_firewall_rules.py"
fi
echo ""

# ---------------------------------------------------------------------------
# Step 11 - Kiosk autostart (desktop only; no-op when headless)
# ---------------------------------------------------------------------------
echo "--- Step 11: Kiosk autostart ---"
aicam_setup_kiosk_autostart "$DEPLOY_PATH"
case "$?" in
    0) aicam_step_done    "Kiosk autostart configured (labwc)" ;;
    *) aicam_step_skipped "Kiosk autostart (run_kiosk.sh not found)" ;;
esac
echo ""

# ---------------------------------------------------------------------------
# Step 12 - Desktop shortcut to relaunch the kiosk UI
# ---------------------------------------------------------------------------
echo "--- Step 12: Desktop shortcut ---"
aicam_install_desktop_shortcut "$DEPLOY_PATH"
case "$?" in
    0) aicam_step_done    "Desktop shortcut installed (relaunch kiosk UI)" ;;
    *) aicam_step_skipped "Desktop shortcut (run_kiosk.sh not found)" ;;
esac
echo ""

# ---------------------------------------------------------------------------
# Step 13 - Desktop wallpaper
# ---------------------------------------------------------------------------
echo "--- Step 13: Desktop wallpaper ---"
aicam_set_desktop_wallpaper "$DEPLOY_PATH"
case "$?" in
    0) aicam_step_done    "Desktop wallpaper set" ;;
    *) aicam_step_skipped "Desktop wallpaper (background image not found)" ;;
esac
echo ""

# ---------------------------------------------------------------------------
# Step 14 - Kernel / firmware tuning
# ---------------------------------------------------------------------------
echo "--- Step 14: Kernel / firmware tuning ---"
aicam_kernel_firmware_tuning
aicam_step_done "Kernel/firmware tuning applied (swappiness + USB current)"
echo ""

# ---------------------------------------------------------------------------
# Step 15 - Chromium profile lock cleanup
# ---------------------------------------------------------------------------
echo "--- Step 15: Chromium profile cleanup ---"
aicam_clean_chromium_locks
aicam_step_done "Chromium profile locks cleaned"
echo ""

# ---------------------------------------------------------------------------
# Summary
# ---------------------------------------------------------------------------
aicam_print_summary "Local install results for $NODE_ID"
