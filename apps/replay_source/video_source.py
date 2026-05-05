"""Video file replay source - decodes .h264/.mp4 and publishes FrameReferenceMessage.

Uses OpenCV VideoCapture for decoding. Publishes frames to ZMQ at configurable
playback speed, enabling post-game analysis through the full pipeline.

Author: Thomas Klute"""

from __future__ import annotations

import tempfile
import time
import uuid
from datetime import UTC, datetime, timedelta
from pathlib import Path

import cv2

from apps.bus.publisher import Publisher
from apps.logging_config import get_logger
from apps.schemas import FrameReferenceMessage

logger = get_logger("video_replay")

TOPIC = "media.frame_refs"


class VideoReplaySource:
    """Reads a video file and publishes FrameReferenceMessage for each frame.

    Args:
        video_path: Path to .h264, .mp4, or other video file.
        publisher: ZMQ Publisher instance.
        session_id: Session identifier (auto-generated if not provided).
        speed: Playback speed multiplier (1.0 = realtime, 0 = as-fast-as-possible).
        subsample: Publish every Nth frame (1 = every frame).
        frame_dir: Directory for writing extracted frame files.
    """

    def __init__(
        self,
        video_path: str | Path,
        publisher: Publisher,
        session_id: str | None = None,
        speed: float = 1.0,
        subsample: int = 1,
        frame_dir: str | Path | None = None,
    ):
        self._video_path = Path(video_path)
        self._publisher = publisher
        self._session_id = session_id or f"replay-{uuid.uuid4().hex[:8]}"
        self._speed = speed
        self._subsample = max(1, subsample)
        self._frame_dir = (
            Path(frame_dir) if frame_dir else Path(tempfile.mkdtemp(prefix="aicam-replay-"))
        )
        self._frame_dir.mkdir(parents=True, exist_ok=True)
        self._running = False
        self._frames_published = 0
        self._total_frames = 0

    def run(self) -> int:
        """Decode video and publish frames. Returns total frames published."""
        if not self._video_path.exists():
            logger.error("video_not_found", path=str(self._video_path))
            return 0

        cap = cv2.VideoCapture(str(self._video_path))
        if not cap.isOpened():
            logger.error("video_open_failed", path=str(self._video_path))
            return 0

        fps = cap.get(cv2.CAP_PROP_FPS) or 30.0
        width = int(cap.get(cv2.CAP_PROP_FRAME_WIDTH))
        height = int(cap.get(cv2.CAP_PROP_FRAME_HEIGHT))
        total = int(cap.get(cv2.CAP_PROP_FRAME_COUNT))

        logger.info(
            "starting",
            path=str(self._video_path),
            fps=fps,
            width=width,
            height=height,
            total_frames=total,
            speed=self._speed,
            subsample=self._subsample,
        )

        self._running = True
        self._total_frames = total
        frame_index = 0
        start_time = time.monotonic()
        interval = (1.0 / fps) / self._speed if self._speed > 0 else 0

        while self._running:
            ret, frame = cap.read()
            if not ret:
                break

            frame_index += 1

            # Subsample
            if frame_index % self._subsample != 0:
                continue

            frame_path = self._frame_dir / f"frame_{frame_index:06d}.jpg"
            cv2.imwrite(str(frame_path), frame)

            # Build and publish message
            now = datetime.now(UTC)
            # Video timestamp (PTS) based on frame index and FPS
            video_ts = timedelta(seconds=frame_index / fps)

            msg = FrameReferenceMessage.model_validate(
                {
                    "schema_version": "1.0",
                    "message_id": f"vreplay-{uuid.uuid4().hex[:12]}",
                    "session_id": self._session_id,
                    "source_module": "video_replay",
                    "created_at": now.isoformat(),
                    "frame_id": f"frame-{frame_index:06d}",
                    "source_timestamp": now.isoformat(),
                    "frame_index": frame_index,
                    "width_px": width,
                    "height_px": height,
                    "frame_ref": {
                        "transport": "file",
                        "name": str(frame_path.resolve()),
                        "offset": 0,
                        "length": frame_path.stat().st_size,
                    },
                    "notes": [f"video_pts_s={video_ts.total_seconds():.3f}"],
                }
            )
            self._publisher.send(TOPIC, msg)
            self._frames_published += 1

            # Rate limiting
            if interval > 0:
                expected_time = frame_index * interval
                elapsed = time.monotonic() - start_time
                sleep_time = expected_time - elapsed
                if sleep_time > 0:
                    time.sleep(sleep_time)

        cap.release()
        logger.info(
            "finished",
            frames_published=self._frames_published,
            total_decoded=frame_index,
        )
        return self._frames_published

    def stop(self) -> None:
        self._running = False

    @property
    def frames_published(self) -> int:
        return self._frames_published

    @property
    def session_id(self) -> str:
        return self._session_id

    @property
    def video_fps(self) -> float:
        """Get FPS from the video file (opens briefly to read metadata)."""
        cap = cv2.VideoCapture(str(self._video_path))
        fps = cap.get(cv2.CAP_PROP_FPS) or 30.0
        cap.release()
        return fps
