"""GC-driven recording controller - auto-starts/stops recordings on game state transitions.

Subscribes to telemetry.game_state on ZMQ and controls recording based on
the GameController state machine.

Trigger rules (automatic mode):
- START when the state transitions INITIAL -> READY (the match/half kickoff).
  A goal kickoff (PLAYING -> READY) is NOT from INITIAL, so it never restarts.
- STOP when the state becomes FINISHED ("Finish" on the GC).
- ABORT (stop) if no GC packet arrives for GC_TIMEOUT_S (the GC went away).

When recording_mode is "manual", the controller is passive (no auto actions).

Author: Thomas Klute"""

from __future__ import annotations

import json
import threading
import time
from collections.abc import Callable
from enum import Enum
from pathlib import Path
from typing import Protocol

from apps.bus.subscriber import Subscriber
from apps.logging_config import get_logger
from apps.schemas.game_state import GamePhase, GameStateMessage

logger = get_logger("recording_controller")

GC_TOPIC = "telemetry.game_state"
DEFAULT_TEAMS_PATH = "config/teams.json"

# Abort an auto recording if the GameController feed goes silent this
# long (no telemetry.game_state packet). game_state now publishes on
# every GC packet (a few Hz), so this only trips when the GC is
# genuinely gone, not during a long PLAYING phase.
GC_TIMEOUT_S = 60.0


class MatchPhase(str, Enum):
    """The five recording-relevant phases of a RoboCup match."""

    FIRST_HALF = "first_half"
    SECOND_HALF = "second_half"
    EXTRA_FIRST_HALF = "extra_first_half"
    EXTRA_SECOND_HALF = "extra_second_half"
    PENALTY_SHOOTOUT = "penalty_shootout"


def derive_match_phase(game_phase: int, first_half: bool) -> MatchPhase | None:
    """Map the (game_phase, first_half) pair to a MatchPhase.

    Returns None for TIMEOUT (or any unrecognised game_phase) - those
    are not recording-worthy and the controller must skip them.
    """
    if game_phase == GamePhase.NORMAL:
        return MatchPhase.FIRST_HALF if first_half else MatchPhase.SECOND_HALF
    if game_phase == GamePhase.OVERTIME:
        return MatchPhase.EXTRA_FIRST_HALF if first_half else MatchPhase.EXTRA_SECOND_HALF
    if game_phase == GamePhase.PENALTY_SHOOTOUT:
        return MatchPhase.PENALTY_SHOOTOUT
    return None


class RecordingMediaClient(Protocol):
    """Protocol for the media client used by the recording controller."""

    def start_recording(self, name: str | None = None) -> object: ...
    def stop_recording(self) -> object: ...


def _load_team_names(teams_path: str = DEFAULT_TEAMS_PATH) -> dict[int, str]:
    """Load team number → name mapping from config/teams.json.

    Accepts both schema forms:

    - legacy plain string:   ``"5": "B-Human"``
    - object with name+colour: ``"5": {"name": "B-Human", "color": "#000000"}``

    The colour (if any) is ignored here; only the name is returned.
    """
    path = Path(teams_path)
    if not path.exists():
        logger.warning("teams_config_missing", path=str(path))
        return {}
    try:
        data = json.loads(path.read_text())
        out: dict[int, str] = {}
        for k, v in data.items():
            try:
                num = int(k)
            except (TypeError, ValueError):
                logger.warning("teams_config_bad_key", key=k)
                continue
            if isinstance(v, str):
                out[num] = v
            elif isinstance(v, dict) and isinstance(v.get("name"), str):
                out[num] = v["name"]
            else:
                logger.warning("teams_config_bad_value", key=k, value=v)
        return out
    except (json.JSONDecodeError, ValueError, OSError) as e:
        logger.warning("teams_config_error", path=str(path), error=str(e))
        return {}


def _build_session_name(
    team1_number: int,
    team2_number: int,
    phase: MatchPhase,
    team_names: dict[int, str],
) -> str:
    """Build auto-recording session name: <team1>_vs_<team2>_<phase>.

    Team names are looked up in team_names dict; falls back to the number
    if missing. The media service prepends an ISO8601 timestamp to this
    name when it creates the session directory, so the on-disk layout is
    `recordings/<timestamp>_<team1>_vs_<team2>_<phase>/`.
    """
    t1 = team_names.get(team1_number, str(team1_number))
    t2 = team_names.get(team2_number, str(team2_number))
    name = f"{t1}_vs_{t2}_{phase.value}"
    name = name.replace(" ", "_")
    # Sanitize: only [a-zA-Z0-9_-]
    name = "".join(c for c in name if c.isascii() and (c.isalnum() or c in "_-"))
    return name


class RecordingController:
    """Subscribes to GC game state and controls recording lifecycle.

    The controller checks recording_mode on each event - it is always running
    but only takes action when mode is "automatic".
    """

    def __init__(
        self,
        subscriber: Subscriber,
        media_client: RecordingMediaClient,
        get_recording_mode: Callable[[], str],
        teams_path: str = DEFAULT_TEAMS_PATH,
    ):
        self._sub = subscriber
        self._media = media_client
        self._get_mode = get_recording_mode
        self._teams_path = teams_path
        self._team_names: dict[int, str] = {}
        self._running = False
        self._recording_active = False
        self._events_processed = 0
        # Previous GC state (to detect the INITIAL -> READY start edge)
        # and the monotonic time of the last GC packet (for the
        # GC-silence abort watchdog).
        self._prev_state: str | None = None
        self._last_packet_time: float | None = None

    def _reload_teams(self) -> None:
        """Reload team names from config file."""
        self._team_names = _load_team_names(self._teams_path)

    def _start_recording(self, msg: GameStateMessage) -> None:
        """Start a recording with auto-generated name from GC data."""
        phase = derive_match_phase(msg.game_phase, msg.first_half) or MatchPhase.FIRST_HALF
        name = _build_session_name(
            msg.team1_number,
            msg.team2_number,
            phase,
            self._team_names,
        )
        logger.info("auto_start_recording", name=name)
        try:
            self._media.start_recording(name=name)
            self._recording_active = True
        except Exception as e:
            logger.error("auto_start_failed", error=str(e))

    def _stop_recording(self, *, reason: str = "finished") -> None:
        """Stop the current recording."""
        logger.info("auto_stop_recording", reason=reason)
        try:
            self._media.stop_recording()
            self._recording_active = False
        except Exception as e:
            logger.error("auto_stop_failed", error=str(e))

    def _handle_event(self, msg: GameStateMessage) -> None:
        """Process a game state event.

        START on the INITIAL -> READY transition; STOP on FINISHED. The
        per-packet timestamp feeds the GC-silence watchdog in `run`.
        """
        self._events_processed += 1
        # Heartbeat for the watchdog - updated on every packet, regardless
        # of mode, so a manual->automatic flip mid-feed times correctly.
        self._last_packet_time = time.monotonic()

        state = msg.state.value  # "INITIAL" / "READY" / ... / "FINISHED"
        prev = self._prev_state
        self._prev_state = state

        # Only take recording actions in automatic mode.
        if self._get_mode() != "automatic":
            return

        # START: kickoff of a match/half is the INITIAL -> READY edge.
        # A goal kickoff is PLAYING -> READY, so it does not match.
        if state == "READY" and prev == "INITIAL" and not self._recording_active:
            self._reload_teams()
            self._start_recording(msg)
            return

        # STOP: "Finish" on the GC.
        if state == "FINISHED" and self._recording_active:
            self._stop_recording(reason="finished")

    def _check_watchdog(self) -> None:
        """Abort an auto recording if the GC feed went silent."""
        if not self._recording_active or self._last_packet_time is None:
            return
        silent = time.monotonic() - self._last_packet_time
        if silent > GC_TIMEOUT_S:
            logger.warning("auto_abort_no_gc", silent_s=round(silent, 1))
            self._stop_recording(reason="gc_timeout")
            # Allow a fresh INITIAL -> READY to start a new recording.
            self._prev_state = None

    def run(self, timeout_ms: int = 1000) -> int:
        """Run the controller loop. Returns total events processed."""
        self._running = True
        self._reload_teams()
        logger.info("controller_started")

        while self._running:
            result = self._sub.receive(timeout_ms=timeout_ms)
            if result is not None:
                _topic, payload = result
                try:
                    msg = GameStateMessage.model_validate_json(payload)
                    self._handle_event(msg)
                except Exception as e:
                    logger.warning("event_parse_error", error=str(e))
            # Runs ~1 Hz (the receive timeout) even when no packets arrive,
            # so the GC-silence watchdog still fires.
            self._check_watchdog()

        return self._events_processed

    def stop(self) -> None:
        self._running = False

    @property
    def events_processed(self) -> int:
        return self._events_processed

    @property
    def recording_active(self) -> bool:
        return self._recording_active


def start_controller_thread(
    subscriber: Subscriber,
    media_client: RecordingMediaClient,
    get_recording_mode: Callable[[], str],
    teams_path: str = DEFAULT_TEAMS_PATH,
) -> tuple[RecordingController, threading.Thread]:
    """Start the recording controller in a daemon thread."""
    controller = RecordingController(
        subscriber=subscriber,
        media_client=media_client,
        get_recording_mode=get_recording_mode,
        teams_path=teams_path,
    )
    thread = threading.Thread(target=controller.run, daemon=True, name="recording-controller")
    thread.start()
    return controller, thread
