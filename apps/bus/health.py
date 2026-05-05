"""Service health tracker - counts processed/dropped frames with rolling window.

Optionally publishes health on ZMQ topic ``service.health`` every 5 seconds
so the control API can collect and display in the dashboard.

Author: Thomas Klute"""

from __future__ import annotations

import json
import logging
import threading
import time
from collections import deque

logger = logging.getLogger(__name__)

#: Rolling window size in seconds for drop rate calculation.
WINDOW_S = 10.0

#: ZMQ topic for service health messages.
HEALTH_TOPIC = "service.health"

#: How often to publish health (seconds).
PUBLISH_INTERVAL_S = 5.0


class ServiceHealthTracker:
    """Tracks frame processing and drop counts for a service.

    Usage::

        health = ServiceHealthTracker("my-service")
        health.start_publishing()  # optional: publish on ZMQ every 5s
        health.record_processed()
        health.record_dropped(reason="queue full")

        # Query
        health.drops_last_10s  # → int
        health.to_dict()       # → {name, processed, dropped, drops_last_10s}
    """

    def __init__(self, name: str):
        self.name = name
        self.frames_processed = 0
        self.frames_dropped = 0
        self._drop_times: deque[float] = deque()
        self._publish_thread: threading.Thread | None = None
        self._running = False

    def record_processed(self) -> None:
        self.frames_processed += 1

    def record_dropped(self, count: int = 1, reason: str = "") -> None:
        now = time.monotonic()
        self.frames_dropped += count
        for _ in range(count):
            self._drop_times.append(now)
        if reason:
            logger.warning("%s: dropped %d frame(s) - %s", self.name, count, reason)
        else:
            logger.warning("%s: dropped %d frame(s)", self.name, count)

    @property
    def drops_last_10s(self) -> int:
        self._prune()
        return len(self._drop_times)

    def _prune(self) -> None:
        cutoff = time.monotonic() - WINDOW_S
        while self._drop_times and self._drop_times[0] < cutoff:
            self._drop_times.popleft()

    def to_dict(self) -> dict:
        return {
            "name": self.name,
            "frames_processed": self.frames_processed,
            "frames_dropped": self.frames_dropped,
            "drops_last_10s": self.drops_last_10s,
        }

    def start_publishing(self) -> None:
        """Start a daemon thread that publishes health on ZMQ every 5 seconds."""
        if self._publish_thread is not None:
            return
        self._running = True
        self._publish_thread = threading.Thread(
            target=self._publish_loop, daemon=True, name=f"health-pub-{self.name}"
        )
        self._publish_thread.start()

    def stop_publishing(self) -> None:
        self._running = False

    def _publish_loop(self) -> None:
        import zmq

        from apps.bus.broker import DEFAULT_XSUB_ENDPOINT

        ctx = zmq.Context()
        sock = ctx.socket(zmq.PUB)
        sock.setsockopt(zmq.SNDHWM, 10)
        sock.connect(DEFAULT_XSUB_ENDPOINT)
        # Brief sleep to let ZMQ connect
        time.sleep(0.2)

        while self._running:
            try:
                payload = json.dumps(self.to_dict())
                sock.send_multipart([HEALTH_TOPIC.encode(), payload.encode()])
            except Exception:
                pass
            time.sleep(PUBLISH_INTERVAL_S)

        sock.close()
        ctx.term()
