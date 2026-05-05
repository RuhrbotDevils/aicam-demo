"""Object detections message - Detector output (doc 08, section 2).

Author: Thomas Klute"""

from __future__ import annotations

from datetime import datetime
from enum import Enum
from typing import Annotated, Literal

from pydantic import BaseModel, Field

from apps.schemas.envelope import MessageEnvelope


class DetectionClass(str, Enum):
    """Canonical detection classes.

    Kept as documentation / reference for the COCO + RoboCup
    detection paths - see ``apps/hailo_postprocess/metadata_export.cpp``
    where Hailo labels are mapped to one of these values.

    The schema does not enforce membership: ``Detection.cls`` is a
    free string so models with arbitrary class names (e.g. the CPU
    landmark detector emitting ``class_1`` / ``class_3``) can
    publish on ``ai.object_detections`` without crashing the
    worker. The Detection page renders unknown classes in a
    fallback colour.
    """

    robot = "robot"
    human = "human"
    ball = "ball"


class DetectorModel(BaseModel):
    name: str
    version: str
    runtime: str  # e.g. "hailo", "onnxruntime", "pytorch"

    model_config = {"extra": "forbid"}


class Detection(BaseModel):
    detection_id: str
    cls: str = Field(alias="class", min_length=1)
    bbox_xywh: Annotated[list[float], Field(min_length=4, max_length=4)]
    bbox_format: Literal["xywh"] = "xywh"
    coordinate_system: Literal["image_px"] = "image_px"
    confidence: float = Field(ge=0.0, le=1.0)

    model_config = {"extra": "forbid", "populate_by_name": True}


class ObjectDetectionsMessage(MessageEnvelope):
    """Detections produced from a single frame."""

    message_type: Literal["object_detections"] = "object_detections"

    frame_id: str
    source_timestamp: datetime
    detector_model: DetectorModel
    detections: list[Detection] = Field(default_factory=list)
