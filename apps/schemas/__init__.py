"""Shared Pydantic schema package for AICam inter-module messages."""

from apps.schemas.envelope import MessageEnvelope
from apps.schemas.frame_reference import FrameRef, FrameReferenceMessage
from apps.schemas.object_detections import Detection, ObjectDetectionsMessage
from apps.schemas.overlay import OverlayElement, OverlayUpdateMessage
from apps.schemas.recording import RecordingSession, RecordingStream, SessionStatus

__all__ = [
    "MessageEnvelope",
    "FrameRef",
    "FrameReferenceMessage",
    "Detection",
    "ObjectDetectionsMessage",
    "OverlayElement",
    "OverlayUpdateMessage",
    "RecordingSession",
    "RecordingStream",
    "SessionStatus",
]
