// Implements Rust media pipeline logic for streaming and camera processing.
// Author: Thomas Klute

//! Recording lifecycle + media-service helpers.
//!
//! The producer-side machinery itself lives in [`crate::producer`].
//! This module is the home for the bits the rest of the service
//! needs that don't fit cleanly elsewhere:
//!
//! - [`AiConfig`] / [`ResolvedModel`] - model registry resolution.
//! - [`build_live_producer`] / [`LiveProducer`] - re-exported from
//!   `producer` so the historic call site name in `main.rs` keeps
//!   working without an additional `use`.
//! - [`start_recording`] / [`stop_recording`] - drive the recording
//!   consumer pipelines from `consumers.rs` through their valve cycle.
//! - [`start_pipeline`] / [`stop_pipeline`] - generic
//!   `gst::Pipeline` lifecycle helpers used by every consumer.
//! - [`query_media_duration`] - used by `/replay/start` to compute
//!   the session duration.
//! - [`resolve_meta_export_so_path`] - used by
//!   `consumers::build_ai_consumer_pipeline`.

use std::path::Path;
use std::sync::atomic::Ordering;

use gstreamer as gst;
use gstreamer::prelude::*;
use tracing::{info, warn};

/// AI inference configuration derived from a resolved entry in the
/// `config/models/` sidecar-JSON registry.
///
/// `None` means "no model selected; skip the AI consumer". The demo
/// build supports a single scope - `object_detection`. Cascade
/// classifiers (robot type, jersey colour) and the
/// `landmark_detection` scope were removed in the demo simplification.
#[derive(Debug, Clone, Default)]
#[allow(dead_code)]
pub struct AiConfig {
    pub object_detection: Option<ResolvedModel>,
}

/// A single model resolved from the registry and ready to wire into
/// the AI consumer pipeline.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct ResolvedModel {
    pub display_name: String,
    pub hef_path: String,
    pub input_width: u32,
    pub input_height: u32,
    pub input_format: String,
    pub postprocess_so: String,
    pub postprocess_fn: String,
    /// Model family tag - used for logging today.
    pub output_format: String,
    /// index→name label map from the sidecar JSON.
    pub label_map: Option<Vec<String>>,
    /// Named label set (e.g. `"coco_80"`). UI-facing metadata.
    pub labels_display: Option<String>,
    /// Integer class-ID → pipeline label remapping.
    pub class_map: Option<std::collections::HashMap<String, String>>,
    /// Target inference rate in fps. Defaults to 3.0 in the consumer.
    pub inference_fps: Option<f32>,
    /// Freeform description. UI-facing metadata.
    pub notes: Option<String>,
    /// Render-only models (pose, etc.) opt out so meta_export does
    /// not crash trying to publish HailoLandmarks-shaped ROIs as
    /// detections.
    pub publish_detections: bool,
}

impl AiConfig {
    /// Returns the object_detection model's input width if a model
    /// is selected.
    pub fn input_width(&self) -> Option<u32> {
        self.object_detection.as_ref().map(|m| m.input_width)
    }
}

pub use crate::producer::{build_live_producer, LiveProducer};

/// Recording statistics collected during a recording session.
pub struct RecordingStats {
    pub frame_count: u64,
    pub pts_log: Vec<(u64, u64)>,
}

/// Start recording on the standalone recording consumer pipelines.
///
/// Cycles the chain through NULL with the new filesink location,
/// syncs downstream-first, then opens the valve. Audio chain
/// follows the same pattern when configured.
pub fn start_recording(
    video: &mut crate::consumers::RecordingVideoConsumer,
    audio: Option<&mut crate::consumers::RecordingAudioConsumer>,
    video_path: &Path,
    audio_path: &Path,
    fps: u32,
    bitrate_kbps: u32,
) -> anyhow::Result<()> {
    video.frame_count.store(0, Ordering::Relaxed);
    video.valve_count.store(0, Ordering::Relaxed);
    video.queue_src_count.store(0, Ordering::Relaxed);
    if let Ok(mut log) = video.pts_log.lock() {
        log.clear();
    }

    video.filesink.set_state(gst::State::Null)?;
    video.encoder.set_state(gst::State::Null)?;
    video.videoconvert.set_state(gst::State::Null)?;
    video.queue.set_state(gst::State::Null)?;

    video
        .filesink
        .set_property("location", video_path.to_str().unwrap_or("video.h264"));
    if video.encoder.find_property("bitrate").is_some() {
        video.encoder.set_property("bitrate", bitrate_kbps);
    }
    if video.encoder.find_property("key-int-max").is_some() {
        video.encoder.set_property("key-int-max", fps);
    }

    video.filesink.sync_state_with_parent()?;
    video.encoder.sync_state_with_parent()?;
    video.videoconvert.sync_state_with_parent()?;
    video.queue.sync_state_with_parent()?;

    if let Some(audio) = audio {
        match (|| -> anyhow::Result<()> {
            audio.filesink.set_state(gst::State::Null)?;
            audio.encoder.set_state(gst::State::Null)?;
            audio.queue.set_state(gst::State::Null)?;
            audio.valve.set_state(gst::State::Null)?;

            audio
                .filesink
                .set_property("location", audio_path.to_str().unwrap_or("audio.flac"));

            audio.filesink.sync_state_with_parent()?;
            audio.encoder.sync_state_with_parent()?;
            audio.queue.sync_state_with_parent()?;
            audio.valve.sync_state_with_parent()?;
            Ok(())
        })() {
            Ok(()) => {
                info!("Audio recording elements ready");
                audio.valve.set_property("drop", false);
            }
            Err(e) => warn!(error = %e, "Audio recording element setup failed - video only"),
        }
    }

    video.valve.set_property("drop", false);

    info!(path = %video_path.display(), "Recording started (valves opened)");
    Ok(())
}

/// Stop recording on the standalone recording consumer pipelines.
///
/// 1. Close the video valve.
/// 2. Send EOS through the video queue and wait for it at the
///    filesink (5 s timeout) - guarantees the file is fully flushed
///    before the next NULL cycle.
/// 3. Same for the audio chain.
/// 4. Cycle elements back to PLAYING with `location = /dev/null`.
pub fn stop_recording(
    video: &mut crate::consumers::RecordingVideoConsumer,
    audio: Option<&mut crate::consumers::RecordingAudioConsumer>,
) -> anyhow::Result<RecordingStats> {
    video.valve.set_property("drop", true);
    video.queue.send_event(gst::event::Eos::new());

    {
        let (tx, rx) = std::sync::mpsc::channel::<()>();
        let sink_pad = video.filesink.static_pad("sink").unwrap();
        let probe_id =
            sink_pad.add_probe(gst::PadProbeType::EVENT_DOWNSTREAM, move |_pad, info| {
                if let Some(gst::PadProbeData::Event(ref event)) = info.data {
                    if event.type_() == gst::EventType::Eos {
                        let _ = tx.send(());
                        return gst::PadProbeReturn::Drop;
                    }
                }
                gst::PadProbeReturn::Ok
            });
        let _ = rx.recv_timeout(std::time::Duration::from_secs(5));
        if let Some(id) = probe_id {
            sink_pad.remove_probe(id);
        }
    }

    let frame_count = video.frame_count.load(Ordering::Relaxed);
    let rec_valve_count = video.valve_count.load(Ordering::Relaxed);
    let rec_queue_src_count = video.queue_src_count.load(Ordering::Relaxed);
    let pts_log = video
        .pts_log
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();

    if let Some(audio) = audio {
        audio.queue.send_event(gst::event::Eos::new());
        {
            let (tx, rx) = std::sync::mpsc::channel::<()>();
            let sink_pad = audio.filesink.static_pad("sink").unwrap();
            let probe_id =
                sink_pad.add_probe(gst::PadProbeType::EVENT_DOWNSTREAM, move |_pad, info| {
                    if let Some(gst::PadProbeData::Event(ref event)) = info.data {
                        if event.type_() == gst::EventType::Eos {
                            let _ = tx.send(());
                            return gst::PadProbeReturn::Drop;
                        }
                    }
                    gst::PadProbeReturn::Ok
                });
            let _ = rx.recv_timeout(std::time::Duration::from_secs(5));
            if let Some(id) = probe_id {
                sink_pad.remove_probe(id);
            }
        }
        audio.valve.set_property("drop", true);

        let _ = audio.filesink.set_state(gst::State::Null);
        let _ = audio.encoder.set_state(gst::State::Null);
        let _ = audio.queue.set_state(gst::State::Null);
        let _ = audio.valve.set_state(gst::State::Null);

        audio.filesink.set_property("location", "/dev/null");

        let _ = audio.filesink.sync_state_with_parent();
        let _ = audio.encoder.sync_state_with_parent();
        let _ = audio.queue.sync_state_with_parent();
        let _ = audio.valve.sync_state_with_parent();
    }

    let _ = video.filesink.set_state(gst::State::Null);
    let _ = video.encoder.set_state(gst::State::Null);
    let _ = video.videoconvert.set_state(gst::State::Null);
    let _ = video.queue.set_state(gst::State::Null);

    video.filesink.set_property("location", "/dev/null");

    let _ = video.filesink.sync_state_with_parent();
    let _ = video.encoder.sync_state_with_parent();
    let _ = video.videoconvert.sync_state_with_parent();
    let _ = video.queue.sync_state_with_parent();

    info!(
        frame_count,
        rec_valve_count, rec_queue_src_count, "Recording stopped (valves closed)"
    );
    Ok(RecordingStats {
        frame_count,
        pts_log,
    })
}

/// Query the duration of an MP4 (or any container `gst-pbutils`
/// understands) in seconds. Used by `/replay/start` for the session
/// metadata. Returns `Ok(0.0)` on discoverer failure rather than
/// erroring - the duration is informational, the replay can still
/// run.
pub fn query_media_duration(path: &Path) -> anyhow::Result<f64> {
    use gstreamer_pbutils as gst_pbutils;

    let uri = format!(
        "file://{}",
        path.canonicalize()
            .unwrap_or_else(|_| path.to_path_buf())
            .display()
    );
    let discoverer = gst_pbutils::Discoverer::new(gst::ClockTime::from_seconds(5))?;
    match discoverer.discover_uri(&uri) {
        Ok(info) => {
            let duration_s = info
                .duration()
                .map(|d| d.nseconds() as f64 / 1_000_000_000.0)
                .unwrap_or(0.0);
            info!(path = %path.display(), duration_s, "query_media_duration: ok");
            Ok(duration_s)
        }
        Err(e) => {
            warn!(path = %path.display(), error = %e, "query_media_duration: discoverer failed - duration unknown");
            Ok(0.0)
        }
    }
}

/// Resolve the absolute path to `libmetadata_export.so`.
///
/// The systemd unit sets `WorkingDirectory=$DEPLOY_PATH`, so the
/// in-tree build at `apps/hailo_postprocess/libmetadata_export.so`
/// relative to CWD is the canonical location. Falls back to the
/// legacy `/opt/robocup-ai-camera/...` path so a manually-launched
/// development binary keeps working when the deploy-relative copy
/// is missing.
pub(crate) fn resolve_meta_export_so_path() -> String {
    const RELATIVE: &str = "apps/hailo_postprocess/libmetadata_export.so";
    const LEGACY: &str = "/opt/robocup-ai-camera/apps/hailo_postprocess/libmetadata_export.so";

    if let Ok(cwd) = std::env::current_dir() {
        let candidate = cwd.join(RELATIVE);
        if candidate.exists() {
            let p = candidate.to_string_lossy().to_string();
            info!(path = %p, "meta_export: using deploy-relative .so");
            return p;
        }
    }
    warn!(
        legacy = LEGACY,
        "meta_export: deploy-relative .so missing; falling back to legacy /opt/ path"
    );
    LEGACY.to_string()
}

/// Set a pipeline to PLAYING and wait up to 5 s for the async
/// transition to complete. Opt-in `.dot` graph dump honours
/// `GST_DEBUG_DUMP_DOT_DIR` for triage.
pub fn start_pipeline(pipeline: &gst::Pipeline) -> anyhow::Result<()> {
    let ret = pipeline.set_state(gst::State::Playing)?;
    info!(state_change = ?ret, "Pipeline set_state(Playing) returned");

    let (res, cur, pending) = pipeline.state(gst::ClockTime::from_seconds(5));
    info!(result = ?res, current = ?cur, pending = ?pending, "Pipeline state after wait");

    if let Ok(dot_dir) = std::env::var("GST_DEBUG_DUMP_DOT_DIR") {
        let _ = std::fs::create_dir_all(&dot_dir);
        pipeline.debug_to_dot_file(gst::DebugGraphDetails::all(), "tee-pipeline-playing");
        info!(dot_dir, "Pipeline .dot graph dumped");
    }

    Ok(())
}

/// Stop a pipeline gracefully (set state to NULL, drop bus events).
pub fn stop_pipeline(pipeline: &gst::Pipeline) -> anyhow::Result<()> {
    pipeline.set_state(gst::State::Null)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_model(name: &str, width: u32, height: u32) -> ResolvedModel {
        ResolvedModel {
            display_name: name.to_string(),
            hef_path: format!("/fake/{}.hef", name),
            input_width: width,
            input_height: height,
            input_format: "RGB".to_string(),
            postprocess_so: "/fake/lib.so".to_string(),
            postprocess_fn: "fake_fn".to_string(),
            output_format: "yolov8".to_string(),
            label_map: None,
            labels_display: None,
            class_map: None,
            inference_fps: None,
            notes: None,
            publish_detections: true,
        }
    }

    #[test]
    fn ai_config_default_is_empty() {
        let cfg = AiConfig::default();
        assert!(cfg.object_detection.is_none());
        assert!(cfg.input_width().is_none());
    }

    #[test]
    fn ai_config_carries_object_detection_field() {
        let cfg = AiConfig {
            object_detection: Some(fake_model("det", 640, 640)),
        };
        assert_eq!(cfg.object_detection.as_ref().unwrap().display_name, "det");
        assert_eq!(cfg.input_width(), Some(640));
    }
}
