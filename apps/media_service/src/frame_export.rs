// Implements Rust media pipeline logic for streaming and camera processing.
// Author: Thomas Klute

//! AI frame export - pulls frames from GStreamer appsink and publishes to ZMQ.
//!
//! Replaces the AI branch fakesink with a real appsink that:
//! 1. Pulls raw video frames from the live producer pipeline
//! 2. Writes frame data to a temp file
//! 3. Publishes FrameReferenceMessage JSON to ZMQ topic "media.frame_refs"

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use chrono::Utc;
use gstreamer_app as gst_app;
use serde_json::json;
use tracing::{info, warn};
use uuid::Uuid;

/// Configuration for the frame exporter.
pub struct FrameExportConfig {
    /// Export every Nth frame (1 = every frame, 3 = every 3rd).
    pub subsample: u32,
    /// Number of frame files to keep on disk (ring buffer).
    /// Older frames are deleted to bound tmpfs usage and give readers
    /// a window of `ring_size * subsample / fps` seconds to open a file.
    pub ring_size: u32,
    /// Directory for frame data files.
    pub frame_dir: PathBuf,
    /// ZMQ endpoint to connect to (XSUB port).
    pub zmq_endpoint: String,
    /// Session ID for frame messages.
    pub session_id: String,
    /// Image dimensions.
    pub width: u32,
    pub height: u32,
}

impl Default for FrameExportConfig {
    fn default() -> Self {
        Self {
            subsample: 3,
            ring_size: 3,
            frame_dir: PathBuf::from("/tmp/aicam-frames"),
            zmq_endpoint: "tcp://127.0.0.1:5559".to_string(),
            session_id: format!("live-{}", &Uuid::new_v4().to_string()[..8]),
            width: 640,
            height: 480,
        }
    }
}

/// Set up the appsink callback that exports frames to ZMQ.
///
/// Call this after building the live producer pipeline but before starting it.
/// The appsink must already exist in the pipeline as "frame_export_sink".
pub fn setup_frame_export(
    pipeline: &gstreamer::Pipeline,
    config: FrameExportConfig,
) -> anyhow::Result<()> {
    use gstreamer::prelude::*;

    let appsink: gst_app::AppSink = pipeline
        .by_name("frame_export_sink")
        .ok_or_else(|| anyhow::anyhow!("frame_export_sink element not found in pipeline"))?
        .dynamic_cast::<gst_app::AppSink>()
        .map_err(|_| anyhow::anyhow!("frame_export_sink is not an AppSink"))?;

    // Create frame directory and clean up leftovers from a previous run
    fs::create_dir_all(&config.frame_dir)?;
    if let Ok(entries) = fs::read_dir(&config.frame_dir) {
        for entry in entries.flatten() {
            let _ = fs::remove_file(entry.path());
        }
    }

    // Set up frame counter
    let frame_counter = Arc::new(AtomicU64::new(0));

    // Spawn ZMQ publisher in background thread (using libzmq via zmq crate)
    let (tx, rx) = std::sync::mpsc::channel::<String>();

    let zmq_endpoint = config.zmq_endpoint.clone();
    std::thread::spawn(move || {
        let ctx = zmq::Context::new();
        let socket = ctx
            .socket(zmq::PUB)
            .expect("Failed to create ZMQ PUB socket");
        if let Err(e) = socket.connect(&zmq_endpoint) {
            warn!(error = %e, "Failed to connect ZMQ publisher to broker");
            return;
        }
        info!(endpoint = %zmq_endpoint, "Frame export ZMQ publisher connected to broker");

        // Brief sleep to let SUB subscriptions propagate through the broker
        std::thread::sleep(std::time::Duration::from_millis(200));

        while let Ok(msg_json) = rx.recv() {
            // Send multipart [topic, payload] matching Python bus format
            let topic = b"media.frame_refs";
            if let Err(e) = socket
                .send(topic.as_slice(), zmq::SNDMORE)
                .and_then(|()| socket.send(msg_json.as_bytes(), 0))
            {
                warn!(error = %e, "Failed to send frame message to ZMQ");
            }
        }
    });

    // Set up appsink callback
    let subsample = config.subsample;
    let ring_size = config.ring_size;
    let frame_dir_log = config.frame_dir.display().to_string();
    let frame_dir = config.frame_dir;
    let session_id = config.session_id;
    let width = config.width;
    let height = config.height;

    appsink.set_callbacks(
        gst_app::AppSinkCallbacks::builder()
            .new_sample(move |sink| {
                let frame_idx = frame_counter.fetch_add(1, Ordering::Relaxed);

                // Subsample: only export every Nth frame
                if !frame_idx.is_multiple_of(subsample as u64) {
                    return Ok(gstreamer::FlowSuccess::Ok);
                }

                let sample = sink.pull_sample().map_err(|_| gstreamer::FlowError::Eos)?;
                let buffer = sample.buffer().ok_or(gstreamer::FlowError::Error)?;

                let map = buffer
                    .map_readable()
                    .map_err(|_| gstreamer::FlowError::Error)?;

                // 1. Write to temp file (atomic write pattern)
                let tmp_path = frame_dir.join(format!("frame_{:06}.raw.tmp", frame_idx));
                if let Err(e) = fs::write(&tmp_path, map.as_slice()) {
                    warn!(error = %e, "Failed to write frame temp file");
                    return Ok(gstreamer::FlowSuccess::Ok);
                }

                // 2. Rename to final path (atomic on same filesystem)
                let frame_path = frame_dir.join(format!("frame_{:06}.raw", frame_idx));
                if let Err(e) = fs::rename(&tmp_path, &frame_path) {
                    warn!(error = %e, "Failed to rename frame file");
                    let _ = fs::remove_file(&tmp_path);
                    return Ok(gstreamer::FlowSuccess::Ok);
                }

                // 3. Update "latest.raw" symlink (atomic via temp + rename)
                let latest_link = frame_dir.join("latest.raw");
                let latest_tmp = frame_dir.join("latest.raw.lnk");
                let _ = fs::remove_file(&latest_tmp);
                if std::os::unix::fs::symlink(&frame_path, &latest_tmp).is_ok() {
                    let _ = fs::rename(&latest_tmp, &latest_link);
                }

                // 4. Publish ZMQ message (after rename - file is complete)
                let now = Utc::now();
                let msg = json!({
                    "schema_version": "1.0",
                    "message_type": "frame_reference",
                    "message_id": format!("live-{}", Uuid::new_v4()),
                    "session_id": session_id,
                    "source_module": "media_service",
                    "created_at": now.to_rfc3339(),
                    "frame_id": format!("frame-{:06}", frame_idx),
                    "source_timestamp": now.to_rfc3339(),
                    "frame_index": frame_idx,
                    "width_px": width,
                    "height_px": height,
                    "pixel_format": "NV12",
                    "coordinate_system": "image_px",
                    "frame_ref": {
                        "transport": "file",
                        "name": frame_path.to_string_lossy(),
                        "offset": 0,
                        "length": map.len(),
                    }
                });

                let _ = tx.send(msg.to_string());

                // 5. Ring buffer: delete frame from N steps ago
                let ring_span = ring_size as u64 * subsample as u64;
                if frame_idx >= ring_span {
                    let old_idx = frame_idx - ring_span;
                    let old_path = frame_dir.join(format!("frame_{:06}.raw", old_idx));
                    let _ = fs::remove_file(&old_path);
                }

                Ok(gstreamer::FlowSuccess::Ok)
            })
            .build(),
    );

    info!(
        subsample = subsample,
        ring_size = ring_size,
        frame_dir = %frame_dir_log,
        "Frame export appsink configured (ring_size={ring_size}, subsample={subsample})"
    );

    Ok(())
}
