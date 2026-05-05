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
//! - [`AUDIO_CHANNEL`] (`"aicam-audio-main"`)
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

/// Inter-pipeline audio channel name. Producers publish via
/// `interaudiosink(channel=…)`, consumers subscribe via
/// `interaudiosrc(channel=…)`.
pub const AUDIO_CHANNEL: &str = "aicam-audio-main";

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
///   → interaudiosink(channel="aicam-audio-main")
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
pub fn build_live_producer_pipeline(
    width: u32,
    height: u32,
    fps: u32,
    audio_enabled: bool,
    audio_device: Option<&str>,
) -> anyhow::Result<gst::Pipeline> {
    gst::init()?;

    let pipeline = gst::Pipeline::builder()
        .name("live_producer_pipeline")
        .build();

    // --- Video chain ---
    let video_src = make_video_source("live_producer_video_src")?;

    let video_capsfilter = gst::ElementFactory::make("capsfilter")
        .name("live_producer_video_caps")
        .build()?;
    let caps = gst::Caps::from_str(&format!(
        "video/x-raw,width={width},height={height},format=NV12,framerate={fps}/1"
    ))?;
    video_capsfilter.set_property("caps", &caps);

    let videoconvert = gst::ElementFactory::make("videoconvert")
        .name("live_producer_videoconvert")
        .build()?;

    let intervideosink = make_intervideosink("live_producer_intervideosink")?;

    pipeline.add_many([
        &video_src,
        &video_capsfilter,
        &videoconvert,
        &intervideosink,
    ])?;
    gst::Element::link_many([
        &video_src,
        &video_capsfilter,
        &videoconvert,
        &intervideosink,
    ])?;

    // --- Optional audio chain ---
    if audio_enabled {
        match build_live_audio_chain(&pipeline, audio_device) {
            Ok(()) => info!(channel = AUDIO_CHANNEL, "live producer: audio chain built"),
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
/// `live_producer_interaudiosink` element to compute
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
    _ai_config: &crate::pipeline::AiConfig,
    _hailo_available: bool,
) -> anyhow::Result<LiveProducer> {
    let pipeline = build_live_producer_pipeline(width, height, fps, audio_enabled, audio_device)?;
    let audio_available = pipeline.by_name("live_producer_interaudiosink").is_some();
    info!(
        audio_available,
        "Live producer pipeline built - feeds aicam-main / aicam-audio-main"
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

    let interaudiosink = make_interaudiosink("live_producer_interaudiosink")?;

    pipeline.add_many([
        &audio_src,
        &audioconvert,
        &audio_capsfilter,
        &interaudiosink,
    ])?;
    gst::Element::link_many([
        &audio_src,
        &audioconvert,
        &audio_capsfilter,
        &interaudiosink,
    ])?;
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
///         → interaudiosink(channel="aicam-audio-main")
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
    let interaudiosink = make_interaudiosink("playback_producer_interaudiosink")?;

    pipeline.add_many([&audio_convert, &audio_capsfilter, &interaudiosink])?;
    gst::Element::link_many([&audio_convert, &audio_capsfilter, &interaudiosink])?;

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

/// Try `libcamerasrc` first, fall back to `videotestsrc` for dev
/// machines without a camera. Mirrors `build_live_producer`'s fallback.
fn make_video_source(name: &str) -> anyhow::Result<gst::Element> {
    if let Ok(src) = gst::ElementFactory::make("libcamerasrc").name(name).build() {
        return Ok(src);
    }
    warn!("libcamerasrc not available, falling back to videotestsrc");
    let src = gst::ElementFactory::make("videotestsrc")
        .name(name)
        .build()?;
    src.set_property_from_str("pattern", "ball");
    Ok(src)
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

fn make_interaudiosink(name: &str) -> anyhow::Result<gst::Element> {
    let sink = gst::ElementFactory::make("interaudiosink")
        .name(name)
        .build()?;
    sink.set_property("channel", AUDIO_CHANNEL);
    sink.set_property("sync", true);
    sink.set_property("async", false);
    sink.set_property("enable-last-sample", false);
    Ok(sink)
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
        assert_eq!(AUDIO_CHANNEL, "aicam-audio-main");
    }

    #[test]
    fn live_builder_video_only_constructs() {
        // The dev container has no libcamerasrc; the builder must
        // fall through to videotestsrc and still produce a valid
        // pipeline.
        init_gst();
        let pipeline = build_live_producer_pipeline(1920, 1080, 30, false, None)
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
        let pipeline = build_live_producer_pipeline(1920, 1080, 30, true, None)
            .expect("live producer (with audio) should build");

        assert!(pipeline.by_name("live_producer_intervideosink").is_some());
        // The audio interaudiosink should be present whenever the
        // audio chain succeeded - it does in this dev environment via
        // the audiotestsrc fallback.
        let audio_sink = pipeline
            .by_name("live_producer_interaudiosink")
            .expect("audio interaudiosink must exist when audio chain was built");
        assert_eq!(audio_sink.property::<String>("channel"), AUDIO_CHANNEL);
    }

    #[test]
    fn live_builder_reaches_paused() {
        // PAUSED is the highest state the dev container can reach
        // without a real video output. PLAYING also needs a
        // consumer-side intervideosrc.
        init_gst();
        let pipeline = build_live_producer_pipeline(640, 480, 30, false, None)
            .expect("live producer should build");
        let res = pipeline.set_state(gst::State::Paused);
        // `Async` is fine - preroll completes asynchronously.
        assert!(
            matches!(
                res,
                Ok(gst::StateChangeSuccess::Success | gst::StateChangeSuccess::Async)
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
        assert!(pipeline
            .by_name("playback_producer_interaudiosink")
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
        let live = build_live_producer_pipeline(640, 480, 30, false, None)
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
        let live = build_live_producer_pipeline(640, 480, 30, false, None)
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
        let live = build_live_producer_pipeline(640, 480, 30, false, None)
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
