"""Telemetry source abstraction - live UDP or test mode.

Both modes produce identical ZMQ output so downstream consumers
(broadcast overlay, recording controller) are source-agnostic.

Usage:
    # Live mode (UDP listener on GameController port 3838)
    source = LiveSource(publisher=pub)
    source.run()

    # Test mode (synthetic random GC events at 1 Hz; useful for overlay
    # demos without a real GC on the wire).
    source = TestSource(publisher=pub)
    source.run()

Author: Thomas Klute"""

from __future__ import annotations

import random
import threading
import uuid
from abc import ABC, abstractmethod
from datetime import UTC, datetime
from typing import Any

from apps.bus.publisher import Publisher
from apps.logging_config import get_logger
from apps.telemetry_service.gamecontroller import PENALTY_LABELS, GameControllerListener

logger = get_logger("telemetry_source")


class TelemetrySource(ABC):
    """Abstract base for telemetry data sources."""

    @abstractmethod
    def run(self) -> None:
        """Run the source (blocking)."""

    @abstractmethod
    def stop(self) -> None:
        """Signal the source to stop."""


class LiveSource(TelemetrySource):
    """Live telemetry via UDP listener.

    Wraps GameControllerListener (port 3838) in a background thread.
    The full project also wires a RobotTelemetryListener on port 3939;
    that path is not part of the demo.
    """

    def __init__(
        self,
        publisher: Publisher,
        gc_port: int = 3838,
        session_id: str | None = None,
    ):
        self._gc = GameControllerListener(publisher=publisher, port=gc_port, session_id=session_id)
        self._threads: list[threading.Thread] = []

    def run(self) -> None:
        """Start the GC UDP listener in a background thread, block until stopped."""
        logger.info("starting_live", gc_port=self._gc._port)

        gc_thread = threading.Thread(target=self._gc.run, daemon=True, name="gc-listener")
        self._threads = [gc_thread]

        gc_thread.start()

        # Block until stopped
        try:
            gc_thread.join()
        except KeyboardInterrupt:
            self.stop()

    def stop(self) -> None:
        self._gc.stop()
        for t in self._threads:
            t.join(timeout=3)
        logger.info("stopped_live", gc_packets=self._gc.packets_received)


# Real HSL team numbers used by the test generator so overlays resolve
# names via `config/teams.json`. Kept small and stable so the rendered
# overlay is recognisable across ticks.
_TEST_TEAM_NUMBERS: list[int] = [1, 5, 9, 14, 18, 24, 33, 48, 50, 60]


def _generate_test_game_state(session_id: str) -> dict[str, Any]:
    """Produce a single synthetic GameStateMessage dict for one tick.

    Built against the HSL v20 schema. Each tick is independent random
    data, no state-machine.

    Weighted toward gameState=PLAYING (3) so the overlay spends most
    of its time in the steady-state rendering path.
    """

    # Game state - heavily weighted toward PLAYING so the overlay
    # mostly shows the steady-state scoreboard. The other four states
    # rotate through to exercise their respective overlay branches.
    state_value = random.choices(
        [0, 1, 2, 3, 4],
        weights=[1, 1, 1, 6, 1],  # INITIAL READY SET PLAYING FINISHED
    )[0]
    state_name = ["INITIAL", "READY", "SET", "PLAYING", "FINISHED"][state_value]

    set_play_value = random.randint(0, 6)
    set_play_name = [
        "NONE",
        "DIRECT_FREE_KICK",
        "INDIRECT_FREE_KICK",
        "PENALTY_KICK",
        "THROW_IN",
        "GOAL_KICK",
        "CORNER_KICK",
    ][set_play_value]

    # Pick two distinct teams so the scoreboard always shows two
    # different home/away. random.sample guarantees uniqueness.
    team1_number, team2_number = random.sample(_TEST_TEAM_NUMBERS, 2)

    # Mostly NORMAL phase; occasional OVERTIME / PENALTY_SHOOTOUT for
    # overlay coverage of the conditional rendering paths.
    game_phase = random.choices(
        [0, 1, 2, 3],
        weights=[12, 2, 2, 1],  # NORMAL PENALTY_SHOOTOUT OVERTIME TIMEOUT
    )[0]
    is_shootout = game_phase == 1

    # secondaryTime is active during SET-with-setPlay and FINISHED
    # (half-time countdown). Otherwise zero.
    if (state_value == 2 and set_play_value != 0) or state_value == 4:
        secondary_time = random.randint(0, 30)
    else:
        secondary_time = 0

    # Shoot-out fields only mean something during PENALTY_SHOOTOUT;
    # outside that phase the overlay reads them and falls back to the
    # row-1 packet-count layout.
    if is_shootout:
        t1_shots = random.randint(1, 5)
        t2_shots = random.randint(1, 5)
        t1_mask = random.randint(0, (1 << t1_shots) - 1)
        t2_mask = random.randint(0, (1 << t2_shots) - 1)
    else:
        t1_shots = t2_shots = 0
        t1_mask = t2_mask = 0

    # GC stopped: 1 in ~8 ticks renders the paused-clock state.
    stopped = random.random() < 0.12

    # kickingTeam wins on one team number or 255 ("no kicker"), so
    # the overlay's kicking_team-NONE suppression branch also gets
    # exercised some of the time.
    kicking_team = random.choice([team1_number, team2_number, 255])

    return {
        "schema_version": "1.0",
        "message_type": "game_state",
        "message_id": f"gc-test-{uuid.uuid4().hex[:12]}",
        "session_id": session_id,
        "source_module": "gc_test_source",
        "created_at": datetime.now(UTC).isoformat(),
        "state": state_name,
        "state_value": state_value,
        "set_play": set_play_name,
        "first_half": random.choice([True, False]),
        "kicking_team": kicking_team,
        "secs_remaining": random.randint(-10, 600),
        "game_phase": game_phase,
        "packet_number": random.randint(0, 255),
        "competition_type": "SMALL",
        "stopped": stopped,
        "secondary_time": secondary_time,
        "team1_number": team1_number,
        "team1_score": random.randint(0, 12),
        "team2_number": team2_number,
        "team2_score": random.randint(0, 12),
        "team1_message_budget": random.randint(0, 1200),
        "team2_message_budget": random.randint(0, 1200),
        "team1_field_player_colour": random.randint(0, 9),
        "team1_goalkeeper_colour": random.randint(0, 9),
        "team1_goalkeeper": random.randint(1, 7),
        "team1_penalty_shot": t1_shots,
        "team1_single_shots": t1_mask,
        "team2_field_player_colour": random.randint(0, 9),
        "team2_goalkeeper_colour": random.randint(0, 9),
        "team2_goalkeeper": random.randint(1, 7),
        "team2_penalty_shot": t2_shots,
        "team2_single_shots": t2_mask,
    }


def _generate_test_penalties(
    session_id: str, team1_number: int, team2_number: int, players_per_team: int = 7
) -> dict[str, Any]:
    """Produce a synthetic PenaltiesMessage dict for one tick.

    Each robot's penalty is heavily weighted toward "No Penalty" (0)
    so a card stack of 0-3 entries is typical - matching what a
    real GC broadcast looks like during normal play. HSL v20 penalty
    enum (0..13) is used; codes 11-13 (Cautioned / Sent Off /
    Substitute) are drawn occasionally so the overlay's penalty
    rendering of those edge cases gets exercised too.
    """

    def one_team(team_number: int) -> list[dict[str, Any]]:
        out: list[dict[str, Any]] = []
        for player_number in range(1, players_per_team + 1):
            # 11 zeros + every code 1..13 once → P(penalty=0) ≈ 0.46.
            code = random.choices(
                [0] + list(range(1, 14)),
                weights=[11] + [1] * 13,
            )[0]
            secs = random.randint(0, 90) if code != 0 else 0
            out.append(
                {
                    "team_number": team_number,
                    "player_number": player_number,
                    "penalty_code": code,
                    "penalty_reason": PENALTY_LABELS.get(code, f"code={code}"),
                    "secs_remaining": secs,
                    "cautions": random.randint(0, 3),
                }
            )
        return out

    return {
        "schema_version": "1.0",
        "message_type": "penalties",
        "message_id": f"pen-test-{uuid.uuid4().hex[:12]}",
        "session_id": session_id,
        "source_module": "gc_test_source",
        "created_at": datetime.now(UTC).isoformat(),
        "team1_number": team1_number,
        "team2_number": team2_number,
        "team1_penalties": one_team(team1_number),
        "team2_penalties": one_team(team2_number),
    }


class TestSource(TelemetrySource):
    """Synthetic GameController test source - random events at ~1 Hz.

    Publishes on ``telemetry.game_state`` and ``telemetry.penalties`` so
    the broadcast overlay renders without a real GameController on the
    wire.

    Intended for demos, overlay parity checks, and box deployments
    that don't have GC traffic available. NOT a replacement for a
    live GC feed - there is no game-state machine, every tick is
    independent random data.
    """

    # Tell pytest not to treat this as a test class.
    __test__ = False

    def __init__(
        self,
        publisher: Publisher,
        period_s: float = 1.0,
        session_id: str | None = None,
    ):
        from apps.schemas.game_state import GameStateMessage
        from apps.schemas.penalties import PenaltiesMessage

        self._publisher = publisher
        self._period_s = period_s
        self._session_id = session_id or f"gc-test-{uuid.uuid4().hex[:8]}"
        self._stop_event = threading.Event()
        self._messages_published = 0
        self._game_state_cls = GameStateMessage
        self._penalties_cls = PenaltiesMessage

    def run(self) -> None:
        logger.info(
            "starting_test_source",
            period_s=self._period_s,
            session_id=self._session_id,
        )
        while not self._stop_event.is_set():
            try:
                gs = _generate_test_game_state(self._session_id)
                pen = _generate_test_penalties(
                    self._session_id, gs["team1_number"], gs["team2_number"]
                )
                self._publisher.send(
                    "telemetry.game_state", self._game_state_cls.model_validate(gs)
                )
                self._publisher.send("telemetry.penalties", self._penalties_cls.model_validate(pen))
                self._messages_published += 2
            except Exception as e:  # noqa: BLE001 - keep ticking on bus glitches
                logger.warning("test_source_publish_error", error=str(e))
            # Wake on stop_event for prompt shutdown.
            self._stop_event.wait(self._period_s)
        logger.info("stopped_test_source", messages_published=self._messages_published)

    def stop(self) -> None:
        self._stop_event.set()

    @property
    def messages_published(self) -> int:
        return self._messages_published


def create_source(
    publisher: Publisher,
    mode: str = "live",
    session_id: str | None = None,
    **kwargs: int,
) -> TelemetrySource:
    """Factory: create a telemetry source by mode name.

    Args:
        mode: "live" for UDP listener, "test" for synthetic random GC
            events at 1 Hz.
    """
    if mode == "live":
        return LiveSource(publisher=publisher, session_id=session_id, **kwargs)
    elif mode == "test":
        return TestSource(publisher=publisher, session_id=session_id)
    else:
        raise ValueError(f"Unknown telemetry mode: {mode}")
