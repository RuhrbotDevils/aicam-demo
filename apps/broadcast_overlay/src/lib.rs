// Crate root: NV12-native broadcast overlay renderer and GStreamer plugin entrypoint.
// Author: Thomas Klute

//! AICam broadcast overlay - NV12-native scoreboard / HUD renderer.
//!
//! Layered design:
//!
//! ```text
//! semantic state (GameOverlayData)
//!     -> layout::scoreboard  (RoboCup-specific; produces commands)
//!     -> commands             (FillRect, DrawText - generic)
//!     -> renderer             (NV12 plane writes, no domain knowledge)
//! ```
//!
//! The renderer and command dispatcher are testable without GStreamer.
//! The `plugin` feature (on by default) compiles the GStreamer element
//! `aicamnv12overlay` into the `cdylib` target.

pub mod commands;
pub mod glyphs;
pub mod layout;
pub mod renderer;

#[cfg(feature = "plugin")]
pub mod plugin;

// `Nv12Overlay` is re-exported only for plugin-internal use (and the
// crate's own integration tests). Other consumers - notably
// `apps/media_service` - must NOT link this type into their main
// binary: glib's subclass registry would then see `AicamNv12Overlay`
// registered twice (once via the dlopen-loaded `.so`, once via the
// statically-linked Cargo dep) and panic on the second registration.
// Producers communicate with the element by setting the
// `scoreboard-state-json` GObject property by name on a generic
// `gst::Element` handle returned from the element factory.
#[cfg(feature = "plugin")]
pub use plugin::Nv12Overlay;

// GStreamer plugin entrypoint. Must live at the crate root because
// `gst::plugin_define!` declares C-ABI symbols GStreamer looks up by
// fixed name when it loads the `.so`.
//
// The first arg here is the *plugin* name. GStreamer derives the
// expected symbol name from the `.so` filename (stripping `lib` and
// the `.so` suffix), so it must match the crate name -
// `libaicam_broadcast_overlay.so` <->
// `gst_plugin_aicam_broadcast_overlay_get_desc`. The *element*
// factory `aicamnv12overlay` is registered separately inside
// `plugin::register` and is what `gst-launch-1.0` users type.
#[cfg(feature = "plugin")]
gst::plugin_define!(
    aicam_broadcast_overlay,
    env!("CARGO_PKG_DESCRIPTION"),
    plugin_init,
    env!("CARGO_PKG_VERSION"),
    "MIT/X11",
    env!("CARGO_PKG_NAME"),
    env!("CARGO_PKG_NAME"),
    "https://github.com/RuhrbotDevils/aicam-demo",
    "2026-06-21"
);

#[cfg(feature = "plugin")]
fn plugin_init(plugin: &gst::Plugin) -> Result<(), gst::glib::BoolError> {
    plugin::register(plugin)
}
