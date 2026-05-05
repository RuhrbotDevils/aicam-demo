"""Main module.

Author: Thomas Klute"""

from __future__ import annotations

import json
import logging
import re
import shutil
import subprocess
import threading
import time
from collections.abc import AsyncIterator
from contextlib import asynccontextmanager
from datetime import UTC, datetime
from pathlib import Path

import cv2
import httpx
import numpy as np
from fastapi import FastAPI, HTTPException, WebSocket, WebSocketDisconnect
from fastapi.responses import FileResponse, Response
from fastapi.staticfiles import StaticFiles
from pydantic import BaseModel as PydanticBaseModel
from starlette.middleware.base import BaseHTTPMiddleware
from starlette.requests import Request

from .config_store import ConfigStore
from .events import broadcaster
from .media_client import MediaServiceClient
from .models import AppConfig, HealthResponse, RuntimeStatus, ServiceStatus, StatusResponse
from .replay_controller import ReplayController
from .state import RuntimeRegistry

BASE_DIR = Path(__file__).resolve().parents[3]
CONFIG_PATH = BASE_DIR / "config.yaml"
STATIC_DIR = BASE_DIR / "apps" / "ui"

# Uvicorn configures its own access/error loggers but leaves the
# ``apps.*`` logger tree handler-less, which silently drops every
# ``logger.info(...)`` in this app. Attach a stdout handler so
# journalctl / docker logs capture them.
from apps.logging_config import configure_stdlib_logging  # noqa: E402

configure_stdlib_logging()

logger = logging.getLogger(__name__)


@asynccontextmanager
async def _lifespan(_app: FastAPI) -> AsyncIterator[None]:
    _start_health_collector()
    _start_detections_collector()
    yield


app = FastAPI(
    title="RoboCup AI Camera Control API",
    version="0.1.0",
    lifespan=_lifespan,
)


class NoCacheStaticMiddleware(BaseHTTPMiddleware):
    """Prevent browsers from caching static files (JS, CSS, HTML)."""

    async def dispatch(self, request: Request, call_next):  # type: ignore[no-untyped-def]
        response = await call_next(request)
        path = request.url.path
        if path.startswith("/static") or path == "/":
            response.headers["Cache-Control"] = "no-cache, no-store, must-revalidate"
            response.headers["Pragma"] = "no-cache"
        return response


app.add_middleware(NoCacheStaticMiddleware)

config_store = ConfigStore(CONFIG_PATH)
runtime = RuntimeRegistry()
media_client = MediaServiceClient()
RECORDINGS_DIR = BASE_DIR / "recordings"
_replay_controller = ReplayController()


# Service health collector - subscribes to service.health on ZMQ,
# caches latest health per service name for the dashboard.
_service_health: dict[str, dict] = {}
_service_health_thread = None

# Latest object_detections cache - subscribes to ai.object_detections
# in a long-lived background thread and indexes by source_module.
# Per-request ZMQ subscribers hit a slow-joiner race with the broker;
# a persistent subscriber avoids that and always has the latest
# detection list ready for the Detection page's preview render.
#
# Each entry is `{"detections": list, "ts": float (monotonic)}`.
_latest_detections: dict[str, dict] = {}
_latest_detections_lock = threading.Lock()
_detections_collector_thread = None
# Stale-detection guard: if no message has arrived for this source
# within the window, return [] so the preview doesn't draw boxes
# from a backend that has stopped publishing (e.g. cpu_detector that
# has just been disabled). Generous enough to cover the slow CPU
# inference cadence (~7 s/frame).
_DETECTIONS_FRESHNESS_S = 30.0


def _start_health_collector() -> None:
    """Start a background thread that collects service health from ZMQ."""
    global _service_health_thread  # noqa: PLW0603
    import threading

    from apps.bus.subscriber import Subscriber

    def _collect() -> None:
        sub = Subscriber(topics=["service.health"])
        while True:
            result = sub.receive(timeout_ms=5000)
            if result is not None:
                _, payload = result
                try:
                    data = json.loads(payload)
                    name = data.get("name")
                    if name:
                        _service_health[name] = data
                except Exception:
                    pass

    _service_health_thread = threading.Thread(target=_collect, daemon=True, name="health-collector")
    _service_health_thread.start()


def _start_detections_collector() -> None:
    """Long-lived ai.object_detections subscriber.

    Indexes the latest detection list per ``source_module``
    (`hailo_meta_export`, `cpu_detector`, ...) so the Detection
    page's preview render can fetch the right backend's boxes
    without spinning up a fresh subscriber per request.
    """
    global _detections_collector_thread  # noqa: PLW0603
    from apps.bus.subscriber import Subscriber

    def _collect() -> None:
        sub = Subscriber(topics=["ai.object_detections"])
        while True:
            result = sub.receive(timeout_ms=5000)
            if result is None:
                continue
            _, payload = result
            try:
                msg = json.loads(payload)
                src = msg.get("source_module")
                if not src:
                    continue
                with _latest_detections_lock:
                    _latest_detections[src] = {
                        "detections": list(msg.get("detections", [])),
                        "ts": time.monotonic(),
                    }
            except Exception:
                pass

    _detections_collector_thread = threading.Thread(
        target=_collect, daemon=True, name="detections-collector"
    )
    _detections_collector_thread.start()


@app.get("/api/v1/health", response_model=HealthResponse)
def get_health() -> HealthResponse:
    # Probe media service health instead of returning stub
    media_status = ServiceStatus.stopped
    try:
        resp = httpx.get(f"{media_client._base}/health", timeout=1.0)
        if resp.status_code == 200:
            media_status = ServiceStatus.running
    except httpx.HTTPError:
        media_status = ServiceStatus.error

    # Telemetry listener runs inside this process (ZMQ broker thread)
    telemetry_status = ServiceStatus.running

    # AI workers are systemd services - check if detection pipeline is active
    ai_status = ServiceStatus.stopped
    try:
        resp = httpx.get(f"{media_client._base}/status", timeout=1.0)
        if resp.status_code == 200:
            data = resp.json()
            if data.get("hailo_available", False):
                ai_status = ServiceStatus.running
    except httpx.HTTPError:
        pass

    return HealthResponse(
        status=runtime.node_status,
        services={
            "control_api": ServiceStatus.running,
            "media_service": media_status,
            "ai_accelerator": ai_status,
            "telemetry": telemetry_status,
        },
    )


@app.get("/api/v1/system/metrics", response_model=None)
def get_system_metrics():  # type: ignore[no-untyped-def]
    """System metrics for the dashboard: CPU, temperature, memory, disk."""
    from .system_metrics import collect_metrics

    cfg = config_store.load()
    recording_dir = cfg.video.recording.directory
    metrics = collect_metrics(recording_dir=recording_dir)

    # Add streaming FPS from media service if streaming is active
    metrics["streaming_fps"] = None
    try:
        resp = httpx.get(f"{media_client._base}/status", timeout=1.0)
        if resp.status_code == 200:
            data = resp.json()
            if data.get("streaming_enabled", False):
                metrics["streaming_fps"] = data.get("streaming_fps")
    except httpx.HTTPError:
        pass

    return metrics


@app.get("/api/v1/services/health", response_model=None)
def get_services_health() -> dict:  # type: ignore[type-arg]
    """Return collected service health data from ZMQ."""
    return {"services": _service_health}


@app.post("/api/v1/system/restart/{service_name}", response_model=None)
def restart_service(service_name: str):  # type: ignore[no-untyped-def]
    """Restart a systemd service by name."""
    from .system_metrics import restart_service as _restart

    ok, msg = _restart(service_name)
    if ok:
        return {"ok": True, "message": msg}
    return Response(
        content=json.dumps({"ok": False, "message": msg}),
        status_code=400,
        media_type="application/json",
    )


@app.get("/api/v1/status", response_model=StatusResponse)
def get_status() -> StatusResponse:
    return StatusResponse(
        status=runtime.node_status,
        features=runtime.feature_states,
        detail={"mode": "skeleton"},
    )


@app.get("/api/v1/config", response_model=AppConfig)
def get_config() -> AppConfig:
    return config_store.load()


@app.put("/api/v1/config", response_model=AppConfig)
def put_config(config: AppConfig) -> AppConfig:
    config_store.save(config)
    return config


# ---------------------------------------------------------------------------
# AI model registry
#
# Models are defined as sidecar JSONs in config/models/. The API never
# exposes or accepts on-disk filenames - display_name is the only public
# identifier. See apps/model_registry.py for the full schema.
# ---------------------------------------------------------------------------


def _model_to_public_dict(md) -> dict:  # type: ignore[no-untyped-def,type-arg]
    """Serialise a ModelDef for the HTTP API.

    Intentionally excludes internal server-side paths (``hef_path``,
    ``postprocess.so_path``, ``postprocess.function_name``) that the UI
    does not need and must not be able to round-trip back to us.
    """
    return {
        "display_name": md.display_name,
        "scope": md.scope.value,
        "active": md.active,
        "input": {
            "width": md.input.width,
            "height": md.input.height,
            "format": md.input.format,
        },
        "output_format": md.postprocess.output_format,
        "runtime": md.runtime,
        "labels": md.labels,
        "notes": md.notes,
    }


def _parse_scope(scope: str | None):  # type: ignore[no-untyped-def]
    from apps.model_registry import ModelScope

    if scope is None:
        return None
    try:
        return ModelScope(scope)
    except ValueError as e:
        raise HTTPException(
            status_code=400,
            detail=(f"unknown scope={scope!r}; expected one of {[s.value for s in ModelScope]}"),
        ) from e


def _cfg_field_for_scope(scope) -> str:  # type: ignore[no-untyped-def]
    from apps.model_registry import ModelScope

    # Special pseudo-scope for CPU object detection - uses the same
    # ModelScope.object_detection for model validation but maps to a
    # separate config field.
    if scope == "object_detection_cpu":
        return "cpu_object_detection_model"
    if scope == ModelScope.object_detection:
        return "object_detection_model"
    raise HTTPException(status_code=400, detail=f"unsupported scope: {scope}")


@app.get("/api/v1/models", response_model=None)
def list_models(scope: str | None = None) -> list:  # type: ignore[type-arg]
    """List available model definitions.

    Query parameters:
        scope: ``object_detection`` | ``landmark_detection`` - if omitted,
               all scopes are returned.

    Hides models that are ``active=false``, have missing ``hef_path``, or
    collide on ``display_name`` with another model in the same scope.
    """
    from apps.model_registry import load_models

    scope_enum = _parse_scope(scope)
    return [_model_to_public_dict(m) for m in load_models(scope=scope_enum)]


@app.get("/api/v1/models/selected", response_model=None)
def get_selected_model(scope: str) -> dict:  # type: ignore[type-arg]
    """Return the currently selected display_name for the given scope."""
    # Handle pseudo-scope before enum conversion
    if scope == "object_detection_cpu":
        field = _cfg_field_for_scope(scope)
        cfg = config_store.load()
        return {"scope": scope, "display_name": getattr(cfg.ai, field)}
    scope_enum = _parse_scope(scope)
    field = _cfg_field_for_scope(scope_enum)
    cfg = config_store.load()
    return {
        "scope": scope_enum.value,
        "display_name": getattr(cfg.ai, field),
    }


@app.put("/api/v1/models/select", response_model=None)
def select_model(body: dict) -> dict:  # type: ignore[type-arg]
    """Select (or clear) a model for a scope.

    Request body:
        ``{"scope": "object_detection", "display_name": "YOLOv8m COCO (Hailo-10H)"}``
        Set ``display_name`` to ``null`` to clear the selection.

    Validation rules:
        - Unknown ``scope`` → 400.
        - Non-null ``display_name`` must resolve in the registry with the
          same scope, ``active=true``, and an existing ``hef_path``.
          Otherwise 404/400 per failure mode.
    """
    from apps.model_registry import ModelScope, load_model_by_display_name

    scope_raw = body.get("scope")
    if not scope_raw:
        raise HTTPException(status_code=400, detail="scope is required")

    # Handle pseudo-scope for CPU detection - validates against
    # object_detection scope but stores in a separate config field.
    if scope_raw == "object_detection_cpu":
        validation_scope = ModelScope.object_detection
        field = _cfg_field_for_scope(scope_raw)
    else:
        validation_scope = _parse_scope(scope_raw)
        field = _cfg_field_for_scope(validation_scope)

    display_name = body.get("display_name")
    if display_name is not None:
        if not isinstance(display_name, str) or not display_name.strip():
            raise HTTPException(
                status_code=400, detail="display_name must be a non-empty string or null"
            )
        md = load_model_by_display_name(display_name, scope=validation_scope)
        if md is None:
            raise HTTPException(
                status_code=404,
                detail=(
                    f"unknown / inactive / unavailable model {display_name!r} in scope {scope_raw}"
                ),
            )

    cfg = config_store.load()
    setattr(cfg.ai, field, display_name)
    config_store.save(cfg)

    # Best-effort: tell the media service its AI config changed so the
    # next pipeline start rebuilds with the new model. A failure here is
    # not fatal - the user will see the change after the next restart.
    try:
        import httpx

        httpx.post(f"{media_client._base}/ai/invalidate", timeout=2.0)
    except Exception as e:  # pragma: no cover - media service optional
        logger.debug("media service /ai/invalidate call failed: %s", e)

    return {"scope": scope_raw, "display_name": display_name}


@app.post("/api/v1/features/{feature}/start")
def start_feature(feature: str) -> dict[str, str]:
    if feature not in runtime.feature_states:
        raise HTTPException(status_code=404, detail="unknown feature")
    runtime.start_feature(feature)
    runtime.node_status = RuntimeStatus.running
    broadcaster.publish({"type": "health", "service": feature, "status": "running"})
    return {"feature": feature, "state": runtime.feature_states[feature]}


@app.post("/api/v1/features/{feature}/stop")
def stop_feature(feature: str) -> dict[str, str]:
    if feature not in runtime.feature_states:
        raise HTTPException(status_code=404, detail="unknown feature")
    runtime.stop_feature(feature)
    broadcaster.publish({"type": "health", "service": feature, "status": "disabled"})
    return {"feature": feature, "state": runtime.feature_states[feature]}


class RecordingStartBody(PydanticBaseModel):
    name: str | None = None


@app.post("/api/v1/recording/start", response_model=None)
def start_recording(body: RecordingStartBody | None = None) -> dict:  # type: ignore[type-arg]
    """Proxy recording start to the media service.

    In cluster master mode, also propagates to all connected slaves.

    Failure paths here used to return ``ok: True`` whenever the
    local media-service call didn't raise, and unconditionally published a
    ``recording started`` health event - even when the media service
    returned ``recording_active: false``. The media service now
    returns HTTP 500 on start failure, but we defend in depth here:
    ``ok: True`` is only set when the local start confirmed
    ``recording_active`` AND every cluster slave reported ``ok``. No
    ``started`` health event is published on failure.

    Guard - refuse to start recording while replay is active.
    Fails open (logs a warning) when the media service is unreachable so
    that a broken media service does not block recording.
    """
    try:
        _ms = media_client.status()
        if getattr(_ms, "input_source", None) == "replay_file":
            raise HTTPException(
                status_code=409,
                detail="cannot start recording while replay is active",
            )
    except HTTPException:
        raise
    except Exception as _guard_exc:
        logger.warning(
            "recording start: could not fetch media status for replay guard (%s) - proceeding",
            _guard_exc,
        )

    with _conversion_lock:
        if _conversion_state["active"]:
            return {"ok": False, "error": "Cannot record while MP4 conversion is running"}

    name = body.name if body else None

    # --- local media service ---
    try:
        status = media_client.start_recording(name=name)
    except Exception as e:
        logger.warning("recording start: local media service rejected request: %s", e)
        return {"ok": False, "error": f"media service: {e}"}

    if not status.recording_active:
        logger.warning(
            "recording start: media service returned 200 but recording_active=false",
        )
        return {
            "ok": False,
            "error": "media service did not activate recording",
            "media_status": status.model_dump(),
        }

    # Local start confirmed - only now publish the health event.
    broadcaster.publish({"type": "health", "service": "recording", "status": "started"})

    return {"ok": True, "media_status": status.model_dump()}


@app.post("/api/v1/recording/stop", response_model=None)
def stop_recording() -> dict:  # type: ignore[type-arg]
    """Proxy recording stop to the media service."""
    try:
        status = media_client.stop_recording()
        broadcaster.publish({"type": "health", "service": "recording", "status": "stopped"})
        return {"ok": True, "media_status": status.model_dump()}
    except Exception as e:
        return {"ok": False, "error": str(e)}


@app.get("/api/v1/recording/status", response_model=None)
def recording_status() -> dict:  # type: ignore[type-arg]
    """Get current recording status from the media service."""
    try:
        status = media_client.status()
        return {
            "recording_active": status.recording_active,
            "recording_started_at": status.recording_started_at,
            "media_status": status.model_dump(),
        }
    except Exception as e:
        return {"recording_active": False, "error": str(e)}


def _grab_camera_frame() -> np.ndarray | None:
    """Read the latest raw NV12 frame from frame export and decode to BGR."""
    try:
        raw_path = Path("/tmp/aicam-frames/latest.raw")
        if not raw_path.exists():
            return None
        data = raw_path.read_bytes()
        # Read camera dimensions from config
        cfg = config_store.load()
        w = cfg.video.camera.width
        h = cfg.video.camera.height
        expected = int(w * h * 1.5)  # NV12: 1.5 bytes per pixel
        if len(data) != expected:
            return None
        nv12 = np.frombuffer(data, dtype=np.uint8).reshape(int(h * 1.5), w)
        return cv2.cvtColor(nv12, cv2.COLOR_YUV2BGR_NV12)
    except Exception:
        return None


@app.get("/api/v1/detection/status", response_model=None)
def detection_status() -> dict:  # type: ignore[type-arg]
    """Get detection pipeline status - both Hailo and CPU side-by-side.

    Always reports both backends so the Detection UI can render two
    independent panels (Hailo / CPU). Each side reports its selected
    model and whether the backend is currently active.
    """
    from apps.model_registry import load_models

    cfg = config_store.load()

    def _registry_payload(display_name: str | None) -> dict | None:
        if not display_name:
            return None
        for m in load_models():
            if m.display_name == display_name:
                labels = m.labels
                if isinstance(labels, list):
                    labels_str = ", ".join(labels)
                elif isinstance(labels, str):
                    labels_str = labels
                else:
                    labels_str = None
                return {
                    "display_name": m.display_name,
                    "input_width": m.input.width,
                    "input_height": m.input.height,
                    "input_format": m.input.format,
                    "output_format": m.postprocess.output_format or "bbox",
                    "labels": labels_str,
                    "notes": m.notes,
                }
        return None

    # Hailo side: prefer the live media-service answer (has runtime
    # `active` truth) and fall back to the registry view if the media
    # service is unreachable.
    hailo_active = False
    hailo_model: dict | None = None
    try:
        media = media_client.detection_status()
        hailo_active = bool(media.get("active"))
        hailo_model = media.get("object_detection")
    except Exception:
        hailo_model = _registry_payload(cfg.ai.object_detection_model)

    # CPU side: registry view + feature flag tells us if it should run.
    cpu_model = _registry_payload(cfg.ai.cpu_object_detection_model)
    cpu_active = bool(cfg.features.cpu_detection and cpu_model is not None)

    return {
        "hailo": {"active": hailo_active, "model": hailo_model},
        "cpu": {"active": cpu_active, "model": cpu_model},
    }


def _grab_latest_detections(timeout_ms: int = 300) -> list:  # type: ignore[type-arg]
    """Return the latest cached detections for the selected backend.

    Reads from `_latest_detections`, populated by the persistent
    `_start_detections_collector` background thread. The selected
    backend is `cpu_detector` when `features.cpu_detection` is on,
    else `hailo_meta_export`. Detections older than
    `_DETECTIONS_FRESHNESS_S` are treated as stale and an empty list
    is returned, so the preview does not keep drawing boxes from a
    backend that has stopped publishing.

    `timeout_ms` is accepted for API compatibility but not used -
    the cache is always immediately available.
    """
    del timeout_ms  # cache lookup is synchronous; kept for compat
    cfg = config_store.load()
    want_source = "cpu_detector" if cfg.features.cpu_detection else "hailo_meta_export"
    with _latest_detections_lock:
        entry = _latest_detections.get(want_source)
    if entry is None:
        return []
    if time.monotonic() - entry["ts"] > _DETECTIONS_FRESHNESS_S:
        return []
    return list(entry["detections"])


def _draw_detection_boxes(frame: np.ndarray, detections: list[dict]) -> None:
    """Compatibility wrapper around `apps.ai_worker.draw.draw_detection_boxes`.

    The shared helper is the source of truth so the CPU detector and
    the control API draw boxes identically.
    """
    from apps.ai_worker.draw import draw_detection_boxes

    draw_detection_boxes(frame, detections)


_CPU_ANNOTATED_PATH = Path("/tmp/aicam-frames/cpu_annotated.jpg")
_CPU_RAW_PATH = Path("/tmp/aicam-frames/cpu_raw.jpg")
_CPU_DETECTIONS_PATH = Path("/tmp/aicam-frames/cpu_detections.json")


def _load_cpu_session(max_age_s: float) -> dict | None:  # type: ignore[type-arg]
    """Return the latest CPU inference session if it is fresh.

    The session is the (raw_jpeg, annotated_jpeg, sidecar_json) triple
    written atomically by ``cpu_detector._write_inference_artifacts``.
    Freshness is gated on ``cpu_detections.json``'s mtime, which the
    worker renames last so the JPEGs are guaranteed present from the
    same inference pass when the gate passes. Returns ``None`` if the
    sidecar is missing, stale, or unparseable, or either JPEG is
    missing.
    """
    try:
        sidecar_mtime = _CPU_DETECTIONS_PATH.stat().st_mtime
    except FileNotFoundError:
        return None
    except OSError as e:
        logger.warning("snapshot: failed to stat cpu_detections.json: %s", e)
        return None
    if time.time() - sidecar_mtime > max_age_s:
        return None
    try:
        sidecar = json.loads(_CPU_DETECTIONS_PATH.read_text())
        raw_bytes = _CPU_RAW_PATH.read_bytes()
        annotated_bytes = _CPU_ANNOTATED_PATH.read_bytes()
    except (OSError, json.JSONDecodeError) as e:
        logger.warning("snapshot: failed to read cpu session artifacts: %s", e)
        return None
    return {
        "sidecar": sidecar,
        "raw_bytes": raw_bytes,
        "annotated_bytes": annotated_bytes,
    }


@app.post("/api/v1/detection/cpu_snap", response_model=None)
def trigger_cpu_snap():  # type: ignore[no-untyped-def]
    """Run one CPU inference on demand and write cpu_* artifacts.

    The CPU detector is not a long-running service in the demo build -
    it only runs when the operator hits Snap with the CPU model
    selected. This endpoint loads the YOLO model lazily (cached after
    first call) and runs one inference. The downstream
    `/object_detection_preview/frame` route then serves the freshly
    written `/tmp/aicam-frames/cpu_annotated.jpg`.

    Returns 200 with `{ok: true, frames_processed: N}` on success;
    400 with `{ok: false, error: "..."}` when no CPU model is
    configured; 500 on inference failure.
    """
    from apps.ai_worker.cpu_detector import run_oneshot_snap

    result = run_oneshot_snap()
    if not result.get("ok"):
        # 400 for "no model selected" (configuration issue), 500 for
        # everything else (runtime / inference errors).
        msg = str(result.get("error", "")).lower()
        status = 400 if "no cpu model" in msg or "no model" in msg else 500
        return Response(
            content=json.dumps(result),
            status_code=status,
            media_type="application/json",
        )
    return result


@app.post("/api/v1/detection/snapshot", response_model=None)
def take_detection_snapshot():  # type: ignore[no-untyped-def]
    """Save raw + annotated frame and detection sidecar JSON to snapshots/.

    In CPU mode, prefer the worker-side session (raw + annotated +
    sidecar) so all three artifacts come from the same inference pass.
    The bus-cached fallback path composes a current camera frame with
    cached detections, which drift apart for any moving object since
    CPU inference takes several seconds per frame.
    """
    cfg = config_store.load()
    camera_id = cfg.node.id

    # Timestamp (millisecond precision, filesystem-safe)
    now = datetime.now(UTC)
    ts_file = now.strftime("%Y-%m-%dT%H-%M-%S") + f"-{now.microsecond // 1000:03d}"
    ts_iso = now.isoformat()

    snap_dir = Path("snapshots")
    snap_dir.mkdir(parents=True, exist_ok=True)
    base = f"{ts_file}_{camera_id}"
    raw_path = snap_dir / f"{base}.jpg"
    ann_path = snap_dir / f"{base}_annotated.jpg"
    json_path = snap_dir / f"{base}.json"

    # Path A - CPU mode: copy the worker session if fresh.
    if cfg.features.cpu_detection:
        session = _load_cpu_session(_DETECTIONS_FRESHNESS_S)
        if session is not None:
            raw_path.write_bytes(session["raw_bytes"])
            ann_path.write_bytes(session["annotated_bytes"])
            worker_sidecar = session["sidecar"]
            sidecar = {
                "timestamp": ts_iso,
                "camera_id": camera_id,
                "image_width": worker_sidecar.get("image_width"),
                "image_height": worker_sidecar.get("image_height"),
                "source": "cpu_detector",
                "inference_timestamp": worker_sidecar.get("timestamp"),
                "frame_id": worker_sidecar.get("frame_id"),
                "detections": worker_sidecar.get("detections", []),
            }
            json_path.write_text(json.dumps(sidecar, indent=2))
            count = len(sidecar["detections"])
            logger.info(
                "detection: snapshot saved to %s (%d detections, source=cpu_detector)",
                snap_dir / base,
                count,
            )
            return {
                "ok": True,
                "path": str(snap_dir / base),
                "timestamp": ts_iso,
                "detection_count": count,
            }
        logger.warning(
            "detection: cpu session artifacts missing or stale; falling back to "
            "bus-cached composition (boxes may not be temporally aligned)"
        )

    # Path B - fallback: compose current camera frame with cached detections.
    frame = _grab_camera_frame()
    if frame is None:
        return {"ok": False, "error": "Failed to grab camera frame"}
    h_px, w_px = frame.shape[:2]

    detections = _grab_latest_detections()

    annotated = frame.copy()
    _draw_detection_boxes(annotated, detections)

    cv2.imwrite(str(raw_path), frame, [cv2.IMWRITE_JPEG_QUALITY, 95])
    cv2.imwrite(str(ann_path), annotated, [cv2.IMWRITE_JPEG_QUALITY, 95])

    sidecar_detections = [
        {
            "class": det.get("class", "unknown"),
            "bbox_xywh": det.get("bbox_xywh", []),
            "confidence": det.get("confidence", 0),
        }
        for det in detections
    ]

    sidecar = {
        "timestamp": ts_iso,
        "camera_id": camera_id,
        "image_width": w_px,
        "image_height": h_px,
        "source": "bus_cache",
        "detections": sidecar_detections,
    }
    json_path.write_text(json.dumps(sidecar, indent=2))

    logger.info(
        "detection: snapshot saved to %s (%d detections, source=bus_cache)",
        snap_dir / base,
        len(detections),
    )
    return {
        "ok": True,
        "path": str(snap_dir / base),
        "timestamp": ts_iso,
        "detection_count": len(detections),
    }


@app.get("/api/v1/object_detection_preview/frame", response_model=None)
def get_object_detection_preview_frame():  # type: ignore[no-untyped-def]
    """object_detection_preview - annotated frame for the Detection page.

    In CPU mode, serve the cpu_detector's own annotated JPEG so the
    boxes are aligned with the exact frame the model saw. Falls back
    to server-side composition if the cpu_annotated.jpg is missing or
    stale.

    In Hailo mode, proxy the pre-annotated frame from the media
    service (`hailooverlay → jpegenc` already produces a
    self-contained JPEG with the boxes drawn).
    """
    cfg = config_store.load()
    use_server_side = cfg.features.cpu_detection

    if not use_server_side:
        try:
            resp = httpx.get(f"{media_client._base}/object_detection_preview/frame", timeout=5.0)
            if resp.status_code == 200:
                return Response(content=resp.content, media_type="image/jpeg")
        except httpx.HTTPError:
            pass
        # Hailo unavailable - fall through to server-side rendering
    else:
        # CPU mode - serve cpu_detector's own annotated JPEG when fresh.
        # Stale cap matches the detection-cache freshness window so the
        # preview goes blank if the worker has stopped publishing.
        try:
            mtime = _CPU_ANNOTATED_PATH.stat().st_mtime
            if time.time() - mtime <= _DETECTIONS_FRESHNESS_S:
                return Response(
                    content=_CPU_ANNOTATED_PATH.read_bytes(),
                    media_type="image/jpeg",
                )
        except FileNotFoundError:
            pass
        except OSError as e:
            logger.warning("preview: failed to read cpu_annotated.jpg: %s", e)
        # cpu_annotated stale or missing - fall through to server-side
        # composition so the operator still gets *some* frame while the
        # worker is warming up.

    # Server-side rendering (Hailo fallback or CPU warm-up).
    frame = _grab_camera_frame()
    if frame is None:
        return Response(status_code=204)
    detections = _grab_latest_detections(timeout_ms=100)
    if detections:
        _draw_detection_boxes(frame, detections)
    _, jpeg = cv2.imencode(".jpg", frame, [cv2.IMWRITE_JPEG_QUALITY, 85])
    return Response(content=jpeg.tobytes(), media_type="image/jpeg")


# ---------------------------------------------------------------------------
# Streaming proxy routes + game state endpoint
# ---------------------------------------------------------------------------


@app.post("/api/v1/streaming/start", response_model=None)
async def start_streaming(request: Request):  # type: ignore[no-untyped-def]
    """Start streaming - uses the configured RTMP URL.

    If no body is provided, reads rtmp_url from config. Returns 400 when
    no URL is configured. Benchmark mode (fakesink) is a build-time
    toggle (`streaming_benchmark` Cargo feature) and is not exposed via
    the API.
    """
    import httpx

    try:
        body = None
        ct = request.headers.get("content-type", "")
        if "json" in ct:
            body = await request.json()

        if not body or not body.get("rtmp_url"):
            cfg = config_store.load()
            sc = cfg.video.streaming
            if not sc.rtmp_url:
                return Response(
                    content=json.dumps({"error": "No RTMP URL configured"}),
                    status_code=400,
                    media_type="application/json",
                )
            url = sc.rtmp_url.rstrip("/")
            if sc.stream_key:
                url = f"{url}/{sc.stream_key}"
            # Pass the configured bitrate through so the media service
            # uses the user-tunable streaming bitrate (default 4000 kbps,
            # appropriate for 720p) instead of its internal
            # recording-derived 8 Mbps ceiling.
            body = {"rtmp_url": url, "bitrate_kbps": sc.bitrate_kbps}

        resp = httpx.post(
            f"{media_client._base}/streaming/start",
            json=body,
            timeout=10.0,
        )
    except httpx.HTTPError:
        return Response(status_code=503)
    return Response(
        content=resp.content, status_code=resp.status_code, media_type="application/json"
    )


@app.post("/api/v1/streaming/stop", response_model=None)
def stop_streaming():  # type: ignore[no-untyped-def]
    """Proxy to media service POST /streaming/stop."""
    import httpx

    try:
        resp = httpx.post(f"{media_client._base}/streaming/stop", timeout=10.0)
    except httpx.HTTPError:
        return Response(status_code=503)
    return Response(
        content=resp.content, status_code=resp.status_code, media_type="application/json"
    )


# Cache for stream-fps delta computation. Holds the last
# `stream_buffer_count` + `monotonic_ns` we observed on /pipeline/stats
# so successive calls to /api/v1/streaming/status can compute fps
# from the delta. Module-level (single-process control_api) - no
# thread synchronisation since FastAPI runs handlers serially per
# worker on uvicorn's default event loop.
_stream_fps_last: dict[str, int | None] = {"count": None, "ns": None}


@app.get("/api/v1/streaming/status", response_model=None)
def get_streaming_status():  # type: ignore[no-untyped-def]
    """Streaming-relevant fields from the media service.

    Returns `{streaming_enabled, streaming_error, streaming_fps,
    rtmp_url_masked}` so the UI can show whether the stream actually
    came up and surface any rtmpsink connect failure. The configured
    URL comes from config; the key is masked so we never echo it back.

    `streaming_fps` is computed from the delta between successive
    `stream_buffer_count` samples on `/pipeline/stats` (the media
    service exposes the raw monotonic counter; computing fps here keeps
    the media service stateless w.r.t. windowing).
    """
    cfg = config_store.load()
    sc = cfg.video.streaming
    masked_url = sc.rtmp_url.rstrip("/")
    if sc.stream_key:
        masked_url = f"{masked_url}/{'*' * 8}"

    out: dict[str, object] = {
        "streaming_enabled": False,
        "streaming_error": None,
        "streaming_fps": None,
        "rtmp_url_masked": masked_url,
    }
    try:
        resp = httpx.get(f"{media_client._base}/status", timeout=2.0)
        if resp.status_code == 200:
            data = resp.json()
            out["streaming_enabled"] = bool(data.get("streaming_enabled", False))
            out["streaming_error"] = data.get("streaming_error")
    except httpx.HTTPError:
        pass

    # Sample /pipeline/stats and compute fps from delta vs the last
    # sample. Resets when streaming flips off so the next stream starts
    # the average fresh.
    try:
        resp = httpx.get(f"{media_client._base}/pipeline/stats", timeout=2.0)
        if resp.status_code == 200:
            d = resp.json()
            count = int(d.get("stream_buffer_count", 0))
            ns = int(d.get("monotonic_ns", 0))
            last_count = _stream_fps_last["count"]
            last_ns = _stream_fps_last["ns"]
            if (
                out["streaming_enabled"]
                and last_count is not None
                and last_ns is not None
                and ns > last_ns
                and count >= last_count
            ):
                d_count = count - last_count
                d_s = (ns - last_ns) / 1_000_000_000.0
                if d_s >= 0.05:
                    out["streaming_fps"] = round(d_count / d_s, 1)
            _stream_fps_last["count"] = count
            _stream_fps_last["ns"] = ns
            if not out["streaming_enabled"]:
                # Reset so the next session computes from its own first sample.
                _stream_fps_last["count"] = None
                _stream_fps_last["ns"] = None
    except httpx.HTTPError:
        pass
    return out


@app.get("/api/v1/streaming/overlay", response_model=None)
def get_overlay_text():  # type: ignore[no-untyped-def]
    """Proxy to media service GET /overlay/text."""
    import httpx

    try:
        resp = httpx.get(f"{media_client._base}/overlay/text", timeout=5.0)
    except httpx.HTTPError:
        return Response(status_code=503)
    return Response(
        content=resp.content, status_code=resp.status_code, media_type="application/json"
    )


@app.put("/api/v1/streaming/overlay", response_model=None)
async def put_overlay_text(request: Request):  # type: ignore[no-untyped-def]
    """Proxy to media service PUT /overlay/text."""
    import httpx

    try:
        body = await request.json()
        resp = httpx.put(
            f"{media_client._base}/overlay/text",
            json=body,
            timeout=5.0,
        )
    except httpx.HTTPError:
        return Response(status_code=503)
    return Response(
        content=resp.content, status_code=resp.status_code, media_type="application/json"
    )


# ---------------------------------------------------------------------------
# Playback (replay) endpoints
# ---------------------------------------------------------------------------


@app.get("/api/v1/playback/sessions", response_model=None)
def list_playback_sessions():  # type: ignore[no-untyped-def]
    """Scan ``playback/`` for subdirectories with a ``playback.yaml``."""
    from apps.control_api.app.replay_controller import scan_sessions, session_to_dict

    return [session_to_dict(s) for s in scan_sessions()]


@app.post("/api/v1/playback/start", response_model=None)
def start_playback(request: Request):  # type: ignore[no-untyped-def]
    """Start a replay session.

    Body: ``{"session": "<dir_name>", "half": 1, "speed": 1.0}``
    """
    import asyncio

    from apps.control_api.app.replay_controller import scan_sessions

    body = asyncio.get_event_loop().run_until_complete(request.json())
    dir_name = body.get("session")
    half = int(body.get("half", 1))
    speed = float(body.get("speed", 1.0))

    sessions = scan_sessions()
    match = next((s for s in sessions if s.dir_name == dir_name), None)
    if match is None:
        return Response(status_code=404, content=f"Session not found: {dir_name}")
    if half == 2 and not match.has_half2:
        return Response(status_code=400, content="Half 2 not available for this session")

    _replay_controller.start(match, half=half, speed=speed)
    return {"status": "started", "session": match.name, "half": half, "speed": speed}


@app.post("/api/v1/playback/stop", response_model=None)
def stop_playback():  # type: ignore[no-untyped-def]
    """Stop the current replay."""
    _replay_controller.stop()
    return {"status": "stopped"}


@app.get("/api/v1/playback/status", response_model=None)
def playback_status():  # type: ignore[no-untyped-def]
    """Return the current replay state and progress."""
    return _replay_controller.status()


# ---------------------------------------------------------------------------
# Recording-replay endpoints - session-scoped replay via media svc
# ---------------------------------------------------------------------------


class ReplayStartBody(PydanticBaseModel):
    session_id: str
    # Optional playback rate multiplier. Defaults to 1.0 (realtime).
    # 0.0 selects the UI's "Max" option - drain at decode speed.
    # Forwarded verbatim to the media service.
    speed: float = 1.0


def _start_replay_for_session(session_id: str, speed: float = 1.0) -> dict:  # type: ignore[type-arg]
    """Resolve *session_id* to its recording.mp4 and forward to media service.

    Returns the raw dict from the media service on success.
    Raises :class:`HTTPException` on validation or resolution failures.
    Raises :class:`httpx.HTTPStatusError` (from ``media_client.replay_start``)
    on media-service-level rejections so callers can forward the status code.
    """
    if not _SAFE_SESSION_ID.match(session_id):
        raise HTTPException(status_code=400, detail="Invalid session ID")

    session_dir = RECORDINGS_DIR / session_id
    if not session_dir.exists() or not session_dir.is_dir():
        raise HTTPException(status_code=404, detail="Session not found")

    mp4_path = session_dir / "recording.mp4"
    if not mp4_path.exists():
        raise HTTPException(
            status_code=409,
            detail="recording.mp4 not found - convert the recording first",
        )

    abs_path = str(mp4_path.resolve())
    return media_client.replay_start(abs_path, speed=speed)


@app.post("/api/v1/replay/start", response_model=None)
def start_replay(body: ReplayStartBody):  # type: ignore[no-untyped-def]
    """Start replay for a recorded session.

    Resolves ``session_id`` → ``RECORDINGS_DIR/<session_id>/recording.mp4``
    and forwards the absolute path to the media service ``/replay/start``.

    - 400 if ``session_id`` fails the safe-name regex.
    - 404 if the session directory is missing.
    - 409 if ``recording.mp4`` is missing (convert first).
    - 409 if the media service rejects (e.g. recording currently active).
    """
    try:
        payload = _start_replay_for_session(body.session_id, speed=body.speed)
        return payload
    except HTTPException:
        raise
    except Exception as exc:
        # Forward 4xx/5xx from media service; turn 5xx/conn errors into 502.
        if hasattr(exc, "response"):
            status_code = exc.response.status_code  # type: ignore[attr-defined]
            try:
                detail = exc.response.json()  # type: ignore[attr-defined]
            except Exception:
                detail = exc.response.text  # type: ignore[attr-defined]
            raise HTTPException(status_code=status_code, detail=detail) from exc
        raise HTTPException(status_code=502, detail=f"media service error: {exc}") from exc


@app.post("/api/v1/replay/stop", response_model=None)
def stop_replay():  # type: ignore[no-untyped-def]
    """Stop an active replay session - proxies to media service /replay/stop."""
    try:
        return media_client.replay_stop()
    except Exception as exc:
        if hasattr(exc, "response"):
            status_code = exc.response.status_code  # type: ignore[attr-defined]
            try:
                detail = exc.response.json()  # type: ignore[attr-defined]
            except Exception:
                detail = exc.response.text  # type: ignore[attr-defined]
            raise HTTPException(status_code=status_code, detail=detail) from exc
        raise HTTPException(status_code=502, detail=f"media service error: {exc}") from exc


@app.get("/api/v1/replay/status", response_model=None)
def get_replay_status():  # type: ignore[no-untyped-def]
    """Return current replay status from the media service."""
    try:
        return media_client.replay_status()
    except Exception as exc:
        if hasattr(exc, "response"):
            status_code = exc.response.status_code  # type: ignore[attr-defined]
            try:
                detail = exc.response.json()  # type: ignore[attr-defined]
            except Exception:
                detail = exc.response.text  # type: ignore[attr-defined]
            raise HTTPException(status_code=status_code, detail=detail) from exc
        raise HTTPException(status_code=502, detail=f"media service error: {exc}") from exc


@app.get("/api/v1/recording/sessions", response_model=None)
def list_recording_sessions() -> list:  # type: ignore[type-arg]
    """List completed recording sessions by scanning recordings/ for session.json files."""
    sessions: list[dict] = []  # type: ignore[type-arg]
    if not RECORDINGS_DIR.exists():
        return sessions
    for session_dir in sorted(RECORDINGS_DIR.iterdir(), reverse=True):
        meta_path = session_dir / "session.json"
        if meta_path.exists():
            try:
                data = json.loads(meta_path.read_text())
                data["has_mp4"] = (session_dir / "recording.mp4").exists()
                sessions.append(data)
            except (json.JSONDecodeError, OSError):
                pass
    return sessions


_SAFE_SESSION_ID = re.compile(r"^[\w\-]+$")


@app.delete("/api/v1/recording/sessions/{session_id}", response_model=None)
def delete_recording_session(session_id: str) -> dict:  # type: ignore[type-arg]
    """Delete a recording session directory from disc."""
    if not _SAFE_SESSION_ID.match(session_id):
        raise HTTPException(status_code=400, detail="Invalid session ID")

    session_dir = RECORDINGS_DIR / session_id
    if not session_dir.exists() or not session_dir.is_dir():
        raise HTTPException(status_code=404, detail="Session not found")

    # Prevent deletion of currently recording session
    try:
        status = media_client.status()
        if status.recording_active:
            meta_path = session_dir / "session.json"
            if meta_path.exists():
                data = json.loads(meta_path.read_text())
                if data.get("status") == "recording":
                    raise HTTPException(status_code=409, detail="Cannot delete active recording")
    except HTTPException:
        raise
    except Exception:
        pass  # media service unreachable - allow deletion

    shutil.rmtree(session_dir)
    return {"ok": True, "session_id": session_id}


_conversion_lock = threading.Lock()
_conversion_state: dict = {  # type: ignore[type-arg]
    "active": False,
    "session_id": None,
    "error": None,
    "progress": 0,
}
_convert_logger = logging.getLogger("mp4_convert")


def _get_duration_secs(session_dir: Path) -> float | None:
    """Read session duration from session.json for progress calculation."""
    meta = session_dir / "session.json"
    if meta.exists():
        try:
            data = json.loads(meta.read_text())
            val = data.get("duration_s")
            return float(val) if val is not None else None
        except Exception:
            pass
    return None


def _run_ffmpeg_conversion(session_dir: Path, session_id: str) -> None:
    """Run ffmpeg in a background thread to mux H.264+FLAC into MP4."""
    try:
        video_path = session_dir / "video.h264"
        audio_path = session_dir / "audio.flac"
        output_path = session_dir / "recording.mp4"

        if not video_path.exists():
            with _conversion_lock:
                _conversion_state["error"] = "No video.h264 found"
                _conversion_state["active"] = False
            return

        duration = _get_duration_secs(session_dir)

        cmd = ["ffmpeg", "-y", "-progress", "pipe:1"]
        if audio_path.exists() and audio_path.stat().st_size > 0:
            cmd += ["-i", str(video_path), "-i", str(audio_path), "-c", "copy", str(output_path)]
        else:
            cmd += ["-i", str(video_path), "-c", "copy", str(output_path)]

        _convert_logger.info("Starting MP4 conversion: %s", " ".join(cmd))
        proc = subprocess.Popen(  # noqa: S603
            cmd, stdout=subprocess.PIPE, stderr=subprocess.PIPE
        )

        # Parse progress from ffmpeg stdout
        if proc.stdout:
            for line in proc.stdout:
                text = line.decode(errors="replace").strip()
                if text.startswith("out_time_us=") and duration and duration > 0:
                    try:
                        us = int(text.split("=")[1])
                        pct = min(99, int((us / 1_000_000) / duration * 100))
                        with _conversion_lock:
                            _conversion_state["progress"] = pct
                    except (ValueError, ZeroDivisionError):
                        pass

        proc.wait(timeout=600)

        with _conversion_lock:
            if proc.returncode != 0:
                stderr = (proc.stderr.read() if proc.stderr else b"").decode(errors="replace")[
                    -500:
                ]
                _conversion_state["error"] = f"ffmpeg failed (rc={proc.returncode}): {stderr}"
                _convert_logger.error("ffmpeg failed: %s", stderr)
            else:
                _conversion_state["error"] = None
                _conversion_state["progress"] = 100
                _convert_logger.info("MP4 conversion complete: %s", output_path)
            _conversion_state["active"] = False
    except subprocess.TimeoutExpired:
        with _conversion_lock:
            _conversion_state["error"] = "Conversion timed out (>10min)"
            _conversion_state["active"] = False
    except Exception as e:
        with _conversion_lock:
            _conversion_state["error"] = str(e)
            _conversion_state["active"] = False


@app.post("/api/v1/recording/sessions/{session_id}/convert", response_model=None)
def start_conversion(session_id: str) -> dict:  # type: ignore[type-arg]
    """Start MP4 conversion for a recording session."""
    if not _SAFE_SESSION_ID.match(session_id):
        raise HTTPException(status_code=400, detail="Invalid session ID")

    # Guard: no recording active
    try:
        status = media_client.status()
        if status.recording_active:
            raise HTTPException(status_code=409, detail="Cannot convert while recording is active")
    except HTTPException:
        raise
    except Exception:
        pass

    # Guard: not in automatic mode
    cfg = config_store.load()
    if cfg.video.recording.recording_mode == "automatic":
        raise HTTPException(status_code=409, detail="Cannot convert in automatic recording mode")

    # Guard: no other conversion running
    with _conversion_lock:
        if _conversion_state["active"]:
            raise HTTPException(
                status_code=409,
                detail=f"Conversion already running for {_conversion_state['session_id']}",
            )

    session_dir = RECORDINGS_DIR / session_id
    if not session_dir.exists():
        raise HTTPException(status_code=404, detail="Session not found")

    # Check if already converted
    if (session_dir / "recording.mp4").exists():
        return {"ok": True, "status": "already_converted", "session_id": session_id}

    with _conversion_lock:
        _conversion_state["active"] = True
        _conversion_state["session_id"] = session_id
        _conversion_state["error"] = None
        _conversion_state["progress"] = 0

    thread = threading.Thread(
        target=_run_ffmpeg_conversion, args=(session_dir, session_id), daemon=True
    )
    thread.start()
    return {"ok": True, "status": "started", "session_id": session_id}


@app.get("/api/v1/recording/sessions/{session_id}/convert", response_model=None)
def get_conversion_status(session_id: str) -> dict:  # type: ignore[type-arg]
    """Poll MP4 conversion status."""
    with _conversion_lock:
        if _conversion_state["active"] and _conversion_state["session_id"] == session_id:
            return {
                "status": "converting",
                "session_id": session_id,
                "progress": _conversion_state["progress"],
            }
        if _conversion_state["session_id"] == session_id and _conversion_state["error"]:
            return {
                "status": "failed",
                "session_id": session_id,
                "error": _conversion_state["error"],
            }

    # Check if MP4 exists
    mp4_path = RECORDINGS_DIR / session_id / "recording.mp4"
    if mp4_path.exists():
        return {"status": "completed", "session_id": session_id}
    return {"status": "not_started", "session_id": session_id}


@app.get("/api/v1/recording/convert/status", response_model=None)
def get_global_conversion_status() -> dict:  # type: ignore[type-arg]
    """Get global conversion state (is any conversion running?)."""
    with _conversion_lock:
        return {
            "active": _conversion_state["active"],
            "session_id": _conversion_state["session_id"],
        }


@app.websocket("/api/v1/events/ws")
async def events_ws(websocket: WebSocket) -> None:
    import asyncio
    import queue as stdlib_queue

    await websocket.accept()
    q = broadcaster.connect()
    try:
        while True:
            try:
                event = q.get_nowait()
                await websocket.send_text(event)
            except stdlib_queue.Empty:
                await asyncio.sleep(0.05)
    except WebSocketDisconnect:
        pass
    finally:
        broadcaster.disconnect(q)


@app.get("/api/v1/camera_preview/frame", response_model=None)
def get_camera_preview_frame() -> Response:
    """camera_preview - JPEG frame for the Dashboard and Recording pages.

    Reads the latest raw NV12 frame from /tmp/aicam-frames/latest.raw,
    decodes it and encodes as JPEG. Returns 204 when no frame is available.
    """
    frame = _grab_camera_frame()
    if frame is None:
        return Response(status_code=204)
    _, jpeg = cv2.imencode(".jpg", frame, [cv2.IMWRITE_JPEG_QUALITY, 85])
    return Response(content=jpeg.tobytes(), media_type="image/jpeg")


app.mount("/static", StaticFiles(directory=STATIC_DIR), name="static")
app.mount("/config", StaticFiles(directory=BASE_DIR / "config"), name="config")


@app.get("/")
def index() -> FileResponse:
    return FileResponse(STATIC_DIR / "index.html")
