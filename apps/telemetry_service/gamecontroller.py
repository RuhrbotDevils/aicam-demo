"""GameController UDP listener - receives game state broadcasts.

HSL GameController broadcasts RoboCupGameControlData on UDP port 3838.
This listener parses the packets and publishes game state events to ZMQ.

Protocol reference (HSL GameController, struct version 20):
- Broadcast: UDP port 3838 (configurable)
- Packet header: "RGme" (4 bytes) + version (1 byte)
- Source: https://github.com/RoboCup-HumanoidSoccerLeague/GameController
  (authoritative header at game_controller_msgs/headers/RoboCupGameControlData.h)
- Layout summary:
  - Offset 7: competitionType (SMALL=0, MIDDLE=1, LARGE=2)
  - Offset 8: stopped (bool)
  - SetPlay values per SetPlay enum below
  - RobotInfo is 3 bytes (penalty, secsTillUnpenalised, cautions)
  - No STANDBY game state

Author: Thomas Klute"""

from __future__ import annotations

import socket
import struct
import uuid
from datetime import UTC, datetime
from enum import IntEnum

from apps.bus.publisher import Publisher
from apps.logging_config import get_logger

logger = get_logger("gamecontroller")

GC_PORT = 3838
GC_HEADER = b"RGme"
GC_TOPIC = "telemetry.game_state"
GC_PENALTIES_TOPIC = "telemetry.penalties"

# Authoritative HSL wire-format version. Packets with any other
# version are dropped by ``_parse_packet`` - the field layout
# (notably RobotInfo size) is version-specific and silently
# misparsing produces corrupt team2 / penalty data. Bump when
# we adopt a newer GC release and re-validate offsets.
EXPECTED_VERSION = 20


class PenaltyCode(IntEnum):
    """RobotInfo penalty byte values from the HSL v20 spec.

    Source: `RoboCupGameControlData.h` PENALTY_* constants. Display
    labels (`PENALTY_LABELS`) are derived from this enum so the
    overlay shows broadcast-friendly text instead of `code=<n>`.
    """

    NONE = 0
    ILLEGAL_POSITIONING = 1
    MOTION_IN_SET = 2
    MOTION_IN_STOP = 3
    LOCAL_GAME_STUCK = 4
    INCAPABLE_ROBOT = 5
    PICK_UP = 6
    BALL_HOLDING = 7
    LEAVING_THE_FIELD = 8
    PLAYING_WITH_ARMS_HANDS = 9
    PUSHING = 10
    CAUTIONED = 11
    SENT_OFF = 12
    SUBSTITUTE = 13


# Broadcast-friendly labels for each PenaltyCode. Keep in sync with
# the enum above - the spec defines 14 codes (0..13). Unknown codes
# (e.g. a future protocol addition) fall through to "code=<n>" in
# `_decode_penalty_reason` so the raw byte stays visible.
PENALTY_LABELS: dict[int, str] = {
    PenaltyCode.NONE: "No Penalty",
    PenaltyCode.ILLEGAL_POSITIONING: "Illegal Position",
    PenaltyCode.MOTION_IN_SET: "Motion In Set",
    PenaltyCode.MOTION_IN_STOP: "Motion In Stop",
    PenaltyCode.LOCAL_GAME_STUCK: "Local Game Stuck",
    PenaltyCode.INCAPABLE_ROBOT: "Incapable Robot",
    PenaltyCode.PICK_UP: "Pick-up",
    PenaltyCode.BALL_HOLDING: "Ball Holding",
    PenaltyCode.LEAVING_THE_FIELD: "Leaving The Field",
    PenaltyCode.PLAYING_WITH_ARMS_HANDS: "Playing With Arms/Hands",
    PenaltyCode.PUSHING: "Pushing",
    PenaltyCode.CAUTIONED: "Cautioned",
    PenaltyCode.SENT_OFF: "Sent Off",
    PenaltyCode.SUBSTITUTE: "Substitute",
}


def _decode_penalty_reason(code: int) -> str:
    """Map a RobotInfo penalty byte to a label. Falls back to
    ``"code=<n>"`` for values outside the spec range so a future
    protocol addition still surfaces."""
    return PENALTY_LABELS.get(code, f"code={code}")


class GameState(IntEnum):
    INITIAL = 0
    READY = 1
    SET = 2
    PLAYING = 3
    FINISHED = 4


class SetPlay(IntEnum):
    NONE = 0
    DIRECT_FREE_KICK = 1
    INDIRECT_FREE_KICK = 2
    PENALTY_KICK = 3
    THROW_IN = 4
    GOAL_KICK = 5
    CORNER_KICK = 6


class CompetitionType(IntEnum):
    SMALL = 0
    MIDDLE = 1
    LARGE = 2


class GameControllerListener:
    """Listens for GameController UDP broadcasts and publishes to ZMQ.

    The HSL GameController broadcasts RoboCupGameControlData packets on
    UDP port 3838. We parse the header and key fields, publish as JSON
    events to the ZMQ bus.
    """

    def __init__(
        self,
        publisher: Publisher,
        port: int = GC_PORT,
        session_id: str | None = None,
    ):
        self._pub = publisher
        self._port = port
        self._session_id = session_id or f"gc-{uuid.uuid4().hex[:8]}"
        self._running = False
        self._packets_received = 0
        self._last_state: GameState | None = None
        # Track which mismatching versions we've already warned about
        # so a misconfigured neighbour doesn't fill the log.
        self._warned_versions: set[int] = set()

    def _parse_packet(self, data: bytes) -> dict | None:
        """Parse a RoboCupGameControlData packet.

        Minimal parsing - extracts header, version, and key game state fields.
        Full struct is ~600+ bytes; we extract the most useful fields.
        """
        if len(data) < 12:
            return None

        # Header: "RGme" (4 bytes)
        header = data[0:4]
        if header != GC_HEADER:
            return None

        # Version: 1 byte at offset 4. Drop packets we don't recognise
        # - the per-version field layout (esp. RobotInfo size) means a
        # mismatch corrupts team2 / penalty data silently. We log once
        # per unique mismatch value to avoid flooding when an old GC
        # accidentally broadcasts onto our subnet.
        version = data[4]
        if version != EXPECTED_VERSION:
            if version not in self._warned_versions:
                logger.warning(
                    "version_mismatch",
                    received=version,
                    expected=EXPECTED_VERSION,
                )
                self._warned_versions.add(version)
            return None

        # Packet number: 1 byte at offset 5
        packet_number = data[5]

        # Players per team: 1 byte at offset 6
        players_per_team = data[6]

        # Competition type: 1 byte at offset 7 (HSL: SMALL=0, MIDDLE=1, LARGE=2)
        competition_type_byte = data[7]
        try:
            competition_type = CompetitionType(competition_type_byte)
        except ValueError:
            competition_type = CompetitionType.SMALL

        # Stopped: 1 byte at offset 8 (HSL: 1=play stopped)
        stopped = bool(data[8])

        # Game phase: 1 byte at offset 9. See apps.schemas.game_state.GamePhase
        # for the value mapping (0=NORMAL, 1=PENALTY_SHOOTOUT, 2=OVERTIME,
        # 3=TIMEOUT). Combined with `first_half` it identifies one of the
        # five recording-relevant match phases.
        game_phase = data[9]

        # Game state: 1 byte at offset 10
        state_byte = data[10]
        try:
            state = GameState(state_byte)
        except ValueError:
            state = GameState.INITIAL

        # Set play: 1 byte at offset 11
        set_play_byte = data[11]
        try:
            set_play = SetPlay(set_play_byte)
        except ValueError:
            set_play = SetPlay.NONE

        # First half: 1 byte at offset 12
        first_half = data[12] if len(data) > 12 else 1

        # Kicking team: 1 byte at offset 13
        kicking_team = data[13] if len(data) > 13 else 0

        # Secs remaining: 2 bytes (int16) at offset 14
        secs_remaining = struct.unpack_from("<h", data, 14)[0] if len(data) > 15 else 0

        # Secondary time (int16) at offset 16 - the GC's
        # "secondary clock" (ready countdown, free-kick wait,
        # half-time break). Enables the secondary-time overlay tile.
        secondary_time = struct.unpack_from("<h", data, 16)[0] if len(data) > 17 else 0

        # Team info starts at offset 18
        # HSL TeamInfo (v20): teamNumber(1) + fieldPlayerColour(1) +
        #               goalkeeperColour(1) + goalkeeper(1) + score(1) +
        #               penaltyShot(1) + singleShots(2) + messageBudget(2) +
        #               RobotInfo[playersPerTeam] (3 bytes each).
        # Total per team = 10 + players_per_team * 3.
        team1_offset = 18
        team1_info = self._parse_team_info(data, team1_offset)
        team_info_size = 10 + players_per_team * 3
        team2_offset = team1_offset + team_info_size
        team2_info = self._parse_team_info(data, team2_offset)

        # Per-robot RobotInfo blocks. Each is 3 bytes
        # (penalty, secsTillUnpenalised, cautions) starting at offset
        # +10 of the TeamInfo. Short packets yield empty lists rather
        # than raising.
        team1_robots = self._parse_robot_infos(data, team1_offset + 10, players_per_team)
        team2_robots = self._parse_robot_infos(data, team2_offset + 10, players_per_team)

        return {
            "version": version,
            "packet_number": packet_number,
            "players_per_team": players_per_team,
            "competition_type": competition_type.name,
            "stopped": stopped,
            "game_phase": game_phase,
            "state": state.name,
            "state_value": state.value,
            "set_play": set_play.name,
            "first_half": bool(first_half),
            "kicking_team": kicking_team,
            "secs_remaining": secs_remaining,
            "secondary_time": secondary_time,
            "team1_number": team1_info["number"],
            "team1_score": team1_info["score"],
            "team1_message_budget": team1_info["message_budget"],
            "team1_field_player_colour": team1_info["field_player_colour"],
            "team1_goalkeeper_colour": team1_info["goalkeeper_colour"],
            "team1_goalkeeper": team1_info["goalkeeper"],
            "team1_penalty_shot": team1_info["penalty_shot"],
            "team1_single_shots": team1_info["single_shots"],
            "team1_robots": team1_robots,
            "team2_number": team2_info["number"],
            "team2_score": team2_info["score"],
            "team2_message_budget": team2_info["message_budget"],
            "team2_field_player_colour": team2_info["field_player_colour"],
            "team2_goalkeeper_colour": team2_info["goalkeeper_colour"],
            "team2_goalkeeper": team2_info["goalkeeper"],
            "team2_penalty_shot": team2_info["penalty_shot"],
            "team2_single_shots": team2_info["single_shots"],
            "team2_robots": team2_robots,
        }

    @staticmethod
    def _parse_team_info(data: bytes, offset: int) -> dict[str, int]:
        """Decode the 10-byte HSL TeamInfo header at ``offset``.

        Returns a dict with the seven non-robot fields. Short packets
        yield zeros for any field that runs past the end so callers
        get the same shape regardless. RobotInfo blocks are parsed
        separately via :meth:`_parse_robot_infos` starting at
        ``offset + 10``.
        """

        def _u8(o: int) -> int:
            return data[o] if len(data) > o else 0

        def _u16(o: int) -> int:
            return struct.unpack_from("<H", data, o)[0] if len(data) >= o + 2 else 0

        def _i16(o: int) -> int:
            return struct.unpack_from("<h", data, o)[0] if len(data) >= o + 2 else 0

        return {
            "number": _u8(offset),
            "field_player_colour": _u8(offset + 1),
            "goalkeeper_colour": _u8(offset + 2),
            "goalkeeper": _u8(offset + 3),
            "score": _u8(offset + 4),
            "penalty_shot": _u8(offset + 5),
            # singleShots is a bitmask of penalty-shot outcomes
            # (bit i = i-th shot scored). Read unsigned so all 16 bits
            # are addressable.
            "single_shots": _u16(offset + 6),
            # messageBudget is the per-team "packets remaining"
            # counter the scoreboard already displays.
            "message_budget": _i16(offset + 8),
        }

    @staticmethod
    def _parse_robot_infos(data: bytes, start: int, n: int) -> list[tuple[int, int, int]]:
        """Decode ``n`` consecutive RobotInfo blocks starting at ``start``.

        Each RobotInfo is 3 bytes (penalty, secsTillUnpenalised,
        cautions) per the HSL v20 spec. Stops short and returns
        whatever was readable if the packet ends mid-block.
        """
        end = start + n * 3
        if len(data) < end:
            # Truncate to whole-blocks-only.
            n = max(0, (len(data) - start) // 3)
            end = start + n * 3
        out: list[tuple[int, int, int]] = []
        for i in range(n):
            o = start + i * 3
            out.append((data[o], data[o + 1], data[o + 2]))
        return out

    def _publish_event(self, parsed: dict) -> None:
        """Publish a game state event to ZMQ."""
        now = datetime.now(UTC)
        event = {
            "schema_version": "1.0",
            "message_type": "game_state",
            "message_id": f"gc-{uuid.uuid4().hex[:12]}",
            "session_id": self._session_id,
            "source_module": "gamecontroller_listener",
            "created_at": now.isoformat(),
            "state": parsed["state"],
            "state_value": parsed["state_value"],
            "set_play": parsed["set_play"],
            "first_half": parsed["first_half"],
            "kicking_team": parsed["kicking_team"],
            "secs_remaining": parsed["secs_remaining"],
            "game_phase": parsed["game_phase"],
            "packet_number": parsed["packet_number"],
            "competition_type": parsed["competition_type"],
            "stopped": parsed["stopped"],
            # Secondary clock + per-team TeamInfo fields.
            "secondary_time": parsed["secondary_time"],
            "team1_number": parsed["team1_number"],
            "team1_score": parsed["team1_score"],
            "team1_message_budget": parsed["team1_message_budget"],
            "team1_field_player_colour": parsed["team1_field_player_colour"],
            "team1_goalkeeper_colour": parsed["team1_goalkeeper_colour"],
            "team1_goalkeeper": parsed["team1_goalkeeper"],
            "team1_penalty_shot": parsed["team1_penalty_shot"],
            "team1_single_shots": parsed["team1_single_shots"],
            "team2_number": parsed["team2_number"],
            "team2_score": parsed["team2_score"],
            "team2_message_budget": parsed["team2_message_budget"],
            "team2_field_player_colour": parsed["team2_field_player_colour"],
            "team2_goalkeeper_colour": parsed["team2_goalkeeper_colour"],
            "team2_goalkeeper": parsed["team2_goalkeeper"],
            "team2_penalty_shot": parsed["team2_penalty_shot"],
            "team2_single_shots": parsed["team2_single_shots"],
        }
        from apps.schemas.game_state import GameStateMessage

        msg = GameStateMessage.model_validate(event)
        self._pub.send(GC_TOPIC, msg)

    def _publish_penalties(self, parsed: dict) -> None:
        """Publish per-robot penalty data on ``telemetry.penalties``.

        Fires alongside ``_publish_event`` for the same GC packet so
        consumers that want both topics see consistent state. Empty
        robot lists (short packet, packet pre-game) still publish to
        keep cadence steady.
        """
        from apps.schemas.penalties import PenaltiesMessage

        now = datetime.now(UTC)

        def _robots_to_payload(team_number: int, robots: list) -> list[dict]:
            return [
                {
                    "team_number": team_number,
                    "player_number": idx + 1,
                    "penalty_code": penalty,
                    "penalty_reason": _decode_penalty_reason(penalty),
                    "secs_remaining": secs,
                    "cautions": caut,
                }
                for idx, (penalty, secs, caut) in enumerate(robots)
            ]

        event = {
            "schema_version": "1.0",
            "message_type": "penalties",
            "message_id": f"gc-pen-{uuid.uuid4().hex[:12]}",
            "session_id": self._session_id,
            "source_module": "gamecontroller_listener",
            "created_at": now.isoformat(),
            "team1_number": parsed["team1_number"],
            "team2_number": parsed["team2_number"],
            "team1_penalties": _robots_to_payload(
                parsed["team1_number"], parsed.get("team1_robots", [])
            ),
            "team2_penalties": _robots_to_payload(
                parsed["team2_number"], parsed.get("team2_robots", [])
            ),
        }
        msg = PenaltiesMessage.model_validate(event)
        self._pub.send(GC_PENALTIES_TOPIC, msg)

    def run(self, timeout_s: float = 1.0) -> int:
        """Listen for GameController packets. Returns total packets received."""
        sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        sock.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        sock.bind(("", self._port))
        sock.settimeout(timeout_s)

        logger.info("listening", port=self._port, session_id=self._session_id)
        self._running = True

        while self._running:
            try:
                data, addr = sock.recvfrom(4096)
                parsed = self._parse_packet(data)
                if parsed is None:
                    continue

                self._packets_received += 1
                new_state = GameState(parsed["state_value"])

                # Penalty countdowns tick every GC broadcast,
                # so the per-robot snapshot must publish on every
                # packet (not just on state changes like the
                # GameStateMessage below). The payload is small
                # (~1 KB at most) so the ~10 Hz cadence is fine.
                self._publish_penalties(parsed)

                # Only publish game_state on state changes (avoid
                # flooding). Penalty data above publishes every cycle.
                if new_state != self._last_state:
                    logger.info(
                        "state_changed",
                        old=self._last_state.name if self._last_state else "none",
                        new=new_state.name,
                    )
                    self._publish_event(parsed)
                    self._last_state = new_state

            except TimeoutError:
                continue

        sock.close()
        return self._packets_received

    def stop(self) -> None:
        self._running = False

    @property
    def packets_received(self) -> int:
        return self._packets_received
