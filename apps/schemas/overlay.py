"""Overlay update message (doc 08, section 11).

Author: Thomas Klute"""

from __future__ import annotations

from datetime import datetime
from enum import Enum
from typing import Annotated, Literal

from pydantic import BaseModel, Field

from apps.schemas.envelope import MessageEnvelope


class OverlayElementType(str, Enum):
    text = "text"
    bbox = "bbox"


class OverlayElement(BaseModel):
    type: OverlayElementType
    # text element fields
    text: str | None = None
    position_norm: Annotated[list[float], Field(min_length=2, max_length=2)] | None = None
    # bbox element fields
    track_id: str | None = None
    bbox_xywh: Annotated[list[float], Field(min_length=4, max_length=4)] | None = None
    coordinate_system: str | None = None
    label: str | None = None

    model_config = {"extra": "forbid"}


class OverlayUpdateMessage(MessageEnvelope):
    """Rendering instructions for overlay composition."""

    message_type: Literal["overlay_update"] = "overlay_update"

    target_profile: str = "operator_debug"
    effective_from: datetime | None = None
    elements: list[OverlayElement] = Field(default_factory=list)
