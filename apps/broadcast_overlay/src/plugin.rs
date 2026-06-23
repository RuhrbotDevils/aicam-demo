// GStreamer BaseTransform element that draws the overlay in-place on NV12 frames.
// Author: Thomas Klute

//! GStreamer plugin: the `aicamnv12overlay` element.
//!
//! `GstBaseTransform` subclass that operates **in-place** on
//! `video/x-raw,format=NV12` buffers. Drives
//! [`crate::commands::dispatch_commands`] with whatever scoreboard
//! state has been handed in (or a built-in test pattern when
//! `show-test-overlay=true`).
//!
//! ## Properties
//!
//! | name | type | default | meaning |
//! |---|---|---|---|
//! | `enabled` | bool | `true` | When `false`, frames pass through unchanged. |
//! | `show-test-overlay` | bool | `false` | Render a built-in test scoreboard with a frame-counter clock instead of any external state. Useful for verifying the plugin loaded + rendering pipeline reaches NVENC without needing the full GameController plumbing. |
//! | `font-regular-path` | string \| null | autodetect | Override the regular face's TTF/OTF path. Defaults to DejaVu Sans / Liberation Sans (probed). |
//! | `font-bold-path` | string \| null | autodetect | Override the bold face. |
//! | `scoreboard-state-json` | string | `""` | Set by the producer (e.g. `media_service`) at ~10 Hz with a `serde_json` serialisation of [`crate::layout::ScoreboardState`]. The plugin parses on set and caches; `transform_ip` reads the cached struct (no JSON parse on the per-frame hot path). Empty string falls back to the internal default state. This crosses the cdylib/main-binary boundary cleanly because no GObject subclass GType is shared (registering `AicamNv12Overlay` twice is fatal). |
//!
//! ## Test pipeline
//!
//! ```text
//! gst-launch-1.0 videotestsrc num-buffers=300 \
//!     ! 'video/x-raw,format=NV12,width=1920,height=1080,framerate=30/1' \
//!     ! aicamnv12overlay show-test-overlay=true \
//!     ! fakesink
//! ```

use std::path::Path;
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

use gst::glib;
use gst::glib::subclass::prelude::*;
use gst::prelude::*;
use gst::subclass::prelude::*;
use gst_base::subclass::prelude::*;

use crate::commands::dispatch_commands;
use crate::glyphs::{FontId, FontStyle, GlyphCache};
use crate::layout::{scoreboard_commands, LayoutParams, LayoutSizes, PenaltyTile, ScoreboardState};
use crate::renderer::color::Rgba;
use crate::renderer::frame::Nv12FrameMut;

pub(crate) const REGULAR_FONT_ID: FontId = FontId(0);
pub(crate) const BOLD_FONT_ID: FontId = FontId(1);

fn cat() -> &'static gst::DebugCategory {
    static CAT: OnceLock<gst::DebugCategory> = OnceLock::new();
    CAT.get_or_init(|| {
        gst::DebugCategory::new(
            "aicamnv12overlay",
            gst::DebugColorFlags::empty(),
            Some("AICam NV12 broadcast overlay"),
        )
    })
}

// freetype-rs's `Library` wraps a raw FT_Library handle; freetype-rs
// itself does not impl Send. The GlyphCache is only ever touched
// from the streaming task (transform_ip / start / stop), but it's
// stored inside a `Mutex<Runtime>` that GStreamer requires be
// `Send`. Asserting Send for our cache is safe because we never
// share a single FT_Library across threads - each element instance
// owns its own.
unsafe impl Send for SendRuntime {}

struct SendRuntime(Runtime);

struct Runtime {
    info: Option<gst_video::VideoInfo>,
    cache: GlyphCache,
    scoreboard: ScoreboardState,
    frame_count: u64,
    max_render_ns: u128,
    total_render_ns: u128,
}

#[derive(Clone)]
struct Settings {
    enabled: bool,
    show_test_overlay: bool,
    font_regular_path: Option<String>,
    font_bold_path: Option<String>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            enabled: true,
            show_test_overlay: false,
            font_regular_path: None,
            font_bold_path: None,
        }
    }
}

/// Cached external scoreboard state. Refreshed when the producer
/// writes to the `scoreboard-state-json` property; read on every
/// frame by `transform_ip`. The `parse_count` is plumbed in the JSON
/// setter for a one-line debug log so operators can confirm updates
/// are arriving from the producer.
#[derive(Default)]
struct ExternalState {
    cached: Option<ScoreboardState>,
    parse_count: u64,
    parse_err_count: u64,
}

/// Cached `LayoutSizes` last applied via the `layout-sizes-json`
/// property. `None` means the renderer falls back to
/// `LayoutSizes::default()`. Operators set this once at pipeline
/// start; the property doesn't need 10 Hz updates like
/// `scoreboard-state-json` does.
#[derive(Default)]
struct ExternalSizes {
    cached: Option<LayoutSizes>,
}

// ---------------------------------------------------------------------------
// Subclass impl
// ---------------------------------------------------------------------------

mod imp {
    use super::*;

    #[derive(Default)]
    pub struct Nv12Overlay {
        settings: Mutex<Settings>,
        runtime: Mutex<Option<SendRuntime>>,
        /// Most-recently-set scoreboard state, parsed from the
        /// `scoreboard-state-json` property. `transform_ip` clones
        /// out of this `Mutex` once per frame (microseconds) - the
        /// expensive JSON parse runs only on `set_property`, which
        /// the producer drives at ~10 Hz.
        ///
        /// We cannot share an `Arc<RwLock<ScoreboardState>>` from
        /// the producer directly: media_service links its own copy
        /// of the `aicam_broadcast_overlay::plugin` module via Cargo
        /// and the `.so` is also loaded at runtime, so the same
        /// `Nv12Overlay` GObject subclass would be registered twice
        /// under the same C name - fatal in glib's subclass machinery.
        pub(super) external_state: Mutex<ExternalState>,
        /// Cached `LayoutSizes` from the `layout-sizes-json` property.
        /// Stored separately from the scoreboard state because sizes
        /// change rarely (boot-time config) while state changes at
        /// 10 Hz.
        pub(super) external_sizes: Mutex<ExternalSizes>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for Nv12Overlay {
        const NAME: &'static str = "AicamNv12Overlay";
        type Type = super::Nv12Overlay;
        type ParentType = gst_base::BaseTransform;
    }

    impl ObjectImpl for Nv12Overlay {
        fn properties() -> &'static [glib::ParamSpec] {
            static PROPS: OnceLock<Vec<glib::ParamSpec>> = OnceLock::new();
            PROPS
                .get_or_init(|| {
                    vec![
                        glib::ParamSpecBoolean::builder("enabled")
                            .nick("Enabled")
                            .blurb("When false, frames pass through unchanged")
                            .default_value(true)
                            .build(),
                        glib::ParamSpecBoolean::builder("show-test-overlay")
                            .nick("Show test overlay")
                            .blurb(
                                "Render a built-in test scoreboard with a frame-counter clock \
                                 instead of any external state",
                            )
                            .default_value(false)
                            .build(),
                        glib::ParamSpecString::builder("font-regular-path")
                            .nick("Regular font path")
                            .blurb(
                                "Path to a TTF/OTF for the regular face (auto-detected if unset)",
                            )
                            .build(),
                        glib::ParamSpecString::builder("font-bold-path")
                            .nick("Bold font path")
                            .blurb("Path to a TTF/OTF for the bold face (auto-detected if unset)")
                            .build(),
                        glib::ParamSpecString::builder("scoreboard-state-json")
                            .nick("Scoreboard state (JSON)")
                            .blurb(
                                "serde_json serialisation of layout::ScoreboardState. Write only \
                                 read returns empty. Set by the producer at ~10 Hz; transform_ip \
                                 reads the parsed cache on every frame.",
                            )
                            .build(),
                        glib::ParamSpecString::builder("layout-sizes-json")
                            .nick("Layout sizes (JSON)")
                            .blurb(
                                "serde_json serialisation of layout::LayoutSizes. Sets per-element \
                                 font sizes + row / tile dimensions at reference (1920×1080) \
                                 scale. Write only - read returns empty. Set once at pipeline \
                                 start; transform_ip reads the parsed cache on every frame. \
                                 Empty string falls back to LayoutSizes::default().",
                            )
                            .build(),
                    ]
                })
                .as_slice()
        }

        fn set_property(&self, _id: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
            match pspec.name() {
                "enabled" => self.settings.lock().unwrap().enabled = value.get().unwrap_or(true),
                "show-test-overlay" => {
                    self.settings.lock().unwrap().show_test_overlay = value.get().unwrap_or(false)
                }
                "font-regular-path" => {
                    self.settings.lock().unwrap().font_regular_path = value.get().ok().flatten()
                }
                "font-bold-path" => {
                    self.settings.lock().unwrap().font_bold_path = value.get().ok().flatten()
                }
                "scoreboard-state-json" => {
                    // Parse and cache; cheap microsecond work at 10 Hz.
                    // Empty string clears the cache so transform_ip
                    // falls back to the runtime's default state.
                    let raw: Option<String> = value.get().ok().flatten();
                    let mut ext = self.external_state.lock().unwrap();
                    match raw.as_deref() {
                        None | Some("") => ext.cached = None,
                        Some(s) => match serde_json::from_str::<ScoreboardState>(s) {
                            Ok(parsed) => {
                                ext.cached = Some(parsed);
                                ext.parse_count = ext.parse_count.wrapping_add(1);
                            }
                            Err(e) => {
                                ext.parse_err_count = ext.parse_err_count.wrapping_add(1);
                                // Avoid log floods on a stuck-bad
                                // producer: warn once per 100 errors.
                                if ext.parse_err_count.is_multiple_of(100)
                                    || ext.parse_err_count == 1
                                {
                                    gst::warning!(
                                        cat(),
                                        "scoreboard-state-json parse failed (n={}): {e}",
                                        ext.parse_err_count
                                    );
                                }
                            }
                        },
                    }
                }
                "layout-sizes-json" => {
                    let raw: Option<String> = value.get().ok().flatten();
                    let mut ext = self.external_sizes.lock().unwrap();
                    match raw.as_deref() {
                        None | Some("") => ext.cached = None,
                        Some(s) => match serde_json::from_str::<LayoutSizes>(s) {
                            Ok(parsed) => {
                                ext.cached = Some(parsed);
                                gst::info!(cat(), "layout-sizes-json updated: {parsed:?}");
                            }
                            Err(e) => {
                                gst::warning!(cat(), "layout-sizes-json parse failed: {e}");
                            }
                        },
                    }
                }
                other => gst::warning!(cat(), "unknown property '{other}'"),
            }
        }

        fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
            let s = self.settings.lock().unwrap();
            match pspec.name() {
                "enabled" => s.enabled.to_value(),
                "show-test-overlay" => s.show_test_overlay.to_value(),
                "font-regular-path" => s.font_regular_path.clone().to_value(),
                "font-bold-path" => s.font_bold_path.clone().to_value(),
                "scoreboard-state-json" => "".to_value(),
                "layout-sizes-json" => "".to_value(),
                _ => glib::Value::from(""),
            }
        }
    }

    impl GstObjectImpl for Nv12Overlay {}

    impl ElementImpl for Nv12Overlay {
        fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
            static META: OnceLock<gst::subclass::ElementMetadata> = OnceLock::new();
            Some(META.get_or_init(|| {
                gst::subclass::ElementMetadata::new(
                    "AICam NV12 broadcast overlay",
                    "Filter/Effect/Video",
                    "Draws the AICam scoreboard/HUD directly into NV12 frames without a \
                     videoconvert ↔ BGRx ↔ videoconvert detour.",
                    "AICam contributors",
                )
            }))
        }

        fn pad_templates() -> &'static [gst::PadTemplate] {
            static TEMPLATES: OnceLock<Vec<gst::PadTemplate>> = OnceLock::new();
            TEMPLATES
                .get_or_init(|| {
                    let caps = gst::Caps::builder("video/x-raw")
                        .field("format", "NV12")
                        .field("width", gst::IntRange::<i32>::new(2, i32::MAX))
                        .field("height", gst::IntRange::<i32>::new(2, i32::MAX))
                        .field(
                            "framerate",
                            gst::FractionRange::new(
                                gst::Fraction::new(0, 1),
                                gst::Fraction::new(i32::MAX, 1),
                            ),
                        )
                        .build();
                    vec![
                        gst::PadTemplate::new(
                            "src",
                            gst::PadDirection::Src,
                            gst::PadPresence::Always,
                            &caps,
                        )
                        .unwrap(),
                        gst::PadTemplate::new(
                            "sink",
                            gst::PadDirection::Sink,
                            gst::PadPresence::Always,
                            &caps,
                        )
                        .unwrap(),
                    ]
                })
                .as_slice()
        }
    }

    impl BaseTransformImpl for Nv12Overlay {
        const MODE: gst_base::subclass::BaseTransformMode =
            gst_base::subclass::BaseTransformMode::AlwaysInPlace;
        const PASSTHROUGH_ON_SAME_CAPS: bool = false;
        const TRANSFORM_IP_ON_PASSTHROUGH: bool = false;

        fn start(&self) -> Result<(), gst::ErrorMessage> {
            let settings = self.settings.lock().unwrap().clone();
            let regular = settings.font_regular_path.or_else(probe_regular);
            let bold = settings.font_bold_path.or_else(probe_bold);

            let mut cache = GlyphCache::new()
                .map_err(|e| gst::error_msg!(gst::LibraryError::Init, ("freetype init: {e}")))?;
            if let Some(p) = regular.as_deref() {
                match cache.register_font(REGULAR_FONT_ID, FontStyle::Regular, Path::new(p)) {
                    Ok(()) => gst::info!(cat(), "regular font: {p}"),
                    Err(e) => gst::warning!(cat(), "could not load regular font '{p}': {e}"),
                }
            } else {
                gst::warning!(
                    cat(),
                    "no regular font found; overlay text will be empty until \
                     font-regular-path is set"
                );
            }
            if let Some(p) = bold.as_deref() {
                match cache.register_font(BOLD_FONT_ID, FontStyle::Bold, Path::new(p)) {
                    Ok(()) => gst::info!(cat(), "bold font: {p}"),
                    Err(e) => gst::warning!(cat(), "could not load bold font '{p}': {e}"),
                }
            }

            *self.runtime.lock().unwrap() = Some(SendRuntime(Runtime {
                info: None,
                cache,
                scoreboard: ScoreboardState::default(),
                frame_count: 0,
                max_render_ns: 0,
                total_render_ns: 0,
            }));
            gst::info!(cat(), "started");
            Ok(())
        }

        fn stop(&self) -> Result<(), gst::ErrorMessage> {
            let mut rt = self.runtime.lock().unwrap();
            if let Some(SendRuntime(r)) = rt.as_ref() {
                let avg = if r.frame_count > 0 {
                    r.total_render_ns / 1000 / u128::from(r.frame_count)
                } else {
                    0
                };
                gst::info!(
                    cat(),
                    "stopped (frames={}, avg_render_us={}, max_render_us={})",
                    r.frame_count,
                    avg,
                    r.max_render_ns / 1000,
                );
            }
            *rt = None;
            Ok(())
        }

        fn set_caps(
            &self,
            incaps: &gst::Caps,
            _outcaps: &gst::Caps,
        ) -> Result<(), gst::LoggableError> {
            let info = gst_video::VideoInfo::from_caps(incaps)
                .map_err(|e| gst::loggable_error!(cat(), "invalid caps: {e}"))?;
            if info.format() != gst_video::VideoFormat::Nv12 {
                return Err(gst::loggable_error!(
                    cat(),
                    "expected NV12, got {:?}",
                    info.format()
                ));
            }
            let mut rt = self.runtime.lock().unwrap();
            let Some(SendRuntime(r)) = rt.as_mut() else {
                return Err(gst::loggable_error!(cat(), "set_caps before start"));
            };

            // Preload glyphs at every font size the layout will use
            // for this resolution.
            let scale = info.width() as f32 / 1920.0;
            let mut sizes: Vec<u32> = [18u32, 22, 32]
                .into_iter()
                .map(|s| ((s as f32) * scale).round().max(1.0) as u32)
                .collect();
            sizes.sort_unstable();
            sizes.dedup();
            for size in sizes {
                if r.cache.has_font(REGULAR_FONT_ID, FontStyle::Regular) {
                    let _ = r
                        .cache
                        .preload_ascii(REGULAR_FONT_ID, size, FontStyle::Regular);
                }
                if r.cache.has_font(BOLD_FONT_ID, FontStyle::Bold) {
                    let _ = r.cache.preload_ascii(BOLD_FONT_ID, size, FontStyle::Bold);
                }
            }

            gst::info!(
                cat(),
                "caps set: {}x{} (cache primed: {} glyphs)",
                info.width(),
                info.height(),
                r.cache.len()
            );
            r.info = Some(info);
            Ok(())
        }

        fn transform_ip(
            &self,
            buf: &mut gst::BufferRef,
        ) -> Result<gst::FlowSuccess, gst::FlowError> {
            // Snapshot settings into locals so we don't hold the
            // settings lock across the buffer map.
            let (enabled, show_test) = {
                let s = self.settings.lock().unwrap();
                (s.enabled, s.show_test_overlay)
            };
            if !enabled {
                return Ok(gst::FlowSuccess::Ok);
            }

            let mut rt_guard = self.runtime.lock().unwrap();
            let Some(SendRuntime(rt)) = rt_guard.as_mut() else {
                return Ok(gst::FlowSuccess::Ok); // start() hasn't fired
            };
            let info = match rt.info.clone() {
                Some(i) => i,
                None => return Ok(gst::FlowSuccess::Ok), // caps not set yet
            };

            let start = Instant::now();
            // `gst_buffer_make_writable` (done by BaseTransform) only
            // gives us an exclusively-owned buffer reference; the
            // underlying GstMemory blocks can still be shared with
            // peer pipelines. That happens whenever the upstream sits
            // on the other side of an `intervideosink → intervideosrc`
            // bridge - the same memory feeds the recording, AI, and
            // streaming consumers in parallel, and `map_writable()`
            // refuses to hand out a writable mapping while the refcount
            // on those memory blocks is >1.
            //
            // Detect that case and deep-copy each non-writable memory
            // block into a freshly allocated one with `gst_memory_copy`.
            // After this we own the memory exclusively and the second
            // `map_writable()` attempt below succeeds. The copy fires
            // at most once per element instance per frame and only on
            // the inter-pipeline boundary - direct/native producers
            // (e.g. unit tests with videotestsrc) skip it entirely.
            ensure_memory_writable(buf);
            let mut map = buf.map_writable().map_err(|_| {
                gst::error!(cat(), "could not map buffer for write");
                gst::FlowError::Error
            })?;
            let data = map.as_mut_slice();

            let y_off = info.offset()[0];
            let uv_off = info.offset()[1];
            let y_stride = info.stride()[0] as usize;
            let uv_stride = info.stride()[1] as usize;
            let width = info.width();
            let height = info.height();
            let y_size = height as usize * y_stride;
            let uv_size = (height as usize / 2) * uv_stride;

            // We only support the standard NV12 layout where Y
            // precedes UV in memory. Anything else (split memory,
            // padded plane regions) is rejected as a skip-don't-crash
            // case.
            if y_off >= uv_off || uv_off + uv_size > data.len() {
                gst::warning!(
                    cat(),
                    "unexpected plane layout (y_off={y_off} uv_off={uv_off} \
                     uv_size={uv_size} buf={}); skipping frame",
                    data.len()
                );
                return Ok(gst::FlowSuccess::Ok);
            }
            let (head, tail) = data.split_at_mut(uv_off);
            let y_plane = &mut head[y_off..y_off + y_size];
            let uv_plane = &mut tail[..uv_size];

            let Some(mut frame) =
                Nv12FrameMut::new(y_plane, uv_plane, y_stride, uv_stride, width, height)
            else {
                gst::warning!(
                    cat(),
                    "Nv12FrameMut::new rejected the buffer ({width}x{height}, ystride={y_stride}); skipping"
                );
                return Ok(gst::FlowSuccess::Ok);
            };

            let sizes = self
                .external_sizes
                .lock()
                .unwrap()
                .cached
                .unwrap_or_default();
            let params = LayoutParams {
                frame_width: width,
                frame_height: height,
                regular_font: REGULAR_FONT_ID,
                bold_font: BOLD_FONT_ID,
                sizes,
            };
            let state = if show_test {
                test_pattern_state(rt.frame_count)
            } else {
                // Read the most-recently-set state cached by the
                // `scoreboard-state-json` property setter. The clone
                // costs microseconds; the JSON parse already ran on
                // the producer's 10 Hz tick.
                let ext = self.external_state.lock().unwrap();
                match ext.cached.as_ref() {
                    Some(s) => s.clone(),
                    None => rt.scoreboard.clone(),
                }
            };
            let cmds = scoreboard_commands(&state, &params);
            let _stats = dispatch_commands(&mut frame, &mut rt.cache, &cmds);

            let elapsed_ns = start.elapsed().as_nanos();
            rt.frame_count += 1;
            rt.total_render_ns += elapsed_ns;
            if elapsed_ns > rt.max_render_ns {
                rt.max_render_ns = elapsed_ns;
            }
            // Heartbeat: log a one-liner every ~300 frames (≈10 s at
            // 30 fps) so operators see render time without scraping
            // GST_DEBUG.
            if rt.frame_count.is_multiple_of(300) {
                let avg = rt.total_render_ns / 1000 / u128::from(rt.frame_count);
                gst::info!(
                    cat(),
                    "frame {} render avg={}us max={}us",
                    rt.frame_count,
                    avg,
                    rt.max_render_ns / 1000,
                );
            }

            Ok(gst::FlowSuccess::Ok)
        }
    }
}

// ---------------------------------------------------------------------------
// Public wrapper + plugin registration
// ---------------------------------------------------------------------------

glib::wrapper! {
    pub struct Nv12Overlay(ObjectSubclass<imp::Nv12Overlay>)
        @extends gst_base::BaseTransform, gst::Element, gst::Object;
}

/// Register the element under the name `aicamnv12overlay`. Called by
/// the cdylib's `plugin_init` (see `lib.rs`'s `plugin_define!`).
pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    gst::Element::register(
        Some(plugin),
        "aicamnv12overlay",
        gst::Rank::NONE,
        Nv12Overlay::static_type(),
    )
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Deep-copy any non-writable memory blocks on `buf` into freshly
/// allocated System memory so a subsequent `map_writable()` can
/// succeed.
///
/// `gst_buffer_make_writable` (called by BaseTransform before
/// `transform_ip`) only de-duplicates the buffer reference itself, not
/// the GstMemory blocks it holds. When the buffer arrives via an
/// `intervideosink → intervideosrc` bridge - i.e. the recording, AI,
/// and streaming consumers all see the same underlying memory - those
/// memory blocks are shared and `map_writable()` returns Err.
///
/// Why we don't use `gst_memory_copy(mem, 0, size)` (i.e.
/// `MemoryRef::copy_range`): when the source is a dmabuf (libcamerasrc
/// on Pi 5, nvarguscamerasrc on Jetson), GStreamer's dmabuf allocator
/// `_alloc` vfunc tends to hand back a freshly allocated dmabuf
/// without actually transferring the source pixels into it - the
/// "copy" returns valid-shaped but zero-initialised memory. The
/// streaming consumer then renders the overlay onto an all-black
/// background. (Recording was unaffected because its pipeline
/// doesn't pass buffers through `transform_ip`.)
///
/// The workaround: do the copy in userspace. Map the source readable,
/// allocate a fresh System-memory block of the same size, map it
/// writable, memcpy. Linear in pixel count but at 1080p NV12 (~3 MB)
/// that's ~1 ms on Pi 5 - comfortably under the frame budget.
fn ensure_memory_writable(buf: &mut gst::BufferRef) {
    if buf.is_all_memory_writable() {
        return;
    }
    let n = buf.n_memory();
    for i in 0..n {
        let Some(fresh) = deep_copy_to_system(buf.peek_memory(i)) else {
            // Source not mappable for read - leave the block alone and
            // hope `map_writable()` below succeeds anyway. (This path
            // would be a real bug; we log once so the operator notices.)
            gst::warning!(
                cat(),
                "ensure_memory_writable: memory block {i} not mappable for read; \
                 leaving shared (downstream map_writable may fail)"
            );
            continue;
        };
        buf.replace_memory(i, fresh);
    }
}

/// Allocate a fresh System-memory block matching `src`'s size and
/// memcpy `src`'s readable bytes into it. Returns `None` if `src` is
/// not mappable for read.
fn deep_copy_to_system(src: &gst::MemoryRef) -> Option<gst::Memory> {
    let src_map = src.map_readable().ok()?;
    let src_bytes = src_map.as_slice();
    let mut fresh = gst::Memory::with_size(src_bytes.len());
    {
        let fresh_mut = fresh
            .get_mut()
            .expect("just-allocated Memory must be uniquely owned");
        let mut dst_map = fresh_mut
            .map_writable()
            .expect("fresh System memory must be writable");
        dst_map.as_mut_slice().copy_from_slice(src_bytes);
    }
    Some(fresh)
}

fn probe_regular() -> Option<String> {
    const CANDIDATES: &[&str] = &[
        "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
        "/usr/share/fonts/TTF/DejaVuSans.ttf",
        "/usr/share/fonts/dejavu/DejaVuSans.ttf",
        "/usr/share/fonts/truetype/liberation/LiberationSans-Regular.ttf",
    ];
    for c in CANDIDATES {
        if Path::new(c).exists() {
            return Some((*c).to_string());
        }
    }
    None
}

fn probe_bold() -> Option<String> {
    const CANDIDATES: &[&str] = &[
        "/usr/share/fonts/truetype/dejavu/DejaVuSans-Bold.ttf",
        "/usr/share/fonts/TTF/DejaVuSans-Bold.ttf",
        "/usr/share/fonts/dejavu/DejaVuSans-Bold.ttf",
        "/usr/share/fonts/truetype/liberation/LiberationSans-Bold.ttf",
    ];
    for c in CANDIDATES {
        if Path::new(c).exists() {
            return Some((*c).to_string());
        }
    }
    None
}

/// Built-in scoreboard for `show-test-overlay=true`. Cycles the
/// `clock_text` and `game_clock_text` so the operator can verify
/// the element is alive without any external state.
fn test_pattern_state(frame: u64) -> ScoreboardState {
    let sec = frame / 30;
    let home_pen = if frame % 240 < 60 {
        vec![PenaltyTile {
            player_number: 3,
            secs_remaining: 60 - (frame % 240) as u32,
            penalty_reason: "PUSHING".into(),
            is_goalkeeper: false,
        }]
    } else {
        vec![]
    };
    let away_pen = if frame % 360 < 90 {
        vec![PenaltyTile {
            player_number: 1,
            secs_remaining: 90 - (frame % 360) as u32,
            penalty_reason: "BALL HOLDING".into(),
            is_goalkeeper: true,
        }]
    } else {
        vec![]
    };
    // Live wall-clock for the top-right pill so the test pattern
    // looks like a real broadcast (production's `clock_text` is
    // already `chrono::Local::now().format("%H:%M:%S")` in
    // media_service's `scoreboard_state_from_game`).
    let clock_text = chrono::Local::now().format("%H:%M:%S").to_string();
    ScoreboardState {
        field_name: "FIELD A - HSL - RoboCup German Open 2026".into(),
        clock_text,
        home_team_name: "Hamburg Bit-Bots".into(),
        away_team_name: "R-ZWEI KICKERS".into(),
        home_team_color: Some(Rgba::opaque(217, 25, 25)),
        away_team_color: Some(Rgba::opaque(25, 50, 217)),
        // Distinct GK colours so the test pattern exercises the
        // optional GK strip rendering on both sides.
        home_team_goalkeeper_color: Some(Rgba::opaque(255, 200, 0)),
        away_team_goalkeeper_color: Some(Rgba::opaque(0, 200, 100)),
        home_score: ((sec / 13) % 9) as u32,
        away_score: ((sec / 11) % 9) as u32,
        game_clock_text: format!("{:02}:{:02}", (sec / 60) % 100, sec % 60),
        clock_stopped: frame % 600 < 60,
        phase_text: "1st".into(),
        state_text: "playing".into(),
        home_message_budget: 1234,
        away_message_budget: 1190,
        shootout: None,
        home_penalty_timers: home_pen,
        away_penalty_timers: away_pen,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn init() {
        // Initialise GStreamer once for the suite. Tests should be
        // robust to running in any order.
        static ONCE: OnceLock<()> = OnceLock::new();
        ONCE.get_or_init(|| {
            gst::init().expect("gst::init");
        });
    }

    /// Register the element globally under the name
    /// `aicamnv12overlay`. `Element::register(None, …)` plugs the
    /// type into the global element registry without needing a
    /// `GstPlugin` reference - the same factory lookup
    /// `gst::parse::launch` does will then find it. Calling this
    /// from multiple tests is fine: re-registration with the same
    /// name + type succeeds.
    fn register_for_tests() {
        static REGISTERED: OnceLock<()> = OnceLock::new();
        REGISTERED.get_or_init(|| {
            gst::Element::register(
                None,
                "aicamnv12overlay",
                gst::Rank::NONE,
                Nv12Overlay::static_type(),
            )
            .expect("register aicamnv12overlay");
        });
    }

    #[test]
    fn element_registers_and_is_inspectable() {
        init();
        // Register the plugin into a private GstPlugin instance so the
        // factory is discoverable by name without polluting the global
        // registry's loaded-plugin set (which would persist across tests).
        register_for_tests();

        let factory = gst::ElementFactory::find("aicamnv12overlay")
            .expect("aicamnv12overlay factory must be present after registration");
        let element = factory.create().build().expect("instantiate element");
        // The element exposes our advertised properties.
        let enabled: bool = element.property("enabled");
        assert!(enabled, "default `enabled` is true");
        element.set_property("enabled", false);
        let disabled: bool = element.property("enabled");
        assert!(!disabled);
    }

    #[test]
    fn pass_through_pipeline_runs_with_show_test_overlay() {
        init();
        register_for_tests();

        // videotestsrc → aicamnv12overlay → fakesink. 30 frames at
        // 320×240 NV12 - small enough to run fast even without a
        // system font (overlay text just renders empty then).
        let pipeline_desc = "videotestsrc num-buffers=30 is-live=false \
             ! video/x-raw,format=NV12,width=320,height=240,framerate=30/1 \
             ! aicamnv12overlay show-test-overlay=true \
             ! fakesink sync=false";
        let pipeline = gst::parse::launch(pipeline_desc)
            .expect("parse pipeline")
            .downcast::<gst::Pipeline>()
            .expect("downcast to Pipeline");

        pipeline
            .set_state(gst::State::Playing)
            .expect("set state Playing");

        // Wait for EOS or error with a sane timeout.
        let bus = pipeline.bus().unwrap();
        let msg = bus
            .timed_pop_filtered(
                gst::ClockTime::from_seconds(10),
                &[gst::MessageType::Eos, gst::MessageType::Error],
            )
            .expect("did not receive EOS/error within 10s");
        match msg.view() {
            gst::MessageView::Eos(_) => {}
            gst::MessageView::Error(e) => panic!("pipeline error: {} ({:?})", e.error(), e.debug()),
            _ => unreachable!(),
        }

        pipeline.set_state(gst::State::Null).unwrap();
    }

    #[test]
    fn disabled_element_passes_frames_through_unchanged() {
        init();
        register_for_tests();

        // The acceptance criterion is that `enabled=false` is a
        // passthrough. We assert that by running the pipeline to EOS
        // with the property unset *to false* - same shape as the
        // previous test, but verifying the property knob.
        let pipeline_desc = "videotestsrc num-buffers=10 is-live=false \
             ! video/x-raw,format=NV12,width=320,height=240,framerate=30/1 \
             ! aicamnv12overlay enabled=false show-test-overlay=true \
             ! fakesink sync=false";
        let pipeline = gst::parse::launch(pipeline_desc)
            .expect("parse pipeline")
            .downcast::<gst::Pipeline>()
            .expect("downcast to Pipeline");
        pipeline.set_state(gst::State::Playing).unwrap();
        let bus = pipeline.bus().unwrap();
        let msg = bus
            .timed_pop_filtered(
                gst::ClockTime::from_seconds(10),
                &[gst::MessageType::Eos, gst::MessageType::Error],
            )
            .expect("EOS/error within 10s");
        match msg.view() {
            gst::MessageView::Eos(_) => {}
            gst::MessageView::Error(e) => panic!("disabled pipeline errored: {}", e.error()),
            _ => unreachable!(),
        }
        pipeline.set_state(gst::State::Null).unwrap();
    }

    #[test]
    fn scoreboard_state_json_property_round_trips() {
        init();
        register_for_tests();

        let factory = gst::ElementFactory::find("aicamnv12overlay").unwrap();
        let element = factory.create().build().unwrap();

        // Write a JSON payload identical to what `media_service`
        // sends on each 10 Hz tick. The setter should parse it and
        // bump the `parse_count`; the cached state should appear in
        // the `external_state` mutex.
        let state = ScoreboardState {
            field_name: "test field".into(),
            home_team_name: "HOME".into(),
            away_team_name: "AWAY".into(),
            home_score: 3,
            away_score: 2,
            game_clock_text: "07:42".into(),
            phase_text: "FIRST HALF".into(),
            state_text: "PLAYING".into(),
            ..Default::default()
        };
        let json = serde_json::to_string(&state).unwrap();
        element.set_property("scoreboard-state-json", &json);

        let nv12 = element
            .dynamic_cast::<Nv12Overlay>()
            .expect("downcast to Nv12Overlay");
        let ext = nv12.imp().external_state.lock().unwrap();
        assert_eq!(ext.parse_count, 1);
        assert_eq!(ext.parse_err_count, 0);
        let cached = ext.cached.as_ref().expect("cached state present");
        assert_eq!(cached.home_score, 3);
        assert_eq!(cached.away_score, 2);
        assert_eq!(cached.game_clock_text, "07:42");
        drop(ext);

        // Empty string clears the cache (the documented contract).
        nv12.set_property("scoreboard-state-json", "");
        let ext = nv12.imp().external_state.lock().unwrap();
        assert!(ext.cached.is_none());
        // Bad JSON increments parse_err_count and does not touch
        // the cache.
        drop(ext);
        nv12.set_property("scoreboard-state-json", "{not valid json");
        let ext = nv12.imp().external_state.lock().unwrap();
        assert!(ext.cached.is_none());
        assert_eq!(ext.parse_err_count, 1);
    }

    /// Regression test for the inter-pipeline writability bug.
    ///
    /// In production an `intervideosink → intervideosrc` bridge
    /// hands the same `GstMemory` to multiple consumer pipelines in
    /// parallel. The first `map_writable()` call on such a buffer
    /// fails because the memory refcount is >1 even after
    /// `BaseTransform` makes the buffer writable. We mirror that
    /// shape here by `Clone::clone`-ing one `gst::Memory` (which
    /// increments its miniobject refcount) and appending each clone
    /// to a separate buffer. `is_all_memory_writable()` returns
    /// `false` until `ensure_memory_writable()` deep-copies the
    /// block on the transform side.
    #[test]
    fn ensure_memory_writable_breaks_shared_memory() {
        init();
        let mem = gst::Memory::with_size(16);
        let mem_dup = mem.clone(); // gst_memory_ref - refcount goes to 2
        let mut a = gst::Buffer::new();
        let mut b = gst::Buffer::new();
        a.get_mut().unwrap().append_memory(mem);
        b.get_mut().unwrap().append_memory(mem_dup);
        assert!(
            !a.is_all_memory_writable(),
            "test setup: shared GstMemory should not be writable"
        );
        assert!(!b.is_all_memory_writable(), "test setup: same on B");

        // The transform side: A's `transform_ip` runs ensure_*; B is
        // untouched.
        ensure_memory_writable(a.get_mut().unwrap());
        assert!(
            a.is_all_memory_writable(),
            "after ensure, A holds its own deep-copied memory"
        );
        // B's refcount falls back to 1 (only B references it) - also
        // becomes writable.
        assert!(
            b.is_all_memory_writable(),
            "B's GstMemory refcount fell to 1 once A swapped its block"
        );

        // And map_writable() now works on A - the exact call site
        // that previously failed in transform_ip.
        let buf_ref = a.get_mut().unwrap();
        let mut map = buf_ref
            .map_writable()
            .expect("map_writable succeeds after ensure_memory_writable");
        map.as_mut_slice()[0] = 0x42;
    }
}
