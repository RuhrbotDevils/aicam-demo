# scripts/lib/install_steps.sh - shared install steps and constants.
#
# Sourced by:
#   - scripts/install_locally.sh    (runs on the target host)
#   - scripts/setup_pi_env.sh       (runs on the dev container, drives
#                                    the install over SSH)
#
# Functions in this file assume they run ON THE TARGET HOST and operate
# on the local filesystem. setup_pi_env.sh wraps each call in SSH; the
# local installer just calls them directly.
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
aicam_setup_model_sidecars() {
    local deploy_path="$1"

    for _sidecar_file in "$deploy_path"/config/models/*.json; do
      aicam_render_sidecar "$_sidecar_file" "$deploy_path"
    done
}

aicam_render_sidecar() {
    local src="$1" deploy_path="$2"
    sed \
        -i "s|$AICAM_OPT_ROOT|$deploy_path|g" \
        "$src"
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

    #echo "    Restarting installed services ..."
    #for _unit in ai-cam-zmq-broker.service \
    #             ai-cam-media.service \
    #             ai-cam-control-api.service; do
    #    sudo systemctl restart "$_unit" 2>/dev/null || true
    #done
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
