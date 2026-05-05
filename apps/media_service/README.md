# Media Service

This directory contains the **Rust** media-service stub for the camera.

- implementation language: **Rust**
- responsibility: GStreamer-based capture, encode, record, split, overlay, stream, and frame fan-out for AI
- API style: small local HTTP control surface

## Why Rust here

This service is long-running, stateful, concurrency-heavy, and close to the media pipeline.
That makes memory safety and explicit state handling more valuable than rapid scripting.

Python remains the right place for:
- CPU object detection (`apps/ai_worker/cpu_detector.py`)
- higher-level control workflows (`apps/control_api`)

## Run locally

```bash
cd apps/media_service
cargo run
```
