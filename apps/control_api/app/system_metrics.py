"""System metrics - CPU, temperature, memory, disk for the dashboard.

Author: Thomas Klute"""

from __future__ import annotations

import logging
import os
import re
import shutil
import subprocess
from pathlib import Path
from typing import Any

logger = logging.getLogger(__name__)

MONITORED_SERVICES = [
    "ai-cam-media",
    "ai-cam-control-api",
    "ai-cam-zmq-broker",
]


def collect_metrics(recording_dir: str = "recordings/") -> dict[str, Any]:
    """Collect system metrics from /proc and Pi-specific tools.

    Returns a dict suitable for JSON serialization to the dashboard.
    """
    return {
        "cpu": _cpu_usage(),
        "temperature": _temperatures(),
        "memory": _memory(),
        "disk": _disk_usage(recording_dir),
        "rtc_battery": _rtc_battery(),
        "services": _service_statuses(),
    }


# ---------------------------------------------------------------------------
# CPU
# ---------------------------------------------------------------------------

_prev_cpu: dict[str, list[int]] = {}


def _cpu_usage() -> dict[str, Any]:
    """Read /proc/stat and return CPU usage as percentages.

    Returns total and per-core usage. Requires two calls (delta between
    snapshots). First call returns 0% for all cores.
    """
    global _prev_cpu  # noqa: PLW0603
    result: dict[str, Any] = {"total": 0.0, "cores": []}

    try:
        with open("/proc/stat") as f:
            lines = f.readlines()
    except OSError:
        return result

    current: dict[str, list[int]] = {}
    for line in lines:
        if line.startswith("cpu"):
            parts = line.split()
            name = parts[0]  # "cpu" (total) or "cpu0", "cpu1", ...
            values = [int(x) for x in parts[1:8]]  # user, nice, system, idle, iowait, irq, softirq
            current[name] = values

    cores: list[float] = []
    for name in sorted(current.keys()):
        if name == "cpu":
            result["total"] = _calc_usage(name, current[name])
        else:
            cores.append(_calc_usage(name, current[name]))
    result["cores"] = cores

    _prev_cpu = current
    return result


def _calc_usage(name: str, cur: list[int]) -> float:
    """Calculate CPU usage percentage from delta of /proc/stat values."""
    prev = _prev_cpu.get(name)
    if not prev:
        return 0.0
    idle_delta = (cur[3] + cur[4]) - (prev[3] + prev[4])
    total_delta = sum(cur) - sum(prev)
    if total_delta <= 0:
        return 0.0
    return round((1.0 - idle_delta / total_delta) * 100.0, 1)


# ---------------------------------------------------------------------------
# Temperature
# ---------------------------------------------------------------------------


def _temperatures() -> dict[str, float | None]:
    """Read SoC and Hailo temperatures.

    The Hailo-10H exposes two on-chip thermistors (TS0 + TS1). We
    surface both individually and as a `hailo` field set to the max
    of the two - the hotter sensor is what the dashboard should warn
    on, and existing UI consumers read `hailo`.
    """
    ts0, ts1 = _hailo_temp()
    hailo_max: float | None = None
    if ts0 is not None and ts1 is not None:
        hailo_max = max(ts0, ts1)
    elif ts0 is not None:
        hailo_max = ts0
    elif ts1 is not None:
        hailo_max = ts1
    return {
        "soc": _soc_temp(),
        "hailo": hailo_max,
        "hailo_ts0": ts0,
        "hailo_ts1": ts1,
    }


def _soc_temp() -> float | None:
    """Read Pi SoC temperature from thermal zone or vcgencmd."""
    # Try thermal zone first (works in containers too)
    tz = Path("/sys/class/thermal/thermal_zone0/temp")
    if tz.exists():
        try:
            return round(int(tz.read_text().strip()) / 1000.0, 1)
        except (ValueError, OSError):
            pass
    # Fallback: vcgencmd (Pi-specific)
    try:
        out = subprocess.check_output(["vcgencmd", "measure_temp"], timeout=2, text=True)
        m = re.search(r"temp=([\d.]+)", out)
        if m:
            return float(m.group(1))
    except (FileNotFoundError, subprocess.SubprocessError):
        pass
    return None


# Cached result for `_hailo_temp()` so the 1 Hz dashboard tick doesn't
# hammer the runtime. Tuple `(ts0, ts1, monotonic_ts_seconds)`. We
# refresh every `HAILO_TEMP_CACHE_S` seconds.
_hailo_temp_cache: tuple[float | None, float | None, float] = (None, None, 0.0)
HAILO_TEMP_CACHE_S = 5.0


def _hailo_temp() -> tuple[float | None, float | None]:
    """Read Hailo (ts0, ts1) chip temperatures.

    Primary path: HailoRT Python API (`hailo_platform.Device`)
    `device.get_chip_temperature()` returns a struct with
    `ts0_temperature` and `ts1_temperature` fields. This is the
    documented HailoRT path and works while `hailonet` is using the
    chip via the runtime scheduler.

    Fallback: `hailortcli measure-temperature`. We try common absolute
    paths because the systemd unit may not have `/usr/local/bin` on
    its PATH.

    Both paths return None values on failure - Hailo not installed,
    chip unreachable, etc. - and the dashboard will show "n/a".
    """
    import time

    global _hailo_temp_cache  # noqa: PLW0603
    now = time.monotonic()
    cached_ts0, cached_ts1, cached_at = _hailo_temp_cache
    if now - cached_at < HAILO_TEMP_CACHE_S and cached_at > 0.0:
        return cached_ts0, cached_ts1

    ts0, ts1 = _hailo_temp_via_pyhailort()
    if ts0 is None and ts1 is None:
        ts0, ts1 = _hailo_temp_via_cli()

    _hailo_temp_cache = (ts0, ts1, now)
    return ts0, ts1


def _hailo_temp_via_pyhailort() -> tuple[float | None, float | None]:
    """Read chip temperatures using the HailoRT Python API.

    On HailoRT 5.1.1 (Hailo-10H on Pi 5), the documented control
    surface is `device.control.get_chip_temperature()` - NOT
    `device.get_chip_temperature()` directly. The returned
    `TemperatureInfo` exposes `ts0_temperature` and
    `ts1_temperature` (the chip has two on-die thermistors).
    """
    try:
        from hailo_platform import Device  # type: ignore[import-not-found]
    except Exception:  # noqa: BLE001 - module may legitimately be absent
        return None, None
    try:
        # Device() with no args picks the first scanned device.
        # `device.control.get_chip_temperature()` is a control-plane
        # query and does not require exclusive access - it works
        # alongside `hailonet` running inference via the runtime
        # scheduler.
        device = Device()
        try:
            info = device.control.get_chip_temperature()
            ts0 = float(info.ts0_temperature) if info.ts0_temperature is not None else None
            ts1 = float(info.ts1_temperature) if info.ts1_temperature is not None else None
            return ts0, ts1
        finally:
            try:
                device.release()
            except Exception:  # noqa: BLE001
                pass
    except Exception as e:  # noqa: BLE001
        logger.debug("hailo temperature via pyhailort failed: %s", e)
        return None, None


def _hailo_temp_via_cli() -> tuple[float | None, float | None]:
    """Fallback: parse `hailortcli measure-temperature` output."""
    cli = shutil.which("hailortcli") or "/usr/local/bin/hailortcli"
    if not Path(cli).exists():
        return None, None
    try:
        out = subprocess.check_output(
            [cli, "measure-temperature"],
            timeout=3,
            text=True,
            stderr=subprocess.DEVNULL,
        )
    except (FileNotFoundError, subprocess.SubprocessError):
        return None, None
    ts0 = _parse_cli_temp(out, r"TS0\s*Temperature[^\d]*([\d.]+)\s*C")
    ts1 = _parse_cli_temp(out, r"TS1\s*Temperature[^\d]*([\d.]+)\s*C")
    if ts0 is None and ts1 is None:
        # Older hailortcli versions printed a single combined value.
        single = _parse_cli_temp(out, r"([\d.]+)\s*C")
        if single is not None:
            return single, single
    return ts0, ts1


def _parse_cli_temp(text: str, pattern: str) -> float | None:
    m = re.search(pattern, text)
    if not m:
        return None
    try:
        return float(m.group(1))
    except (TypeError, ValueError):
        return None


# ---------------------------------------------------------------------------
# RTC battery
# ---------------------------------------------------------------------------

# Pi 5: PMIC ADC channel BATT_V is the RTC backup battery. With a
# healthy CR2032-class cell installed and the trickle charger
# configured, voltage sits in the 2.9–3.1 V range. Without a battery
# the channel reads close to 0 V; below 3.0 V indicates the cell is
# discharged or the contact is bad. The Pi will still boot and run,
# but the RTC won't keep time across power loss - surface this as a
# warning so an operator notices before the next site visit.
RTC_BATTERY_LOW_V = 3.0
RTC_BATTERY_MISSING_V = 0.5

_rtc_cache: tuple[dict[str, Any], float] = ({}, 0.0)
RTC_CACHE_S = 30.0


def _rtc_battery() -> dict[str, Any]:
    """Read the Pi 5 RTC backup battery voltage from the PMIC.

    Returns a dict with `voltage_v` (float|None) and `state`
    ("ok" | "low" | "missing" | "unknown"). Cached for 30 s - the
    voltage doesn't move on the dashboard timescale.
    """
    import time

    global _rtc_cache  # noqa: PLW0603
    now = time.monotonic()
    cached, cached_at = _rtc_cache
    if cached and now - cached_at < RTC_CACHE_S:
        return cached

    voltage = _vcgencmd_pmic_batt_v()
    if voltage is None:
        result: dict[str, Any] = {"voltage_v": None, "state": "unknown"}
    elif voltage < RTC_BATTERY_MISSING_V:
        result = {"voltage_v": round(voltage, 3), "state": "missing"}
    elif voltage < RTC_BATTERY_LOW_V:
        result = {"voltage_v": round(voltage, 3), "state": "low"}
    else:
        result = {"voltage_v": round(voltage, 3), "state": "ok"}
    _rtc_cache = (result, now)
    return result


def _vcgencmd_pmic_batt_v() -> float | None:
    """Run `vcgencmd pmic_read_adc BATT_V` and parse the voltage.

    Sample output: `BATT_V volt(16)=3.04687500V`
    """
    cmd = shutil.which("vcgencmd")
    if not cmd:
        return None
    try:
        out = subprocess.check_output(
            [cmd, "pmic_read_adc", "BATT_V"],
            timeout=2,
            text=True,
            stderr=subprocess.DEVNULL,
        )
    except (FileNotFoundError, subprocess.SubprocessError):
        return None
    m = re.search(r"BATT_V\s+volt\([^)]*\)=([\d.]+)\s*V", out)
    if not m:
        return None
    try:
        return float(m.group(1))
    except (TypeError, ValueError):
        return None


# ---------------------------------------------------------------------------
# Memory
# ---------------------------------------------------------------------------


def _memory() -> dict[str, Any]:
    """Read memory and swap usage from /proc/meminfo."""
    result: dict[str, Any] = {
        "total_mb": 0,
        "used_mb": 0,
        "percent": 0.0,
        "swap_total_mb": 0,
        "swap_used_mb": 0,
        "swap_percent": 0.0,
    }
    try:
        with open("/proc/meminfo") as f:
            info = {}
            for line in f:
                parts = line.split()
                if len(parts) >= 2:
                    info[parts[0].rstrip(":")] = int(parts[1])
        total = info.get("MemTotal", 0)
        available = info.get("MemAvailable", 0)
        used = total - available
        result["total_mb"] = round(total / 1024)
        result["used_mb"] = round(used / 1024)
        result["percent"] = round(used / total * 100, 1) if total > 0 else 0.0

        swap_total = info.get("SwapTotal", 0)
        swap_free = info.get("SwapFree", 0)
        swap_used = swap_total - swap_free
        result["swap_total_mb"] = round(swap_total / 1024)
        result["swap_used_mb"] = round(swap_used / 1024)
        result["swap_percent"] = round(swap_used / swap_total * 100, 1) if swap_total > 0 else 0.0
    except (OSError, ValueError, ZeroDivisionError):
        pass
    return result


# ---------------------------------------------------------------------------
# Disk
# ---------------------------------------------------------------------------


def _disk_usage(recording_dir: str) -> dict[str, Any]:
    """Check disk usage for the recording directory."""
    result: dict[str, Any] = {
        "total_gb": 0.0,
        "free_gb": 0.0,
        "used_percent": 0.0,
    }
    try:
        # Resolve to absolute path relative to CWD
        path = Path(recording_dir).resolve()
        if not path.exists():
            path = Path(".")
        stat = os.statvfs(str(path))
        total = stat.f_blocks * stat.f_frsize
        free = stat.f_bavail * stat.f_frsize
        used = total - free
        result["total_gb"] = round(total / (1024**3), 1)
        result["free_gb"] = round(free / (1024**3), 1)
        result["used_percent"] = round(used / total * 100, 1) if total > 0 else 0.0
    except (OSError, ZeroDivisionError):
        pass
    return result


# ---------------------------------------------------------------------------
# Systemd service status
# ---------------------------------------------------------------------------


def _service_statuses() -> list[dict[str, str]]:
    """Query systemd for the status of monitored services."""
    results: list[dict[str, str]] = []
    for name in MONITORED_SERVICES:
        unit = f"{name}.service"
        status = _systemctl_status(unit)
        results.append({"name": name, "unit": unit, "status": status})
    return results


def _systemctl_status(unit: str) -> str:
    """Get the ActiveState of a systemd unit.

    Returns one of: "running", "stopped", "failed", "not-found".
    """
    try:
        out = subprocess.check_output(
            ["systemctl", "is-active", unit],
            timeout=2,
            text=True,
            stderr=subprocess.DEVNULL,
        ).strip()
        if out == "active":
            return "running"
        elif out == "failed":
            return "failed"
        elif out == "inactive":
            return "stopped"
        elif out == "activating":
            return "starting"
        else:
            return "stopped"
    except subprocess.CalledProcessError as e:
        # is-active returns exit code 3 for inactive/failed
        out = (e.output or "").strip() if e.output else ""
        if out == "failed":
            return "failed"
        if out == "inactive":
            return "stopped"
        return "stopped"
    except (FileNotFoundError, subprocess.SubprocessError):
        return "not-found"


def restart_service(name: str) -> tuple[bool, str]:
    """Restart a systemd service by name.

    Only allows restarting services in the MONITORED_SERVICES list.
    Returns (success, message).
    """
    if name not in MONITORED_SERVICES:
        return False, f"Unknown service: {name}"

    unit = f"{name}.service"
    try:
        subprocess.check_output(
            ["sudo", "systemctl", "restart", unit],
            timeout=10,
            text=True,
            stderr=subprocess.STDOUT,
        )
        return True, f"Restarted {unit}"
    except subprocess.CalledProcessError as e:
        return False, f"Failed to restart {unit}: {e.output}"
    except (FileNotFoundError, subprocess.SubprocessError) as e:
        return False, f"Failed to restart {unit}: {e}"
