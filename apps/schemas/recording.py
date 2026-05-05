"""Recording session schemas - session lifecycle, stream metadata, sidecar format.

Author: Thomas Klute"""

from __future__ import annotations

from datetime import datetime
from enum import Enum

from pydantic import BaseModel, Field


class SessionStatus(str, Enum):
    created = "created"
    recording = "recording"
    stopping = "stopping"
    completed = "completed"
    failed = "failed"


class VideoCodec(str, Enum):
    h264 = "h264"


class AudioCodec(str, Enum):
    flac = "flac"


class RecordingStreamType(str, Enum):
    video = "video"
    audio = "audio"


class RecordingStream(BaseModel):
    """Metadata for a single recording stream (video or audio)."""

    stream_type: RecordingStreamType
    codec: str  # e.g. "h264", "flac"
    file_name: str  # e.g. "video.h264", "audio.flac"
    status: SessionStatus = SessionStatus.created

    model_config = {"extra": "forbid"}


class RecordingSession(BaseModel):
    """Full recording session metadata - written as session.json sidecar."""

    session_id: str
    status: SessionStatus = SessionStatus.created
    directory: str  # e.g. "recordings/2026-04-06T12-30-00_abc123/"
    name: str | None = None  # user-provided or auto-generated session name
    start_time: datetime | None = None
    end_time: datetime | None = None
    duration_s: float | None = None
    streams: list[RecordingStream] = Field(default_factory=list)

    # Config snapshot at recording start
    video_width: int | None = None
    video_height: int | None = None
    video_fps: int | None = None
    audio_device: str | None = None
    audio_enabled: bool = True

    # Enriched after recording stops
    actual_frame_count: int | None = None
    video_file_size: int | None = None
    audio_file_size: int | None = None

    model_config = {"extra": "forbid"}
