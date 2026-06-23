"""GC-driven recording controller - auto-starts/stops recordings on game state transitions.

Subscribes to telemetry.game_state on ZMQ and controls recording based on
GameController state machine.

Segmentation rule: each recording covers exactly one (team1, team2, match_phase)
tuple - first half, second half, extra-time first half, extra-time second half,
or penalty shootout. The controller restarts the recording only when that tuple
changes. PLAYING → READY → SET → PLAYING within the same half (which happens on
every goal kickoff in SPL/HSL) does NOT split the recording.

When recording_mode is "manual", the controller is passive (no auto actions).

Author: Thomas Klute"""

from __future__ import annotations

import json
import threading
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
        self._last_session_key: tuple[int, int, MatchPhase] | None = None

    def _reload_teams(self) -> None:
        """Reload team names from config file."""
        self._team_names = _load_team_names(self._teams_path)

    def _start_recording(self, msg: GameStateMessage, phase: MatchPhase) -> None:
        """Start a recording with auto-generated name from GC data."""
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

    def _stop_recording(self) -> None:
        """Stop the current recording."""
        logger.info("auto_stop_recording")
        try:
            self._media.stop_recording()
            self._recording_active = False
        except Exception as e:
            logger.error("auto_stop_failed", error=str(e))

    def _handle_event(self, msg: GameStateMessage) -> None:
        """Process a game state event.

        Recording is keyed by `(team1, team2, MatchPhase)`. We start a
        new recording only when that key changes; PLAYING → READY → SET
        → PLAYING within the same half (every goal kickoff in SPL/HSL)
        keeps the existing recording running.
        """
        self._events_processed += 1

        # Only act in automatic mode
        if self._get_mode() != "automatic":
            return

        state = msg.state.value  # string like "PLAYING", "FINISHED"

        if state == "FINISHED":
            if self._recording_active:
                self._stop_recording()
            self._last_session_key = None
            return

        if state != "PLAYING":
            # READY / SET / INITIAL: never start, never stop. Recording
            # (if any) keeps rolling through the kickoff handshake.
            return

        new_phase = derive_match_phase(msg.game_phase, msg.first_half)
        if new_phase is None:
            # TIMEOUT (or unrecognised phase) - do not record.
            return

        new_key = (msg.team1_number, msg.team2_number, new_phase)
        if new_key == self._last_session_key:
            # Same half, same teams - already recording, nothing to do.
            return

        # Phase change or new match: stop the previous recording, start fresh.
        # Reload team names so a config edit between halves is picked up.
        self._reload_teams()
        if self._recording_active:
            logger.info(
                "auto_restart_recording",
                old=self._last_session_key[2].value if self._last_session_key else None,
                new=new_phase.value,
            )
            self._stop_recording()
        self._start_recording(msg, new_phase)
        self._last_session_key = new_key

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
