"""Read-only field-wifi status for the dashboard.

Best-effort nmcli queries (no root needed) that report whether the
selected field-wifi profile is connected on the wifi interface, so the
dashboard's Services view can show the link state and offer a manual
reconnect. The activation path itself lives in
`scripts/apply_wifi_profile.py`; this module only observes.

Pure parsers (`parse_device_status`, `parse_ip4`) are split out so they
are unit-testable without a NetworkManager host.

Author: Thomas Klute"""

from __future__ import annotations

import logging
import shutil
import subprocess

logger = logging.getLogger("aicam.wifi.status")

# The dedicated NetworkManager connection the applier owns. Mirrors
# `scripts/apply_wifi_profile.py:CONNECTION_NAME`.
CONNECTION_NAME = "aicam-field"


def parse_device_status(terse_output: str, interface: str) -> tuple[str, str]:
    """Parse `nmcli -t -f DEVICE,STATE,CONNECTION device status`.

    Returns `(state, connection)` for `interface` (e.g.
    `("connected", "aicam-field")`), or `("unavailable", "")` if the
    interface is not present. Terse output is colon-separated; our
    managed names contain no colons, so a plain split is sufficient.
    """
    for line in terse_output.splitlines():
        parts = line.split(":")
        if len(parts) >= 3 and parts[0] == interface:
            return parts[1], parts[2]
    return "unavailable", ""


def parse_ip4(terse_output: str) -> str | None:
    """Parse the first address from `nmcli -t -f IP4.ADDRESS device show`.

    Lines look like `IP4.ADDRESS[1]:10.0.12.201/16`; returns the bare
    address without the prefix, or None when there is none.
    """
    for line in terse_output.splitlines():
        if line.startswith("IP4.ADDRESS") and ":" in line:
            value = line.split(":", 1)[1].strip()
            addr = value.split("/")[0]
            if addr:
                return addr
    return None


def query_status(interface: str, selected_profile: str | None) -> dict:
    """Report the field-wifi link state. Never raises.

    Shape:
        selected_profile: str | None  (None -> "no profile selected")
        interface:        str
        device_state:     str  (nmcli device state, or a sentinel)
        active_connection: str | None
        ip:               str | None
        connected:        bool  (our managed connection is up)
    """
    status: dict = {
        "selected_profile": selected_profile,
        "interface": interface,
        "device_state": "unknown",
        "active_connection": None,
        "ip": None,
        "connected": False,
    }

    nmcli = shutil.which("nmcli")
    if not nmcli:
        status["device_state"] = "no-networkmanager"
        return status

    try:
        dev = subprocess.run(
            [nmcli, "-t", "-f", "DEVICE,STATE,CONNECTION", "device", "status"],
            capture_output=True,
            text=True,
            check=False,
            timeout=5,
        )
    except (OSError, subprocess.SubprocessError) as e:
        logger.warning("nmcli device status failed: %s", e)
        return status

    state, connection = parse_device_status(dev.stdout, interface)
    status["device_state"] = state
    status["active_connection"] = connection or None
    status["connected"] = state == "connected" and connection == CONNECTION_NAME

    if status["connected"]:
        try:
            show = subprocess.run(
                [nmcli, "-t", "-f", "IP4.ADDRESS", "device", "show", interface],
                capture_output=True,
                text=True,
                check=False,
                timeout=5,
            )
            status["ip"] = parse_ip4(show.stdout)
        except (OSError, subprocess.SubprocessError) as e:
            logger.warning("nmcli device show %s failed: %s", interface, e)

    return status
