"""Replay controller - manages video replay threads (demo build).

Starts a ``VideoReplaySource`` as a daemon thread inside the control
API process. The source publishes on the same ZMQ topic as the live
path (``media.frame_refs``) so downstream AI workers see replay frames
indistinguishably from live frames.

Demo build does not contain the parallel gc-log replay path; the
demo does not run the GameController telemetry pipeline.

Author: Thomas Klute"""

from __future__ import annotations

import logging
import threading
import uuid
from dataclasses import dataclass
from pathlib import Path
from typing import Any

import yaml

from apps.bus.publisher import Publisher
from apps.replay_source.video_source import VideoReplaySource

logger = logging.getLogger(__name__)

PLAYBACK_DIR = Path("playback")


@dataclass
class PlaybackSession:
    """Parsed contents of a ``playback.yaml`` sidecar."""

    dir_name: str
    name: str
    gc_log: str
    gc_log_format: str
    half1_video: str
    half1_offset_s: float
    half2_video: str | None = None
    half2_offset_s: float | None = None

    @property
    def has_half2(self) -> bool:
        return self.half2_video is not None

    def video_path(self, half: int) -> Path:
        base = PLAYBACK_DIR / self.dir_name
        if half == 2 and self.half2_video:
            return base / self.half2_video
        return base / self.half1_video

    def offset_s(self, half: int) -> float:
        if half == 2 and self.half2_offset_s is not None:
            return self.half2_offset_s
        return self.half1_offset_s

    def gc_log_path(self) -> Path:
        return PLAYBACK_DIR / self.dir_name / self.gc_log


def scan_sessions() -> list[PlaybackSession]:
    """Scan ``playback/`` for subdirectories with a ``playback.yaml``."""
    sessions: list[PlaybackSession] = []
    if not PLAYBACK_DIR.is_dir():
        return sessions
    for sub in sorted(PLAYBACK_DIR.iterdir()):
        if not sub.is_dir():
            continue
        sidecar = sub / "playback.yaml"
        if not sidecar.exists():
            continue
        try:
            with sidecar.open() as f:
                data = yaml.safe_load(f)
            if not isinstance(data, dict):
                continue
            half1 = data.get("half1", {})
            half2 = data.get("half2")
            session = PlaybackSession(
                dir_name=sub.name,
                name=data.get("name", sub.name),
                gc_log=data.get("gc_log", "gc_log.yaml"),
                gc_log_format=data.get("gc_log_format", "2024"),
                half1_video=half1.get("video", "half1.mp4"),
                half1_offset_s=float(half1.get("offset_s", 0.0)),
                half2_video=half2.get("video") if half2 else None,
                half2_offset_s=float(half2.get("offset_s", 0.0)) if half2 else None,
            )
            sessions.append(session)
        except Exception as e:
            logger.warning("replay: failed to parse %s: %s", sidecar, e)
    return sessions


def session_to_dict(s: PlaybackSession) -> dict[str, Any]:
    """Serialize a session for the API response."""
    d: dict[str, Any] = {
        "dir_name": s.dir_name,
        "name": s.name,
        "gc_log_format": s.gc_log_format,
        "has_half2": s.has_half2,
    }
    return d


class ReplayController:
    """Owns the replay threads. At most one replay runs at a time."""

    def __init__(self) -> None:
        self._video_source: VideoReplaySource | None = None
        self._video_thread: threading.Thread | None = None
        self._publisher: Publisher | None = None
        self._session: PlaybackSession | None = None
        self._half: int = 1
        self._speed: float = 1.0
        self._state: str = "idle"  # idle | playing | done
        self._lock = threading.Lock()

    @property
    def state(self) -> str:
        return self._state

    def status(self) -> dict[str, Any]:
        with self._lock:
            d: dict[str, Any] = {"state": self._state}
            if self._session:
                d["session"] = self._session.name
                d["half"] = self._half
                d["speed"] = self._speed
            if self._video_source:
                d["frames_published"] = self._video_source.frames_published
                d["total_frames"] = self._video_source._total_frames
            return d

    def start(
        self,
        session: PlaybackSession,
        half: int = 1,
        speed: float = 1.0,
    ) -> None:
        """Start a replay. Stops any running replay first."""
        self.stop()
        with self._lock:
            self._session = session
            self._half = half
            self._speed = speed
            self._state = "playing"

        video_path = session.video_path(half)
        if not video_path.exists():
            logger.error("replay: video file not found: %s", video_path)
            with self._lock:
                self._state = "idle"
            return

        pub = Publisher()
        self._publisher = pub
        session_id = f"replay-{uuid.uuid4().hex[:8]}"

        # Video replay thread
        vs = VideoReplaySource(
            video_path=video_path,
            publisher=pub,
            session_id=session_id,
            speed=speed,
            subsample=1,
        )
        self._video_source = vs

        def _run_video() -> None:
            try:
                vs.run()
            except Exception as e:
                logger.warning("replay: video thread error: %s", e)
            finally:
                with self._lock:
                    if self._state == "playing":
                        self._state = "done"

        self._video_thread = threading.Thread(target=_run_video, name="replay-video", daemon=True)
        self._video_thread.start()

        logger.info(
            "replay: started session=%s half=%d speed=%.1f video=%s",
            session.name,
            half,
            speed,
            video_path,
        )

    def stop(self) -> None:
        """Stop the current replay if running."""
        with self._lock:
            if self._state == "idle":
                return
            self._state = "idle"

        if self._video_source:
            self._video_source.stop()
        if self._video_thread and self._video_thread.is_alive():
            self._video_thread.join(timeout=5.0)

        self._video_source = None
        self._video_thread = None
        self._publisher = None
        self._session = None
        logger.info("replay: stopped")
