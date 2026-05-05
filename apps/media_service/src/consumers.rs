// Implements Rust media pipeline logic for streaming and camera processing.
// Author: Thomas Klute

//! Consumer-side pipelines that subscribe to the producer-side
//! `intervideosink` / `interaudiosink` channels.
//!
//! Each consumer is a standalone `gst::Pipeline` so a wedge in one
//! consumer cannot back-propagate to the producer or to other
//! consumers. Each gets its own bus, its own state machine, and is
//! restarted independently.
//!
//! - **Frame export**: [`build_frame_export_consumer_pipeline`]
//! - **Streaming**: [`build_streaming_consumer_pipeline`]
//! - **Recording video + audio**:
//!   [`build_recording_video_consumer_pipeline`] and
//!   [`build_recording_audio_consumer_pipeline`]
//! - **AI / Hailo**: [`build_ai_consumer_pipeline`]

use std::str::FromStr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;
use tracing::info;

use crate::overlay::OverlayState;
use crate::pipeline::ResolvedModel;
use crate::producer::{AUDIO_CHANNEL, VIDEO_CHANNEL};

/// Element name of the appsink that the existing `frame_export.rs`
/// callback locates by name. Kept stable across the migration so the
/// callback module needs no changes.
pub const FRAME_EXPORT_APPSINK_NAME: &str = "frame_export_sink";

/// Build the **frame_export** consumer pipeline.
///
/// Topology:
/// ```text
/// intervideosrc(channel="aicam-main", do-timestamp=false,
///               timeout=0, format=time)
///   → queue(leaky=downstream, max-size-buffers=2)
///   → appsink(name="frame_export_sink",
///             max-buffers=2, drop=true, sync=false)
/// ```
///
/// The appsink callback (`frame_export::setup_frame_export`) is
/// unchanged - it locates the appsink by the
/// [`FRAME_EXPORT_APPSINK_NAME`] string. Caller is expected to invoke
/// `setup_frame_export(&pipeline, ...)` with the returned pipeline
/// before transitioning it to `Playing`.
///
/// The `intervideosrc` is configured for `is-live=true` so its
/// state-change semantics match a live source: it does not block
/// preroll waiting for a buffer the producer has not yet pushed.
/// `do-timestamp=false` preserves the producer's PTS so downstream
/// timing matches the camera clock rather than the consumer's wall
/// clock.
///
/// The leaky downstream queue absorbs jitter when the appsink
/// callback (which writes to tmpfs and publishes a ZMQ message) falls
/// behind. Upstream - the `intervideosink` on the producer side - is
/// itself a sink and does not back-pressure the camera.
pub fn build_frame_export_consumer_pipeline() -> anyhow::Result<gst::Pipeline> {
    gst::init()?;

    let pipeline = gst::Pipeline::builder()
        .name("frame_export_consumer_pipeline")
        .build();

    let intervideosrc = gst::ElementFactory::make("intervideosrc")
        .name("frame_export_intervideosrc")
        .build()?;
    intervideosrc.set_property("channel", VIDEO_CHANNEL);
    // do-timestamp=false preserves the producer's PTS so downstream
    // timing matches the camera clock rather than the consumer's wall
    // clock. (This is also the factory default - set explicitly for
    // documentation.)
    intervideosrc.set_property("do-timestamp", false);
    // intervideosrc is a live base source - no `is-live` property is
    // exposed by the element. Default `timeout` is 1 second; we keep
    // it so the consumer emits black frames after a 1 s producer gap.

    let queue = gst::ElementFactory::make("queue")
        .name("frame_export_queue")
        .build()?;
    queue.set_property("max-size-buffers", 2u32);
    queue.set_property("max-size-bytes", 0u32);
    queue.set_property("max-size-time", 0u64);
    queue.set_property_from_str("leaky", "downstream");

    let appsink = gst_app::AppSink::builder()
        .name(FRAME_EXPORT_APPSINK_NAME)
        .max_buffers(2)
        .drop(true)
        .sync(false)
        .build();

    pipeline.add_many([&intervideosrc, &queue, appsink.upcast_ref::<gst::Element>()])?;
    gst::Element::link_many([&intervideosrc, &queue, appsink.upcast_ref::<gst::Element>()])?;

    info!(
        channel = VIDEO_CHANNEL,
        appsink = FRAME_EXPORT_APPSINK_NAME,
        "frame_export consumer pipeline built"
    );
    Ok(pipeline)
}

// ---------------------------------------------------------------------------
// Streaming consumer
// ---------------------------------------------------------------------------

/// Bundle of handles the caller needs after constructing a streaming
/// consumer pipeline. The pipeline itself is the lifecycle handle
/// (`set_state`, `bus`); the buffer counter is read by the
/// grace-period flow check to detect "valve open, no buffers" silent
/// failures.
pub struct StreamingConsumer {
    pub pipeline: gst::Pipeline,
    /// Increments on every buffer leaving `stream_flvmux.src` - the
    /// last common element before the sink branch diverges
    /// (rtmpsink in production, fakesink in benchmark builds). The
    /// `spawn_streaming_flow_check` task in `main.rs` reads this to
    /// decide whether the session actually carried any payload.
    pub buffer_count: Arc<AtomicU64>,
}

/// Build the **streaming** consumer pipeline.
///
/// The pipeline is built fresh on every `/streaming/start`. On
/// `/streaming/stop` the caller transitions the pipeline to `Null` and
/// drops it. A fresh pipeline per session means a fresh `rtmpsink`
/// instance per session - closes the documented cycle-N
/// `rtmpsink` bug (`docs/gstreamer/pipeline-overview.md` § *Known
/// limitation*).
///
/// Topology:
/// ```text
/// intervideosrc(channel="aicam-main", do-timestamp=false)
///   → queue(stream_queue, leaky=downstream, 2 s)
///   → videoscale → videorate(skip-to-first=true)
///   → capsfilter(1280×720 @ 15/1)
///   → videoconvert(BGRx) → cairooverlay → videoconvert(I420)
///   → x264enc(zerolatency, ultrafast, threads=1, bitrate, key-int-max)
///   → h264parse(config-interval=-1)
///   → capsfilter(stream-format=avc, alignment=au)
///                                                         ┐
///                                                         ├→ flvmux(streamable=true) → stream_sink
///                                                         │
/// (when has_audio:)                                       │
/// interaudiosrc(channel="aicam-audio-main")               │
///   → queue(audio_stream_queue, leaky=downstream, 2 s)    │
///   → voaacenc / avenc_aac → aacparse ──────────────────┘
///
/// (when not has_audio:)
/// audiotestsrc(silence) → capsfilter(S16LE 48k 2ch) → audio_stream_queue → ...
/// ```
///
/// `stream_sink` is `rtmpsink` (or `rtmp2sink` fallback) in default
/// builds, swapped for `fakesink` under the `streaming_benchmark`
/// Cargo feature. See [`make_stream_sink`].
///
/// The audio side is always built - RTMP receivers (YouTube, mediamtx)
/// silently drop sessions that carry no audio FLV tags. When
/// `has_audio` is `false` the chain feeds from an internal silent
/// `audiotestsrc` so the FLV always carries audio tags.
pub fn build_streaming_consumer_pipeline(
    rtmp_url: &str,
    stream_bitrate_kbps: u32,
    fps: u32,
    has_audio: bool,
    overlay_state: OverlayState,
) -> anyhow::Result<StreamingConsumer> {
    gst::init()?;

    let pipeline = gst::Pipeline::builder()
        .name("streaming_consumer_pipeline")
        .build();

    // --- video chain ---
    let intervideosrc = gst::ElementFactory::make("intervideosrc")
        .name("stream_intervideosrc")
        .build()?;
    intervideosrc.set_property("channel", VIDEO_CHANNEL);
    // do-timestamp=true: re-stamp at the consumer pipeline's clock.
    // The producer pipeline runs on its own base time, so its PTS values
    // are not meaningful in the streaming consumer's clock. RTMP / YouTube
    // require monotonically increasing PTS relative to the stream's start,
    // so we drop the producer PTS and re-timestamp here. interaudiosrc is
    // configured the same way below - both run on the streaming consumer
    // pipeline clock, so A/V stays in sync.
    intervideosrc.set_property("do-timestamp", true);

    let stream_queue = gst::ElementFactory::make("queue")
        .name("stream_queue")
        .build()?;
    stream_queue.set_property_from_str("leaky", "downstream");
    stream_queue.set_property("max-size-time", 2_000_000_000u64);
    stream_queue.set_property("max-size-buffers", 0u32);
    stream_queue.set_property("max-size-bytes", 0u32);

    // Downscale to 1280×720 @ 15 fps BEFORE cairooverlay + x264enc.
    // The Pi 5 can't sustain a second 1080p30 software encoder while
    // the rest of the pipeline (camera, AI, recording) is also active.
    // 720p / 15 fps drops per-second encoder work by ~4.5× compared to
    // 1080p / 30 (resolution 2.25× × fps 2×). 720p / 15 is a
    // reasonable YouTube/Twitch streaming default for an
    // event-broadcast scoreboard view; picture quality on the platform
    // side is unchanged for the typical viewer.
    let stream_videoscale = gst::ElementFactory::make("videoscale")
        .name("stream_videoscale")
        .build()?;
    let stream_videorate = gst::ElementFactory::make("videorate")
        .name("stream_videorate")
        .build()?;
    // `skip-to-first=true` - don't emit any output buffer before the
    // first real input arrives. With the default `false`, starting
    // the streaming pipeline mid-service makes videorate pad its
    // output schedule from the parent clock with duplicates of stale
    // buffers, which freezes the streamed frame for the whole session.
    stream_videorate.set_property("skip-to-first", true);
    let stream_downscale_caps = gst::Caps::builder("video/x-raw")
        .field("width", 1280i32)
        .field("height", 720i32)
        .field("framerate", gst::Fraction::new(15, 1))
        .build();
    let stream_downscale_capsfilter = gst::ElementFactory::make("capsfilter")
        .name("stream_downscale_caps")
        .property("caps", &stream_downscale_caps)
        .build()?;

    // cairooverlay needs video/x-raw,format=BGRx - wrap in videoconverts.
    let stream_pre_convert = gst::ElementFactory::make("videoconvert")
        .name("stream_pre_convert")
        .build()?;
    let stream_cairooverlay = gst::ElementFactory::make("cairooverlay")
        .name("stream_cairooverlay")
        .build()?;
    let stream_post_convert = gst::ElementFactory::make("videoconvert")
        .name("stream_post_convert")
        .build()?;

    let stream_encoder = try_create_element("x264enc", "stream_encoder")
        .inspect(|enc| {
            enc.set_property_from_str("speed-preset", "ultrafast");
            enc.set_property("threads", 1u32);
            enc.set_property("bitrate", stream_bitrate_kbps);
            enc.set_property("key-int-max", fps);
            enc.set_property_from_str("tune", "zerolatency");
            info!(
                bitrate_kbps = stream_bitrate_kbps,
                "streaming consumer: video encoder = x264enc"
            );
        })
        .or_else(|_| {
            try_create_element("openh264enc", "stream_encoder").inspect(|enc| {
                // openh264enc takes bitrate in bps, not kbps.
                enc.set_property("bitrate", stream_bitrate_kbps * 1000);
                info!("streaming consumer: video encoder = openh264enc (fallback)");
            })
        })?;

    let stream_h264parse = gst::ElementFactory::make("h264parse")
        .name("stream_h264parse")
        .build()?;
    // config-interval=-1 inserts SPS/PPS with every IDR - required by
    // RTMP receivers (mediamtx) that expect AVCDecoderConfigurationRecord.
    stream_h264parse.set_property("config-interval", -1i32);

    // Force AVC stream-format so flvmux sees AVCC, not Annex-B.
    let stream_avc_caps = gst::Caps::builder("video/x-h264")
        .field("stream-format", "avc")
        .field("alignment", "au")
        .build();
    let stream_avc_capsfilter = gst::ElementFactory::make("capsfilter")
        .name("stream_avc_capsfilter")
        .property("caps", &stream_avc_caps)
        .build()?;

    let stream_flvmux = gst::ElementFactory::make("flvmux")
        .name("stream_flvmux")
        .build()?;
    stream_flvmux.set_property("streamable", true);
    // Latency window so flvmux gathers buffers from both sink pads
    // before emitting tags. Empirically prevents the "unexpected video
    // packet" rejection on cycle 1 of a fresh service start.
    stream_flvmux.set_property("latency", 1_000_000_000u64);

    let stream_sink = make_stream_sink(rtmp_url)?;

    pipeline.add_many([
        &intervideosrc,
        &stream_queue,
        &stream_videoscale,
        &stream_videorate,
        &stream_downscale_capsfilter,
        &stream_pre_convert,
        &stream_cairooverlay,
        &stream_post_convert,
        &stream_encoder,
        &stream_h264parse,
        &stream_avc_capsfilter,
        &stream_flvmux,
        &stream_sink,
    ])?;

    gst::Element::link_many([
        &intervideosrc,
        &stream_queue,
        &stream_videoscale,
        &stream_videorate,
        &stream_downscale_capsfilter,
        &stream_pre_convert,
        &stream_cairooverlay,
        &stream_post_convert,
        &stream_encoder,
        &stream_h264parse,
        &stream_avc_capsfilter,
    ])?;

    let flvmux_video_pad = stream_flvmux
        .request_pad_simple("video")
        .ok_or_else(|| anyhow::anyhow!("streaming consumer: failed to get flvmux video pad"))?;
    let avc_src = stream_avc_capsfilter.static_pad("src").unwrap();
    avc_src.link(&flvmux_video_pad)?;

    let flvmux_src = stream_flvmux
        .static_pad("src")
        .ok_or_else(|| anyhow::anyhow!("flvmux has no src pad"))?;
    flvmux_src.link(
        &stream_sink
            .static_pad("sink")
            .ok_or_else(|| anyhow::anyhow!("stream_sink has no sink pad"))?,
    )?;

    // --- audio chain ---
    // YouTube and most RTMP ingests require an audio track in the FLV
    // stream - without it the receiver silently drops the connection.
    // Always produce an audio track:
    //   has_audio=true  → audio comes from the producer-side
    //                     interaudiosink(channel="aicam-audio-main")
    //                     via interaudiosrc here
    //   has_audio=false → an internal audiotestsrc(wave=silence) keeps
    //                     flvmux happy when no microphone is present
    let audio_stream_queue = gst::ElementFactory::make("queue")
        .name("audio_stream_queue")
        .build()?;
    audio_stream_queue.set_property_from_str("leaky", "downstream");
    audio_stream_queue.set_property("max-size-time", 2_000_000_000u64);
    audio_stream_queue.set_property("max-size-buffers", 0u32);
    audio_stream_queue.set_property("max-size-bytes", 0u32);

    let audio_stream_encoder = try_create_element("voaacenc", "audio_stream_encoder")
        .or_else(|_| try_create_element("avenc_aac", "audio_stream_encoder"))?;

    let audio_stream_aacparse = gst::ElementFactory::make("aacparse")
        .name("audio_stream_aacparse")
        .build()?;

    pipeline.add_many([
        &audio_stream_queue,
        &audio_stream_encoder,
        &audio_stream_aacparse,
    ])?;

    if has_audio {
        let interaudiosrc = gst::ElementFactory::make("interaudiosrc")
            .name("stream_interaudiosrc")
            .build()?;
        interaudiosrc.set_property("channel", AUDIO_CHANNEL);
        // do-timestamp=true: re-stamp at the streaming pipeline's clock,
        // matching intervideosrc above. Keeps A/V in sync on the consumer
        // side and gives flvmux/rtmpsink monotonically increasing PTS
        // starting near zero - what RTMP receivers expect.
        interaudiosrc.set_property("do-timestamp", true);
        pipeline.add(&interaudiosrc)?;
        gst::Element::link_many([
            &interaudiosrc,
            &audio_stream_queue,
            &audio_stream_encoder,
            &audio_stream_aacparse,
        ])?;
        info!(
            channel = AUDIO_CHANNEL,
            "streaming consumer: audio chain via interaudiosrc → voaacenc/avenc_aac"
        );
    } else {
        let silent_src = gst::ElementFactory::make("audiotestsrc")
            .name("audio_silence_src")
            .build()?;
        silent_src.set_property_from_str("wave", "silence");
        silent_src.set_property("is-live", true);

        let silent_caps = gst::Caps::builder("audio/x-raw")
            .field("format", "S16LE")
            .field("rate", 48000i32)
            .field("channels", 2i32)
            .field("layout", "interleaved")
            .build();
        let silent_capsfilter = gst::ElementFactory::make("capsfilter")
            .name("audio_silence_capsfilter")
            .property("caps", &silent_caps)
            .build()?;

        pipeline.add_many([&silent_src, &silent_capsfilter])?;
        gst::Element::link_many([
            &silent_src,
            &silent_capsfilter,
            &audio_stream_queue,
            &audio_stream_encoder,
            &audio_stream_aacparse,
        ])?;
        info!("streaming consumer: audio chain = audiotestsrc(silence) → voaacenc/avenc_aac");
    }

    let flvmux_audio_pad = stream_flvmux
        .request_pad_simple("audio")
        .ok_or_else(|| anyhow::anyhow!("streaming consumer: failed to get flvmux audio pad"))?;
    let aacparse_src = audio_stream_aacparse.static_pad("src").unwrap();
    aacparse_src.link(&flvmux_audio_pad)?;

    // --- cairooverlay draw signal ---
    stream_cairooverlay.connect("draw", false, move |args| {
        let cr: &cairo::Context = args[1].get().unwrap();
        let element: gst::Element = args[0].get().unwrap();
        let (width, height) = get_overlay_dimensions(&element);
        if let Ok(data) = overlay_state.read() {
            crate::overlay::draw_overlay(cr, width, height, &data);
        }
        None
    });
    info!("streaming consumer: cairooverlay draw signal connected");

    // --- buffer-flow probe on stream_h264parse.src ---
    // Counts ENCODED VIDEO buffers only - the rate that the UI exposes as
    // "streaming fps". Probing flvmux.src instead would count audio AAC
    // packets (~47/s at 48 kHz / 1024-sample frames) plus video plus
    // script tags, which gave a misleading ~62-67 buffers/s readout
    // even though the actual encoded video is 15 fps. h264parse.src is
    // the last point on the video-only chain before flvmux merges the
    // audio sink pad in.
    let buffer_count = Arc::new(AtomicU64::new(0));
    {
        let counter = buffer_count.clone();
        let h264parse_src = stream_h264parse
            .static_pad("src")
            .expect("stream_h264parse must have a src pad");
        h264parse_src.add_probe(gst::PadProbeType::BUFFER, move |_pad, _info| {
            counter.fetch_add(1, Ordering::Relaxed);
            gst::PadProbeReturn::Ok
        });
    }

    info!(
        rtmp_url,
        bitrate_kbps = stream_bitrate_kbps,
        has_audio,
        "streaming consumer pipeline built"
    );
    Ok(StreamingConsumer {
        pipeline,
        buffer_count,
    })
}

/// Build the streaming sink element.
///
/// In the default release build returns `rtmpsink` (librtmp), falling
/// back to `rtmp2sink` from `gst-plugins-bad` if librtmp is missing.
/// The preference is opposite to what the element ranks suggest because
/// `rtmp2sink` fails against mediamtx 1.x on live A+V with
/// "received type 3 chunk without previous chunk"; reproduced outside
/// `media_service` by `experiments/gst-launch-scratchpad/av_live_to_mediamtx.sh`.
///
/// With the `streaming_benchmark` Cargo feature enabled the sink is
/// `fakesink` so the smoke harness runs without an RTMP receiver.
fn make_stream_sink(rtmp_url: &str) -> anyhow::Result<gst::Element> {
    #[cfg(feature = "streaming_benchmark")]
    {
        let s = gst::ElementFactory::make("fakesink")
            .name("stream_sink")
            .build()?;
        s.set_property("sync", false);
        s.set_property("async", false);
        info!("streaming consumer: benchmark build - stream_sink = fakesink");
        let _ = rtmp_url;
        Ok(s)
    }
    #[cfg(not(feature = "streaming_benchmark"))]
    {
        match gst::ElementFactory::make("rtmpsink")
            .name("stream_sink")
            .property("location", rtmp_url)
            .build()
        {
            Ok(s) => {
                s.set_property("sync", false);
                s.set_property("async", false);
                info!(
                    rtmp_url,
                    "streaming consumer: stream_sink = rtmpsink (librtmp)"
                );
                Ok(s)
            }
            Err(_) => {
                let s = gst::ElementFactory::make("rtmp2sink")
                    .name("stream_sink")
                    .property("location", rtmp_url)
                    .build()?;
                s.set_property("sync", false);
                s.set_property("async", false);
                info!(
                    rtmp_url,
                    "streaming consumer: stream_sink = rtmp2sink (rtmpsink unavailable)"
                );
                Ok(s)
            }
        }
    }
}

/// Extract video width/height from a cairooverlay element's negotiated sink caps.
fn get_overlay_dimensions(element: &gst::Element) -> (f64, f64) {
    if let Some(pad) = element.static_pad("sink") {
        if let Some(caps) = pad.current_caps() {
            if let Some(s) = caps.structure(0) {
                let w = s.get::<i32>("width").unwrap_or(960) as f64;
                let h = s.get::<i32>("height").unwrap_or(540) as f64;
                return (w, h);
            }
        }
    }
    (960.0, 540.0)
}

fn try_create_element(factory_name: &str, element_name: &str) -> anyhow::Result<gst::Element> {
    gst::ElementFactory::make(factory_name)
        .name(element_name)
        .build()
        .map_err(|e| anyhow::anyhow!("Failed to create element '{}': {}", factory_name, e))
}

// ---------------------------------------------------------------------------
// Recording consumers
// ---------------------------------------------------------------------------

/// Bundle of element handles needed to drive the **video** recording
/// consumer pipeline through `start_recording` / `stop_recording`.
///
/// The pipeline is built once at service start, brought to PLAYING,
/// and left running for the lifetime of the service. The
/// `valve` gates the chain (drop=true at idle); `start_recording`
/// cycles the downstream elements through NULL → PLAYING with the
/// real `location` set on `filesink`, then opens the valve.
/// `stop_recording` closes the valve, sends EOS through `queue`,
/// waits for EOS at `filesink`, then cycles back to `/dev/null`.
pub struct RecordingVideoConsumer {
    pub pipeline: gst::Pipeline,
    pub valve: gst::Element,
    pub queue: gst::Element,
    pub videoconvert: gst::Element,
    pub encoder: gst::Element,
    pub filesink: gst::Element,
    /// Buffer counter on `encoder.sink` - reset by `start_recording`,
    /// consumed by both `RecordingStats` (file frame count) and the
    /// grace-period flow check (silent-failure detection).
    pub frame_count: Arc<AtomicU64>,
    /// Buffer counter on `valve.src` - diagnostic, used to
    /// localise a missing-frames bug between valve, queue, encoder.
    pub valve_count: Arc<AtomicU64>,
    /// Buffer counter on `queue.src`.
    pub queue_src_count: Arc<AtomicU64>,
    /// PTS log: every (frame_index, pts_ns) pair from `encoder.sink`.
    /// Used to write the per-recording PTS CSV.
    pub pts_log: Arc<Mutex<Vec<(u64, u64)>>>,
}

/// Bundle of element handles for the **audio** recording consumer
/// pipeline. Same lifecycle pattern as
/// [`RecordingVideoConsumer`].
pub struct RecordingAudioConsumer {
    pub pipeline: gst::Pipeline,
    pub valve: gst::Element,
    pub queue: gst::Element,
    pub encoder: gst::Element,
    pub filesink: gst::Element,
}

/// Build the **video** recording consumer pipeline.
///
/// Topology:
/// ```text
/// intervideosrc(channel="aicam-main", do-timestamp=false)
///   → valve(rec_valve, drop=true)
///   → queue(rec_queue, max-size-buffers=60, leaky=downstream)
///   → videoconvert(rec_videoconvert)
///   → x264enc(rec_encoder, ultrafast, zerolatency, threads=0,
///             bitrate=stream_bitrate_kbps, key-int-max=fps)
///   → filesink(rec_filesink, location=/dev/null, async=false, sync=false)
/// ```
///
/// Element names are kept identical to the legacy tee branch
/// (`rec_valve`, `rec_queue`, `rec_videoconvert`, `rec_encoder`,
/// `rec_filesink`) so any element-by-name lookups in `pipeline.rs`,
/// the bus watch's recording error classifier, and the smoke
/// harness keep working.
pub fn build_recording_video_consumer_pipeline(
    fps: u32,
    bitrate_kbps: u32,
) -> anyhow::Result<RecordingVideoConsumer> {
    gst::init()?;

    let pipeline = gst::Pipeline::builder()
        .name("recording_video_consumer_pipeline")
        .build();

    let intervideosrc = gst::ElementFactory::make("intervideosrc")
        .name("rec_intervideosrc")
        .build()?;
    intervideosrc.set_property("channel", VIDEO_CHANNEL);
    intervideosrc.set_property("do-timestamp", false);

    let valve = gst::ElementFactory::make("valve")
        .name("rec_valve")
        .build()?;
    valve.set_property("drop", true);

    let queue = gst::ElementFactory::make("queue")
        .name("rec_queue")
        .build()?;
    // Two seconds of slack at 30 fps. Capacity is buffer-bounded
    // because raw 1080p NV12 ≈ 3 MB/frame breaks the default 10 MB
    // byte limit at ~3 buffers - encoder hiccups would back-pressure
    // the inter-pipeline channel. With max-size-bytes=0 +
    // leaky=downstream the queue drops oldest raw video on overrun
    // rather than back-pressuring upstream.
    queue.set_property("max-size-buffers", 60u32);
    queue.set_property("max-size-bytes", 0u32);
    queue.set_property("max-size-time", 0u64);
    queue.set_property_from_str("leaky", "downstream");

    let videoconvert = gst::ElementFactory::make("videoconvert")
        .name("rec_videoconvert")
        .build()?;

    let encoder = try_create_element("x264enc", "rec_encoder").map_err(|_| {
        anyhow::anyhow!(
            "x264enc element not available - install gstreamer1.0-plugins-ugly on the target. \
             openh264enc is intentionally not used as a fallback for recording (subtle \
             quality / parser-compat issues with mp4 mux)."
        )
    })?;
    encoder.set_property_from_str("speed-preset", "ultrafast");
    encoder.set_property_from_str("tune", "zerolatency");
    // threads=0 lets x264 pick worker count; matches the legacy tee
    // branch.
    encoder.set_property("threads", 0u32);
    encoder.set_property("bitrate", bitrate_kbps);
    encoder.set_property("key-int-max", fps);

    let filesink = gst::ElementFactory::make("filesink")
        .name("rec_filesink")
        .build()?;
    filesink.set_property("location", "/dev/null");
    // async=false: don't wait for preroll. The chain sits behind a
    // closed valve at idle so no buffers ever arrive - without this
    // the pipeline cannot transition PAUSED → PLAYING and the
    // service cannot start.
    filesink.set_property("async", false);
    filesink.set_property("sync", false);

    pipeline.add_many([
        &intervideosrc,
        &valve,
        &queue,
        &videoconvert,
        &encoder,
        &filesink,
    ])?;
    gst::Element::link_many([
        &intervideosrc,
        &valve,
        &queue,
        &videoconvert,
        &encoder,
        &filesink,
    ])?;

    // Probes (frame_count on encoder.sink, valve_count, queue_src_count, pts_log).
    let frame_count = Arc::new(AtomicU64::new(0));
    let pts_log: Arc<Mutex<Vec<(u64, u64)>>> = Arc::new(Mutex::new(Vec::new()));
    {
        let fc = frame_count.clone();
        let pl = pts_log.clone();
        let enc_sink_pad = encoder
            .static_pad("sink")
            .ok_or_else(|| anyhow::anyhow!("rec_encoder has no sink pad"))?;
        enc_sink_pad.add_probe(gst::PadProbeType::BUFFER, move |_pad, info| {
            if let Some(gst::PadProbeData::Buffer(ref buf)) = info.data {
                let idx = fc.fetch_add(1, Ordering::Relaxed);
                let pts_ns = buf.pts().map(|t| t.nseconds()).unwrap_or(0);
                if let Ok(mut log) = pl.lock() {
                    log.push((idx, pts_ns));
                }
            }
            gst::PadProbeReturn::Ok
        });
    }

    let valve_count = Arc::new(AtomicU64::new(0));
    let queue_src_count = Arc::new(AtomicU64::new(0));
    {
        let c = valve_count.clone();
        let pad = valve
            .static_pad("src")
            .ok_or_else(|| anyhow::anyhow!("rec_valve has no src pad"))?;
        pad.add_probe(gst::PadProbeType::BUFFER, move |_pad, _info| {
            c.fetch_add(1, Ordering::Relaxed);
            gst::PadProbeReturn::Ok
        });
    }
    {
        let c = queue_src_count.clone();
        let pad = queue
            .static_pad("src")
            .ok_or_else(|| anyhow::anyhow!("rec_queue has no src pad"))?;
        pad.add_probe(gst::PadProbeType::BUFFER, move |_pad, _info| {
            c.fetch_add(1, Ordering::Relaxed);
            gst::PadProbeReturn::Ok
        });
    }

    info!(
        channel = VIDEO_CHANNEL,
        bitrate_kbps, "recording video consumer pipeline built (valve closed, filesink=/dev/null)"
    );
    Ok(RecordingVideoConsumer {
        pipeline,
        valve,
        queue,
        videoconvert,
        encoder,
        filesink,
        frame_count,
        valve_count,
        queue_src_count,
        pts_log,
    })
}

/// Build the **audio** recording consumer pipeline.
///
/// Topology:
/// ```text
/// interaudiosrc(channel="aicam-audio-main", do-timestamp=false)
///   → valve(audio_rec_valve, drop=true)
///   → queue(audio_rec_queue)
///   → flacenc(audio_rec_encoder)
///   → filesink(audio_rec_filesink, location=/dev/null, async=false, sync=false)
/// ```
///
/// Element names are kept identical to the legacy audio recording
/// branch on the `audio_tee` so the bus error classifier and any
/// observability hooks (smoke harness, journalctl filters) keep
/// working.
pub fn build_recording_audio_consumer_pipeline() -> anyhow::Result<RecordingAudioConsumer> {
    gst::init()?;

    let pipeline = gst::Pipeline::builder()
        .name("recording_audio_consumer_pipeline")
        .build();

    let interaudiosrc = gst::ElementFactory::make("interaudiosrc")
        .name("rec_interaudiosrc")
        .build()?;
    interaudiosrc.set_property("channel", AUDIO_CHANNEL);
    interaudiosrc.set_property("do-timestamp", false);

    let valve = gst::ElementFactory::make("valve")
        .name("audio_rec_valve")
        .build()?;
    valve.set_property("drop", true);

    let queue = gst::ElementFactory::make("queue")
        .name("audio_rec_queue")
        .build()?;

    let encoder = gst::ElementFactory::make("flacenc")
        .name("audio_rec_encoder")
        .build()?;

    let filesink = gst::ElementFactory::make("filesink")
        .name("audio_rec_filesink")
        .build()?;
    filesink.set_property("location", "/dev/null");
    filesink.set_property("async", false);
    filesink.set_property("sync", false);

    pipeline.add_many([&interaudiosrc, &valve, &queue, &encoder, &filesink])?;
    gst::Element::link_many([&interaudiosrc, &valve, &queue, &encoder, &filesink])?;

    info!(
        channel = AUDIO_CHANNEL,
        "recording audio consumer pipeline built (valve closed, filesink=/dev/null)"
    );
    Ok(RecordingAudioConsumer {
        pipeline,
        valve,
        queue,
        encoder,
        filesink,
    })
}

// ---------------------------------------------------------------------------
// AI / Hailo consumer
// ---------------------------------------------------------------------------

/// Element name of the appsink that `object_detection_preview.rs`
/// locates by name to install its callback.
pub const AI_APPSINK_NAME: &str = "ai_sink";

/// Bundle of handles returned by [`build_ai_consumer_pipeline`].
///
/// The pipeline is the lifecycle handle. The caller registers the
/// `object_detection_preview` appsink callback on it via
/// `setup_object_detection_preview(&consumer.pipeline, …)` before
/// transitioning to PLAYING.
pub struct AiConsumer {
    pub pipeline: gst::Pipeline,
}

/// Build the **AI / Hailo** consumer pipeline.
///
/// Topology (default detection-only path):
/// ```text
/// intervideosrc(channel="aicam-main", do-timestamp=false)
///   → queue(ai_queue, leaky=downstream, max-size-buffers=2)
///   → videoscale(ai_videoscale)
///   → videoconvert(ai_videoconvert, n-threads=2)
///   → capsfilter(ai_caps,  model.input_format / width / height)
///   → videorate(ai_videorate)
///   → capsfilter(ai_fps_caps, framerate = model.inference_fps)
///   → hailonet(ai_hailonet, hef-path = model.hef_path, is-active=true)
///   → hailofilter(ai_hailofilter, postprocess.so / function-name)
///   ┌──────────────────────────────────────────────────────────────┐
///   │ when model.publish_detections:                               │
///   │   → hailofilter(ai_meta_export, libmetadata_export.so)       │
///   │     publishes ObjectDetectionsMessage on ZMQ ai.object_detections │
///   └──────────────────────────────────────────────────────────────┘
///   → hailooverlay(ai_hailooverlay)
///   → videoconvert(ai_post_convert)         // hailooverlay → BGR/RGB → I420 for jpegenc
///   → jpegenc(ai_jpegenc, quality=80)
///   → appsink(ai_sink, max-buffers=1, drop=true, sync=false)
/// ```
///
/// Returns `Ok(None)` when no detector model is configured or Hailo
/// is unavailable on the host - in that case there is no AI consumer
/// at all (the producer-side `intervideosink` does not back-pressure,
/// so nothing fills up). Caller skips the lifecycle.
///
/// The AI branch's special "passthrough fakesink" terminator on the
/// legacy tee is gone - there's no reason to drain the channel just
/// to keep tee topology stable when the channel itself is on the
/// producer side.
///
/// `AiConfig` carries only `object_detection: Option<ResolvedModel>`;
/// the AI consumer is detector-only. Re-introducing classifier
/// cascades would change the topology here.
pub fn build_ai_consumer_pipeline(
    detector: &ResolvedModel,
    frame_width: u32,
    frame_height: u32,
) -> anyhow::Result<AiConsumer> {
    gst::init()?;

    // Advertise the original camera frame dimensions to the
    // metadata_export .so before the pipeline is constructed. The .so
    // reads these env vars once on its first call and scales the
    // normalised HailoBBox coordinates it walks to pixel space matching
    // ObjectDetectionsMessage.coordinate_system="image_px".
    //
    // SAFETY: std::env::set_var is unsafe in Rust 2024 because it is
    // not thread-safe, but the AI consumer is built on a single
    // thread during startup before any other code touches env vars,
    // so this is sound.
    unsafe {
        std::env::set_var("AICAM_META_EXPORT_WIDTH", frame_width.to_string());
        std::env::set_var("AICAM_META_EXPORT_HEIGHT", frame_height.to_string());
        // Cascade-classifier label maps are no longer set; clear them
        // so a stale value from a previous run doesn't leak.
        std::env::remove_var("AICAM_META_EXPORT_CLS1_LABELS");
        std::env::remove_var("AICAM_META_EXPORT_CLS2_LABELS");
        // Class ID remapping for detector output. Format: "0:human,32:ball"
        if let Some(cm) = detector.class_map.as_ref() {
            let csv: String = cm
                .iter()
                .map(|(k, v)| format!("{k}:{v}"))
                .collect::<Vec<_>>()
                .join(",");
            std::env::set_var("AICAM_META_EXPORT_DET_CLASS_MAP", csv);
        } else {
            std::env::remove_var("AICAM_META_EXPORT_DET_CLASS_MAP");
        }
        // Hand the detector's index→label map to the generic yolo26
        // postprocess .so so it can stamp real class names on each
        // HailoDetection. When the sidecar uses a named set
        // (e.g. "coco_80") label_map is None and we clear the env
        // var - the .so falls back to its built-in COCO table when
        // num_classes == 80, or synthesises "class_N" otherwise.
        if let Some(lm) = detector.label_map.as_ref() {
            std::env::set_var("AICAM_YOLO26_POST_LABELS", lm.join(","));
        } else {
            std::env::remove_var("AICAM_YOLO26_POST_LABELS");
        }
    }

    let pipeline = gst::Pipeline::builder()
        .name("ai_consumer_pipeline")
        .build();

    let intervideosrc = gst::ElementFactory::make("intervideosrc")
        .name("ai_intervideosrc")
        .build()?;
    intervideosrc.set_property("channel", VIDEO_CHANNEL);
    intervideosrc.set_property("do-timestamp", false);

    let ai_queue = gst::ElementFactory::make("queue")
        .name("ai_queue")
        .build()?;
    ai_queue.set_property("max-size-buffers", 2u32);
    ai_queue.set_property_from_str("leaky", "downstream");

    let ai_videoscale = gst::ElementFactory::make("videoscale")
        .name("ai_videoscale")
        .build()?;
    let ai_videoconvert = gst::ElementFactory::make("videoconvert")
        .name("ai_videoconvert")
        .build()?;
    ai_videoconvert.set_property("n-threads", 2u32);

    let ai_caps_el = gst::ElementFactory::make("capsfilter")
        .name("ai_caps")
        .build()?;
    let ai_caps = gst::Caps::from_str(&format!(
        "video/x-raw,format={},width={},height={}",
        detector.input_format, detector.input_width, detector.input_height
    ))?;
    ai_caps_el.set_property("caps", &ai_caps);

    // Throttle inference via videorate - drops frames to target fps.
    let inference_fps = detector.inference_fps.unwrap_or(3.0).max(0.1);
    let inference_fps_int = inference_fps.round() as i32;
    let ai_videorate = gst::ElementFactory::make("videorate")
        .name("ai_videorate")
        .build()?;
    let ai_fps_caps_el = gst::ElementFactory::make("capsfilter")
        .name("ai_fps_caps")
        .build()?;
    let ai_fps_caps = gst::Caps::from_str(&format!(
        "video/x-raw,framerate={}/1",
        inference_fps_int.max(1)
    ))?;
    ai_fps_caps_el.set_property("caps", &ai_fps_caps);

    let hailonet = try_create_element("hailonet", "ai_hailonet")?;
    hailonet.set_property_from_str("hef-path", &detector.hef_path);
    // hailonet defaults to is-active=false and only auto-activates
    // when the pipeline contains *exactly one* hailonet in a trivial
    // topology. Even though our consumer pipeline carries exactly one
    // hailonet, it sits behind videorate / capsfilter / multiple
    // queues which the auto-activation heuristic is finicky about; be
    // explicit. See `gst-inspect-1.0 hailonet`.
    hailonet.set_property("is-active", true);

    let hailofilter = try_create_element("hailofilter", "ai_hailofilter")?;
    hailofilter.set_property_from_str("so-path", &detector.postprocess_so);
    hailofilter.set_property_from_str("function-name", &detector.postprocess_fn);
    hailofilter.set_property("qos", false);

    let meta_export = if detector.publish_detections {
        let m = try_create_element("hailofilter", "ai_meta_export")?;
        m.set_property_from_str("so-path", &crate::pipeline::resolve_meta_export_so_path());
        m.set_property_from_str("function-name", "export_metadata");
        m.set_property("qos", false);
        Some(m)
    } else {
        None
    };

    let hailooverlay = try_create_element("hailooverlay", "ai_hailooverlay")?;
    hailooverlay.set_property("qos", false);
    // Defaults (line-thickness=1, font-thickness=1,
    // landmark-point-radius=3) are unreadable on a 1080p monitor and
    // basically invisible on the Pi's 7" touch screen. Bump to widths
    // that survive scaling down to a small preview tile and being
    // viewed across the room without the magnifier.
    hailooverlay.set_property("line-thickness", 2i32);
    hailooverlay.set_property("font-thickness", 2i32);
    hailooverlay.set_property("landmark-point-radius", 6.0f32);

    // Convert back for JPEG encoding (hailooverlay outputs RGB)
    let ai_post_convert = gst::ElementFactory::make("videoconvert")
        .name("ai_post_convert")
        .build()?;

    let ai_jpegenc = gst::ElementFactory::make("jpegenc")
        .name("ai_jpegenc")
        .build()?;
    ai_jpegenc.set_property("quality", 80i32);

    let ai_appsink = gst::ElementFactory::make("appsink")
        .name(AI_APPSINK_NAME)
        .build()?;
    ai_appsink.set_property("max-buffers", 1u32);
    ai_appsink.set_property("drop", true);
    ai_appsink.set_property("sync", false);

    let topology = if meta_export.is_some() {
        "AI consumer: hailonet → hailofilter → hailofilter(meta_export) → hailooverlay → jpegenc → appsink"
    } else {
        "AI consumer: hailonet → hailofilter → hailooverlay → jpegenc → appsink (publish_detections=false)"
    };
    info!(
        model = %detector.display_name,
        hef_path = %detector.hef_path,
        postprocess_so = %detector.postprocess_so,
        postprocess_fn = %detector.postprocess_fn,
        output_format = %detector.output_format,
        publish_detections = detector.publish_detections,
        inference_fps = inference_fps,
        ai_input = format!("{}x{} {}", detector.input_width, detector.input_height, detector.input_format),
        "{}", topology
    );

    let mut elements: Vec<&gst::Element> = vec![
        &intervideosrc,
        &ai_queue,
        &ai_videoscale,
        &ai_videoconvert,
        &ai_caps_el,
        &ai_videorate,
        &ai_fps_caps_el,
        &hailonet,
        &hailofilter,
    ];
    if let Some(ref m) = meta_export {
        elements.push(m);
    }
    elements.push(&hailooverlay);
    elements.push(&ai_post_convert);
    elements.push(&ai_jpegenc);
    elements.push(&ai_appsink);

    pipeline.add_many(elements.to_vec())?;
    gst::Element::link_many(elements.to_vec())?;

    info!(
        channel = VIDEO_CHANNEL,
        appsink = AI_APPSINK_NAME,
        "AI consumer pipeline built"
    );
    Ok(AiConsumer { pipeline })
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

    #[test]
    fn frame_export_consumer_builds_with_expected_elements() {
        init_gst();
        let pipeline = build_frame_export_consumer_pipeline()
            .expect("frame_export consumer pipeline should build");

        let src = pipeline
            .by_name("frame_export_intervideosrc")
            .expect("intervideosrc must exist");
        assert_eq!(src.factory().unwrap().name(), "intervideosrc");
        assert_eq!(src.property::<String>("channel"), VIDEO_CHANNEL);
        assert!(!src.property::<bool>("do-timestamp"));

        let queue = pipeline
            .by_name("frame_export_queue")
            .expect("queue must exist");
        assert_eq!(queue.property::<u32>("max-size-buffers"), 2);

        let sink = pipeline
            .by_name(FRAME_EXPORT_APPSINK_NAME)
            .expect("appsink must exist");
        // AppSink::builder() bypasses the element factory, so
        // `factory()` may return None - assert the name instead.
        assert_eq!(sink.name(), FRAME_EXPORT_APPSINK_NAME);
        assert!(sink.dynamic_cast_ref::<gst_app::AppSink>().is_some());
    }

    #[test]
    fn frame_export_consumer_reaches_paused_with_no_producer() {
        // intervideosrc is is-live so PAUSED can be reached even with
        // no producer pushing on the channel - it just emits no
        // buffers until the producer connects. The pipeline must not
        // error out at preroll.
        init_gst();
        let pipeline = build_frame_export_consumer_pipeline()
            .expect("frame_export consumer pipeline should build");
        let res = pipeline.set_state(gst::State::Paused);
        assert!(
            matches!(
                res,
                Ok(gst::StateChangeSuccess::Success
                    | gst::StateChangeSuccess::Async
                    | gst::StateChangeSuccess::NoPreroll)
            ),
            "frame_export consumer must reach PAUSED, got {res:?}"
        );
        let _ = pipeline.set_state(gst::State::Null);
    }

    #[test]
    fn frame_export_consumer_appsink_name_is_stable() {
        // The frame_export.rs callback locates the appsink by this
        // string. If we rename it here without updating the callback,
        // the lookup breaks at runtime. Lock the contract.
        assert_eq!(FRAME_EXPORT_APPSINK_NAME, "frame_export_sink");
    }

    // --- streaming consumer ---

    #[test]
    fn streaming_consumer_video_only_builds() {
        init_gst();
        let overlay = crate::overlay::new_overlay_state();
        let consumer = build_streaming_consumer_pipeline(
            "rtmp://127.0.0.1:1935/test",
            500,
            30,
            false,
            overlay,
        )
        .expect("streaming consumer should build (video-only)");

        // Required structural elements present.
        for name in [
            "stream_intervideosrc",
            "stream_queue",
            "stream_videoscale",
            "stream_videorate",
            "stream_downscale_caps",
            "stream_pre_convert",
            "stream_cairooverlay",
            "stream_post_convert",
            "stream_encoder",
            "stream_h264parse",
            "stream_avc_capsfilter",
            "stream_flvmux",
            "stream_sink",
            "audio_stream_queue",
            "audio_stream_encoder",
            "audio_stream_aacparse",
            "audio_silence_src",
        ] {
            assert!(
                consumer.pipeline.by_name(name).is_some(),
                "expected element '{name}' in streaming consumer pipeline"
            );
        }

        // No interaudiosrc on the video-only path - silence comes
        // from the internal audiotestsrc.
        assert!(consumer.pipeline.by_name("stream_interaudiosrc").is_none());

        // Channel set on the video src.
        let src = consumer.pipeline.by_name("stream_intervideosrc").unwrap();
        assert_eq!(src.property::<String>("channel"), VIDEO_CHANNEL);
        // Streaming consumer must re-stamp at its own clock - RTMP/YouTube
        // require PTS that start near zero relative to the stream.
        assert!(src.property::<bool>("do-timestamp"));

        // videorate.skip-to-first must be true - freeze protection.
        let vr = consumer.pipeline.by_name("stream_videorate").unwrap();
        assert!(vr.property::<bool>("skip-to-first"));

        // h264parse.config-interval must be -1 (mediamtx interop).
        let hp = consumer.pipeline.by_name("stream_h264parse").unwrap();
        assert_eq!(hp.property::<i32>("config-interval"), -1);

        // flvmux.streamable must be true.
        let mux = consumer.pipeline.by_name("stream_flvmux").unwrap();
        assert!(mux.property::<bool>("streamable"));

        // Buffer counter starts at zero.
        assert_eq!(consumer.buffer_count.load(Ordering::Relaxed), 0);

        let _ = consumer.pipeline.set_state(gst::State::Null);
    }

    #[test]
    fn streaming_consumer_with_audio_uses_interaudiosrc() {
        init_gst();
        let overlay = crate::overlay::new_overlay_state();
        let consumer =
            build_streaming_consumer_pipeline("rtmp://127.0.0.1:1935/test", 500, 30, true, overlay)
                .expect("streaming consumer should build (with audio)");

        let src = consumer
            .pipeline
            .by_name("stream_interaudiosrc")
            .expect("interaudiosrc must exist when has_audio=true");
        assert_eq!(src.factory().unwrap().name(), "interaudiosrc");
        assert_eq!(src.property::<String>("channel"), AUDIO_CHANNEL);
        // Streaming consumer re-stamps at its own clock so RTMP/YouTube
        // see monotonically increasing PTS starting near zero.
        assert!(src.property::<bool>("do-timestamp"));

        // Internal silence source must NOT be present - the
        // interaudiosrc replaces it.
        assert!(consumer.pipeline.by_name("audio_silence_src").is_none());

        let _ = consumer.pipeline.set_state(gst::State::Null);
    }

    #[test]
    fn streaming_consumer_avc_capsfilter_forces_avc_format() {
        // mediamtx interop guard: stream_avc_capsfilter must pin
        // stream-format=avc, alignment=au into flvmux. Dropping this
        // makes mediamtx 1.x reject the session with "unexpected
        // video packet".
        init_gst();
        let overlay = crate::overlay::new_overlay_state();
        let consumer = build_streaming_consumer_pipeline(
            "rtmp://127.0.0.1:1935/test",
            500,
            30,
            false,
            overlay,
        )
        .expect("streaming consumer should build");

        let cf = consumer
            .pipeline
            .by_name("stream_avc_capsfilter")
            .expect("stream_avc_capsfilter must exist");
        let caps: gst::Caps = cf.property("caps");
        let s = caps.structure(0).expect("caps must have a structure");
        assert_eq!(s.get::<String>("stream-format").as_deref(), Ok("avc"));
        assert_eq!(s.get::<String>("alignment").as_deref(), Ok("au"));

        let _ = consumer.pipeline.set_state(gst::State::Null);
    }

    /// Verify the streaming consumer pipeline can transition to
    /// PAUSED under the `streaming_benchmark` feature where the sink
    /// is `fakesink`. The default-build path uses `rtmpsink` and
    /// would require a live RTMP receiver to preroll cleanly, which
    /// the unit-test environment doesn't have - that's covered by
    /// `make smoke` and Pi validation. End-to-end buffer flow across
    /// the inter-pipeline transport is exercised on Pi via
    /// `scripts/run_pi_smoke_tests.sh`.
    #[cfg(feature = "streaming_benchmark")]
    #[test]
    fn streaming_consumer_reaches_paused_in_fakesink_mode() {
        init_gst();
        let overlay = crate::overlay::new_overlay_state();
        let consumer = build_streaming_consumer_pipeline(
            "rtmp://127.0.0.1:1935/unused",
            500,
            30,
            false,
            overlay,
        )
        .expect("streaming consumer should build");
        let res = consumer.pipeline.set_state(gst::State::Paused);
        let _ = consumer.pipeline.set_state(gst::State::Null);
        assert!(
            matches!(
                res,
                Ok(gst::StateChangeSuccess::Success
                    | gst::StateChangeSuccess::Async
                    | gst::StateChangeSuccess::NoPreroll)
            ),
            "streaming consumer must reach PAUSED, got {res:?}"
        );
    }

    // --- recording consumers ---

    #[test]
    fn recording_video_consumer_builds_with_expected_elements() {
        init_gst();
        let consumer = match build_recording_video_consumer_pipeline(30, 8192) {
            Ok(c) => c,
            Err(e) => {
                // x264enc may be absent on a stripped CI image; that's a
                // missing-plugin error, not a logic bug. Skip in that
                // case rather than report a false failure.
                eprintln!("recording video consumer unavailable: {e}");
                return;
            }
        };

        // Element-name contracts (the bus error classifier matches
        // `rec_*` prefixes against these names).
        for name in [
            "rec_intervideosrc",
            "rec_valve",
            "rec_queue",
            "rec_videoconvert",
            "rec_encoder",
            "rec_filesink",
        ] {
            assert!(
                consumer.pipeline.by_name(name).is_some(),
                "expected element '{name}' in recording video consumer pipeline"
            );
        }

        // Channel set on the inter-pipeline source.
        assert_eq!(
            consumer
                .pipeline
                .by_name("rec_intervideosrc")
                .unwrap()
                .property::<String>("channel"),
            VIDEO_CHANNEL
        );

        // Idle invariants: valve closed, filesink at /dev/null.
        assert!(consumer.valve.property::<bool>("drop"));
        assert_eq!(
            consumer.filesink.property::<String>("location"),
            "/dev/null"
        );

        // Counters all start at zero.
        assert_eq!(consumer.frame_count.load(Ordering::Relaxed), 0);
        assert_eq!(consumer.valve_count.load(Ordering::Relaxed), 0);
        assert_eq!(consumer.queue_src_count.load(Ordering::Relaxed), 0);
        assert!(consumer.pts_log.lock().unwrap().is_empty());

        let _ = consumer.pipeline.set_state(gst::State::Null);
    }

    #[test]
    fn recording_video_consumer_reaches_paused_with_no_producer() {
        init_gst();
        let consumer = match build_recording_video_consumer_pipeline(30, 1000) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("recording video consumer unavailable: {e}");
                return;
            }
        };
        let res = consumer.pipeline.set_state(gst::State::Paused);
        let _ = consumer.pipeline.set_state(gst::State::Null);
        assert!(
            matches!(
                res,
                Ok(gst::StateChangeSuccess::Success
                    | gst::StateChangeSuccess::Async
                    | gst::StateChangeSuccess::NoPreroll)
            ),
            "recording video consumer must reach PAUSED, got {res:?}"
        );
    }

    #[test]
    fn recording_audio_consumer_builds_with_expected_elements() {
        init_gst();
        let consumer = match build_recording_audio_consumer_pipeline() {
            Ok(c) => c,
            Err(e) => {
                eprintln!("recording audio consumer unavailable: {e}");
                return;
            }
        };

        for name in [
            "rec_interaudiosrc",
            "audio_rec_valve",
            "audio_rec_queue",
            "audio_rec_encoder",
            "audio_rec_filesink",
        ] {
            assert!(
                consumer.pipeline.by_name(name).is_some(),
                "expected element '{name}' in recording audio consumer pipeline"
            );
        }

        assert_eq!(
            consumer
                .pipeline
                .by_name("rec_interaudiosrc")
                .unwrap()
                .property::<String>("channel"),
            AUDIO_CHANNEL
        );
        assert!(consumer.valve.property::<bool>("drop"));
        assert_eq!(
            consumer.filesink.property::<String>("location"),
            "/dev/null"
        );

        let _ = consumer.pipeline.set_state(gst::State::Null);
    }

    #[test]
    fn recording_audio_consumer_reaches_paused_with_no_producer() {
        init_gst();
        let consumer = match build_recording_audio_consumer_pipeline() {
            Ok(c) => c,
            Err(e) => {
                eprintln!("recording audio consumer unavailable: {e}");
                return;
            }
        };
        let res = consumer.pipeline.set_state(gst::State::Paused);
        let _ = consumer.pipeline.set_state(gst::State::Null);
        assert!(
            matches!(
                res,
                Ok(gst::StateChangeSuccess::Success
                    | gst::StateChangeSuccess::Async
                    | gst::StateChangeSuccess::NoPreroll)
            ),
            "recording audio consumer must reach PAUSED, got {res:?}"
        );
    }

    // --- AI consumer ---
    //
    // The full AI consumer pipeline can only be built on a host with
    // the Hailo plugins (`hailonet`, `hailofilter`, `hailooverlay`)
    // and a real .hef. The dev container has neither - these tests
    // therefore probe the few things that *are* deterministic
    // without Hailo: the appsink-name contract, and that the env-var
    // setup happens before the (Hailo-element-failing) build call
    // returns. End-to-end is exercised on Pi via
    // `scripts/run_pi_smoke_tests.sh dev-pi-04` and live
    // `/api/v1/object_detection_preview/frame` polling.

    #[test]
    fn ai_consumer_appsink_name_is_stable() {
        // The object_detection_preview.rs callback locates the appsink
        // by this string. If we rename it here without updating the
        // callback, the lookup breaks at runtime. Lock the contract.
        assert_eq!(AI_APPSINK_NAME, "ai_sink");
    }

    fn fake_detector_model() -> ResolvedModel {
        ResolvedModel {
            display_name: "test".to_string(),
            hef_path: "/fake/test.hef".to_string(),
            input_width: 640,
            input_height: 640,
            input_format: "RGB".to_string(),
            postprocess_so: "/fake/lib.so".to_string(),
            postprocess_fn: "fake_fn".to_string(),
            output_format: "yolov8".to_string(),
            label_map: None,
            labels_display: None,
            class_map: None,
            inference_fps: Some(3.0),
            notes: None,
            publish_detections: true,
        }
    }

    #[test]
    fn ai_consumer_sets_meta_export_env_vars() {
        init_gst();
        let model = fake_detector_model();
        // Build is allowed to fail (Hailo plugins absent) - what we
        // check is that the env vars were set before the failure.
        let _ = build_ai_consumer_pipeline(&model, 1920, 1080);
        assert_eq!(
            std::env::var("AICAM_META_EXPORT_WIDTH").ok().as_deref(),
            Some("1920")
        );
        assert_eq!(
            std::env::var("AICAM_META_EXPORT_HEIGHT").ok().as_deref(),
            Some("1080")
        );
        // Cascade vars are always cleared.
        assert!(std::env::var("AICAM_META_EXPORT_CLS1_LABELS").is_err());
        assert!(std::env::var("AICAM_META_EXPORT_CLS2_LABELS").is_err());
    }
}
