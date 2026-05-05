"""Frame reference message - Media Service → AI workers (doc 08, section 1).

Author: Thomas Klute"""

from __future__ import annotations

from datetime import datetime
from typing import Literal

from pydantic import BaseModel, Field

from apps.schemas.envelope import MessageEnvelope


class FrameRef(BaseModel):
    """Pointer to the actual frame data; transport-agnostic."""

    transport: str  # e.g. "shared_memory", "mmap", "file"
    name: str
    offset: int = 0
    length: int

    model_config = {"extra": "forbid"}


class FrameReferenceMessage(MessageEnvelope):
    """Tell AI workers that a frame is available for processing."""

    message_type: Literal["frame_reference"] = "frame_reference"

    frame_id: str
    stream_id: str = "main_video"
    source_timestamp: datetime
    arrival_timestamp: datetime | None = None

    frame_index: int = Field(ge=0)
    width_px: int = Field(gt=0)
    height_px: int = Field(gt=0)
    pixel_format: str = "BGR"
    coordinate_system: Literal["image_px"] = "image_px"

    frame_ref: FrameRef

    calibration_ref: str | None = None
    calibration_state: str | None = None
    notes: list[str] = Field(default_factory=list)
