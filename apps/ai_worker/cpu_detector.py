"""CPU object detector - runs PyTorch/Ultralytics inference on camera frames.

Subscribes to ``media.frame_refs`` on ZMQ, grabs the camera frame via
HTTP, runs a YOLO .pt model, and publishes ``ObjectDetectionsMessage``
+ ``RobotAttributesMessage`` on the standard ZMQ topics so the tracker
and downstream pipeline work identically to the Hailo path.

Only starts when ``features.cpu_detection`` is enabled and
``ai.cpu_object_detection_model`` is set in config.yaml. The run loop
re-checks ``features.cpu_detection`` periodically and exits cleanly
when the operator toggles it off, so an idle detector does not sit
around publishing stale messages alongside the Hailo pipeline.

Author: Thomas Klute"""

from __future__ import annotations

import json
import logging
import signal
import sys
import time
import uuid
from collections.abc import Callable
from datetime import UTC, datetime
from pathlib import Path

import cv2
import httpx
import numpy as np

from apps.bus.publisher import Publisher
from apps.bus.subscriber import Subscriber
from apps.schemas import (
    FrameReferenceMessage,
    ObjectDetectionsMessage,
)
from apps.schemas.object_detections import DetectorModel

logger = logging.getLogger(__name__)

DETECTIONS_TOPIC = "ai.object_detections"
FRAME_REF_TOPIC = "media.frame_refs"
MEDIA_FRAME_URL = "http://127.0.0.1:8000/api/v1/camera_preview/frame"

# Output directory for the worker-side inference session artifacts
# (cpu_raw.jpg, cpu_annotated.jpg, cpu_detections.json). Module-level
# so tests can redirect the path without monkeypatching ``Path``.
INFERENCE_ARTIFACTS_DIR = Path("/tmp/aicam-frames")


class FeatureFlagPoller:
    """Cache a boolean config flag and re-read it at most once per interval.

    The loader callback returns the current flag value. The poller keeps
    the most recent result for ``interval_s`` seconds and only re-invokes
    the loader when that window expires. Loader exceptions are logged and
    the previous value is preserved so transient read errors do not cause
    the caller to mistakenly shut down.
    """

    def __init__(
        self,
        loader: Callable[[], bool],
        interval_s: float = 5.0,
        now: Callable[[], float] = time.monotonic,
    ):
        self._loader = loader
        self._interval_s = interval_s
        self._now = now
        self._last_read_at: float | None = None
        self._value: bool = True  # optimistic default: assume enabled

    def enabled(self) -> bool:
        t = self._now()
        if self._last_read_at is not None and (t - self._last_read_at) < self._interval_s:
            return self._value
        try:
            self._value = bool(self._loader())
        except Exception as e:  # noqa: BLE001 - log and keep last known value
            logger.warning("cpu_detector: config reload failed, keeping last value: %s", e)
        self._last_read_at = t
        return self._value


def _default_cpu_detection_flag_loader() -> bool:
    """Load ``features.cpu_detection`` from the on-disk config.yaml."""
    from apps.control_api.app.config_store import ConfigStore

    return bool(ConfigStore(Path("config.yaml")).load().features.cpu_detection)


def _atomic_write_jpeg(path: Path, frame: np.ndarray, quality: int = 95) -> bool:
    """Encode *frame* to JPEG and replace *path* atomically.

    Writes to a per-pid sibling tempfile (via :func:`tempfile.mkstemp`
    which carries OS-level PID + random suffix) then ``os.replace``s it
    onto *path*. Readers either see the previous file or the new file -
    never a half-written stream. Returns False on encode failure so
    callers can log and skip.
    """
    import os
    import tempfile

    ok, buf = cv2.imencode(".jpg", frame, [cv2.IMWRITE_JPEG_QUALITY, quality])
    if not ok:
        return False
    fd, tmp_name = tempfile.mkstemp(prefix=f".{path.name}.", suffix=".tmp", dir=str(path.parent))
    try:
        with os.fdopen(fd, "wb") as f:
            f.write(buf.tobytes())
        os.replace(tmp_name, path)
    except OSError:
        try:
            os.unlink(tmp_name)
        except OSError:
            pass
        return False
    return True


def _atomic_write_text(path: Path, content: str) -> None:
    """Write *content* to *path* atomically (UTF-8).

    Paired with :func:`_atomic_write_jpeg` for the sidecar so consumers
    gating on the sidecar's mtime can't read a partial file. Raises
    ``OSError`` on failure so the caller can log and skip.
    """
    import os
    import tempfile

    fd, tmp_name = tempfile.mkstemp(prefix=f".{path.name}.", suffix=".tmp", dir=str(path.parent))
    try:
        with os.fdopen(fd, "w", encoding="utf-8") as f:
            f.write(content)
        os.replace(tmp_name, path)
    except OSError:
        try:
            os.unlink(tmp_name)
        except OSError:
            pass
        raise


def _normalize_model_names(names: object) -> dict[int, str]:
    """Normalise an Ultralytics ``model.names`` value to ``dict[int, str]``.

    Ultralytics has shipped both a
    ``dict[int, str]`` (typical on fine-tuned checkpoints loaded
    from ``.pt``) and a ``list[str]`` / ``tuple[str, ...]`` (some
    older COCO defaults) in ``model.names``. Returning a uniform
    mapping lets the resolver use ``.get(cls_id)`` regardless of
    source shape. Unrecognised inputs return an empty dict so the
    synthetic ``class_<id>`` fallback still kicks in.
    """
    if isinstance(names, dict):
        return {int(k): str(v) for k, v in names.items()}
    if isinstance(names, list | tuple):
        return {i: str(v) for i, v in enumerate(names)}
    return {}


class CpuDetector:
    """Object detector running YOLO on CPU via Ultralytics."""

    _inference_fps: float = 3.0

    def __init__(
        self,
        subscriber: Subscriber,
        publisher: Publisher,
        model_path: str,
        labels: list[str] | None = None,
        class_map: dict[str, str] | None = None,
        confidence_threshold: float = 0.25,
        flag_poller: FeatureFlagPoller | None = None,
    ):
        self._sub = subscriber
        self._pub = publisher
        self._model_path = model_path
        self._labels = labels
        self._class_map = class_map
        self._conf_threshold = confidence_threshold
        self._flag_poller = flag_poller or FeatureFlagPoller(
            loader=_default_cpu_detection_flag_loader
        )
        from typing import Any

        self._model: Any = None
        # Index→name map. Filled from the loaded .pt's `model.names`
        # attribute (Ultralytics carries the dataset class names baked
        # into the checkpoint). Populated on first successful load;
        # used as a fallback when the sidecar `labels` array is absent.
        self._model_names: dict[int, str] = {}
        self._running = False
        self._frames_processed = 0
        self._detector_model = DetectorModel(
            name=Path(model_path).stem, version="cpu", runtime="pytorch"
        )

    def _ensure_model(self) -> bool:
        if self._model is not None:
            return True
        try:
            from ultralytics import YOLO

            self._model = YOLO(self._model_path, task="detect")
            # Capture the checkpoint's class-name map via the
            # shape-agnostic helper.
            self._model_names = _normalize_model_names(getattr(self._model, "names", None))
            logger.info(
                "cpu_detector: loaded %s (model.names=%s)",
                self._model_path,
                self._model_names or "<unset>",
            )
            return True
        except Exception as e:
            logger.error("cpu_detector: failed to load model: %s", e)
            return False

    def _resolve_raw_label(self, cls_id: int) -> str:
        """Pick a human-readable label for the given class id.

        Resolution priority:
          1. Sidecar-provided ``labels`` array (operator override).
          2. Model-baked names captured at load time.
          3. ``f"class_{cls_id}"`` numeric fallback.
        """
        if self._labels and 0 <= cls_id < len(self._labels):
            return self._labels[cls_id]
        if cls_id in self._model_names:
            return self._model_names[cls_id]
        return f"class_{cls_id}"

    def _grab_frame(self) -> np.ndarray | None:
        try:
            resp = httpx.get(MEDIA_FRAME_URL, timeout=5.0)
            if resp.status_code != 200:
                return None
            arr = np.frombuffer(resp.content, dtype=np.uint8)
            return cv2.imdecode(arr, cv2.IMREAD_COLOR)
        except Exception:
            return None

    def _map_class(self, cls_id: int, raw_label: str) -> str:
        """Map a class ID/label to a pipeline label (ball/robot/human)."""
        if self._class_map:
            mapped = self._class_map.get(str(cls_id))
            if mapped:
                return mapped
            return ""  # unlisted class → drop
        # Fallback: use raw label as-is
        return raw_label

    def _process_frame(self, frame_msg: FrameReferenceMessage) -> None:
        import time as _time

        if not self._ensure_model():
            logger.warning("cpu_detector: model not loaded, skipping frame")
            return

        logger.info("cpu_detector: grabbing frame...")
        t0 = _time.monotonic()
        frame = self._grab_frame()
        if frame is None:
            logger.warning("cpu_detector: failed to grab frame from %s", MEDIA_FRAME_URL)
            return
        t_grab = _time.monotonic() - t0
        h_px, w_px = frame.shape[:2]
        logger.info("cpu_detector: frame grabbed %dx%d (%.1fs)", w_px, h_px, t_grab)

        logger.info("cpu_detector: running inference...")
        t1 = _time.monotonic()
        results = self._model(frame, conf=self._conf_threshold, verbose=False)
        t_infer = _time.monotonic() - t1
        logger.info("cpu_detector: inference done (%.1fs)", t_infer)

        now = datetime.now(UTC)
        detections = []

        for result in results:
            boxes = result.boxes
            if boxes is None:
                continue
            n_raw = len(boxes)
            logger.info("cpu_detector: %d raw boxes from model", n_raw)
            for i in range(n_raw):
                box = boxes[i]
                cls_id = int(box.cls[0])
                conf = float(box.conf[0])
                xyxy = box.xyxy[0].cpu().numpy()

                raw_label = self._resolve_raw_label(cls_id)
                cls = self._map_class(cls_id, raw_label)
                if not cls:
                    logger.debug(
                        "cpu_detector: dropping cls_id=%d raw=%s (unmapped)", cls_id, raw_label
                    )
                    continue

                x = float(xyxy[0])
                y = float(xyxy[1])
                w = float(xyxy[2] - xyxy[0])
                h = float(xyxy[3] - xyxy[1])

                detections.append(
                    {
                        "detection_id": f"det-{uuid.uuid4().hex[:8]}",
                        "class": cls,
                        "bbox_xywh": [x, y, w, h],
                        "confidence": conf,
                    }
                )

        det_msg = ObjectDetectionsMessage.model_validate(
            {
                "schema_version": "1.0",
                "message_id": f"cpu-det-{uuid.uuid4().hex[:8]}",
                "session_id": frame_msg.session_id,
                "source_module": "cpu_detector",
                "created_at": now.isoformat(),
                "frame_id": frame_msg.frame_id,
                "source_timestamp": frame_msg.source_timestamp.isoformat(),
                "detector_model": self._detector_model.model_dump(),
                "detections": detections,
            }
        )
        self._pub.send(DETECTIONS_TOPIC, det_msg)

        # Render boxes onto the exact frame this inference saw and
        # write the raw + annotated JPEGs and a detections sidecar so
        # the preview route and the snapshot endpoint can serve a frame
        # whose detections are temporally aligned. Without this they
        # would compose the latest raw camera frame with cached
        # detections from a ~7 s old inference, and boxes drift off
        # any moving object.
        self._write_inference_artifacts(frame, detections, frame_msg)

        self._frames_processed += 1
        logger.info(
            "cpu_detector: frame %d - %d detections (grab=%.1fs infer=%.1fs)",
            self._frames_processed,
            len(detections),
            t_grab,
            t_infer,
        )

    def _write_inference_artifacts(
        self,
        frame: np.ndarray,
        detections: list[dict],
        frame_msg: FrameReferenceMessage,
    ) -> None:
        """Atomically publish raw + annotated JPEG and detections sidecar.

        Writes three files into ``/tmp/aicam-frames/``:

        - ``cpu_raw.jpg`` - the exact frame this inference saw.
        - ``cpu_annotated.jpg`` - the same frame with boxes drawn.
        - ``cpu_detections.json`` - sidecar holding the detection list,
          frame dimensions, frame_id, and inference timestamp.

        The sidecar is renamed last so a consumer that checks its mtime
        for staleness can rely on the matching raw and annotated JPEGs
        already being from the same inference pass - bridging the
        slow-joiner race that the bus-cached fallback path could not.
        """
        from apps.ai_worker.draw import draw_detection_boxes

        annotated = frame.copy()
        if detections:
            draw_detection_boxes(annotated, detections)

        out_dir = INFERENCE_ARTIFACTS_DIR
        try:
            out_dir.mkdir(parents=True, exist_ok=True)
        except OSError as e:
            logger.warning("cpu_detector: failed to mkdir %s: %s", out_dir, e)
            return

        ann_path = out_dir / "cpu_annotated.jpg"
        raw_path = out_dir / "cpu_raw.jpg"
        sidecar_path = out_dir / "cpu_detections.json"

        # Each file goes through a per-pid sibling tempfile and a
        # final os.replace so the
        # preview endpoint never sees a half-written JPEG (cv2.imwrite
        # is not atomic on all filesystems). Sidecar is written last
        # so any consumer that gates on its mtime is guaranteed to see
        # matching JPEGs from the same inference pass.
        if not _atomic_write_jpeg(raw_path, frame, quality=85):
            logger.warning("cpu_detector: failed to write raw JPEG %s", raw_path)
            return
        if not _atomic_write_jpeg(ann_path, annotated, quality=85):
            logger.warning("cpu_detector: failed to write annotated JPEG %s", ann_path)
            return

        h_px, w_px = frame.shape[:2]
        sidecar = {
            "timestamp": datetime.now(UTC).isoformat(),
            "frame_id": frame_msg.frame_id,
            "image_width": int(w_px),
            "image_height": int(h_px),
            "detections": [
                {
                    "class": d.get("class", "unknown"),
                    "bbox_xywh": list(d.get("bbox_xywh", [])),
                    "confidence": float(d.get("confidence", 0.0)),
                }
                for d in detections
            ],
        }
        try:
            _atomic_write_text(sidecar_path, json.dumps(sidecar))
        except OSError as e:
            logger.warning("cpu_detector: failed to write sidecar %s: %s", sidecar_path, e)
            return

    def run(self, timeout_ms: int = 1000) -> int:
        """Run the detector loop.

        First tries to receive ``media.frame_refs`` from ZMQ (the Rust
        media service publishes these when Hailo is not active). If no
        frame_refs arrive within 5 s, switches to self-paced mode where
        the detector grabs frames at ``inference_fps`` via HTTP. This
        allows CPU detection to work alongside a running Hailo pipeline.

        Between iterations the run loop also polls
        ``features.cpu_detection`` from ``config.yaml`` (throttled by
        the ``FeatureFlagPoller``). When the flag flips to false the
        loop returns cleanly; the caller should then ``sys.exit(0)``
        so systemd treats the shutdown as "success" and does not try
        to restart the service.
        """
        self._running = True

        # Probe for frame_refs (5 s window)
        probe_result = self._sub.receive_model(FrameReferenceMessage, timeout_ms=5000)
        if probe_result is not None:
            logger.info("cpu_detector: frame_refs available - ZMQ-triggered mode")
            _, frame_msg = probe_result
            self._process_frame(frame_msg)
            while self._running:
                if not self._flag_poller.enabled():
                    logger.info("cpu_detector: features.cpu_detection flipped off - stopping")
                    self._running = False
                    break
                result = self._sub.receive_model(FrameReferenceMessage, timeout_ms=timeout_ms)
                if result is not None:
                    _, frame_msg = result
                    self._process_frame(frame_msg)
        else:
            logger.info(
                "cpu_detector: no frame_refs - self-paced mode at %.1f fps",
                self._inference_fps,
            )
            interval = 1.0 / max(self._inference_fps, 0.1)
            frame_counter = 0
            session_id = f"cpu-live-{uuid.uuid4().hex[:8]}"
            while self._running:
                if not self._flag_poller.enabled():
                    logger.info("cpu_detector: features.cpu_detection flipped off - stopping")
                    self._running = False
                    break
                t0 = time.monotonic()
                frame_counter += 1
                # Synthesize a FrameReferenceMessage for _process_frame
                now = datetime.now(UTC)
                synthetic_msg = FrameReferenceMessage.model_validate(
                    {
                        "schema_version": "1.0",
                        "message_type": "frame_reference",
                        "message_id": f"cpu-ref-{uuid.uuid4().hex[:8]}",
                        "session_id": session_id,
                        "source_module": "cpu_detector",
                        "created_at": now.isoformat(),
                        "frame_id": f"cpu-{frame_counter}",
                        "source_timestamp": now.isoformat(),
                        "frame_index": frame_counter,
                        "width_px": 1920,
                        "height_px": 1080,
                        "pixel_format": "BGR",
                        "frame_ref": {
                            "transport": "http",
                            "name": MEDIA_FRAME_URL,
                            "length": 0,
                        },
                    }
                )
                self._process_frame(synthetic_msg)
                elapsed = time.monotonic() - t0
                sleep_time = interval - elapsed
                if sleep_time > 0:
                    time.sleep(sleep_time)

        return self._frames_processed

    def stop(self) -> None:
        self._running = False


# ---------------------------------------------------------------------------
# On-demand one-shot snap
# ---------------------------------------------------------------------------
#
# The demo build doesn't keep the cpu_detector systemd service running -
# it only does CPU inference when the operator clicks Snap on the
# Detection page with the CPU model selected. The control_api invokes
# `run_oneshot_snap()` synchronously, which loads the YOLO model on
# first call (~3-5 s), then runs one inference per call. The model is
# cached in this module so subsequent snaps cost only inference time
# (~1-3 s on a Pi 5 CPU at 640×640).


class _OneShotRuntime:
    """Lazy-loaded YOLO inference helper for the demo Snap flow.

    Loads the model selected by `ai.cpu_object_detection_model` from
    `config.yaml` on first use and caches it for the lifetime of the
    control_api process. Each call to `run_once()` grabs one camera
    frame, runs inference, writes the standard cpu_* artifacts, and
    returns a small summary dict.
    """

    _instance: _OneShotRuntime | None = None
    _load_failed: bool = False

    @classmethod
    def get(cls) -> _OneShotRuntime | None:
        if cls._load_failed:
            return None
        if cls._instance is not None:
            return cls._instance
        rt = cls._try_load()
        if rt is None:
            cls._load_failed = True
        cls._instance = rt
        return rt

    @classmethod
    def _try_load(cls) -> _OneShotRuntime | None:
        from pathlib import Path as _Path

        from apps.control_api.app.config_store import ConfigStore
        from apps.model_registry import ModelScope, load_model_by_display_name

        cfg = ConfigStore(_Path("config.yaml")).load()
        model_name = cfg.ai.cpu_object_detection_model
        if not model_name:
            logger.info("cpu_snap: no CPU model selected - refusing to load")
            return None
        md = load_model_by_display_name(model_name, scope=ModelScope.object_detection)
        if md is None:
            logger.error("cpu_snap: model %r not found in registry", model_name)
            return None
        model_path = md.model_path or md.hef_path
        if not model_path or not _Path(model_path).exists():
            logger.error("cpu_snap: model file not found: %s", model_path)
            return None

        labels = md.labels if isinstance(md.labels, list) else None
        class_map = md.class_map

        # Reuse the existing CpuDetector for its _process_frame logic;
        # it wants a Subscriber + Publisher but neither is touched in
        # the one-shot path (we synthesize the frame ref locally and
        # don't read the bus). Pass minimal stubs and silence the
        # type-checker - the duck-typed `send` / `receive_model`
        # methods on the noop helpers match the protocol the run loop
        # actually exercises.
        detector = CpuDetector(
            subscriber=_NoopSubscriber(),  # type: ignore[arg-type]
            publisher=_NoopPublisher(),  # type: ignore[arg-type]
            model_path=str(model_path),
            labels=labels,
            class_map=class_map,
        )
        if not detector._ensure_model():
            return None
        logger.info("cpu_snap: model loaded (%s)", model_name)
        return cls(detector=detector)

    def __init__(self, detector: CpuDetector):
        self._detector = detector

    def run_once(self) -> dict[str, object]:
        """Run one inference and return a summary."""
        frame_counter = self._detector._frames_processed + 1
        now = datetime.now(UTC)
        synthetic_msg = FrameReferenceMessage.model_validate(
            {
                "schema_version": "1.0",
                "message_type": "frame_reference",
                "message_id": f"cpu-snap-{uuid.uuid4().hex[:8]}",
                "session_id": f"cpu-snap-{uuid.uuid4().hex[:8]}",
                "source_module": "cpu_detector",
                "created_at": now.isoformat(),
                "frame_id": f"cpu-snap-{frame_counter}",
                "source_timestamp": now.isoformat(),
                "frame_index": frame_counter,
                "width_px": 1920,
                "height_px": 1080,
                "pixel_format": "BGR",
                "frame_ref": {"transport": "http", "name": MEDIA_FRAME_URL, "length": 0},
            }
        )
        self._detector._process_frame(synthetic_msg)
        return {
            "ok": True,
            "frames_processed": self._detector._frames_processed,
        }


class _NoopSubscriber:
    """Stand-in for `Subscriber` in the one-shot path - never read."""

    def receive_model(self, *_args: object, **_kwargs: object) -> None:
        return None


class _NoopPublisher:
    """Stand-in for `Publisher` in the one-shot path - drops all sends.

    The control_api already serves detections via /api/v1/detection/state
    by reading the artifact files; the bus publish is a leftover from
    the long-running service mode and unnecessary for the on-demand
    Snap flow.
    """

    def send(self, *_args: object, **_kwargs: object) -> None:
        return None


def run_oneshot_snap() -> dict[str, object]:
    """Run one CPU inference, called from `/api/v1/detection/cpu_snap`.

    Returns:
        ``{"ok": True, "frames_processed": N}`` on success
        ``{"ok": False, "error": "..."}`` when the model can't be loaded
        (no CPU model selected, missing file, registry miss).
    """
    rt = _OneShotRuntime.get()
    if rt is None:
        return {
            "ok": False,
            "error": "no CPU model loaded - select one in Configuration",
        }
    try:
        return rt.run_once()
    except Exception as e:  # noqa: BLE001
        logger.error("cpu_snap: inference failed: %s", e)
        return {"ok": False, "error": f"inference failed: {e}"}


def main() -> None:
    """Entry point for the CPU detector service."""
    logging.basicConfig(
        level=logging.INFO,
        format="%(asctime)s %(levelname)s %(name)s: %(message)s",
    )

    sys.path.insert(0, str(Path(__file__).resolve().parents[2]))
    from apps.control_api.app.config_store import ConfigStore

    cfg = ConfigStore(Path("config.yaml")).load()

    if not cfg.features.cpu_detection:
        logger.info("cpu_detector: features.cpu_detection is disabled, exiting")
        sys.exit(0)

    model_name = cfg.ai.cpu_object_detection_model
    if not model_name:
        logger.info("cpu_detector: no CPU model selected, exiting")
        sys.exit(0)

    # Resolve model from registry
    from apps.model_registry import ModelScope, load_model_by_display_name

    md = load_model_by_display_name(model_name, scope=ModelScope.object_detection)
    if md is None:
        logger.error("cpu_detector: model %r not found in registry", model_name)
        sys.exit(1)

    model_path = md.model_path or md.hef_path
    if not model_path or not Path(model_path).exists():
        logger.error("cpu_detector: model file not found: %s", model_path)
        sys.exit(1)

    labels = md.labels if isinstance(md.labels, list) else None
    class_map = md.class_map

    logger.info(
        "cpu_detector: starting with model=%s path=%s labels=%s class_map=%s",
        model_name,
        model_path,
        labels,
        class_map,
    )

    sub = Subscriber(topics=[FRAME_REF_TOPIC])
    pub = Publisher()
    detector = CpuDetector(
        subscriber=sub,
        publisher=pub,
        model_path=model_path,
        labels=labels,
        class_map=class_map,
    )
    detector._inference_fps = md.inference_fps or 3.0

    def _shutdown(*_args: object) -> None:
        detector.stop()
        sys.exit(0)

    signal.signal(signal.SIGTERM, _shutdown)

    try:
        detector.run()
    except KeyboardInterrupt:
        detector.stop()


if __name__ == "__main__":
    main()
