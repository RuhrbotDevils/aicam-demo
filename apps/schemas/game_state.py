"""GameController game state message schema.

Author: Thomas Klute"""

from __future__ import annotations

from enum import Enum, IntEnum

from pydantic import BaseModel


class GameStateEnum(str, Enum):
    INITIAL = "INITIAL"
    READY = "READY"
    SET = "SET"
    PLAYING = "PLAYING"
    FINISHED = "FINISHED"


class SetPlayEnum(str, Enum):
    NONE = "NONE"
    DIRECT_FREE_KICK = "DIRECT_FREE_KICK"
    INDIRECT_FREE_KICK = "INDIRECT_FREE_KICK"
    PENALTY_KICK = "PENALTY_KICK"
    THROW_IN = "THROW_IN"
    GOAL_KICK = "GOAL_KICK"
    CORNER_KICK = "CORNER_KICK"


class GamePhase(IntEnum):
    """Values of the `gamePhase` byte in the SPL/HSL GameController packet.

    These integers are the authoritative wire values used by both the live
    UDP parser and the YAML log replay path. The `first_half` bool
    distinguishes the two halves within NORMAL and OVERTIME.
    """

    NORMAL = 0
    PENALTY_SHOOTOUT = 1
    OVERTIME = 2
    TIMEOUT = 3


class TeamColour(IntEnum):
    """Jersey colour enum from the HSL v20 TeamInfo block.

    Used by the broadcast overlay as a packet-driven fallback when
    ``config/teams.json`` has no entry for the team number. The
    ``to_rgb`` classmethod returns an approximate (r, g, b) tuple
    in 0..1 suitable for Cairo's ``set_source_rgb``.
    """

    BLUE = 0
    RED = 1
    YELLOW = 2
    BLACK = 3
    WHITE = 4
    GREEN = 5
    ORANGE = 6
    PURPLE = 7
    BROWN = 8
    GRAY = 9

    @classmethod
    def to_rgb(cls, value: int) -> tuple[float, float, float]:
        """Map a colour byte to a normalised (r, g, b) tuple.

        Values outside the spec fall through to a neutral mid-grey
        so an unexpected wire byte doesn't bleed white-on-white.
        """
        table: dict[int, tuple[float, float, float]] = {
            cls.BLUE: (0.10, 0.40, 0.90),
            cls.RED: (0.85, 0.15, 0.15),
            cls.YELLOW: (0.95, 0.85, 0.15),
            cls.BLACK: (0.10, 0.10, 0.10),
            cls.WHITE: (0.95, 0.95, 0.95),
            cls.GREEN: (0.15, 0.70, 0.30),
            cls.ORANGE: (0.95, 0.55, 0.10),
            cls.PURPLE: (0.55, 0.20, 0.70),
            cls.BROWN: (0.45, 0.30, 0.15),
            cls.GRAY: (0.55, 0.55, 0.55),
        }
        return table.get(value, (0.50, 0.50, 0.50))


class GameStateMessage(BaseModel):
    """Game state event from GameController (published on telemetry.game_state)."""

    schema_version: str = "1.0"
    message_type: str = "game_state"
    message_id: str
    session_id: str
    source_module: str
    created_at: str

    state: GameStateEnum
    state_value: int
    set_play: SetPlayEnum
    first_half: bool
    kicking_team: int
    secs_remaining: int
    # Wire-level phase byte. Values are documented by `GamePhase`:
    # 0=NORMAL, 1=PENALTY_SHOOTOUT, 2=OVERTIME, 3=TIMEOUT.
    game_phase: int
    packet_number: int
    competition_type: str = "SMALL"
    stopped: bool = False
    # GC's secondary clock (ready countdown, free-kick wait,
    # half-time break). Surfaces as the overlay's secondary-time tile.
    secondary_time: int = 0
    team1_number: int = 0
    team1_score: int = 0
    team2_number: int = 0
    team2_score: int = 0
    # Per-team message budget - the "packets remaining" int16 the HSL
    # TeamInfo block carries (~1200 at match start, decreases as the
    # team's robots transmit). The broadcast overlay's scoreboard row 1
    # surfaces these as the home and away cells.
    team1_message_budget: int = 0
    team2_message_budget: int = 0
    # Extended TeamInfo fields. Wire byte values for the colour enums
    # are documented by `TeamColour`. All default to 0 so older
    # publishers continue to validate; consumers should treat 0 as
    # "no info" rather than literal blue / player 0.
    team1_field_player_colour: int = 0
    team1_goalkeeper_colour: int = 0
    team1_goalkeeper: int = 0
    team1_penalty_shot: int = 0
    team1_single_shots: int = 0
    team2_field_player_colour: int = 0
    team2_goalkeeper_colour: int = 0
    team2_goalkeeper: int = 0
    team2_penalty_shot: int = 0
    team2_single_shots: int = 0
