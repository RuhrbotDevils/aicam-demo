#!/usr/bin/env python3
"""Activate the selected field-wifi profile on the Pi.

Usage:
    apply_wifi_profile.py [--config <path>] [--dry-run]

Reads `network.field_wifi` from config.yaml and reconciles the Pi's
wifi to the `selected_profile`:

- `none` / unset / unknown  -> tear the managed connection down and
  turn the wifi radio off (wifi switched off).
- a named profile           -> (re)create a dedicated NetworkManager
  connection (`aicam-field`) bound to that SSID with a static IPv4
  address, no gateway and no DNS (`ipv4.never-default yes`,
  `ipv6.method disabled`), then bring it up.

The static, gateway-less, DNS-less profile means the camera has no
route to send anything off the field link; the firewall
(`apply_firewall_rules.py`, re-applied at the end of this script)
additionally drops all outbound IP on the wifi interface, so the
camera is receive-only on the field. 802.11 association and the
WPA handshake are link-layer and unaffected, so joining the AP still
works.

Why NetworkManager/nmcli: the Pi (Debian) runs NetworkManager; nmcli
is the supported way to define and activate connections. The wired
control interface (eth0) is untouched.

Exit codes:
    0 - reconciled successfully (or dry-run completed)
    1 - an nmcli command failed
    2 - could not read config.yaml
    3 - argv / environment misuse

Author: Thomas Klute"""

from __future__ import annotations

import argparse
import ipaddress
import logging
import os
import shutil
import subprocess
import sys
from pathlib import Path

_REPO_ROOT = Path(__file__).resolve().parent.parent
DEFAULT_CONFIG = _REPO_ROOT / "config.yaml"
FIREWALL_SCRIPT = _REPO_ROOT / "scripts" / "apply_firewall_rules.py"

# Dedicated NetworkManager connection name we own. Reused (deleted +
# recreated) on every change so there is never more than one AICam
# field connection, regardless of which SSID was selected before.
CONNECTION_NAME = "aicam-field"

logger = logging.getLogger("aicam.wifi.apply")


def netmask_to_prefix(netmask: str) -> int:
    """Convert a dotted-quad netmask (e.g. 255.255.0.0) to a prefix (16)."""
    return ipaddress.IPv4Network(f"0.0.0.0/{netmask}").prefixlen


def build_nmcli_add_args(
    profile: dict, interface: str, con_name: str = CONNECTION_NAME
) -> list[str]:
    """Build the `nmcli` argument list to create the field connection.

    Pure (no I/O) so the command shape is unit-testable. Static IPv4,
    no gateway/DNS, no default route, IPv6 disabled - the link can only
    receive, never route out. PMF is set to optional (2) so the camera
    negotiates Protected Management Frames with APs that advertise them
    (the standard for current WPA2/WPA3 field APs) while still joining
    APs that do not.
    """
    address = profile["address"]
    prefix = netmask_to_prefix(profile["netmask"])
    return [
        "connection",
        "add",
        "type",
        "wifi",
        "ifname",
        interface,
        "con-name",
        con_name,
        "ssid",
        profile["ssid"],
        "wifi-sec.key-mgmt",
        "wpa-psk",
        "wifi-sec.psk",
        profile["password"],
        "wifi-sec.pmf",
        "2",
        "ipv4.method",
        "manual",
        "ipv4.addresses",
        f"{address}/{prefix}",
        "ipv4.never-default",
        "yes",
        "ipv6.method",
        "disabled",
        "connection.autoconnect",
        "yes",
    ]


def _read_field_wifi(config_path: Path) -> dict:
    """Pull `network.field_wifi` from config.yaml (or raise on read error)."""
    import yaml

    with config_path.open("r") as f:
        data = yaml.safe_load(f) or {}
    return (data.get("network") or {}).get("field_wifi") or {}


def _resolve_selected(field_wifi: dict) -> dict | None:
    """Return the selected profile dict, or None for 'wifi off'.

    None when `selected_profile` is unset / "none" / does not match any
    configured profile name.
    """
    selected = field_wifi.get("selected_profile")
    if not selected or str(selected).strip().lower() == "none":
        return None
    for p in field_wifi.get("profiles") or []:
        if isinstance(p, dict) and p.get("name") == selected:
            return p
    logger.warning("selected_profile=%r not found in profiles - treating as wifi off", selected)
    return None


def _run(cmd: list[str], *, dry_run: bool, check: bool = True) -> int:
    """Run a command (or just print it under --dry-run). Returns rc."""
    printable = " ".join(cmd)
    if dry_run:
        sys.stdout.write(f"{printable}\n")
        return 0
    logger.info("run: %s", printable)
    try:
        result = subprocess.run(cmd, capture_output=True, text=True, check=False)
    except OSError as e:
        logger.error("failed to invoke %s: %s", cmd[0], e)
        return 1
    if result.returncode != 0 and check:
        logger.error("`%s` exited %d: %s", printable, result.returncode, result.stderr.strip())
    return result.returncode


def _nmcli_bin() -> str | None:
    return shutil.which("nmcli")


def _reapply_firewall(dry_run: bool) -> None:
    """Re-apply the firewall so the wifi egress lock matches the new state.

    Best-effort: a failure here is logged, not fatal (the firewall can
    be re-applied independently). Skipped cleanly when the script is
    missing (e.g. a partial checkout).
    """
    if not FIREWALL_SCRIPT.exists():
        logger.warning("firewall script %s missing - egress lock not refreshed", FIREWALL_SCRIPT)
        return
    cmd = [sys.executable, str(FIREWALL_SCRIPT)]
    rc = _run(cmd, dry_run=dry_run, check=False)
    if rc != 0:
        logger.warning("firewall re-apply exited %d - re-run apply_firewall_rules.py", rc)


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--config", type=Path, default=DEFAULT_CONFIG, help="Path to config.yaml")
    parser.add_argument(
        "--dry-run",
        action="store_true",
        help="Print the nmcli commands instead of running them.",
    )
    args = parser.parse_args(argv)

    logging.basicConfig(
        level=os.environ.get("AICAM_LOG_LEVEL", "INFO").upper(),
        format="%(asctime)s %(levelname)s %(name)s: %(message)s",
    )

    if not args.config.exists():
        logger.error("Config file not found: %s", args.config)
        return 2
    try:
        field_wifi = _read_field_wifi(args.config)
    except (OSError, ImportError) as e:
        logger.error("Could not read %s: %s", args.config, e)
        return 2

    interface = field_wifi.get("interface") or "wlan0"
    nmcli = _nmcli_bin()
    if not nmcli and not args.dry_run:
        logger.error("nmcli not found - install NetworkManager")
        return 1
    nmcli = nmcli or "nmcli"  # dry-run can still show the intended commands

    profile = _resolve_selected(field_wifi)
    rc = 0

    # Always start from a clean slate: remove any prior managed
    # connection so a profile switch can't leave a stale SSID behind.
    _run([nmcli, "connection", "down", CONNECTION_NAME], dry_run=args.dry_run, check=False)
    _run([nmcli, "connection", "delete", CONNECTION_NAME], dry_run=args.dry_run, check=False)

    if profile is None:
        logger.info("No field profile selected - switching wifi off on %s", interface)
        rc = _run([nmcli, "radio", "wifi", "off"], dry_run=args.dry_run, check=True)
    else:
        logger.info("Activating field profile %r on %s", profile.get("name"), interface)
        _run([nmcli, "radio", "wifi", "on"], dry_run=args.dry_run, check=False)
        add_args = build_nmcli_add_args(profile, interface)
        # Creating/configuring the connection is the part that must
        # succeed; a failure here is a real error.
        rc = _run([nmcli, *add_args], dry_run=args.dry_run, check=True)
        if rc == 0:
            # Bringing it up can fail simply because the AP is out of
            # range / not reachable right now. The connection is created
            # with autoconnect, so it will join when the AP appears -
            # don't fail the apply (and the deploy) over that. The
            # dashboard wifi status reflects the live link state.
            up_rc = _run(
                [nmcli, "connection", "up", CONNECTION_NAME], dry_run=args.dry_run, check=False
            )
            if up_rc != 0:
                logger.warning(
                    "Field wifi %r configured but could not associate now (rc=%d); "
                    "it will autoconnect when the AP is reachable.",
                    profile.get("name"),
                    up_rc,
                )

    # Reconcile the firewall egress lock to the new wifi state.
    _reapply_firewall(args.dry_run)

    if rc != 0:
        logger.error("wifi profile configuration failed (rc=%d)", rc)
        return 1
    logger.info("Field wifi reconciled successfully.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
