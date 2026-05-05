"""WebSocket event broadcaster - fans out events to connected clients.

Author: Thomas Klute"""

from __future__ import annotations

import json
import queue
from datetime import UTC, datetime
from typing import Any


class EventBroadcaster:
    """In-memory event broadcaster for WebSocket clients.

    Uses stdlib ``queue.Queue`` (thread-safe) so publish() can be called
    from sync endpoint handlers running in threadpool workers, while the
    async WebSocket handler polls from the event loop.
    """

    def __init__(self) -> None:
        self._clients: set[queue.Queue[str]] = set()

    def connect(self) -> queue.Queue[str]:
        """Register a new client and return its event queue."""
        q: queue.Queue[str] = queue.Queue(maxsize=100)
        self._clients.add(q)
        return q

    def disconnect(self, q: queue.Queue[str]) -> None:
        """Unregister a client."""
        self._clients.discard(q)

    def publish(self, event: dict[str, Any]) -> None:
        """Push an event dict to all connected clients (non-blocking)."""
        if "timestamp" not in event:
            event["timestamp"] = datetime.now(UTC).isoformat()
        payload = json.dumps(event)
        for q in list(self._clients):
            try:
                q.put_nowait(payload)
            except queue.Full:
                pass  # drop event for slow clients

    @property
    def client_count(self) -> int:
        return len(self._clients)


# Singleton broadcaster used by the control API.
broadcaster = EventBroadcaster()
