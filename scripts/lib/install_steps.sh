# scripts/lib/install_steps.sh - shared install steps and constants.
#
# Sourced by:
#   - scripts/install_locally.sh    (runs on the target host)
#
# Functions in this file assume they run ON THE TARGET HOST and operate
# on the local filesystem.
#
# All functions are idempotent. Most return 0 on success and non-zero
# on failure; a few are pure data emitters (no return value).
#
# Naming:
#   - Public symbols: AICAM_* (constants) or aicam_* (functions).
#   - Internal helpers: _aicam_* (functions only).
#
# Do NOT `set -e` here - callers control their own strictness.

# ---------------------------------------------------------------------------
# Constants
# ---------------------------------------------------------------------------

# Single source of truth for the apt packages every install needs.
# `dkms` and `hailo-h10-all` together pull in the Hailo-10H PCIe driver
# + HailoRT + the system-wide `hailo_platform` Python package.
# `gstreamer1.0-plugins-ugly` ships x264enc - recording falls back to
# a much slower openh264enc without it.
AICAM_SYSTEM_PKGS=(
    build-essential
    pkg-config
    python3-venv
    python3-dev
    libglib2.0-dev
    libgstreamer1.0-dev
    libgstreamer-plugins-base1.0-dev
    gstreamer1.0-plugins-base
    gstreamer1.0-plugins-good
    gstreamer1.0-plugins-bad
    gstreamer1.0-plugins-ugly
    gstreamer1.0-libcamera
    libzmq3-dev
    libcairo2-dev
    dkms
    hailo-h10-all
)

# Pre-demo systemd units that must not coexist with the v1.0 demo
# build. Cleanup is conservative: only these names are touched.
AICAM_LEGACY_UNITS=(
    ai-cam-detector
    ai-cam-tracker
    ai-cam-jersey-color
    ai-cam-posture
    ai-cam-led
    ai-cam-overlay
)

# /opt symlink targets. Many `config/models/*.json` sidecars hardcode
# /opt/robocup-ai-camera/{models,apps/hailo_postprocess}/<file> paths
# (a leftover from the original full-project install layout). Linking
# these from the install root keeps those sidecars resolvable without
# editing every JSON.
AICAM_OPT_ROOT="/opt/robocup-ai-camera"

# Extra GStreamer plugin dir. The NV12-native overlay plugin
# (libaicam_broadcast_overlay.so) is installed here and prepended to
# GST_PLUGIN_PATH via a systemd drop-in so the media service finds it.
AICAM_EXTRA_PLUGINS_DIR="/opt/aicam-extra-plugins"

# ---------------------------------------------------------------------------
# Status tracking helpers
# ---------------------------------------------------------------------------
#
# Callers seed two integers and an array, then use these helpers to
# accumulate per-step results:
#
#     _pass=0; _fail=0; _results=()
#     aicam_step_done   "Python venv installed"
#     aicam_step_skipped "Rust install (already present)"
#     aicam_step_fail   "apt install failed"
#
# At the end:
#     aicam_print_summary
# ---------------------------------------------------------------------------

aicam_step_done() {
    _pass=$((_pass + 1))
    _results+=("  [DONE]    $1")
}

aicam_step_skipped() {
    _results+=("  [SKIPPED] $1")
}

aicam_step_fail() {
    _fail=$((_fail + 1))
    _results+=("  [FAIL]    $1")
}

aicam_print_summary() {
    local label="${1:-Setup results}"
    echo ""
    echo "==> $label"
    for _r in "${_results[@]}"; do
        echo "$_r"
    done
    echo ""
    echo "    Done: $_pass  |  Failed: $_fail"
    echo ""
    if [ "$_fail" -gt 0 ]; then
        echo "RESULT: FAIL"
        return 1
    fi
    echo "RESULT: OK"
    return 0
}

# ---------------------------------------------------------------------------
# apt
# ---------------------------------------------------------------------------

# aicam_apt_install_missing - install any of AICAM_SYSTEM_PKGS that
# aren't already present. Echoes the to-install list. Returns 0 if
# everything is/becomes installed, non-zero if apt-get fails.
aicam_apt_install_missing() {
    local _to_install=()
    for _pkg in "${AICAM_SYSTEM_PKGS[@]}"; do
        if ! dpkg-query -W -f='${Status}' "$_pkg" 2>/dev/null | grep -q 'install ok installed'; then
            _to_install+=("$_pkg")
        fi
    done

    if [ ${#_to_install[@]} -eq 0 ]; then
        echo "    All system packages already installed."
        return 0
    fi

    echo "    Installing ${#_to_install[@]} package(s): ${_to_install[*]}"
    if sudo apt-get update -qq && \
       sudo DEBIAN_FRONTEND=noninteractive apt-get install -y -qq "${_to_install[@]}"; then
        return 0
    fi
    return 1
}

# ---------------------------------------------------------------------------
# Python venv
# ---------------------------------------------------------------------------

# aicam_venv_uses_system_packages PATH - returns 0 if PATH/.venv was
# created with --system-site-packages.
aicam_venv_uses_system_packages() {
    local deploy_path="$1"
    local cfg="$deploy_path/.venv/pyvenv.cfg"
    [ -f "$cfg" ] && grep -q '^include-system-site-packages = true' "$cfg"
}

# aicam_setup_python_venv DEPLOY_PATH - create or recreate the venv with
# --system-site-packages so hailo_platform is importable, then `pip
# install -r requirements.txt`. Returns non-zero on failure.
aicam_setup_python_venv() {
    local deploy_path="$1"
    if [ ! -d "$deploy_path" ]; then
        echo "    Deploy path not found: $deploy_path" >&2
        return 1
    fi

    if [ ! -f "$deploy_path/.venv/bin/activate" ]; then
        echo "    Creating venv (--system-site-packages) ..."
        if ! python3 -m venv --system-site-packages "$deploy_path/.venv"; then
            echo "    Failed to create Python venv" >&2
            return 1
        fi
    elif ! aicam_venv_uses_system_packages "$deploy_path"; then
        echo "    Existing venv lacks --system-site-packages - recreating ..."
        rm -rf "$deploy_path/.venv"
        if ! python3 -m venv --system-site-packages "$deploy_path/.venv"; then
            echo "    Failed to recreate Python venv" >&2
            return 1
        fi
    else
        echo "    Venv already exists with system site-packages."
    fi

    echo "    Installing Python deps ..."
    (
        cd "$deploy_path" && \
        .venv/bin/pip install -q -r requirements.txt
    )
}

# ---------------------------------------------------------------------------
# Rust media service
# ---------------------------------------------------------------------------

# aicam_have_cargo - returns 0 if cargo is on PATH or under ~/.cargo.
aicam_have_cargo() {
    command -v cargo > /dev/null 2>&1 || [ -x "$HOME/.cargo/bin/cargo" ]
}

# aicam_cargo_path - print the cargo executable path.
aicam_cargo_path() {
    if command -v cargo > /dev/null 2>&1; then
        command -v cargo
    elif [ -x "$HOME/.cargo/bin/cargo" ]; then
        echo "$HOME/.cargo/bin/cargo"
    fi
}

# aicam_install_rust_via_rustup - non-interactive rustup install,
# minimal profile, stable toolchain. Returns non-zero on failure.
aicam_install_rust_via_rustup() {
    echo "    Installing Rust via rustup (minimal, stable) ..."
    python3 -c "import urllib.request; urllib.request.urlretrieve('https://sh.rustup.rs', '/tmp/rustup-init.sh')" \
        && bash /tmp/rustup-init.sh -y --profile minimal --default-toolchain stable
    local _rc=$?
    rm -f /tmp/rustup-init.sh
    return $_rc
}

# aicam_build_media_service DEPLOY_PATH - release build of the Rust media
# service. Caller must have ensured cargo is available.
aicam_build_media_service() {
    local deploy_path="$1"
    local cargo
    cargo="$(aicam_cargo_path)"
    if [ -z "$cargo" ]; then
        echo "    cargo not found" >&2
        return 1
    fi
    (
        cd "$deploy_path" && \
        "$cargo" build --release --manifest-path apps/media_service/Cargo.toml
    )
}

# ---------------------------------------------------------------------------
# Hailo postprocess libraries
# ---------------------------------------------------------------------------

aicam_build_hailo_postprocess() {
    local deploy_path="$1"
    local mk="$deploy_path/apps/hailo_postprocess/Makefile"
    if [ ! -f "$mk" ]; then
        echo "    No Hailo postprocess Makefile present at $mk - skipping"
        return 2  # 2 = skipped sentinel; caller decides
    fi
    (cd "$deploy_path/apps/hailo_postprocess" && make)
}

# ---------------------------------------------------------------------------
# Legacy systemd unit cleanup
# ---------------------------------------------------------------------------

# aicam_disable_legacy_units - stop / disable / remove pre-demo units.
# Echoes count_present count_removed.
aicam_disable_legacy_units() {
    local _present=0 _removed=0
    for _legacy in "${AICAM_LEGACY_UNITS[@]}"; do
        if [ -f "/etc/systemd/system/${_legacy}.service" ]; then
            _present=$((_present + 1))
            echo "    Removing legacy unit: ${_legacy}.service" >&2
            sudo systemctl stop    "${_legacy}.service" 2>/dev/null || true
            sudo systemctl disable "${_legacy}.service" 2>/dev/null || true
            sudo rm -f "/etc/systemd/system/${_legacy}.service" \
                && _removed=$((_removed + 1))
        fi
    done
    if [ "$_present" -gt 0 ]; then
        sudo systemctl daemon-reload 2>/dev/null || true
        sudo systemctl reset-failed   2>/dev/null || true
    fi
    echo "$_present $_removed"
}

# ---------------------------------------------------------------------------
# /opt sidecar symlinks
# ---------------------------------------------------------------------------

# aicam_setup_opt_symlinks DEPLOY_PATH - symlink /opt paths to the install
# root if they don't already exist. Pre-existing real dirs / symlinks are
# left intact so an older full-project install can coexist.
aicam_setup_opt_symlinks() {
    local deploy_path="$1"
    _aicam_link_if_absent "$AICAM_OPT_ROOT/models" "$deploy_path/models"
    _aicam_link_if_absent \
        "$AICAM_OPT_ROOT/apps/hailo_postprocess" \
        "$deploy_path/apps/hailo_postprocess"
}

_aicam_link_if_absent() {
    local opt_path="$1"
    local target="$2"
    if [ -e "$opt_path" ]; then
        echo "    /opt path exists, leaving alone: $opt_path"
        return 0
    fi
    sudo mkdir -p "$(dirname "$opt_path")"
    sudo ln -sfn "$target" "$opt_path" \
        && echo "    Symlinked: $opt_path -> $target"
}

# ---------------------------------------------------------------------------
# Systemd unit installation
# ---------------------------------------------------------------------------

# aicam_render_systemd_unit SRC DEPLOY_PATH USER - emit the rendered
# unit file content on stdout. Substitutes {{DEPLOY_PATH}} and
# {{TARGET_USER}} placeholders.
aicam_render_systemd_unit() {
    local src="$1" deploy_path="$2" target_user="$3"
    sed \
        -e "s|{{DEPLOY_PATH}}|$deploy_path|g" \
        -e "s|{{TARGET_USER}}|$target_user|g" \
        "$src"
}

# aicam_install_systemd_units DEPLOY_PATH USER - substitute templates in
# config/systemd/*.service and install to /etc/systemd/system/. Enables
# all units except ai-cam-cpu-detector (which runs on demand per
# runs on demand), then restarts the demo trio in start_all.sh order. Echoes
# a one-line summary.
aicam_install_systemd_units() {
    local deploy_path="$1" target_user="$2"
    local unit_dir="$deploy_path/config/systemd"
    if [ ! -d "$unit_dir" ] || [ -z "$(ls "$unit_dir"/*.service 2>/dev/null)" ]; then
        echo "    No unit templates found at $unit_dir - skipping" >&2
        return 2
    fi

    for _unit_file in "$unit_dir"/*.service; do
        local _unit_name
        _unit_name="$(basename "$_unit_file")"
        echo "    Installing $_unit_name ..."
        local _rendered
        _rendered="$(aicam_render_systemd_unit "$_unit_file" "$deploy_path" "$target_user")"
        if ! echo "$_rendered" | sudo tee "/etc/systemd/system/$_unit_name" > /dev/null; then
            echo "    Failed to install $_unit_name" >&2
            return 1
        fi
    done

    echo "    Reloading systemd daemon ..."
    sudo systemctl daemon-reload || true

    echo "    Enabling services for auto-start on boot ..."
    for _unit_file in "$unit_dir"/*.service; do
        local _unit_name
        _unit_name="$(basename "$_unit_file")"
        if [ "$_unit_name" = "ai-cam-cpu-detector.service" ]; then
            sudo systemctl stop    "$_unit_name" 2>/dev/null || true
            sudo systemctl disable "$_unit_name" 2>/dev/null || true
            continue
        fi
        sudo systemctl enable "$_unit_name" 2>/dev/null || true
    done

    echo "    Restarting installed services ..."
    for _unit in ai-cam-zmq-broker.service \
                 ai-cam-media.service \
                 ai-cam-control-api.service; do
        sudo systemctl restart "$_unit" 2>/dev/null || true
    done
}

# ---------------------------------------------------------------------------
# config.yaml bootstrap
# ---------------------------------------------------------------------------

# aicam_bootstrap_config_yaml DEPLOY_PATH NODE_ID - copy
# config.example.yaml → config.yaml if missing, set node.id.
aicam_bootstrap_config_yaml() {
    local deploy_path="$1" node_id="$2"
    local cfg="$deploy_path/config.yaml"
    local example="$deploy_path/config.example.yaml"

    if [ ! -f "$cfg" ]; then
        if [ ! -f "$example" ]; then
            echo "    Neither config.yaml nor config.example.yaml present at $deploy_path" >&2
            return 1
        fi
        echo "    Creating config.yaml from config.example.yaml ..."
        cp "$example" "$cfg"
    fi
    sed -i "s/^  id: .*/  id: $node_id/" "$cfg"
}

# ---------------------------------------------------------------------------
# NV12-native overlay plugin (libaicam_broadcast_overlay.so)
# ---------------------------------------------------------------------------

# aicam_build_install_overlay_plugin DEPLOY_PATH - build the
# broadcast_overlay crate from its own directory (default features ->
# plugin ON; the media-service build links it with the plugin feature
# OFF, so it produces no GStreamer entry point). Installs the cdylib
# into AICAM_EXTRA_PLUGINS_DIR and drops a systemd snippet that prepends
# that dir to GST_PLUGIN_PATH so the `aicamnv12overlay` element is
# discoverable (the default video.streaming.overlay_renderer). The
# streaming pipeline falls back to cairo when the plugin is absent, so
# this is recoverable rather than fatal. Idempotent.
# Returns 0 on success, 1 on failure, 2 when skipped.
aicam_build_install_overlay_plugin() {
    local deploy_path="$1"
    local crate="$deploy_path/apps/broadcast_overlay"
    local src="$crate/target/release/libaicam_broadcast_overlay.so"
    local dst="$AICAM_EXTRA_PLUGINS_DIR/libaicam_broadcast_overlay.so"
    local cargo
    cargo="$(aicam_cargo_path)"
    [ -n "$cargo" ] || return 2
    [ -f "$crate/Cargo.toml" ] || return 2

    if ! "$cargo" build --release --manifest-path "$crate/Cargo.toml"; then
        return 1
    fi
    [ -f "$src" ] || return 1

    sudo mkdir -p "$AICAM_EXTRA_PLUGINS_DIR"
    sudo install -m 0644 "$src" "$dst"
    sudo mkdir -p /etc/systemd/system/ai-cam-media.service.d
    sudo tee /etc/systemd/system/ai-cam-media.service.d/aicam-plugins.conf > /dev/null <<'OVERRIDE'
# Expose /opt/aicam-extra-plugins on GST_PLUGIN_PATH so
# libaicam_broadcast_overlay.so (aicamnv12overlay) is loadable. The
# distro plugin path is kept alongside so stock plugins still resolve.
[Service]
Environment="GST_PLUGIN_PATH=/opt/aicam-extra-plugins:/usr/lib/aarch64-linux-gnu/gstreamer-1.0"
OVERRIDE
    sudo systemctl daemon-reload 2>/dev/null || true
}

# ---------------------------------------------------------------------------
# Inbound firewall
# ---------------------------------------------------------------------------

# aicam_setup_firewall DEPLOY_PATH USER - install iptables-persistent
# (so the ruleset survives reboot), install the sudoers drop-in that
# lets the control_api user re-apply the firewall on config-PUT without
# a password, then apply the current ruleset rendered from config.yaml.
# Default config (allowed_ip_ranges "*") allows inbound TCP 22 + 8000
# from anywhere and drops all other inbound. Idempotent.
aicam_setup_firewall() {
    local deploy_path="$1" target_user="$2"

    if ! command -v iptables-restore > /dev/null 2>&1; then
        echo "    Installing iptables-persistent ..."
        echo 'iptables-persistent iptables-persistent/autosave_v4 boolean false' | sudo debconf-set-selections
        echo 'iptables-persistent iptables-persistent/autosave_v6 boolean false' | sudo debconf-set-selections
        sudo DEBIAN_FRONTEND=noninteractive apt-get install -y -qq iptables-persistent || return 1
    fi
    sudo systemctl enable netfilter-persistent.service 2>/dev/null || true

    local sudoers_src="$deploy_path/config/sudoers.d/aicam-firewall"
    if [ -f "$sudoers_src" ]; then
        sed -e "s|{{TARGET_USER}}|$target_user|g" \
            -e "s|{{DEPLOY_PATH}}|$deploy_path|g" \
            "$sudoers_src" \
            | sudo tee /etc/sudoers.d/aicam-firewall > /dev/null
        sudo chmod 0440 /etc/sudoers.d/aicam-firewall
    fi

    # Apply via the same Python renderer the control_api uses at runtime,
    # so the deploy-time and runtime rulesets are identical.
    ( cd "$deploy_path" && sudo "$deploy_path/.venv/bin/python3" scripts/apply_firewall_rules.py )
}

# ---------------------------------------------------------------------------
# Desktop / kiosk integration (best-effort; no-ops on a headless box)
# ---------------------------------------------------------------------------

# aicam_setup_kiosk_autostart DEPLOY_PATH - auto-launch the kiosk browser
# on desktop start (Pi OS Bookworm uses labwc). Returns 2 if run_kiosk.sh
# is absent.
aicam_setup_kiosk_autostart() {
    local deploy_path="$1"
    local kiosk="$deploy_path/scripts/run_kiosk.sh"
    [ -f "$kiosk" ] || return 2
    mkdir -p "$HOME/.config/labwc"
    cat > "$HOME/.config/labwc/autostart" <<KIOSKEOF
# AICam kiosk - auto-launch browser on desktop start
$kiosk &
KIOSKEOF
    chmod +x "$HOME/.config/labwc/autostart"
}

# aicam_install_desktop_shortcut DEPLOY_PATH - drop a desktop launcher
# that reopens the kiosk UI if the operator closes the browser. Returns 2
# if run_kiosk.sh is absent.
aicam_install_desktop_shortcut() {
    local deploy_path="$1"
    local kiosk="$deploy_path/scripts/run_kiosk.sh"
    local icon="$deploy_path/assets/favicon.png"
    [ -f "$kiosk" ] || return 2
    mkdir -p "$HOME/Desktop"
    cat > "$HOME/Desktop/aicam-kiosk.desktop" <<DESKTOPEOF
[Desktop Entry]
Type=Application
Version=1.0
Name=AICam
Comment=Reopen the AICam camera UI
Exec=$kiosk
Icon=$icon
Terminal=false
Categories=Network;
DESKTOPEOF
    chmod +x "$HOME/Desktop/aicam-kiosk.desktop"
}

# aicam_set_desktop_wallpaper DEPLOY_PATH - set the AICam background on
# the Pi OS desktop (pcmanfm GTK, with a pcmanfm-qt fallback). Returns 2
# if the background image is absent.
aicam_set_desktop_wallpaper() {
    local deploy_path="$1"
    local bg="$deploy_path/assets/aicam-background.jpg"
    [ -f "$bg" ] || return 2
    local content="[Desktop Entry]
Wallpaper=$bg
WallpaperMode=stretch"
    mkdir -p "$HOME/.config/pcmanfm/default"
    for f in "$HOME"/.config/pcmanfm/default/desktop-items-*.conf; do
        [ -f "$f" ] && printf '%s\n' "$content" > "$f"
    done
    printf '%s\n' "$content" > "$HOME/.config/pcmanfm/default/desktop-items-0.conf"
    mkdir -p "$HOME/.config/pcmanfm-qt/default"
    printf '%s\n' "$content" > "$HOME/.config/pcmanfm-qt/default/desktop-items-0.conf"
}

# ---------------------------------------------------------------------------
# Kernel / firmware tuning + kiosk hygiene
# ---------------------------------------------------------------------------

# aicam_kernel_firmware_tuning - lower vm.swappiness to 10 and enable
# full USB current (usb_max_current_enable=1) in the Pi firmware config,
# which the Hailo-10H needs for its 1.2A draw. The config.txt edit is
# skipped on non-Pi hosts. Idempotent.
aicam_kernel_firmware_tuning() {
    sudo sysctl -w vm.swappiness=10 > /dev/null 2>&1 || true
    echo 'vm.swappiness=10' | sudo tee /etc/sysctl.d/99-aicam-swappiness.conf > /dev/null

    local cfg=/boot/firmware/config.txt
    if [ -f "$cfg" ]; then
        if grep -qE '^usb_max_current_enable=' "$cfg"; then
            sudo sed -i 's/^usb_max_current_enable=.*/usb_max_current_enable=1/' "$cfg"
        else
            echo 'usb_max_current_enable=1' | sudo tee -a "$cfg" > /dev/null
        fi
        echo "    usb_max_current_enable=1 (reboot required to take effect)"
    fi
}

# aicam_clean_chromium_locks - remove stale Chromium singleton locks left
# behind by a cloned SD card, which otherwise block the kiosk browser.
aicam_clean_chromium_locks() {
    rm -f "$HOME"/.config/chromium/SingletonLock \
          "$HOME"/.config/chromium/SingletonSocket \
          "$HOME"/.config/chromium/SingletonCookie 2>/dev/null || true
}

# ---------------------------------------------------------------------------
# Sanity: hostname guard for the local installer
# ---------------------------------------------------------------------------

# aicam_refuse_unsafe_install_root - fail when the install was launched
# from somewhere it absolutely should not be (root, /tmp, /home).
# Returns 0 if the path is safe, non-zero otherwise.
aicam_refuse_unsafe_install_root() {
    local p="$1"
    case "$p" in
        /|/tmp|/tmp/*|/home|/var|/var/*|/usr|/usr/*|/etc|/etc/*)
            echo "Refusing to install into $p - pick a non-system-controlled directory." >&2
            return 1
            ;;
    esac
    if [ ! -f "$p/requirements.txt" ] || [ ! -d "$p/apps/media_service" ]; then
        echo "Path $p does not look like an AICam.demo checkout (missing requirements.txt or apps/media_service)." >&2
        return 1
    fi
    return 0
}
