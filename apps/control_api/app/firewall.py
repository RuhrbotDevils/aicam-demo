"""Renders iptables/ip6tables rulesets for the inbound firewall.

Pure module - no I/O, no iptables calls. The renderer turns a config
string (the operator's `network.firewall.allowed_ip_ranges`) into
two `iptables-restore`-compatible scripts (one for IPv4, one for
IPv6) that:

- Drop all inbound by default (the INPUT chain's default policy).
- Always allow loopback, established/related, ICMP echo, and the
  DHCP / DHCPv6 client port (so the box keeps getting a lease on
  field networks).
- Allow inbound TCP 22 (SSH) and 8000 (control_api web UI) only
  from the configured CIDR allowlist.
- Drop every other inbound TCP/UDP port regardless of source -
  the media service's 8090, the broker's 5559/5560, and so on are
  intentionally loopback-only on the listening side, but the
  firewall enforces the same boundary from the outside.

The `"*"` sentinel (and any malformed config that produces zero
valid CIDRs) is rendered as "allow 22 + 8000 from anywhere", which
avoids the operator-lock-themselves-out failure mode.

iptables is used rather than nftables for portability: legacy
`iptable_filter` ships in the kernel on the supported platforms,
whereas `nf_tables` is not always available. On Pi 5 / Bookworm the
`iptables` CLI is `iptables-nft` (the compatibility wrapper that
translates iptables rules to nftables under the hood), so the same
rendered ruleset works identically on both platforms.

Author: Thomas Klute"""

from __future__ import annotations

import ipaddress
import logging
from dataclasses import dataclass, field
from typing import Final

logger = logging.getLogger("aicam.firewall")

# The two ports the operator allowlist gates. Other inbound ports
# are dropped regardless of source - see the chain rendering below.
SSH_PORT: Final = 22
UI_PORT: Final = 8000

# GameController data protocol (UDP broadcast). Allowed only from the
# camera's directly-connected subnets - never from networks reachable
# via the gateway.
GAMECONTROLLER_PORT: Final = 3838

# Single-word sentinel meaning "allow from any source". Whitespace
# around the asterisk is tolerated by `parse_allowed_ip_ranges` -
# the comparison is post-strip.
WILDCARD: Final = "*"


@dataclass(frozen=True)
class FirewallPolicy:
    """Parsed allowlist split by address family.

    `wildcard=True` means "allow from anywhere" - the v4/v6 lists
    are ignored when this is set. This is the safe fallback for
    malformed config (so the operator can SSH in to fix it).

    `ipv4` and `ipv6` are CIDR strings preserved verbatim from the
    operator's config (with the host bits masked off, so
    `192.168.3.5/24` becomes `192.168.3.0/24`); iptables accepts
    either with-mask or strict-network form, but normalising here
    means the rule file diffs cleanly across config tweaks.
    """

    wildcard: bool = False
    ipv4: list[str] = field(default_factory=list)
    ipv6: list[str] = field(default_factory=list)


def parse_scope_link_subnets(ip_route_output: str) -> list[str]:
    """Extract directly-connected subnets from `ip route` output.

    Input is the text of `ip -o route show scope link` (v4 or v6). Those
    are exactly the on-link networks - the interface subnets the box is
    directly attached to - so anything reachable only via the gateway
    (the default route, routed networks) is excluded by construction.

    Returns normalised, de-duplicated, sorted CIDR strings. Lines for the
    loopback device and any token that is not a valid network are skipped.
    Pure (no I/O) so it is unit-testable without a host network stack.
    """
    subnets: set[str] = set()
    for line in ip_route_output.splitlines():
        fields = line.split()
        if not fields or fields[0] in ("default", "unreachable", "blackhole"):
            continue
        if "dev" in fields:
            dev = fields[fields.index("dev") + 1] if fields.index("dev") + 1 < len(fields) else ""
            if dev == "lo":
                continue
        try:
            net = ipaddress.ip_network(fields[0], strict=False)
        except ValueError:
            continue
        if net.is_loopback:
            continue
        subnets.add(str(net))
    return sorted(subnets)


def parse_allowed_ip_ranges(raw: str) -> FirewallPolicy:
    """Parse the `network.firewall.allowed_ip_ranges` config value.

    Returns a `FirewallPolicy`. The fallback to `wildcard=True` is
    intentional: any path that ends up with zero valid CIDRs would
    otherwise drop all inbound and lock the operator out of the
    box. We log a warning every time the fallback is taken so the
    misconfiguration is visible in the journal.
    """
    stripped = (raw or "").strip()
    if stripped == "" or stripped == WILDCARD:
        return FirewallPolicy(wildcard=True)

    ipv4: list[str] = []
    ipv6: list[str] = []
    for token in stripped.split(","):
        entry = token.strip()
        if not entry:
            continue
        try:
            net = ipaddress.ip_network(entry, strict=False)
        except ValueError as e:
            logger.warning(
                "firewall: dropping invalid CIDR %r from allowed_ip_ranges: %s",
                entry,
                e,
            )
            continue
        # Mask host bits off so the rendered rule normalises to the
        # network form - iptables accepts both shapes, but
        # normalising keeps the rule file diff-stable.
        if isinstance(net, ipaddress.IPv4Network):
            ipv4.append(str(net))
        else:
            ipv6.append(str(net))

    if not ipv4 and not ipv6:
        logger.warning(
            "firewall: allowed_ip_ranges=%r produced zero valid CIDRs - "
            "falling back to wildcard so the operator can SSH in to fix it",
            raw,
        )
        return FirewallPolicy(wildcard=True)

    return FirewallPolicy(wildcard=False, ipv4=ipv4, ipv6=ipv6)


def _render_gamecontroller_block(local: list[str]) -> str:
    """UDP 3838 ACCEPT rules from directly-connected subnets.

    Independent of the SSH/UI allowlist - GameController is always
    scoped to the on-link networks and never to gateway-routed ones.
    """
    rules = [f"-A INPUT -s {cidr} -p udp --dport {GAMECONTROLLER_PORT} -j ACCEPT" for cidr in local]
    if not rules:
        return "# (no directly-connected networks discovered - GameController 3838 not opened)"
    return "\n".join(rules)


def _render_v4(policy: FirewallPolicy, local_v4: list[str]) -> str:
    """Render the IPv4 ruleset as iptables-restore format.

    INPUT chain policy is DROP - only the explicit allow rules let
    traffic through. Safety baseline (loopback, established,
    ICMP echo, DHCP client) is always present. Operator-gated TCP
    rules use either a wildcard accept (no `-s`) or per-CIDR
    `-s <range>` rules; per-CIDR avoids ipset to keep the ruleset
    importable into the most stripped-down iptables build (no
    ipset module needed on the Jetson kernel).

    `local_v4` is the camera's directly-connected IPv4 subnets; the
    GameController port (UDP 3838) is opened to those only.
    """
    gated: list[str] = []
    if policy.wildcard:
        gated.append(f"-A INPUT -p tcp --dport {SSH_PORT} -j ACCEPT")
        gated.append(f"-A INPUT -p tcp --dport {UI_PORT} -j ACCEPT")
    else:
        for cidr in policy.ipv4:
            gated.append(f"-A INPUT -s {cidr} -p tcp --dport {SSH_PORT} -j ACCEPT")
            gated.append(f"-A INPUT -s {cidr} -p tcp --dport {UI_PORT} -j ACCEPT")
    gated_block = "\n".join(gated) if gated else "# (no IPv4 allowlist - SSH/UI dropped from v4)"
    gc_block = _render_gamecontroller_block(local_v4)

    return f"""# AICam inbound firewall (IPv4).
# Generated by apps/control_api/app/firewall.py - do not hand-edit.
# Applied via `iptables-restore -n` (atomic for the filter table).
*filter
:INPUT DROP [0:0]
:FORWARD DROP [0:0]
:OUTPUT ACCEPT [0:0]
# Flush only our INPUT chain. Applied with `iptables-restore --noflush`,
# so this is the one chain we own: it clears prior AICam rules atomically
# (no duplicate accumulation across re-applies) while leaving FORWARD /
# OUTPUT and any other filter chains (Docker, ufw) and the nat table
# untouched.
-F INPUT

# Loopback (127.0.0.1) is always allowed - control_api ↔
# media_service (8090) and ZMQ broker (5559/5560) ride lo.
-A INPUT -i lo -j ACCEPT

# Outbound-initiated return traffic (RTMP push to YouTube, NTP,
# apt update, etc.).
-A INPUT -m conntrack --ctstate ESTABLISHED,RELATED -j ACCEPT

# ICMP from everywhere (ping + diagnostics).
-A INPUT -p icmp -j ACCEPT

# DHCP client (bootpc/68). Replies ride the established rule above.
-A INPUT -p udp --dport 68 -j ACCEPT

# GameController (UDP 3838) from directly-connected networks only.
{gc_block}

# Operator-gated rules - TCP 22 (SSH) + TCP 8000 (web UI) from
# the configured allowlist. Everything else is dropped by the
# chain default policy.
{gated_block}
COMMIT
"""


def _render_v6(policy: FirewallPolicy, local_v6: list[str]) -> str:
    """Render the IPv6 ruleset as ip6tables-restore format.

    Mirrors `_render_v4` but with the IPv6-appropriate ICMPv6
    types (echo + neighbor discovery + router advertisements - ND
    is essential for the v6 stack to resolve link-local neighbors)
    and DHCPv6 client port (UDP 546). `local_v6` is the camera's
    directly-connected IPv6 subnets the GameController port opens to.
    """
    gated: list[str] = []
    if policy.wildcard:
        gated.append(f"-A INPUT -p tcp --dport {SSH_PORT} -j ACCEPT")
        gated.append(f"-A INPUT -p tcp --dport {UI_PORT} -j ACCEPT")
    else:
        for cidr in policy.ipv6:
            gated.append(f"-A INPUT -s {cidr} -p tcp --dport {SSH_PORT} -j ACCEPT")
            gated.append(f"-A INPUT -s {cidr} -p tcp --dport {UI_PORT} -j ACCEPT")
    gated_block = "\n".join(gated) if gated else "# (no IPv6 allowlist - SSH/UI dropped from v6)"
    gc_block = _render_gamecontroller_block(local_v6)

    return f"""# AICam inbound firewall (IPv6).
# Generated by apps/control_api/app/firewall.py - do not hand-edit.
# Applied via `ip6tables-restore -n` (atomic for the filter table).
*filter
:INPUT DROP [0:0]
:FORWARD DROP [0:0]
:OUTPUT ACCEPT [0:0]
# Flush only our INPUT chain; see the IPv4 renderer.
-F INPUT

-A INPUT -i lo -j ACCEPT

-A INPUT -m conntrack --ctstate ESTABLISHED,RELATED -j ACCEPT

# ICMPv6 from everywhere (ping + neighbor discovery + router
# advertisements; ND is essential for the v6 stack).
-A INPUT -p icmpv6 -j ACCEPT

# DHCPv6 client port.
-A INPUT -p udp --dport 546 -j ACCEPT

# GameController (UDP 3838) from directly-connected networks only.
{gc_block}

{gated_block}
COMMIT
"""


def render_ruleset(
    policy: FirewallPolicy,
    local_v4: list[str] | None = None,
    local_v6: list[str] | None = None,
) -> tuple[str, str]:
    """Render the parsed policy as two iptables-restore scripts.

    Returns `(v4_rules, v6_rules)`. The apply script writes each
    to a separate file and pipes them through `iptables-restore
    -n` and `ip6tables-restore -n` respectively. The `-n` flag
    preserves any other operator-installed iptables tables (NAT
    rules etc.); the filter table replace is itself atomic.

    `local_v4` / `local_v6` are the camera's directly-connected
    subnets; GameController (UDP 3838) is opened to those only. They
    default to empty, leaving 3838 closed.
    """
    return (
        _render_v4(policy, local_v4 or []),
        _render_v6(policy, local_v6 or []),
    )


def render_from_config(
    raw: str,
    local_v4: list[str] | None = None,
    local_v6: list[str] | None = None,
) -> tuple[str, str]:
    """Convenience: parse + render in one shot.

    Used by the apply script and the runtime config-PUT hook.
    `local_v4` / `local_v6` carry the directly-connected subnets the
    GameController port opens to.
    """
    return render_ruleset(parse_allowed_ip_ranges(raw), local_v4, local_v6)
