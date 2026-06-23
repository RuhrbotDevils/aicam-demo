// NV12 renderer core primitives shared by the overlay command dispatcher.
// Author: Thomas Klute

//! NV12 renderer core.
//!
//! Domain-free. Operates on a borrowed view of an NV12 frame buffer and
//! writes directly into the Y and UV planes. Does not allocate per
//! frame, does not call FreeType, does not know about RoboCup.
//!
//! All public entry points come from [`super::commands`]; this module
//! exposes the primitives those commands compile down to.

pub mod color;
pub mod fill_rect;
pub mod frame;
pub mod glyph_blit;
