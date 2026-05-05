"""HTTP client for the Rust media service (port 8090).

Author: Thomas Klute"""

from __future__ import annotations

from typing import Any

import httpx
from pydantic import BaseModel

MEDIA_SERVICE_DEFAULT_URL = "http://localhost:8090"


class MediaStatus(BaseModel):
    """Mirrors the Rust ``RuntimeStatus`` struct.

    The raw-NV12 camera preview used to live behind a dedicated
    GStreamer branch with its own ``camera_preview_active`` flag, but
    a refactor deleted that branch and now serves preview frames
    from the frame_export appsink via a direct file read from the
    control API. The flag was dropped from ``RuntimeStatus`` at the
    same time; this model drops it here, plus ``hailo_available`` is
    now tracked so we stay in sync with what the service sends.
    """

    state: str
    input_source: str
    audio_available: bool
    hailo_available: bool = False
    recording_active: bool
    # RFC3339 timestamp of when the active recording session started.
    # ``None`` while no recording is in progress. Used by the
    # Recording-page elapsed timer so it survives page navigation.
    recording_started_at: str | None = None
    streaming_enabled: bool
    camera_device: str | None = None
    camera_name: str | None = None
    audio_device: str | None = None
    audio_name: str | None = None


class MediaServiceClient:
    """Thin HTTP client for the media service control contract.

    All methods return ``MediaStatus`` (or a raw dict for health).
    On connection failure, methods raise ``httpx.ConnectError``.
    """

    def __init__(self, base_url: str = MEDIA_SERVICE_DEFAULT_URL, timeout: float = 5.0):
        self._base = base_url.rstrip("/")
        self._timeout = timeout
        self._long_timeout = 15.0

    def _get(self, path: str) -> dict[str, Any]:
        resp = httpx.get(f"{self._base}{path}", timeout=self._timeout)
        resp.raise_for_status()
        return resp.json()  # type: ignore[no-any-return]

    def _post(
        self, path: str, json: dict[str, Any] | None = None, timeout: float | None = None
    ) -> MediaStatus:
        resp = httpx.post(f"{self._base}{path}", json=json, timeout=timeout or self._timeout)
        resp.raise_for_status()
        return MediaStatus.model_validate(resp.json())

    def health(self) -> dict[str, Any]:
        return self._get("/health")

    def status(self) -> MediaStatus:
        return MediaStatus.model_validate(self._get("/status"))

    def start_pipeline(self) -> MediaStatus:
        return self._post("/start")

    def stop_pipeline(self) -> MediaStatus:
        return self._post("/stop")

    def start_recording(self, name: str | None = None) -> MediaStatus:
        body = {}
        if name:
            body["name"] = name
        return self._post("/recording/start", json=body if body else None)

    def stop_recording(self) -> MediaStatus:
        return self._post("/recording/stop", timeout=self._long_timeout)

    def detection_status(self) -> dict[str, Any]:
        return self._get("/detection/status")

    # ------------------------------------------------------------------
    # Replay helpers - these return raw dicts because the media
    # service /replay/* responses are not MediaStatus objects.
    # ------------------------------------------------------------------

    def replay_start(self, path: str, speed: float = 1.0) -> dict[str, Any]:
        """Start replay for the given absolute file path.

        ``speed`` matches the media service's `/replay/start` semantics:
        ``1.0`` is realtime, ``0.0`` is "Max" (drain at decode speed),
        positive values are playback rate multipliers.

        Raises ``httpx.HTTPStatusError`` on 4xx/5xx so callers can
        inspect ``exc.response.status_code`` and forward the status code.
        """
        resp = httpx.post(
            f"{self._base}/replay/start",
            json={"path": path, "speed": speed},
            timeout=self._timeout,
        )
        resp.raise_for_status()
        return resp.json()  # type: ignore[no-any-return]

    def replay_stop(self) -> dict[str, Any]:
        """Stop an active replay session.

        Raises ``httpx.HTTPStatusError`` on 4xx/5xx.
        """
        resp = httpx.post(f"{self._base}/replay/stop", timeout=self._timeout)
        resp.raise_for_status()
        return resp.json()  # type: ignore[no-any-return]

    def replay_status(self) -> dict[str, Any]:
        """Return current replay status from the media service.

        Raises ``httpx.HTTPStatusError`` on 4xx/5xx.
        """
        return self._get("/replay/status")
