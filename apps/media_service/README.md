# Media Service

This directory contains the **Rust** media-service stub for the camera.

Decision:
- implementation language: **Rust**
- responsibility: GStreamer-based capture, encode, record, split, overlay, stream, and frame fan-out for AI
- API style: small local HTTP control surface first; can later move to Unix socket / gRPC if needed

## Why Rust here

This service is long-running, stateful, concurrency-heavy, and close to the media pipeline.
That makes memory safety and explicit state handling more valuable than rapid scripting.

Python remains the right place for:
- CPU object detection (`apps/ai_worker/cpu_detector.py`)
- higher-level control workflows (`apps/control_api`)

## Current state

This is intentionally a stub, not the real pipeline.
It already establishes:
- a runnable process on the Raspberry Pi
- status and feature endpoints
- a state model that the Python control plane can integrate against

## Run locally

```bash
cd apps/media_service
cargo run
```

Service endpoints:
- `GET /health`
- `GET /status`
- `POST /start`
- `POST /stop`
- `POST /features`

## Near-term implementation plan

1. replace the stub state machine with a real GStreamer pipeline lifecycle
2. add camera/file/image source selection
3. add recording branch
4. add frame tee for AI worker handoff
5. add streaming branch and overlay input hooks
6. add graceful degradation when audio is missing
