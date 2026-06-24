// Implements Rust media pipeline logic for streaming and camera processing.
// Author: Thomas Klute

//! Producer-side of the multi-pipeline media architecture.
//!
//! Two **standalone** `gst::Pipeline` shapes - live and playback -
//! each publish one camera stream over `intervideosink` /
//! `interaudiosink` channels. Consumer pipelines (`consumers.rs`)
//! subscribe via `intervideosrc` / `interaudiosrc`.
//!
//! Channels:
//! - [`VIDEO_CHANNEL`] (`"aicam-main"`)
//! - [`AUDIO_CHANNEL_RECORDING`] (`"aicam-audio-rec"`) - recording consumer
//! - [`AUDIO_CHANNEL_STREAMING`] (`"aicam-audio-stream"`) - streaming consumer
//!
//! The audio channel is split per consumer because `interaudiosrc`
//! drains the surface adapter destructively - see the constant's
//! docstring for the rationale.
//!
//! Only **one producer active at a time**. `intervideosink` does not
//! arbitrate competing producers (last-writer-wins, frames interleave)
//! so [`ProducerController`] enforces single-active discipline by
//! transitioning the outgoing producer to `Null` before the incoming
//! one reaches `Playing`.

use std::path::Path;
use std::str::FromStr;
use std::sync::{Arc, Mutex};

use gstreamer as gst;
use gstreamer::prelude::*;
use tracing::{info, warn};

/// Inter-pipeline video channel name. Producers publish via
/// `intervideosink(channel=…)`, consumers subscribe via
/// `intervideosrc(channel=…)`.
pub const VIDEO_CHANNEL: &str = "aicam-main";

/// Inter-pipeline channel name for the recording consumer's audio.
///
/// **Why split per consumer (vs. one shared channel)**:
/// `gst-plugins-bad`'s `interaudiosrc` *destructively* takes from
/// the shared `GstInterSurface::audio_adapter` - see
/// `gst_adapter_take_buffer` in `gstinteraudiosrc.c`. With two
/// consumers reading the same channel, they race to consume; each
/// gets ~half the bytes, and `interaudiosrc` prepends silence to
/// its output to fill the gap. The recording then plays back as
/// audio chopped up with silence - the "distortion when streaming
/// starts" failure mode.
///
/// We fix this by tee-ing the producer's audio chain once and
/// writing each branch into its own `aicam-audio-*` channel, so
/// every consumer's `interaudiosrc` has a dedicated adapter to
/// drain.
pub const AUDIO_CHANNEL_RECORDING: &str = "aicam-audio-rec";

/// Inter-pipeline channel name for the streaming consumer's audio.
/// See [`AUDIO_CHANNEL_RECORDING`] for the per-consumer-channel
/// rationale.
pub const AUDIO_CHANNEL_STREAMING: &str = "aicam-audio-stream";

/// Which GStreamer source element the live producer uses.
///
/// `Libcamera` (the existing Pi path) uses `libcamerasrc`. `Nvargus`
/// (the Jetson CSI path) uses `nvarguscamerasrc` plus an `nvvidconv`
/// bridge from NVMM to system memory because `intervideosink` cannot
/// consume NVMM buffers. `V4l2` (generic USB cameras / future
/// hardware) uses `v4l2src`. Every variant falls back to
/// `videotestsrc` when the configured source element isn't
/// registered with GStreamer - this lets the dev container build
/// the pipeline for unit tests on any backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CameraBackend {
    Libcamera,
    Nvargus,
    V4l2,
}

/// Combined producer-side orientation derived from the
/// two config flags `camera.flip_horizontal` + `camera.rotate_180`.
///
/// The four (rotate_180, flip_horizontal) combinations each map to
/// a single `videoflip` / `nvvidconv flip-method` enum value, so
/// the producer never needs more than one transform element (or
/// one VIC pass on Tegra) regardless of how the operator wants the
/// frames oriented.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Orientation {
    /// No transform; the renderer skips the element entirely on
    /// the CPU path and leaves the existing nvvidconv's
    /// `flip-method` at its default on the Nvargus path.
    Identity,
    /// Mirror left↔right only. Top/bottom unchanged.
    HorizontalFlip,
    /// 180° rotation - equivalent to flipping both axes.
    Rotate180,
    /// 180° + horizontal-flip - equivalent to a vertical-only
    /// flip (top↔bottom, sides unchanged). Tegra exposes this as
    /// a distinct `flip-method` value (a single VIC op) and so
    /// does CPU `videoflip`.
    VerticalFlip,
}

impl Orientation {
    /// Map the two config booleans to a single enum.
    pub fn from_flags(flip_horizontal: bool, rotate_180: bool) -> Self {
        match (rotate_180, flip_horizontal) {
            (false, false) => Self::Identity,
            (false, true) => Self::HorizontalFlip,
            (true, false) => Self::Rotate180,
            (true, true) => Self::VerticalFlip,
        }
    }

    /// GEnum nickname string accepted by both `videoflip method`
    /// and `nvvidconv flip-method`. `None` for `Identity` - the
    /// caller skips the element / leaves the property at its
    /// default in that case.
    pub fn method_str(self) -> Option<&'static str> {
        match self {
            Self::Identity => None,
            Self::HorizontalFlip => Some("horizontal-flip"),
            Self::Rotate180 => Some("rotate-180"),
            Self::VerticalFlip => Some("vertical-flip"),
        }
    }
}

impl CameraBackend {
    /// Parse the `deployment.camera_backend` string from `config.yaml`.
    /// Unknown values warn and fall back to `Libcamera` so a typo in
    /// the deployment script doesn't take the box down - the pipeline
    /// still builds against the (likely correct on Pi) default.
    pub fn parse(s: &str) -> Self {
        match s {
            "libcamera" => Self::Libcamera,
            "nvargus" => Self::Nvargus,
            "v4l2" => Self::V4l2,
            other => {
                warn!(
                    value = %other,
                    "CameraBackend::parse: unknown value, falling back to libcamera"
                );
                Self::Libcamera
            }
        }
    }
}

/// Build the **live** producer pipeline.
///
/// Topology:
/// ```text
/// libcamerasrc (or videotestsrc fallback)
///   → capsfilter(NV12, width × height @ fps)
///   → videoconvert
///   → intervideosink(channel="aicam-main")
///
/// (when audio_enabled and a microphone is present:)
/// alsasrc (or audiotestsrc fallback)
///   → audioconvert
///   → capsfilter(S16LE, 48 kHz, 2 ch, interleaved)
///   → interaudiosink(channel="aicam-audio-{rec,stream}")
/// ```
///
/// `intervideosink` / `interaudiosink` are configured with
/// `sync=true`, `async=false`, `enable-last-sample=false` -
/// `enable-last-sample` keeps the sink from buffering the most recent
/// sample for `last-sample` callers we don't use, and the `async=false`
/// keeps the live source from waiting for a preroll buffer it can't
/// generate on its own.
///
/// Returns the unstarted pipeline (caller transitions to PLAYING via
/// `set_state`). The audio sub-pipeline is added in the same
/// `gst::Pipeline` so the bus is shared.
#[allow(clippy::too_many_arguments)]
pub fn build_live_producer_pipeline(
    width: u32,
    height: u32,
    fps: u32,
    audio_enabled: bool,
    audio_device: Option<&str>,
    camera_backend: CameraBackend,
    orientation: Orientation,
) -> anyhow::Result<gst::Pipeline> {
    gst::init()?;

    let pipeline = gst::Pipeline::builder()
        .name("live_producer_pipeline")
        .build();

    // --- Video chain --- per-backend branching.
    let video_elements = build_live_video_head(camera_backend, width, height, fps, orientation)?;
    pipeline.add_many(&video_elements)?;
    gst::Element::link_many(&video_elements)?;

    // --- Optional audio chain ---
    if audio_enabled {
        match build_live_audio_chain(&pipeline, audio_device) {
            Ok(()) => info!(
                rec_channel = AUDIO_CHANNEL_RECORDING,
                stream_channel = AUDIO_CHANNEL_STREAMING,
                "live producer: audio chain built (tee → 2 sinks)"
            ),
            Err(e) => warn!(error = %e, "live producer: audio chain failed - video-only"),
        }
    }

    info!(
        width,
        height,
        fps,
        audio_enabled,
        channel = VIDEO_CHANNEL,
        "live producer pipeline built"
    );
    Ok(pipeline)
}

/// Live producer pipeline + audio-availability flag.
///
/// Returned by [`build_live_producer`]. Held in the
/// `AppState.live_producer` slot so `main.rs` can introspect whether
/// the audio chain came up (the gate for building the recording-audio
/// consumer + offering the audio-on-streaming option) without
/// re-detecting the device.
pub struct LiveProducer {
    pub pipeline: gst::Pipeline,
    /// True when the audio sub-pipeline (alsasrc → … → interaudiosink)
    /// was successfully wired up.
    pub audio_available: bool,
}

/// Build the live producer pipeline and report whether the audio
/// sub-pipeline was actually wired up.
///
/// Thin wrapper around [`build_live_producer_pipeline`]: the lower-level
/// builder is best-effort on audio (alsasrc errors get swallowed and
/// the chain is simply not added). This wrapper looks up the named
/// `live_producer_audio_tee` element to compute
/// [`LiveProducer::audio_available`].
///
/// The `ai_config` and `hailo_available` arguments are unused by
/// this builder - the AI consumer pipeline reads them directly via
/// `build_ai_consumer_pipeline` in `main.rs`. They're carried in the
/// signature so the call site in `main.rs::ensure_live_producer`
/// can pass through values it already has, without an awkward
/// shuffle of locals.
#[allow(clippy::too_many_arguments)]
pub fn build_live_producer(
    width: u32,
    height: u32,
    fps: u32,
    audio_enabled: bool,
    audio_device: Option<&str>,
    camera_backend: CameraBackend,
    flip_horizontal: bool,
    rotate_180: bool,
    _ai_config: &crate::pipeline::AiConfig,
    _hailo_available: bool,
) -> anyhow::Result<LiveProducer> {
    let orientation = Orientation::from_flags(flip_horizontal, rotate_180);
    let pipeline = build_live_producer_pipeline(
        width,
        height,
        fps,
        audio_enabled,
        audio_device,
        camera_backend,
        orientation,
    )?;
    // The per-consumer audio tee is the canonical
    // marker that the audio sub-pipeline came up (it sits upstream of
    // both rec + stream sinks; if it's there, both branches were
    // added too).
    let audio_available = pipeline.by_name("live_producer_audio_tee").is_some();
    info!(
        audio_available,
        "Live producer pipeline built - feeds aicam-main / aicam-audio-{{rec,stream}}"
    );
    Ok(LiveProducer {
        pipeline,
        audio_available,
    })
}

fn build_live_audio_chain(
    pipeline: &gst::Pipeline,
    audio_device: Option<&str>,
) -> anyhow::Result<()> {
    // Tee the audio chain once and write into a dedicated
    // interaudiosink per consumer.
    //
    //   alsasrc → audioconvert → caps → tee
    //     ├── queue → interaudiosink(channel=aicam-audio-rec)
    //     └── queue → interaudiosink(channel=aicam-audio-stream)
    //
    // Two interaudiosinks (one per consumer) instead of one shared
    // channel - see the docstring on AUDIO_CHANNEL_RECORDING for why
    // interaudiosrc cannot fan out from a single channel without
    // chopping the audio into silence-padded shards. Each branch
    // carries its own `queue` with `leaky=downstream` so an inactive
    // consumer can't back-pressure alsasrc into starving the other
    // branch - see `build_audio_fanout_branch`.
    let audio_src = make_audio_source("live_producer_audio_src", audio_device)?;

    let audioconvert = gst::ElementFactory::make("audioconvert")
        .name("live_producer_audioconvert")
        .build()?;

    let audio_capsfilter = gst::ElementFactory::make("capsfilter")
        .name("live_producer_audio_caps")
        .build()?;
    let audio_caps = gst::Caps::builder("audio/x-raw")
        .field("format", "S16LE")
        .field("rate", 48000i32)
        .field("channels", 2i32)
        .field("layout", "interleaved")
        .build();
    audio_capsfilter.set_property("caps", &audio_caps);

    let audio_tee = gst::ElementFactory::make("tee")
        .name("live_producer_audio_tee")
        .build()?;
    audio_tee.set_property("allow-not-linked", true);

    let (rec_queue, rec_sink) = build_audio_fanout_branch(
        "live_producer_audio_rec_queue",
        "live_producer_audio_rec_sink",
        AUDIO_CHANNEL_RECORDING,
    )?;
    let (stream_queue, stream_sink) = build_audio_fanout_branch(
        "live_producer_audio_stream_queue",
        "live_producer_audio_stream_sink",
        AUDIO_CHANNEL_STREAMING,
    )?;

    pipeline.add_many([
        &audio_src,
        &audioconvert,
        &audio_capsfilter,
        &audio_tee,
        &rec_queue,
        &rec_sink,
        &stream_queue,
        &stream_sink,
    ])?;
    gst::Element::link_many([&audio_src, &audioconvert, &audio_capsfilter, &audio_tee])?;
    gst::Element::link_many([&audio_tee, &rec_queue, &rec_sink])?;
    gst::Element::link_many([&audio_tee, &stream_queue, &stream_sink])?;
    Ok(())
}

/// Build the **playback** producer pipeline.
///
/// Topology:
/// ```text
/// filesrc(location=path)
///   → decodebin (dynamic pads)
///       video → videoscale → videoconvert → videorate
///         → capsfilter(NV12, 1920×1080 @ 30/1)
///         → identity(sync=true|false depending on `speed`)
///         → intervideosink(channel="aicam-main")
///       audio → audioconvert
///         → capsfilter(S16LE, 48 kHz, 2 ch, interleaved)
///         → interaudiosink(channel="aicam-audio-{rec,stream}")
/// ```
///
/// `speed` follows the same semantics as the existing
/// [`pipeline::build_replay_input_bin`]:
/// - `1.0` (default): realtime.
/// - `0.0`: drain at decode speed (disable identity sync).
/// - other positive values: scale the videorate output PTS so identity's
///   sync gate paces buffers at the new rate.
///
/// Negative or non-finite values are rejected.
pub fn build_playback_producer_pipeline(path: &Path, speed: f64) -> anyhow::Result<gst::Pipeline> {
    if !speed.is_finite() || speed < 0.0 {
        return Err(anyhow::anyhow!(
            "playback speed must be 0 or a positive finite number, got {speed}"
        ));
    }
    gst::init()?;

    let pipeline = gst::Pipeline::builder()
        .name("playback_producer_pipeline")
        .build();

    let location = path
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("playback path is not valid UTF-8"))?;

    let filesrc = gst::ElementFactory::make("filesrc")
        .name("playback_producer_filesrc")
        .build()?;
    filesrc.set_property("location", location);

    let decodebin = gst::ElementFactory::make("decodebin")
        .name("playback_producer_decodebin")
        .build()?;

    pipeline.add_many([&filesrc, &decodebin])?;
    filesrc.link(&decodebin)?;

    // --- Video sink chain (built upfront, decodebin links to it via pad-added) ---
    let vid_scale = gst::ElementFactory::make("videoscale")
        .name("playback_producer_videoscale")
        .build()?;
    let vid_convert = gst::ElementFactory::make("videoconvert")
        .name("playback_producer_videoconvert")
        .build()?;
    let vid_rate = gst::ElementFactory::make("videorate")
        .name("playback_producer_videorate")
        .build()?;
    // `skip-to-first=true`: when the playback pipeline starts after
    // the shared pipeline clock has advanced, videorate must not pad
    // output with duplicates of the first frame to fill the gap.
    vid_rate.set_property("skip-to-first", true);
    if speed > 0.0 && (speed - 1.0).abs() > f64::EPSILON {
        vid_rate.set_property("rate", speed);
    }

    let vid_capsfilter = gst::ElementFactory::make("capsfilter")
        .name("playback_producer_video_caps")
        .build()?;
    let vid_caps =
        gst::Caps::from_str("video/x-raw,format=NV12,width=1920,height=1080,framerate=30/1")?;
    vid_capsfilter.set_property("caps", &vid_caps);

    // identity sync=true paces buffers against the pipeline clock so
    // playback runs in realtime. `speed == 0.0` is the operator's
    // "Max" option in the UI: drain as fast as decode allows.
    let vid_sync = gst::ElementFactory::make("identity")
        .name("playback_producer_identity")
        .build()?;
    vid_sync.set_property("sync", speed != 0.0);

    let intervideosink = make_intervideosink("playback_producer_intervideosink")?;

    pipeline.add_many([
        &vid_scale,
        &vid_convert,
        &vid_rate,
        &vid_capsfilter,
        &vid_sync,
        &intervideosink,
    ])?;
    gst::Element::link_many([
        &vid_scale,
        &vid_convert,
        &vid_rate,
        &vid_capsfilter,
        &vid_sync,
        &intervideosink,
    ])?;

    // --- Audio sink chain (always built; decodebin links to it if the
    //     file carries audio. Otherwise the chain stays unlinked and
    //     idle - interaudiosink itself emits nothing, the consumer
    //     side falls back to silence via `interaudiosrc`'s `timeout`.) ---
    let audio_convert = gst::ElementFactory::make("audioconvert")
        .name("playback_producer_audioconvert")
        .build()?;
    let audio_capsfilter = gst::ElementFactory::make("capsfilter")
        .name("playback_producer_audio_caps")
        .build()?;
    let audio_caps = gst::Caps::builder("audio/x-raw")
        .field("format", "S16LE")
        .field("rate", 48000i32)
        .field("channels", 2i32)
        .field("layout", "interleaved")
        .build();
    audio_capsfilter.set_property("caps", &audio_caps);

    // Mirror the live producer's tee fan-out so playback
    // audio also drains into per-consumer channels. See
    // `build_audio_fanout_branch` for the per-channel rationale.
    let audio_tee = gst::ElementFactory::make("tee")
        .name("playback_producer_audio_tee")
        .build()?;
    audio_tee.set_property("allow-not-linked", true);
    let (rec_queue, rec_sink) = build_audio_fanout_branch(
        "playback_producer_audio_rec_queue",
        "playback_producer_audio_rec_sink",
        AUDIO_CHANNEL_RECORDING,
    )?;
    let (stream_queue, stream_sink) = build_audio_fanout_branch(
        "playback_producer_audio_stream_queue",
        "playback_producer_audio_stream_sink",
        AUDIO_CHANNEL_STREAMING,
    )?;

    pipeline.add_many([
        &audio_convert,
        &audio_capsfilter,
        &audio_tee,
        &rec_queue,
        &rec_sink,
        &stream_queue,
        &stream_sink,
    ])?;
    gst::Element::link_many([&audio_convert, &audio_capsfilter, &audio_tee])?;
    gst::Element::link_many([&audio_tee, &rec_queue, &rec_sink])?;
    gst::Element::link_many([&audio_tee, &stream_queue, &stream_sink])?;

    // Wire decodebin pad-added → video-sink-chain or audio-sink-chain
    // by examining the new pad's caps.
    let vid_scale_clone = vid_scale.clone();
    let audio_convert_clone = audio_convert.clone();
    decodebin.connect_pad_added(move |_dbin, src_pad| {
        let Some(caps) = src_pad.current_caps().or_else(|| src_pad.allowed_caps()) else {
            warn!("playback_producer: decodebin pad-added with no caps - ignoring");
            return;
        };
        let Some(structure) = caps.structure(0) else {
            return;
        };
        let media_type = structure.name();

        if media_type.starts_with("video/") {
            let sink_pad = match vid_scale_clone.static_pad("sink") {
                Some(p) => p,
                None => {
                    warn!("playback_producer: videoscale has no sink pad");
                    return;
                }
            };
            if sink_pad.is_linked() {
                return;
            }
            if let Err(e) = src_pad.link(&sink_pad) {
                warn!(error = ?e, "playback_producer: failed to link decodebin video → videoscale");
            } else {
                info!("playback_producer: decodebin video pad linked");
            }
        } else if media_type.starts_with("audio/") {
            let sink_pad = match audio_convert_clone.static_pad("sink") {
                Some(p) => p,
                None => {
                    warn!("playback_producer: audioconvert has no sink pad");
                    return;
                }
            };
            if sink_pad.is_linked() {
                return;
            }
            if let Err(e) = src_pad.link(&sink_pad) {
                warn!(error = ?e, "playback_producer: failed to link decodebin audio → audioconvert");
            } else {
                info!("playback_producer: decodebin audio pad linked");
            }
        }
    });

    // When decodebin reports `no-more-pads`, anything still unlinked
    // is a sink chain we built speculatively that the file doesn't
    // populate. Push an EOS event into it so the pipeline reaches
    // GST_MESSAGE_EOS on its bus when the file ends - without this,
    // an unlinked audio sink keeps the pipeline in a "waiting for
    // EOS from all sinks" state forever, and the EOS auto-revert
    // bus watch in `main.rs` never fires.
    let audio_convert_for_eos = audio_convert.clone();
    let vid_scale_for_eos = vid_scale.clone();
    decodebin.connect("no-more-pads", false, move |_args| {
        for (sink_chain_head, name) in [
            (&audio_convert_for_eos, "audio_convert"),
            (&vid_scale_for_eos, "vid_scale"),
        ] {
            let Some(pad) = sink_chain_head.static_pad("sink") else {
                continue;
            };
            if pad.is_linked() {
                continue;
            }
            // Send EOS into the unlinked sink-chain head pad so the
            // chain reaches EOS state when the rest of the pipeline
            // does. `pad.send_event` on an unlinked sink pad is the
            // canonical way to flush an unused branch.
            let sent = pad.send_event(gst::event::Eos::new());
            info!(
                chain = name,
                eos_accepted = sent,
                "playback_producer: no-more-pads, flushing unused sink chain with EOS"
            );
        }
        None
    });

    info!(
        path = %path.display(),
        speed,
        channel = VIDEO_CHANNEL,
        "playback producer pipeline built"
    );
    Ok(pipeline)
}

/// Lifecycle helper that enforces single-active discipline across the
/// live and playback producer pipelines.
///
/// `intervideosink` does not arbitrate competing producers - last
/// writer wins, frames interleave. The controller therefore transitions
/// the outgoing producer to `Null` *before* setting the incoming one to
/// `Playing`.
///
/// Both slots are `Option<gst::Pipeline>` so the controller can serve
/// callers that have one but not the other (e.g. live-only at boot,
/// playback built per `/replay/start`).
pub struct ProducerController {
    inner: Mutex<Inner>,
}

struct Inner {
    live: Option<gst::Pipeline>,
    playback: Option<gst::Pipeline>,
    /// Which slot last drove the channel. Used to short-circuit
    /// no-op transitions (idempotent `start_live` while live is
    /// already PLAYING).
    active: Active,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Active {
    None,
    Live,
    Playback,
}

impl Default for ProducerController {
    fn default() -> Self {
        Self::new()
    }
}

impl ProducerController {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(Inner {
                live: None,
                playback: None,
                active: Active::None,
            }),
        }
    }

    /// Install the live pipeline (built once at boot). Any previously
    /// installed live pipeline is replaced - the caller is responsible
    /// for stopping the old one first via `stop_all`.
    pub fn install_live(&self, pipeline: gst::Pipeline) {
        let mut inner = self.inner.lock().expect("ProducerController mutex");
        inner.live = Some(pipeline);
    }

    /// Switch the channel to the live producer. Idempotent: returns
    /// `Ok(())` when live is already active.
    pub fn start_live(&self) -> anyhow::Result<()> {
        let mut inner = self.inner.lock().expect("ProducerController mutex");
        if inner.active == Active::Live {
            return Ok(());
        }
        // 1. Transition the outgoing producer to NULL first so it
        //    releases the channel before the incoming one claims it.
        if let Some(playback) = inner.playback.take() {
            playback.set_state(gst::State::Null)?;
        }
        // 2. Bring live to PLAYING.
        let live = inner
            .live
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("live producer not installed"))?;
        live.set_state(gst::State::Playing)?;
        inner.active = Active::Live;
        Ok(())
    }

    /// Build the playback pipeline against the given file and switch
    /// the channel to it. Any previously running playback is stopped
    /// first.
    pub fn start_playback(&self, path: &Path, speed: f64) -> anyhow::Result<()> {
        let new_playback = build_playback_producer_pipeline(path, speed)?;
        self.transition_to_playback(new_playback)
    }

    /// Take ownership of an already-built playback pipeline and switch
    /// the channel to it. Used by [`Self::start_playback`] and by tests
    /// that inject a stub pipeline (the file-based playback builder
    /// requires a real on-disk fixture which the in-tree test
    /// environment does not carry).
    pub fn transition_to_playback(&self, new_playback: gst::Pipeline) -> anyhow::Result<()> {
        let mut inner = self.inner.lock().expect("ProducerController mutex");
        // 1. Tear down any prior playback session.
        if let Some(prev) = inner.playback.take() {
            prev.set_state(gst::State::Null)?;
        }
        // 2. Take the live source off the channel before bringing
        //    playback up.
        if let Some(live) = inner.live.as_ref() {
            live.set_state(gst::State::Null)?;
        }
        // 3. Bring playback to PLAYING.
        new_playback.set_state(gst::State::Playing)?;
        inner.playback = Some(new_playback);
        inner.active = Active::Playback;
        Ok(())
    }

    /// Stop the playback producer and return to live. Idempotent:
    /// returns `Ok(())` when no playback is active.
    pub fn stop_playback(&self) -> anyhow::Result<()> {
        let mut inner = self.inner.lock().expect("ProducerController mutex");
        if let Some(prev) = inner.playback.take() {
            prev.set_state(gst::State::Null)?;
        }
        if inner.active != Active::Playback {
            return Ok(());
        }
        // Fall back to live if installed.
        if let Some(live) = inner.live.as_ref() {
            live.set_state(gst::State::Playing)?;
            inner.active = Active::Live;
        } else {
            inner.active = Active::None;
        }
        Ok(())
    }

    /// Tear down both producers. Used on service shutdown.
    pub fn stop_all(&self) -> anyhow::Result<()> {
        let mut inner = self.inner.lock().expect("ProducerController mutex");
        if let Some(prev) = inner.playback.take() {
            prev.set_state(gst::State::Null)?;
        }
        if let Some(live) = inner.live.take() {
            live.set_state(gst::State::Null)?;
        }
        inner.active = Active::None;
        Ok(())
    }

    /// Test/observability helper: which producer is currently driving
    /// the channel?
    #[allow(dead_code)]
    pub fn active(&self) -> &'static str {
        match self.inner.lock().expect("ProducerController mutex").active {
            Active::None => "none",
            Active::Live => "live",
            Active::Playback => "playback",
        }
    }

    /// Borrow the bus of the currently-active playback pipeline if
    /// one is installed. Used by `main.rs` to set up an EOS watch
    /// that auto-reverts to live when the playback file ends.
    /// Returns `None` if no playback pipeline is installed or its
    /// bus is unavailable.
    pub fn playback_bus(&self) -> Option<gst::Bus> {
        self.inner
            .lock()
            .expect("ProducerController mutex")
            .playback
            .as_ref()
            .and_then(|p| p.bus())
    }
}

/// Type-erased pointer used by callers that wrap the controller in an
/// `Arc` for sharing across async tasks.
#[allow(dead_code)]
pub type SharedProducerController = Arc<ProducerController>;

// --- Element-factory helpers ---

/// Build the per-backend video head for the live producer.
///
/// Returns a `Vec<gst::Element>` in link order so the caller can do
/// `pipeline.add_many(&v)?; gst::Element::link_many(&v)?;` regardless
/// of which backend produced the chain. Last element is always the
/// `intervideosink` so the producer/consumer boundary stays intact.
///
/// Each backend's primary source element is wrapped in a
/// `videotestsrc` fallback so the dev container (no camera) can
/// still build the pipeline for unit tests. The fallback emits the
/// same caps the primary source would produce, just synthetic.
fn build_live_video_head(
    backend: CameraBackend,
    width: u32,
    height: u32,
    fps: u32,
    orientation: Orientation,
) -> anyhow::Result<Vec<gst::Element>> {
    // Shared sink - every backend feeds the same inter-pipeline
    // channel so consumers don't see the backend swap.
    let make_sink = || make_intervideosink("live_producer_intervideosink");

    // Shared raw-NV12 capsfilter (system memory). Used as the final
    // caps step before intervideosink for every backend.
    let raw_caps = gst::Caps::from_str(&format!(
        "video/x-raw,format=NV12,width={},height={},framerate={}/1",
        width, height, fps
    ))?;
    let make_raw_capsfilter = |name: &str| -> anyhow::Result<gst::Element> {
        let cf = gst::ElementFactory::make("capsfilter").name(name).build()?;
        cf.set_property("caps", &raw_caps);
        Ok(cf)
    };

    // videotestsrc fallback used by every backend so the dev
    // container builds without a real camera. Pattern + live flag
    // match what the Pi-only path used previously.
    let testsrc_fallback = |label: &str| -> anyhow::Result<gst::Element> {
        warn!(
            backend = ?backend,
            "live producer: source unavailable ({label}), falling back to videotestsrc"
        );
        let src = gst::ElementFactory::make("videotestsrc")
            .name("live_producer_video_src")
            .build()?;
        src.set_property_from_str("pattern", "ball");
        src.set_property("is-live", true);
        Ok(src)
    };

    // Build a videoflip element when the
    // operator opted in to any orientation other than Identity.
    // CPU path used by Libcamera / V4l2 / videotestsrc fallback
    // paths; the Nvargus real path folds the flip into the
    // existing nvvidconv via `flip-method=<…>` (Tegra VIC, zero
    // CPU) below. The method enum value is shared between the
    // CPU videoflip and Tegra nvvidconv flip-method properties.
    let videoflip_method = orientation.method_str();
    let make_videoflip = || -> anyhow::Result<gst::Element> {
        let flip = gst::ElementFactory::make("videoflip")
            .name("live_producer_video_flip")
            .build()?;
        if let Some(m) = videoflip_method {
            flip.set_property_from_str("method", m);
        }
        Ok(flip)
    };

    match backend {
        CameraBackend::Libcamera => {
            // libcamerasrc → caps NV12 → [videoflip] → videoconvert → intervideosink
            let src = gst::ElementFactory::make("libcamerasrc")
                .name("live_producer_video_src")
                .build()
                .or_else(|_| testsrc_fallback("libcamerasrc"))?;
            let cf = make_raw_capsfilter("live_producer_video_caps")?;
            let cv = gst::ElementFactory::make("videoconvert")
                .name("live_producer_videoconvert")
                .build()?;
            let mut elements: Vec<gst::Element> = vec![src, cf];
            if videoflip_method.is_some() {
                elements.push(make_videoflip()?);
            }
            elements.push(cv);
            elements.push(make_sink()?);
            Ok(elements)
        }
        CameraBackend::Nvargus => {
            // On a real Jetson: nvarguscamerasrc → caps NVMM NV12 →
            // nvvidconv → caps NV12 raw → intervideosink. The
            // nvvidconv bridge is mandatory because intervideosink
            // can't consume NVMM-resident buffers.
            //
            // On the dev container neither nvarguscamerasrc nor
            // nvvidconv are registered, so we explicitly fall back to
            // the libcamera-shaped simple chain. (The videotestsrc
            // fallback can't produce NVMM caps, so we have to detect
            // the fallback path and drop the NVMM caps step entirely.)
            if let Ok(src) = gst::ElementFactory::make("nvarguscamerasrc")
                .name("live_producer_video_src")
                .build()
            {
                let nvmm_caps = gst::Caps::from_str(&format!(
                    "video/x-raw(memory:NVMM),format=NV12,width={},height={},framerate={}/1",
                    width, height, fps
                ))?;
                let nvmm_capsfilter = gst::ElementFactory::make("capsfilter")
                    .name("live_producer_video_nvmm_caps")
                    .build()?;
                nvmm_capsfilter.set_property("caps", &nvmm_caps);
                let nvconv = gst::ElementFactory::make("nvvidconv")
                    .name("live_producer_video_nvvidconv")
                    .build()?;
                if let Some(m) = videoflip_method {
                    // Tegra VIC does the
                    // flip / rotation on the existing NVMM-NV12 →
                    // system-NV12 download - no extra pass, no CPU
                    // cost. `flip-method` accepts the same enum
                    // nicknames as videoflip's `method`
                    // (horizontal-flip, rotate-180, vertical-flip).
                    nvconv.set_property_from_str("flip-method", m);
                }
                let raw_capsfilter = make_raw_capsfilter("live_producer_video_caps")?;
                Ok(vec![
                    src,
                    nvmm_capsfilter,
                    nvconv,
                    raw_capsfilter,
                    make_sink()?,
                ])
            } else {
                // Dev container fallback. Same CPU-flip shape as
                // Libcamera since videotestsrc can't produce NVMM.
                let src = testsrc_fallback("nvarguscamerasrc")?;
                let cf = make_raw_capsfilter("live_producer_video_caps")?;
                let cv = gst::ElementFactory::make("videoconvert")
                    .name("live_producer_videoconvert")
                    .build()?;
                let mut elements: Vec<gst::Element> = vec![src, cf];
                if videoflip_method.is_some() {
                    elements.push(make_videoflip()?);
                }
                elements.push(cv);
                elements.push(make_sink()?);
                Ok(elements)
            }
        }
        CameraBackend::V4l2 => {
            // v4l2src → caps NV12 → [videoflip] → videoconvert → intervideosink
            let src = gst::ElementFactory::make("v4l2src")
                .name("live_producer_video_src")
                .build()
                .or_else(|_| testsrc_fallback("v4l2src"))?;
            let cf = make_raw_capsfilter("live_producer_video_caps")?;
            let cv = gst::ElementFactory::make("videoconvert")
                .name("live_producer_videoconvert")
                .build()?;
            let mut elements: Vec<gst::Element> = vec![src, cf];
            if videoflip_method.is_some() {
                elements.push(make_videoflip()?);
            }
            elements.push(cv);
            elements.push(make_sink()?);
            Ok(elements)
        }
    }
}

/// Try `alsasrc` (with optional device), fall back to
/// `audiotestsrc(silence)` for dev machines without a microphone.
fn make_audio_source(name: &str, audio_device: Option<&str>) -> anyhow::Result<gst::Element> {
    if let Ok(src) = gst::ElementFactory::make("alsasrc").name(name).build() {
        if let Some(dev) = audio_device {
            src.set_property("device", dev);
        }
        return Ok(src);
    }
    warn!("alsasrc not available, falling back to audiotestsrc");
    let src = gst::ElementFactory::make("audiotestsrc")
        .name(name)
        .build()?;
    src.set_property("is-live", true);
    src.set_property_from_str("wave", "silence");
    Ok(src)
}

fn make_intervideosink(name: &str) -> anyhow::Result<gst::Element> {
    let sink = gst::ElementFactory::make("intervideosink")
        .name(name)
        .build()?;
    sink.set_property("channel", VIDEO_CHANNEL);
    sink.set_property("sync", true);
    sink.set_property("async", false);
    sink.set_property("enable-last-sample", false);
    Ok(sink)
}

/// One branch off the producer's audio tee:
/// `queue (leaky=downstream, max-size-time=200 ms) → interaudiosink(channel)`.
///
/// `leaky=downstream` lets the branch silently drop audio when the
/// consumer on the other end of the channel is inactive (e.g. when
/// streaming hasn't been started yet). Without it, the queue would
/// fill, back-pressure alsasrc, and the *other* branch's audio would
/// stall too - leaving recording's audio choppy whenever the
/// streaming side wasn't draining.
///
/// 200 ms is well above the interaudiosink's own surface flush
/// threshold (`buffer-time`, default 200 ms) so the queue normally
/// stays empty; it's only there as a leak relief valve.
fn build_audio_fanout_branch(
    queue_name: &str,
    sink_name: &str,
    channel: &str,
) -> anyhow::Result<(gst::Element, gst::Element)> {
    let queue = gst::ElementFactory::make("queue")
        .name(queue_name)
        .build()?;
    queue.set_property_from_str("leaky", "downstream");
    queue.set_property("max-size-time", 200_000_000u64);
    queue.set_property("max-size-buffers", 0u32);
    queue.set_property("max-size-bytes", 0u32);

    let sink = gst::ElementFactory::make("interaudiosink")
        .name(sink_name)
        .build()?;
    sink.set_property("channel", channel);
    sink.set_property("sync", true);
    sink.set_property("async", false);
    sink.set_property("enable-last-sample", false);
    Ok((queue, sink))
}

#[cfg(test)]
mod tests {
    use super::*;

    static GST_INIT: std::sync::Once = std::sync::Once::new();

    fn init_gst() {
        GST_INIT.call_once(|| {
            let _ = gst::init();
        });
    }

    fn fixture_path(relative: &str) -> std::path::PathBuf {
        let manifest_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let project_root = manifest_dir.parent().unwrap().parent().unwrap();
        project_root.join(relative)
    }

    #[test]
    fn channel_constants_are_stable() {
        // Stable channel names - consumer pipelines depend on them.
        assert_eq!(VIDEO_CHANNEL, "aicam-main");
        assert_eq!(AUDIO_CHANNEL_RECORDING, "aicam-audio-rec");
        assert_eq!(AUDIO_CHANNEL_STREAMING, "aicam-audio-stream");
    }

    /// Every backend builds in the dev container - the
    /// fallback chain works for every backend.
    #[test]
    fn live_builder_constructs_for_every_camera_backend() {
        init_gst();
        for backend in [
            CameraBackend::Libcamera,
            CameraBackend::Nvargus,
            CameraBackend::V4l2,
        ] {
            let pipeline = build_live_producer_pipeline(
                640,
                480,
                30,
                false,
                None,
                backend,
                Orientation::Identity,
            )
            .unwrap_or_else(|e| panic!("live builder failed for {backend:?}: {e}"));
            assert!(
                pipeline.by_name("live_producer_intervideosink").is_some(),
                "{backend:?}: intervideosink must exist",
            );
            let _ = pipeline.set_state(gst::State::Null);
        }
    }

    /// Parse defaults to Libcamera on unknown so a typo in
    /// the deployment script can't take the box down - the (likely
    /// correct on Pi) default still produces a pipeline.
    #[test]
    fn camera_backend_parse_table() {
        assert_eq!(CameraBackend::parse("libcamera"), CameraBackend::Libcamera);
        assert_eq!(CameraBackend::parse("nvargus"), CameraBackend::Nvargus);
        assert_eq!(CameraBackend::parse("v4l2"), CameraBackend::V4l2);
        assert_eq!(CameraBackend::parse("typo"), CameraBackend::Libcamera);
        assert_eq!(CameraBackend::parse(""), CameraBackend::Libcamera);
    }

    /// With `flip_horizontal: false` (the default), the
    /// producer pipeline must be byte-identical to the default path - no
    /// `live_producer_video_flip` element inserted, no extra cap step.
    /// This is the **must-not-degrade-performance** path.
    #[test]
    fn live_builder_flip_off_inserts_no_videoflip() {
        init_gst();
        for backend in [
            CameraBackend::Libcamera,
            CameraBackend::Nvargus,
            CameraBackend::V4l2,
        ] {
            let pipeline = build_live_producer_pipeline(
                640,
                480,
                30,
                false,
                None,
                backend,
                Orientation::Identity,
            )
            .unwrap_or_else(|e| panic!("flip-off builder failed for {backend:?}: {e}"));
            assert!(
                pipeline.by_name("live_producer_video_flip").is_none(),
                "{backend:?}: videoflip must NOT be inserted when flip_horizontal=false"
            );
            let _ = pipeline.set_state(gst::State::Null);
        }
    }

    /// With `flip_horizontal: true`, the Libcamera path
    /// inserts a `videoflip method=horizontal-flip` element between
    /// the source-side caps and `videoconvert`. (The Nvargus real
    /// path uses `nvvidconv flip-method=horizontal-flip` on the
    /// existing converter and is only reachable on Tegra hardware -
    /// the dev-container Nvargus path falls back to videotestsrc and
    /// the CPU-flip shape, covered by the next test.)
    #[test]
    fn live_builder_flip_on_libcamera_inserts_videoflip() {
        init_gst();
        let pipeline = build_live_producer_pipeline(
            640,
            480,
            30,
            false,
            None,
            CameraBackend::Libcamera,
            Orientation::HorizontalFlip,
        )
        .expect("flip-on Libcamera build");
        let flip = pipeline
            .by_name("live_producer_video_flip")
            .expect("videoflip must exist when flip_horizontal=true on Libcamera");
        assert_eq!(flip.factory().unwrap().name(), "videoflip");
        // method is a GEnum (GstVideoOrientationMethod). Read via
        // property_value().serialize() to get the nick back and avoid
        // a gstreamer-video dep just for the enum type.
        let m = flip.property_value("method");
        let nick = m.serialize().expect("serialize enum value");
        assert_eq!(nick.as_str(), "horizontal-flip");
        let _ = pipeline.set_state(gst::State::Null);
    }

    /// The four (rotate_180, flip_horizontal)
    /// combinations each map to a single videoflip / nvvidconv
    /// flip-method enum value. This pins the mapping so a future
    /// re-order doesn't silently break operator config.
    #[test]
    fn orientation_from_flags_table() {
        assert_eq!(Orientation::from_flags(false, false), Orientation::Identity);
        assert_eq!(
            Orientation::from_flags(true, false),
            Orientation::HorizontalFlip
        );
        assert_eq!(Orientation::from_flags(false, true), Orientation::Rotate180);
        assert_eq!(
            Orientation::from_flags(true, true),
            Orientation::VerticalFlip
        );
    }

    /// Each variant maps to the GEnum nickname accepted
    /// by both `videoflip method` and `nvvidconv flip-method`.
    /// Identity returns `None` so the renderer skips the element /
    /// leaves the existing nvvidconv at its default.
    #[test]
    fn orientation_method_str_mapping() {
        assert_eq!(Orientation::Identity.method_str(), None);
        assert_eq!(
            Orientation::HorizontalFlip.method_str(),
            Some("horizontal-flip")
        );
        assert_eq!(Orientation::Rotate180.method_str(), Some("rotate-180"));
        assert_eq!(
            Orientation::VerticalFlip.method_str(),
            Some("vertical-flip")
        );
    }

    /// Producer inserts a single videoflip element with
    /// the right method for each non-Identity orientation. (Real
    /// Jetson uses the existing nvvidconv's flip-method property
    /// instead - only reachable on Tegra hardware; covered by
    /// operator validation.)
    #[test]
    fn live_builder_orientation_maps_to_videoflip_method() {
        init_gst();
        for (orientation, expected_nick) in [
            (Orientation::HorizontalFlip, "horizontal-flip"),
            (Orientation::Rotate180, "rotate-180"),
            (Orientation::VerticalFlip, "vertical-flip"),
        ] {
            let pipeline = build_live_producer_pipeline(
                640,
                480,
                30,
                false,
                None,
                CameraBackend::Libcamera,
                orientation,
            )
            .unwrap_or_else(|e| panic!("build failed for {orientation:?}: {e}"));
            let flip = pipeline
                .by_name("live_producer_video_flip")
                .unwrap_or_else(|| panic!("videoflip element must exist for {orientation:?}"));
            let nick = flip
                .property_value("method")
                .serialize()
                .expect("serialize enum value");
            assert_eq!(
                nick.as_str(),
                expected_nick,
                "for orientation {orientation:?}"
            );
            let _ = pipeline.set_state(gst::State::Null);
        }
    }

    /// Dev-container Nvargus falls back to videotestsrc
    /// (no nvarguscamerasrc available), so the flip lands as a
    /// `videoflip` element the same way Libcamera does. On real
    /// Jetson the flip is folded into the existing nvvidconv via
    /// `flip-method=4` (covered by hardware validation).
    #[test]
    fn live_builder_flip_on_nvargus_devcontainer_falls_back_to_videoflip() {
        init_gst();
        let pipeline = build_live_producer_pipeline(
            640,
            480,
            30,
            false,
            None,
            CameraBackend::Nvargus,
            Orientation::HorizontalFlip,
        )
        .expect("flip-on Nvargus build (videotestsrc fallback)");
        assert!(
            pipeline.by_name("live_producer_video_flip").is_some(),
            "dev-container Nvargus fallback must insert videoflip"
        );
        let _ = pipeline.set_state(gst::State::Null);
    }

    #[test]
    fn live_builder_video_only_constructs() {
        // The dev container has no libcamerasrc; the builder must
        // fall through to videotestsrc and still produce a valid
        // pipeline.
        init_gst();
        let pipeline = build_live_producer_pipeline(
            1920,
            1080,
            30,
            false,
            None,
            CameraBackend::Libcamera,
            Orientation::Identity,
        )
        .expect("live producer (video-only) should build");

        // The intervideosink must exist on the channel.
        let sink = pipeline
            .by_name("live_producer_intervideosink")
            .expect("intervideosink must exist");
        assert_eq!(sink.factory().unwrap().name(), "intervideosink");
        assert_eq!(sink.property::<String>("channel"), VIDEO_CHANNEL);
    }

    #[test]
    fn live_builder_with_audio_constructs() {
        // The dev container has no alsasrc either - `audio_enabled=true`
        // takes the audiotestsrc fallback path. The audio chain is
        // best-effort; we only assert the video chain is present.
        init_gst();
        let pipeline = build_live_producer_pipeline(
            1920,
            1080,
            30,
            true,
            None,
            CameraBackend::Libcamera,
            Orientation::Identity,
        )
        .expect("live producer (with audio) should build");

        assert!(pipeline.by_name("live_producer_intervideosink").is_some());
        // The audio chain now has per-consumer interaudiosinks.
        // Whenever the audio chain came up, both branches were added -
        // assert both sinks exist on their respective channels.
        let rec_sink = pipeline
            .by_name("live_producer_audio_rec_sink")
            .expect("audio rec sink must exist when audio chain was built");
        assert_eq!(
            rec_sink.property::<String>("channel"),
            AUDIO_CHANNEL_RECORDING
        );
        let stream_sink = pipeline
            .by_name("live_producer_audio_stream_sink")
            .expect("audio stream sink must exist when audio chain was built");
        assert_eq!(
            stream_sink.property::<String>("channel"),
            AUDIO_CHANNEL_STREAMING
        );
    }

    #[test]
    fn live_builder_reaches_paused() {
        // PAUSED is the highest state the dev container can reach
        // without a real video output. PLAYING also needs a
        // consumer-side intervideosrc.
        init_gst();
        let pipeline = build_live_producer_pipeline(
            640,
            480,
            30,
            false,
            None,
            CameraBackend::Libcamera,
            Orientation::Identity,
        )
        .expect("live producer should build");
        let res = pipeline.set_state(gst::State::Paused);
        // `Async` is fine - preroll completes asynchronously.
        // `NoPreroll` is the proper return for live sources
        // (the videotestsrc fallback sets is-live=true to
        // match the real libcamerasrc / nvarguscamerasrc / v4l2src
        // behaviour); we only assert the state-change itself was OK.
        assert!(
            matches!(
                res,
                Ok(gst::StateChangeSuccess::Success
                    | gst::StateChangeSuccess::Async
                    | gst::StateChangeSuccess::NoPreroll)
            ),
            "live producer must reach PAUSED, got {res:?}"
        );
        let _ = pipeline.set_state(gst::State::Null);
    }

    #[test]
    fn playback_builder_with_audio_fixture_constructs() {
        // Reuses the same fixture the legacy replay-bin tests use.
        init_gst();
        let path = fixture_path("fixtures/replay/sample_with_audio.mp4");
        let pipeline = build_playback_producer_pipeline(&path, 1.0)
            .expect("playback producer should build against fixture");

        assert!(pipeline
            .by_name("playback_producer_intervideosink")
            .is_some());
        // Per-consumer audio sinks.
        assert!(pipeline
            .by_name("playback_producer_audio_rec_sink")
            .is_some());
        assert!(pipeline
            .by_name("playback_producer_audio_stream_sink")
            .is_some());
    }

    #[test]
    fn playback_builder_negative_speed_rejected() {
        init_gst();
        let path = fixture_path("fixtures/replay/sample_with_audio.mp4");
        let err =
            build_playback_producer_pipeline(&path, -1.0).expect_err("negative speed must error");
        assert!(err.to_string().contains("speed"));
    }

    #[test]
    fn playback_builder_max_speed_disables_identity_sync() {
        // speed=0.0 → identity.sync=false (drain at decode rate).
        init_gst();
        let path = fixture_path("fixtures/replay/sample_with_audio.mp4");
        let pipeline = build_playback_producer_pipeline(&path, 0.0)
            .expect("playback producer should build at speed=0");
        let identity = pipeline
            .by_name("playback_producer_identity")
            .expect("identity element must exist");
        assert!(!identity.property::<bool>("sync"));
    }

    #[test]
    fn playback_builder_2x_speed_sets_videorate_rate() {
        init_gst();
        let path = fixture_path("fixtures/replay/sample_with_audio.mp4");
        let pipeline = build_playback_producer_pipeline(&path, 2.0)
            .expect("playback producer should build at speed=2");
        let vr = pipeline
            .by_name("playback_producer_videorate")
            .expect("videorate element must exist");
        let rate: f64 = vr.property("rate");
        assert!((rate - 2.0).abs() < f64::EPSILON);
    }

    #[test]
    fn playback_builder_realtime_speed_leaves_videorate_rate_default() {
        // speed=1.0 should leave videorate.rate at its factory default
        // (1.0). Setting it would fight identity.sync's pacing.
        init_gst();
        let path = fixture_path("fixtures/replay/sample_with_audio.mp4");
        let pipeline = build_playback_producer_pipeline(&path, 1.0)
            .expect("playback producer should build at speed=1");
        let vr = pipeline
            .by_name("playback_producer_videorate")
            .expect("videorate element must exist");
        let rate: f64 = vr.property("rate");
        assert!((rate - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn controller_starts_uninitialised() {
        let c = ProducerController::new();
        assert_eq!(c.active(), "none");
    }

    #[test]
    fn controller_start_live_requires_install() {
        let c = ProducerController::new();
        let err = c
            .start_live()
            .expect_err("start_live without install fails");
        assert!(err.to_string().contains("live"));
    }

    /// Build a stub playback pipeline that exercises the same
    /// transition machinery without depending on an on-disk file
    /// fixture. The dev container does not ship recorded mp4s, and
    /// `build_playback_producer_pipeline` only opens the file at
    /// state-change time. This stub uses `videotestsrc` which
    /// transitions cleanly without any external state.
    fn build_stub_playback_pipeline() -> gst::Pipeline {
        let pipeline = gst::Pipeline::builder()
            .name("stub_playback_pipeline")
            .build();
        let src = gst::ElementFactory::make("videotestsrc")
            .name("stub_playback_src")
            .build()
            .unwrap();
        src.set_property("is-live", false);
        let convert = gst::ElementFactory::make("videoconvert")
            .name("stub_playback_convert")
            .build()
            .unwrap();
        let sink = make_intervideosink("stub_playback_intervideosink").unwrap();
        pipeline.add_many([&src, &convert, &sink]).unwrap();
        gst::Element::link_many([&src, &convert, &sink]).unwrap();
        pipeline
    }

    #[test]
    fn controller_single_active_invariant() {
        // Drive start_live → transition_to_playback → start_live and
        // assert each transition cycles the previous producer to NULL
        // before the next reaches PLAYING. The mutex-internal state
        // machine is the source of truth - both pipelines are not
        // allowed to be Playing simultaneously.
        init_gst();
        let live = build_live_producer_pipeline(
            640,
            480,
            30,
            false,
            None,
            CameraBackend::Libcamera,
            Orientation::Identity,
        )
        .expect("live producer should build");

        let c = ProducerController::new();
        c.install_live(live);

        c.start_live().expect("start_live should succeed");
        assert_eq!(c.active(), "live");

        c.transition_to_playback(build_stub_playback_pipeline())
            .expect("transition_to_playback should succeed");
        assert_eq!(c.active(), "playback");

        c.start_live()
            .expect("start_live (re-entry) should succeed");
        assert_eq!(c.active(), "live");

        c.stop_all().expect("stop_all should succeed");
        assert_eq!(c.active(), "none");
    }

    #[test]
    fn controller_stop_playback_falls_back_to_live() {
        init_gst();
        let live = build_live_producer_pipeline(
            640,
            480,
            30,
            false,
            None,
            CameraBackend::Libcamera,
            Orientation::Identity,
        )
        .expect("live producer should build");

        let c = ProducerController::new();
        c.install_live(live);
        c.start_live().expect("start_live");
        c.transition_to_playback(build_stub_playback_pipeline())
            .expect("transition_to_playback");
        assert_eq!(c.active(), "playback");

        c.stop_playback().expect("stop_playback");
        assert_eq!(c.active(), "live");

        c.stop_all().expect("stop_all");
    }

    #[test]
    fn controller_start_playback_with_missing_file_returns_error() {
        // `start_playback` builds the playback pipeline (succeeds - no
        // file open at construction) and then transitions it to
        // PLAYING (fails - filesrc cannot open a non-existent file).
        // The error must propagate as `Err` rather than panicking, and
        // the controller's active state must NOT advance to "playback".
        init_gst();
        let live = build_live_producer_pipeline(
            640,
            480,
            30,
            false,
            None,
            CameraBackend::Libcamera,
            Orientation::Identity,
        )
        .expect("live producer should build");
        let c = ProducerController::new();
        c.install_live(live);
        c.start_live().expect("start_live");

        let missing = std::path::Path::new("/nonexistent/missing.mp4");
        let err = c
            .start_playback(missing, 1.0)
            .expect_err("start_playback against missing file must error");
        // Whatever the upstream gst error wording, it must surface
        // through the controller - and the live-cycle-down step is
        // already complete by the time we observe the failure (the
        // controller does not roll back). The acceptance is just:
        // we got an error, and the active state is not "playback".
        assert!(!err.to_string().is_empty());
        assert_ne!(c.active(), "playback");
        c.stop_all().expect("stop_all");
    }

    #[test]
    fn controller_stop_playback_idempotent_when_inactive() {
        let c = ProducerController::new();
        // No live, no playback installed. stop_playback should still
        // be Ok and active should remain "none".
        c.stop_playback().expect("stop_playback no-op");
        assert_eq!(c.active(), "none");
    }
}
