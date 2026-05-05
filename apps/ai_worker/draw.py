"""Shared annotation helpers for detection workers.

Drawing logic moved out of ``apps/control_api/app/main.py`` so the
CPU detector can render its own annotated JPEG against the exact
frame it ran inference on, removing the timing skew between the
"latest raw camera frame" and the ZMQ-cached detections.

Author: Thomas Klute"""

from __future__ import annotations

import cv2
import numpy as np

# Per-class colour map for the demo classes plus an explicit grey
# fallback. Picking by exact match on the class string the worker
# emits (after the optional ``class_map`` remap). Anything outside
# the demo set draws in the fallback colour.
_CLASS_COLORS: dict[str, tuple[int, int, int]] = {
    "ball": (0, 165, 255),
    "robot": (0, 200, 0),
    "human": (0, 255, 255),
}
_FALLBACK_COLOR: tuple[int, int, int] = (200, 200, 200)


def draw_detection_boxes(frame: np.ndarray, detections: list[dict]) -> None:
    """Draw bounding boxes + labels onto ``frame`` in-place.

    Each detection is a dict with at least ``class``, ``confidence``,
    and ``bbox_xywh`` (x, y, w, h in pixel coords matching ``frame``).
    Detections with malformed bboxes are silently skipped.
    """
    for det in detections:
        bbox = det.get("bbox_xywh", [])
        if len(bbox) < 4:
            continue
        x, y, w, h = bbox[:4]
        cls = det.get("class", "unknown")
        conf = det.get("confidence", 0.0)
        color = _CLASS_COLORS.get(cls, _FALLBACK_COLOR)
        cv2.rectangle(frame, (int(x), int(y)), (int(x + w), int(y + h)), color, 2)
        cv2.putText(
            frame,
            f"{cls} {conf:.0%}",
            (int(x), int(y) - 5),
            cv2.FONT_HERSHEY_SIMPLEX,
            0.5,
            color,
            1,
        )
