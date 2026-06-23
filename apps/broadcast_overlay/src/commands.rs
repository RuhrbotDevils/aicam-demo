// Pixel overlay command enum plus the dispatcher and validator over it.
// Author: Thomas Klute

//! Pixel overlay command API.
//!
//! Domain-free. Layout producers (e.g. `crate::layout::scoreboard`)
//! emit `OverlayCommand` lists; the dispatcher consumes them and
//! drives the renderer. Later commands paint over earlier ones -
//! see [`dispatch_commands`].

use crate::glyphs::{layout_left_to_right, FontId, FontStyle, GlyphCache};
use crate::renderer::color::Rgba;
use crate::renderer::fill_rect::{fill_rect_alpha, fill_rect_opaque};
use crate::renderer::frame::{Nv12FrameMut, Rect};
use crate::renderer::glyph_blit::blit_glyph;

/// A single drawing command. Coordinates are signed because the spec
/// allows negative anchors (clipped against the frame by the
/// renderer, not rejected up front).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OverlayCommand {
    /// Filled rectangle. Uses [`fill_rect_opaque`] when `color.a ==
    /// 255` (faster `slice::fill` path), otherwise [`fill_rect_alpha`].
    /// `color.a == 0` is a no-op.
    FillRect {
        x: i32,
        y: i32,
        width: i32,
        height: i32,
        color: Rgba,
    },
    /// Text at a baseline anchor. `(x, y)` is the baseline-left
    /// pen position; the layout helper translates per-glyph bearings
    /// into top-left blit coords.
    DrawText {
        x: i32,
        y: i32,
        text: String,
        font_id: FontId,
        size_px: u32,
        style: FontStyle,
        color: Rgba,
    },
}

/// Counters surfaced from a single [`dispatch_commands`] pass.
/// Plugin-level observability folds these into element stats; here it
/// just gives tests a way to assert how many commands actually ran.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct DispatchStats {
    pub fill_rect_executed: u32,
    pub draw_text_executed: u32,
    pub skipped_invalid: u32,
    pub skipped_no_op: u32,
}

/// Render `commands` over `frame` in list order.
///
/// The dispatcher does **not** call [`validate_commands`] first -
/// invalid commands (unknown font, zero font size, etc.) are
/// silently skipped at runtime so a corrupt command stream never
/// crashes the streaming pipeline. The plugin layer should still
/// call `validate_commands` once at handoff time to surface
/// authoring bugs early, but treat the runtime path as
/// defence-in-depth.
///
/// `cache` is `&mut` because lazy glyph rasterization may insert
/// new entries on first sight. At steady-state - after
/// `preload_ascii` plus a few non-ASCII codepoints - the dispatcher
/// only does cache *lookups*, not FreeType calls.
pub fn dispatch_commands(
    frame: &mut Nv12FrameMut<'_>,
    cache: &mut GlyphCache,
    commands: &[OverlayCommand],
) -> DispatchStats {
    let mut stats = DispatchStats::default();
    for cmd in commands {
        match cmd {
            OverlayCommand::FillRect {
                x,
                y,
                width,
                height,
                color,
            } => {
                if color.a == 0 {
                    stats.skipped_no_op += 1;
                    continue;
                }
                let rect = Rect {
                    x: *x,
                    y: *y,
                    width: *width,
                    height: *height,
                };
                if color.a == 255 {
                    fill_rect_opaque(frame, rect, *color);
                } else {
                    fill_rect_alpha(frame, rect, *color);
                }
                stats.fill_rect_executed += 1;
            }
            OverlayCommand::DrawText {
                x,
                y,
                text,
                font_id,
                size_px,
                style,
                color,
            } => {
                if *size_px == 0 || color.a == 0 || text.is_empty() {
                    stats.skipped_no_op += 1;
                    continue;
                }
                let Some(placements) =
                    layout_left_to_right(cache, text, *font_id, *size_px, *style, *x, *y)
                else {
                    // Unknown font for this (font_id, style) pair -
                    // skip silently, matching the runtime defence
                    // contract above.
                    stats.skipped_invalid += 1;
                    continue;
                };
                for p in &placements {
                    let g = cache.get_or_rasterize(p.key).cloned();
                    if let Some(g) = g {
                        blit_glyph(frame, &g, p.x, p.y, *color);
                    }
                }
                stats.draw_text_executed += 1;
            }
        }
    }
    stats
}

/// Reasons a command is structurally invalid. Caller-friendly so the
/// plugin layer can log a useful message at state-update time
/// instead of silently dropping draws.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandError {
    /// `size_px == 0` on a `DrawText` command.
    ZeroFontSize,
    /// `(font_id, style)` not registered in the cache.
    UnknownFont { font_id: FontId, style: FontStyle },
    /// `width` or `height` is negative on a `FillRect`. (Zero is
    /// allowed - it's just a no-op rectangle.)
    NegativeDimensions,
}

/// One validation failure: the index into the input slice plus the
/// reason. The plugin will log these and decide whether to reject
/// the whole command list or accept the rest.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ValidationFailure {
    pub index: usize,
    pub error: CommandError,
}

/// Walk the list once and surface any structural problems. Does
/// **not** mutate the frame or the cache. Cheap; intended to be
/// called from the state-update path, not from the streaming hot
/// path.
pub fn validate_commands(
    cache: &GlyphCache,
    commands: &[OverlayCommand],
) -> Vec<ValidationFailure> {
    let mut failures = Vec::new();
    for (index, cmd) in commands.iter().enumerate() {
        match cmd {
            OverlayCommand::FillRect { width, height, .. } => {
                if *width < 0 || *height < 0 {
                    failures.push(ValidationFailure {
                        index,
                        error: CommandError::NegativeDimensions,
                    });
                }
            }
            OverlayCommand::DrawText {
                size_px,
                font_id,
                style,
                ..
            } => {
                if *size_px == 0 {
                    failures.push(ValidationFailure {
                        index,
                        error: CommandError::ZeroFontSize,
                    });
                }
                if !cache.has_font(*font_id, *style) {
                    failures.push(ValidationFailure {
                        index,
                        error: CommandError::UnknownFont {
                            font_id: *font_id,
                            style: *style,
                        },
                    });
                }
            }
        }
    }
    failures
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::glyphs::{FontId, FontStyle};
    use crate::renderer::color::Rgba;
    use std::path::PathBuf;

    const F: FontId = FontId(0);

    fn probe_font() -> Option<PathBuf> {
        let candidates = [
            "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
            "/usr/share/fonts/TTF/DejaVuSans.ttf",
            "/usr/share/fonts/dejavu/DejaVuSans.ttf",
            "/usr/share/fonts/truetype/liberation/LiberationSans-Regular.ttf",
        ];
        for c in candidates {
            let p = PathBuf::from(c);
            if p.exists() {
                return Some(p);
            }
        }
        None
    }

    fn black_frame_64x32(y: &mut Vec<u8>, uv: &mut Vec<u8>) {
        y.clear();
        y.resize(64 * 32, 16);
        uv.clear();
        uv.resize(64 * 16, 128);
    }

    fn view<'a>(y: &'a mut [u8], uv: &'a mut [u8]) -> Nv12FrameMut<'a> {
        Nv12FrameMut::new(y, uv, 64, 64, 64, 32).unwrap()
    }

    // -------------------- validate_commands --------------------

    #[test]
    fn validate_empty_list_is_clean() {
        let cache = GlyphCache::new().unwrap();
        let failures = validate_commands(&cache, &[]);
        assert!(failures.is_empty());
    }

    #[test]
    fn validate_fillrect_negative_dim_flagged() {
        let cache = GlyphCache::new().unwrap();
        let cmds = vec![
            OverlayCommand::FillRect {
                x: 0,
                y: 0,
                width: -10,
                height: 5,
                color: Rgba::opaque(255, 0, 0),
            },
            OverlayCommand::FillRect {
                x: 0,
                y: 0,
                width: 5,
                height: -3,
                color: Rgba::opaque(255, 0, 0),
            },
        ];
        let failures = validate_commands(&cache, &cmds);
        assert_eq!(failures.len(), 2);
        assert_eq!(failures[0].index, 0);
        assert_eq!(failures[0].error, CommandError::NegativeDimensions);
        assert_eq!(failures[1].index, 1);
    }

    #[test]
    fn validate_drawtext_zero_size_flagged() {
        let cache = GlyphCache::new().unwrap();
        let cmds = vec![OverlayCommand::DrawText {
            x: 0,
            y: 0,
            text: "hi".into(),
            font_id: F,
            size_px: 0,
            style: FontStyle::Regular,
            color: Rgba::opaque(255, 255, 255),
        }];
        let failures = validate_commands(&cache, &cmds);
        // Both ZeroFontSize and UnknownFont fire (font not registered).
        assert!(failures
            .iter()
            .any(|f| f.error == CommandError::ZeroFontSize));
    }

    #[test]
    fn validate_drawtext_unknown_font_flagged() {
        let cache = GlyphCache::new().unwrap();
        let cmds = vec![OverlayCommand::DrawText {
            x: 0,
            y: 0,
            text: "hi".into(),
            font_id: FontId(99),
            size_px: 16,
            style: FontStyle::Bold,
            color: Rgba::opaque(255, 255, 255),
        }];
        let failures = validate_commands(&cache, &cmds);
        assert_eq!(failures.len(), 1);
        assert_eq!(
            failures[0].error,
            CommandError::UnknownFont {
                font_id: FontId(99),
                style: FontStyle::Bold,
            }
        );
    }

    // -------------------- dispatch ordering --------------------

    #[test]
    fn dispatch_runs_commands_in_list_order() {
        // Two opaque fill rectangles in the same region; the second
        // command should win because it paints last.
        let mut cache = GlyphCache::new().unwrap();
        let mut yp = Vec::new();
        let mut uv = Vec::new();
        black_frame_64x32(&mut yp, &mut uv);
        let stats = {
            let mut frame = view(&mut yp, &mut uv);
            dispatch_commands(
                &mut frame,
                &mut cache,
                &[
                    OverlayCommand::FillRect {
                        x: 4,
                        y: 4,
                        width: 16,
                        height: 16,
                        color: Rgba::opaque(255, 0, 0), // red
                    },
                    OverlayCommand::FillRect {
                        x: 4,
                        y: 4,
                        width: 16,
                        height: 16,
                        color: Rgba::opaque(0, 0, 255), // blue paints over red
                    },
                ],
            )
        };
        assert_eq!(stats.fill_rect_executed, 2);
        // Sample any pixel in the rect - should be blue's Y (≈32), not red's (≈63).
        let v = yp[6 * 64 + 8];
        let blue_y = crate::renderer::color::rgb_to_ycbcr_rec709_limited(0, 0, 255).y;
        assert_eq!(v, blue_y, "later command must paint over earlier");
    }

    #[test]
    fn dispatch_empty_list_is_noop() {
        let mut cache = GlyphCache::new().unwrap();
        let mut yp = Vec::new();
        let mut uv = Vec::new();
        black_frame_64x32(&mut yp, &mut uv);
        let before_y = yp.clone();
        let stats = {
            let mut frame = view(&mut yp, &mut uv);
            dispatch_commands(&mut frame, &mut cache, &[])
        };
        assert_eq!(stats, DispatchStats::default());
        assert_eq!(yp, before_y);
    }

    // -------------------- dispatch silent-skip behaviour --------------------

    #[test]
    fn dispatch_fillrect_alpha_zero_is_noop_and_counted() {
        let mut cache = GlyphCache::new().unwrap();
        let mut yp = Vec::new();
        let mut uv = Vec::new();
        black_frame_64x32(&mut yp, &mut uv);
        let before_y = yp.clone();
        let stats = {
            let mut frame = view(&mut yp, &mut uv);
            dispatch_commands(
                &mut frame,
                &mut cache,
                &[OverlayCommand::FillRect {
                    x: 0,
                    y: 0,
                    width: 10,
                    height: 10,
                    color: Rgba::new(255, 255, 255, 0),
                }],
            )
        };
        assert_eq!(stats.fill_rect_executed, 0);
        assert_eq!(stats.skipped_no_op, 1);
        assert_eq!(yp, before_y);
    }

    #[test]
    fn dispatch_drawtext_zero_size_is_noop_and_counted() {
        let mut cache = GlyphCache::new().unwrap();
        let mut yp = Vec::new();
        let mut uv = Vec::new();
        black_frame_64x32(&mut yp, &mut uv);
        let stats = {
            let mut frame = view(&mut yp, &mut uv);
            dispatch_commands(
                &mut frame,
                &mut cache,
                &[OverlayCommand::DrawText {
                    x: 0,
                    y: 0,
                    text: "ignored".into(),
                    font_id: F,
                    size_px: 0,
                    style: FontStyle::Regular,
                    color: Rgba::opaque(255, 255, 255),
                }],
            )
        };
        assert_eq!(stats.draw_text_executed, 0);
        assert_eq!(stats.skipped_no_op, 1);
    }

    #[test]
    fn dispatch_drawtext_empty_text_is_noop_and_counted() {
        let mut cache = GlyphCache::new().unwrap();
        let mut yp = Vec::new();
        let mut uv = Vec::new();
        black_frame_64x32(&mut yp, &mut uv);
        let stats = {
            let mut frame = view(&mut yp, &mut uv);
            dispatch_commands(
                &mut frame,
                &mut cache,
                &[OverlayCommand::DrawText {
                    x: 0,
                    y: 0,
                    text: String::new(),
                    font_id: F,
                    size_px: 16,
                    style: FontStyle::Regular,
                    color: Rgba::opaque(255, 255, 255),
                }],
            )
        };
        assert_eq!(stats.draw_text_executed, 0);
        assert_eq!(stats.skipped_no_op, 1);
    }

    #[test]
    fn dispatch_drawtext_unknown_font_is_skipped() {
        let mut cache = GlyphCache::new().unwrap();
        let mut yp = Vec::new();
        let mut uv = Vec::new();
        black_frame_64x32(&mut yp, &mut uv);
        let before_y = yp.clone();
        let stats = {
            let mut frame = view(&mut yp, &mut uv);
            dispatch_commands(
                &mut frame,
                &mut cache,
                &[OverlayCommand::DrawText {
                    x: 0,
                    y: 0,
                    text: "hi".into(),
                    font_id: F,
                    size_px: 16,
                    style: FontStyle::Regular,
                    color: Rgba::opaque(255, 255, 255),
                }],
            )
        };
        // No font registered for FontId(0)/Regular - the dispatcher
        // must skip silently rather than panic, and the frame must
        // be unmodified.
        assert_eq!(stats.draw_text_executed, 0);
        assert_eq!(stats.skipped_invalid, 1);
        assert_eq!(yp, before_y);
    }

    // -------------------- end-to-end with real font --------------------

    #[test]
    fn dispatch_drawtext_with_real_font_paints_pixels() {
        let Some(font) = probe_font() else {
            eprintln!("[skip] no system font");
            return;
        };
        let mut cache = GlyphCache::new().unwrap();
        cache.register_font(F, FontStyle::Regular, &font).unwrap();

        let mut yp = Vec::new();
        let mut uv = Vec::new();
        black_frame_64x32(&mut yp, &mut uv);

        let cmds = vec![
            OverlayCommand::FillRect {
                x: 0,
                y: 0,
                width: 64,
                height: 32,
                color: Rgba::new(0, 0, 0, 178), // 70 % alpha black backdrop
            },
            OverlayCommand::DrawText {
                x: 8,
                y: 22, // baseline near vertical centre of the 32-row frame
                text: "OK".into(),
                font_id: F,
                size_px: 16,
                style: FontStyle::Regular,
                color: Rgba::opaque(255, 255, 255),
            },
        ];
        let failures = validate_commands(&cache, &cmds);
        assert!(failures.is_empty(), "{failures:?}");

        let stats = {
            let mut frame = view(&mut yp, &mut uv);
            dispatch_commands(&mut frame, &mut cache, &cmds)
        };
        assert_eq!(stats.fill_rect_executed, 1);
        assert_eq!(stats.draw_text_executed, 1);
        // At least one Y byte should now be near white (235 ± noise).
        let bright = yp.iter().filter(|&&v| v > 200).count();
        assert!(bright > 0, "text command produced no bright Y pixels");
    }
}
