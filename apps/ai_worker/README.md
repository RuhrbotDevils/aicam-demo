# AI Worker

Python services that consume the ZMQ bus - the non-GStreamer half
of the AI pipeline.

## Demo build

The demo keeps a single worker:

| Module | Role | Topics in → out |
|---|---|---|
| `cpu_detector.py` | YOLO-on-CPU object detector via Ultralytics. Activates when `features.cpu_detection` is enabled in `config.yaml`. Subscribes to `media.frame_refs` (when the Rust media service is publishing frame refs) or self-paces by polling `/api/v1/camera_preview/frame` over HTTP. Loads the model selected via `AIConfig.cpu_object_detection_model` from the `apps/model_registry.py` registry. | `media.frame_refs` → `ai.object_detections` |

Hailo detection runs **in-pipeline** inside `apps/media_service/`
(GStreamer's `hailonet` + `hailofilter` elements), not as a separate
Python worker. There is no Python detector when Hailo is the
configured backend - the Rust media service emits annotated frames
directly via the `object_detection_preview` AppSink.

## Removed in the demo build

The original full-scope `apps/ai_worker/` carried:

- `detector.py` (FakeDetector dev fallback) - removed.
- `landmark_detector.py` - removed.
- `jersey_color.py` - removed.
- `posture.py` - removed.
- `tracker.py` + `tracking/` subpackage - removed.

The cascade classifier flow, the bundle-collector enricher protocol,
and the completeness-marker convention all went with the tracker.
