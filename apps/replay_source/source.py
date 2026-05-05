"""Replay frame source - scans a directory of images and publishes frame references.

Author: Thomas Klute"""

from __future__ import annotations

import time
import uuid
from datetime import UTC, datetime
from pathlib import Path

from apps.bus.publisher import Publisher
from apps.logging_config import get_logger
from apps.schemas import FrameReferenceMessage

logger = get_logger("replay_source")

TOPIC = "media.frame_refs"


class ReplayFrameSource:
    """Reads ordered images from a directory and publishes FrameReferenceMessage.

    Args:
        frames_dir: Directory containing PNG/JPG images (sorted by name).
        publisher: ZMQ Publisher instance.
        session_id: Session identifier (auto-generated if not provided).
        fps: Target publish rate in frames per second.
        loop: If True, repeat the frame sequence indefinitely.
    """

    def __init__(
        self,
        frames_dir: str | Path,
        publisher: Publisher,
        session_id: str | None = None,
        fps: float = 5.0,
        loop: bool = False,
    ):
        self._frames_dir = Path(frames_dir)
        self._publisher = publisher
        self._session_id = session_id or f"replay-{uuid.uuid4().hex[:8]}"
        self._fps = fps
        self._loop = loop
        self._running = False
        self._frames_published = 0

    def scan_frames(self) -> list[Path]:
        """Return sorted list of image files in the frames directory."""
        exts = {".png", ".jpg", ".jpeg"}
        files = sorted(f for f in self._frames_dir.iterdir() if f.suffix.lower() in exts)
        return files

    def _read_dimensions(self, frame_path: Path) -> tuple[int, int]:
        """Read image dimensions without loading full image data."""
        try:
            from PIL import Image

            with Image.open(frame_path) as img:
                w, h = img.size
                return (w, h)
        except Exception:
            return (1, 1)

    def _make_message(
        self, frame_path: Path, frame_index: int, width: int, height: int
    ) -> FrameReferenceMessage:
        """Build a FrameReferenceMessage for the given frame file."""
        stat = frame_path.stat()
        now = datetime.now(UTC)
        return FrameReferenceMessage.model_validate(
            {
                "schema_version": "1.0",
                "message_id": f"replay-{uuid.uuid4().hex[:12]}",
                "session_id": self._session_id,
                "source_module": "replay_source",
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
                    "length": stat.st_size,
                },
            }
        )

    def run(self) -> int:
        """Run the replay loop. Returns total frames published.

        Blocks until all frames are published (or loop is interrupted via stop()).
        """
        frames = self.scan_frames()
        if not frames:
            logger.warning("no_frames_found", dir=str(self._frames_dir))
            return 0

        logger.info("starting", frames=len(frames), fps=self._fps, loop=self._loop)

        self._running = True
        interval = 1.0 / self._fps
        frame_index = 0

        while self._running:
            for frame_path in frames:
                if not self._running:
                    break

                w, h = self._read_dimensions(frame_path)
                msg = self._make_message(frame_path, frame_index, w, h)
                self._publisher.send(TOPIC, msg)
                self._frames_published += 1
                frame_index += 1

                time.sleep(interval)

            if not self._loop:
                break

        self._running = False
        return self._frames_published

    def stop(self) -> None:
        """Signal the replay loop to stop."""
        self._running = False

    @property
    def frames_published(self) -> int:
        return self._frames_published

    @property
    def session_id(self) -> str:
        return self._session_id
