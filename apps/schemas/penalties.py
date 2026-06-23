"""Per-robot penalty data extracted from the GameController broadcast.

Published on the ``telemetry.penalties`` ZMQ topic alongside
``telemetry.game_state``. The schema mirrors the per-robot
``RobotInfo`` block of the HSL ``RoboCupGameControlData`` packet
(v20, 3 bytes per robot): each robot reports a penalty enum code,
a countdown timer until the penalty expires, and an accumulated
caution counter.

Consumers (broadcast overlay, statistics UI) subscribe to this topic
when they need per-robot data without bloating the lighter-weight
``GameStateMessage`` payload.

The ``penalty_code`` is surfaced as the raw uint8 from the GC
broadcast so consumers can map it to whichever HSL revision they
target; a best-effort decoded ``penalty_reason`` string is also
included for direct display use, falling back to ``"code=<n>"`` for
values not yet in the mapping table.

Author: Thomas Klute"""

from __future__ import annotations

from pydantic import BaseModel, Field


class PerRobotPenalty(BaseModel):
    """One robot's penalty state from the GC RobotInfo block.

    Fields come directly from the 3-byte HSL v20 RobotInfo layout
    (`gamecontroller.py` module docstring):
    (penalty, secsTillUnpenalised, cautions).
    """

    team_number: int = Field(ge=0)
    """The team this robot belongs to (matches `team{1,2}_number`
    in :class:`apps.schemas.game_state.GameStateMessage`)."""

    player_number: int = Field(ge=1)
    """1-based player slot within the team (1..players_per_team)."""

    penalty_code: int = Field(ge=0, le=255)
    """Raw uint8 from the RobotInfo penalty byte. 0 = no penalty."""

    penalty_reason: str
    """Decoded penalty label (e.g. ``"No Penalty"``, ``"Pushing"``,
    ``"Substitute"``) per :data:`apps.telemetry_service.gamecontroller.PENALTY_LABELS`,
    or the fallback ``"code=<n>"`` for codes outside the HSL v20 spec."""

    secs_remaining: int = Field(ge=0, le=255)
    """Countdown timer in seconds until the penalty expires.
    0 when no penalty is active."""

    cautions: int = Field(ge=0, le=255)
    """Accumulated caution count for the robot."""


class PenaltiesMessage(BaseModel):
    """Per-robot penalty snapshot for both teams.

    Published on the ``telemetry.penalties`` topic once per GC
    broadcast cycle (~10 Hz). Consumers may rely on the array
    lengths matching ``players_per_team`` from the corresponding
    ``GameStateMessage`` event, but should tolerate empty arrays
    for short or malformed packets.
    """

    schema_version: str = "1.0"
    message_type: str = "penalties"
    message_id: str
    session_id: str
    source_module: str
    created_at: str

    team1_number: int = 0
    """Team-number for ``team1_penalties`` entries (matches the
    ``team1_number`` field in the matching GameStateMessage)."""

    team2_number: int = 0

    team1_penalties: list[PerRobotPenalty] = Field(default_factory=list)
    team2_penalties: list[PerRobotPenalty] = Field(default_factory=list)
