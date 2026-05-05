// Implements Rust media pipeline logic for streaming and camera processing.
// Author: Thomas Klute

//! Object detection preview - annotated frames from the Hailo AI branch.
//!
//! This module backs the `/object_detection_preview/frame` endpoint shown on
//! the Object Detection page. The other preview in the system is
//! `camera_preview` (raw camera JPEGs, dashboard + recording page).
//!
//! When the AI branch includes hailooverlay → jpegenc → appsink, this module
//! sets up a callback on the terminal `ai_sink` element to store the latest
//! annotated JPEG in a shared buffer. The element is still named `ai_sink`
//! because it is the generic terminal of the AI branch - depending on
//! hailo_available it is either this annotated-preview AppSink or a raw
//! frame-export AppSink owned by `frame_export.rs`.

use std::sync::{Arc, RwLock};

use gstreamer_app as gst_app;
use tracing::info;

/// Shared buffer holding the latest annotated JPEG frame from hailooverlay.
pub type ObjectDetectionPreviewBuffer = Arc<RwLock<Vec<u8>>>;

/// Create a new empty object-detection preview buffer.
pub fn new_object_detection_preview_buffer() -> ObjectDetectionPreviewBuffer {
    Arc::new(RwLock::new(Vec::new()))
}

/// Set up the ai_sink appsink callback to store annotated JPEG frames.
///
/// The appsink must already be in the pipeline as "ai_sink" and must receive
/// JPEG-encoded buffers (from hailooverlay → videoconvert → jpegenc chain).
pub fn setup_object_detection_preview(
    pipeline: &gstreamer::Pipeline,
    buffer: ObjectDetectionPreviewBuffer,
) -> anyhow::Result<()> {
    use gstreamer::prelude::*;

    let appsink_el = pipeline
        .by_name("ai_sink")
        .ok_or_else(|| anyhow::anyhow!("ai_sink not found in pipeline"))?;

    let appsink = appsink_el
        .downcast::<gst_app::AppSink>()
        .map_err(|_| anyhow::anyhow!("ai_sink is not an AppSink - Hailo not active?"))?;

    appsink.set_callbacks(
        gst_app::AppSinkCallbacks::builder()
            .new_sample(move |sink| {
                let sample = sink.pull_sample().map_err(|_| gstreamer::FlowError::Eos)?;
                let buf = sample.buffer().ok_or(gstreamer::FlowError::Error)?;
                let map = buf
                    .map_readable()
                    .map_err(|_| gstreamer::FlowError::Error)?;
                if let Ok(mut frame) = buffer.write() {
                    frame.clear();
                    frame.extend_from_slice(map.as_slice());
                }
                Ok(gstreamer::FlowSuccess::Ok)
            })
            .build(),
    );

    info!("Object detection preview appsink configured (annotated JPEG from hailooverlay)");
    Ok(())
}
