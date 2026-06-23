#!/usr/bin/env python3
"""Render + apply the AICam inbound firewall.

Usage:
    apply_firewall_rules.py [--config <path>] [--rule-dir <path>] [--dry-run]

Reads `network.firewall.allowed_ip_ranges` from config.yaml, renders
two iptables-restore-compatible scripts (v4 + v6) via
`apps.control_api.app.firewall`, writes them to
`<rule-dir>/aicam-firewall.v4.rules` and
`<rule-dir>/aicam-firewall.v6.rules`, and atomically applies each
via `iptables-restore -n` / `ip6tables-restore -n`.

Why iptables and not nftables: JetPack 4.6's kernel `4.9.337-tegra`
has no `nf_tables` kernel module at all. The legacy `iptable_filter`
is loaded on both platforms. On Pi 5 / Bookworm the `iptables` CLI is
`iptables-nft` (the transparent translator to nftables), so the same
rendered rules work on both targets.

This is invoked on every deploy so a fresh box gets the configured
firewall, and by `PUT /api/v1/config` (the runtime config-PUT hook in
control_api/main.py) when allowed_ip_ranges changes.

iptables-restore `-n` (no flush) preserves any other operator-
installed tables (NAT rules, ufw chains, etc.); the filter table
replace is itself atomic.

Exit codes:
    0 - applied successfully (or dry-run completed)
    1 - render OK but `iptables-restore` (or v6) failed
    2 - could not read config.yaml
    3 - argv / environment misuse

Author: Thomas Klute"""

from __future__ import annotations

import argparse
import logging
import os
import shutil
import subprocess
import sys
from pathlib import Path

# When invoked from the venv on the box, apps/ is on sys.path
# because the script lives one level above. Allow standalone
# invocation too.
_REPO_ROOT = Path(__file__).resolve().parent.parent
if str(_REPO_ROOT) not in sys.path:
    sys.path.insert(0, str(_REPO_ROOT))

from apps.control_api.app.firewall import render_from_config  # noqa: E402

DEFAULT_CONFIG = _REPO_ROOT / "config.yaml"
DEFAULT_RULE_DIR = Path("/etc/iptables")
V4_RULE_FILE_NAME = "aicam-firewall.v4.rules"
V6_RULE_FILE_NAME = "aicam-firewall.v6.rules"

logger = logging.getLogger("aicam.firewall.apply")


def _read_allowed_ip_ranges(config_path: Path) -> str:
    """Pull `network.firewall.allowed_ip_ranges` from config.yaml.

    Defaults to "*" if the file is unreadable or the field is
    absent - matches the renderer's wildcard-fallback contract so
    a missing config never locks the operator out.
    """
    try:
        import yaml

        with config_path.open("r") as f:
            data = yaml.safe_load(f) or {}
    except (OSError, ImportError) as e:
        logger.warning(
            "Could not read %s (%s) - applying wildcard policy",
            config_path,
            e,
        )
        return "*"
    return ((data.get("network") or {}).get("firewall") or {}).get("allowed_ip_ranges") or "*"


def _apply_restore(rule_path: Path, restore_binary: str) -> int:
    """Run `<restore_binary> -n <rule-path>`.

    `-n` preserves any other tables (nat, mangle, ufw chains, …);
    the `filter` table replace is itself atomic for the chains we
    declare. Returns 0 on success, 1 on failure.
    """
    bin_path = shutil.which(restore_binary)
    if not bin_path:
        logger.error(
            "%s not found on PATH. Install iptables (apt install iptables).",
            restore_binary,
        )
        return 1
    cmd = [bin_path, "-n", str(rule_path)]
    logger.info("Applying firewall: %s", " ".join(cmd))
    try:
        result = subprocess.run(cmd, capture_output=True, text=True, check=False)
    except OSError as e:
        logger.error("Failed to invoke %s: %s", restore_binary, e)
        return 1
    if result.returncode != 0:
        logger.error(
            "%s exited %d. stderr: %s",
            restore_binary,
            result.returncode,
            result.stderr.strip(),
        )
        return 1
    return 0


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--config",
        type=Path,
        default=DEFAULT_CONFIG,
        help="Path to config.yaml. Default: %(default)s",
    )
    parser.add_argument(
        "--rule-dir",
        type=Path,
        default=DEFAULT_RULE_DIR,
        help=(
            "Directory to write the rendered iptables-restore ruleset "
            "files into. Default: %(default)s"
        ),
    )
    parser.add_argument(
        "--dry-run",
        action="store_true",
        help="Render to stdout instead of writing + applying.",
    )
    args = parser.parse_args(argv)

    logging.basicConfig(
        level=os.environ.get("AICAM_LOG_LEVEL", "INFO").upper(),
        format="%(asctime)s %(levelname)s %(name)s: %(message)s",
    )

    if not args.config.exists():
        logger.error("Config file not found: %s", args.config)
        return 2

    raw = _read_allowed_ip_ranges(args.config)
    v4_rules, v6_rules = render_from_config(raw)

    if args.dry_run:
        sys.stdout.write("# ===== IPv4 (iptables-restore) =====\n")
        sys.stdout.write(v4_rules)
        sys.stdout.write("\n# ===== IPv6 (ip6tables-restore) =====\n")
        sys.stdout.write(v6_rules)
        return 0

    args.rule_dir.mkdir(parents=True, exist_ok=True)
    v4_path = args.rule_dir / V4_RULE_FILE_NAME
    v6_path = args.rule_dir / V6_RULE_FILE_NAME
    v4_path.write_text(v4_rules)
    v6_path.write_text(v6_rules)
    logger.info("Wrote v4 ruleset to %s (%d bytes)", v4_path, len(v4_rules))
    logger.info("Wrote v6 ruleset to %s (%d bytes)", v6_path, len(v6_rules))

    rc_v4 = _apply_restore(v4_path, "iptables-restore")
    rc_v6 = _apply_restore(v6_path, "ip6tables-restore")
    if rc_v4 != 0 or rc_v6 != 0:
        logger.error("Firewall apply failed (v4_rc=%d, v6_rc=%d)", rc_v4, rc_v6)
        return 1

    # Snapshot the full post-apply state to the standard
    # `/etc/iptables/rules.v{4,6}` paths so netfilter-persistent
    # re-applies them on boot. `iptables-save` reads from the
    # kernel and includes ALL current tables (filter + any nat /
    # mangle the operator has installed) - so this captures our
    # filter rules alongside anything else.
    _snapshot_for_boot(args.rule_dir / "rules.v4", "iptables-save")
    _snapshot_for_boot(args.rule_dir / "rules.v6", "ip6tables-save")

    logger.info("Firewall applied successfully (v4 + v6); boot-persistence snapshot updated.")
    return 0


def _snapshot_for_boot(out_path: Path, save_binary: str) -> None:
    """Snapshot current iptables state to `out_path` for
    netfilter-persistent boot replay. Best-effort - a failure
    here doesn't fail the apply (the rules ARE live; they just
    won't survive reboot)."""
    bin_path = shutil.which(save_binary)
    if not bin_path:
        logger.warning(
            "%s not found - skipping boot-persistence snapshot (rules will not survive reboot)",
            save_binary,
        )
        return
    try:
        result = subprocess.run([bin_path], capture_output=True, text=True, check=True)
    except (OSError, subprocess.CalledProcessError) as e:
        logger.warning("%s failed (%s) - boot persistence not updated", save_binary, e)
        return
    try:
        out_path.write_text(result.stdout)
        logger.info(
            "Wrote boot-persistence snapshot to %s (%d bytes)", out_path, len(result.stdout)
        )
    except OSError as e:
        logger.warning("Could not write %s (%s) - boot persistence not updated", out_path, e)


if __name__ == "__main__":
    sys.exit(main())
