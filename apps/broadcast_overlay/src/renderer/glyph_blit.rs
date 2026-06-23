// Composite a pre-rasterized glyph alpha mask into NV12 Y and UV planes.
// Author: Thomas Klute

//! Glyph blit - alpha-mask compositing into NV12 planes.
//!
//! Consumes a pre-rasterized 8-bit alpha mask (from
//! [`crate::glyphs::GlyphBitmap`]) plus a foreground RGBA + screen-
//! space `(x, y)` top-left anchor. Uses the same per-2×2 chroma
//! block model as `fill_rect` (see that module's docstring for the
//! trade-off rationale).
//!
//! Two passes:
//!
//! 1. **Y plane** - per pixel. Effective alpha is
//!    `(mask × color.a + 128) / 255`, then blended into the existing
//!    Y byte. Branch-y at the inner level (skip mask==0) for the
//!    common case of mostly-empty glyph rasters.
//!
//! 2. **UV plane** - per 2×2 chroma block. We sample the four mask
//!    values that fall in the block, average them (any out-of-bitmap
//!    samples count as 0), and use that as the chroma alpha. Mean is
//!    chosen over max so anti-aliased glyph edges produce softer
//!    chroma transitions rather than colour-saturated edges.

use crate::glyphs::GlyphBitmap;
use crate::renderer::color::{blend_u8, Rgba, YCbCr};
use crate::renderer::frame::{Nv12FrameMut, Rect};

/// Blit `bitmap` into `frame` at `(x, y)` (top-left anchor of the
/// bitmap; the caller - e.g. [`crate::glyphs::layout_left_to_right`]
/// - already translated baseline + bearings into top-left coords).
///
/// `color.a` is **multiplied** with the per-pixel mask, so a fully
/// transparent foreground is a no-op and a fully opaque foreground
/// uses the mask directly.
pub fn blit_glyph(frame: &mut Nv12FrameMut<'_>, bitmap: &GlyphBitmap, x: i32, y: i32, color: Rgba) {
    if color.a == 0 || bitmap.is_empty() {
        return;
    }
    let glyph_rect = Rect {
        x,
        y,
        width: bitmap.width as i32,
        height: bitmap.height as i32,
    };
    let Some(clipped) = glyph_rect.clip_to_frame(frame.width, frame.height) else {
        return;
    };

    let YCbCr {
        y: y_fg,
        cb: cb_fg,
        cr: cr_fg,
    } = color.into();

    // Bitmap-space offset of the clipped origin.
    let bx_offset = (clipped.x as i32 - x) as usize;
    let by_offset = (clipped.y as i32 - y) as usize;
    let bitmap_w = bitmap.width as usize;

    // --- Y plane pass ---
    let y_stride = frame.y_stride;
    for row in 0..clipped.height as usize {
        let bitmap_row = (by_offset + row) * bitmap_w;
        let frame_row = (clipped.y as usize + row) * y_stride;
        for col in 0..clipped.width as usize {
            let mask = bitmap.alpha[bitmap_row + bx_offset + col];
            if mask == 0 {
                continue;
            }
            let effective = scale_alpha(mask, color.a);
            let off = frame_row + clipped.x as usize + col;
            frame.y_plane[off] = blend_u8(frame.y_plane[off], y_fg, effective);
        }
    }

    // --- UV plane pass (per 2×2 chroma block, mean of the 4 covered
    // mask values within the bitmap) ---
    let uv_stride = frame.uv_stride;
    let cx0 = clipped.x / 2;
    let cy0 = clipped.y / 2;
    let cx1 = (clipped.x + clipped.width).div_ceil(2);
    let cy1 = (clipped.y + clipped.height).div_ceil(2);

    for cy in cy0..cy1 {
        for cx in cx0..cx1 {
            // Average the 4 mask samples in this 2×2 Y block;
            // out-of-bitmap samples count as 0.
            let mut sum: u32 = 0;
            for dy in 0..2 {
                for dx in 0..2 {
                    let fx = (cx * 2 + dx) as i32;
                    let fy = (cy * 2 + dy) as i32;
                    let bx = fx - x;
                    let by = fy - y;
                    if bx < 0
                        || by < 0
                        || (bx as u32) >= bitmap.width
                        || (by as u32) >= bitmap.height
                    {
                        continue;
                    }
                    sum += bitmap.alpha[by as usize * bitmap_w + bx as usize] as u32;
                }
            }
            // Mean over 4 samples (not `count` - covering 2 samples
            // with mask=255 still gives a 50%-strength chroma).
            let avg_mask = (sum >> 2) as u8;
            if avg_mask == 0 {
                continue;
            }
            let effective = scale_alpha(avg_mask, color.a);
            let off = cy as usize * uv_stride + cx as usize * 2;
            frame.uv_plane[off] = blend_u8(frame.uv_plane[off], cb_fg, effective);
            frame.uv_plane[off + 1] = blend_u8(frame.uv_plane[off + 1], cr_fg, effective);
        }
    }
}

/// `(mask × color_a + 128) / 255` → effective 8-bit alpha for the
/// blend. Same exact reciprocal as
/// [`crate::renderer::color::blend_u8`] uses internally.
#[inline]
fn scale_alpha(mask: u8, color_a: u8) -> u8 {
    let acc = u32::from(mask) * u32::from(color_a);
    ((acc + 1 + (acc >> 8)) >> 8) as u8
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::glyphs::{FontId, FontStyle, GlyphBitmap, GlyphCache, GlyphKey};
    use crate::renderer::color::Rgba;
    use std::path::PathBuf;

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

    fn black_frame(w: u32, h: u32) -> (Vec<u8>, Vec<u8>) {
        (
            vec![16u8; (w * h) as usize],
            vec![128u8; (w * h / 2) as usize],
        )
    }

    /// Manually construct a synthetic 4×4 alpha-mask glyph so the
    /// blit logic is testable without FreeType.
    fn synthetic_glyph_4x4_full() -> GlyphBitmap {
        GlyphBitmap {
            width: 4,
            height: 4,
            bearing_x: 0,
            bearing_y: 4, // baseline is at top of bitmap (test convenience)
            advance_x: 4,
            alpha: vec![255u8; 16],
        }
    }

    fn synthetic_glyph_diagonal() -> GlyphBitmap {
        // 4×4 with the diagonal fully painted, off-diagonal zero.
        let mut alpha = vec![0u8; 16];
        for i in 0..4 {
            alpha[i * 4 + i] = 255;
        }
        GlyphBitmap {
            width: 4,
            height: 4,
            bearing_x: 0,
            bearing_y: 4,
            advance_x: 4,
            alpha,
        }
    }

    #[test]
    fn empty_glyph_is_noop() {
        let (mut yp, mut uv) = black_frame(32, 16);
        let before_y = yp.clone();
        let before_uv = uv.clone();
        {
            let mut frame = Nv12FrameMut::new(&mut yp, &mut uv, 32, 32, 32, 16).unwrap();
            blit_glyph(
                &mut frame,
                &GlyphBitmap {
                    width: 0,
                    height: 0,
                    bearing_x: 0,
                    bearing_y: 0,
                    advance_x: 6,
                    alpha: vec![],
                },
                4,
                4,
                Rgba::opaque(255, 255, 255),
            );
        }
        assert_eq!(yp, before_y);
        assert_eq!(uv, before_uv);
    }

    #[test]
    fn fully_opaque_white_glyph_writes_white_y_inside_mask() {
        let (mut yp, mut uv) = black_frame(32, 16);
        {
            let mut frame = Nv12FrameMut::new(&mut yp, &mut uv, 32, 32, 32, 16).unwrap();
            blit_glyph(
                &mut frame,
                &synthetic_glyph_4x4_full(),
                4,
                4,
                Rgba::opaque(255, 255, 255),
            );
        }
        // Every pixel inside the 4×4 rect should be limited-range
        // white (235). Outside should stay at black (16).
        for row in 4..8 {
            for col in 4..8 {
                assert_eq!(yp[row * 32 + col], 235, "y@({},{})", col, row);
            }
        }
        assert_eq!(yp[0], 16);
        assert_eq!(yp[3 * 32 + 4], 16, "row above must stay black");
        assert_eq!(yp[4 * 32 + 3], 16, "col left must stay black");
    }

    #[test]
    fn alpha_zero_color_is_noop_even_with_full_mask() {
        let (mut yp, mut uv) = black_frame(32, 16);
        let before_y = yp.clone();
        {
            let mut frame = Nv12FrameMut::new(&mut yp, &mut uv, 32, 32, 32, 16).unwrap();
            blit_glyph(
                &mut frame,
                &synthetic_glyph_4x4_full(),
                4,
                4,
                Rgba::new(255, 0, 0, 0),
            );
        }
        assert_eq!(yp, before_y);
    }

    #[test]
    fn diagonal_mask_only_writes_diagonal_pixels() {
        let (mut yp, mut uv) = black_frame(32, 16);
        {
            let mut frame = Nv12FrameMut::new(&mut yp, &mut uv, 32, 32, 32, 16).unwrap();
            blit_glyph(
                &mut frame,
                &synthetic_glyph_diagonal(),
                4,
                4,
                Rgba::opaque(255, 255, 255),
            );
        }
        // Y plane: the 4 diagonal pixels become white; everything
        // else stays black.
        for row in 0..16 {
            for col in 0..32 {
                let v = yp[row * 32 + col];
                let on_diag =
                    (4..8).contains(&row) && (4..8).contains(&col) && (col - 4) == (row - 4);
                let expected: u8 = if on_diag { 235 } else { 16 };
                assert_eq!(v, expected, "y@({},{}) expected {}", col, row, expected);
            }
        }
    }

    #[test]
    fn glyph_clipped_to_frame_at_top_left() {
        let (mut yp, mut uv) = black_frame(32, 16);
        {
            let mut frame = Nv12FrameMut::new(&mut yp, &mut uv, 32, 32, 32, 16).unwrap();
            // Place the 4×4 glyph half off the top-left so only (0..2, 0..2) is in-frame.
            blit_glyph(
                &mut frame,
                &synthetic_glyph_4x4_full(),
                -2,
                -2,
                Rgba::opaque(255, 255, 255),
            );
        }
        // (0..2, 0..2) should be white; (2..4, *) and (*, 2..4) stay black.
        for row in 0..2 {
            for col in 0..2 {
                assert_eq!(yp[row * 32 + col], 235);
            }
        }
        assert_eq!(yp[2], 16);
        assert_eq!(yp[2 * 32], 16);
    }

    #[test]
    fn glyph_fully_offscreen_is_noop() {
        let (mut yp, mut uv) = black_frame(32, 16);
        let before_y = yp.clone();
        {
            let mut frame = Nv12FrameMut::new(&mut yp, &mut uv, 32, 32, 32, 16).unwrap();
            blit_glyph(
                &mut frame,
                &synthetic_glyph_4x4_full(),
                100,
                100,
                Rgba::opaque(255, 0, 0),
            );
            blit_glyph(
                &mut frame,
                &synthetic_glyph_4x4_full(),
                -100,
                -100,
                Rgba::opaque(0, 255, 0),
            );
        }
        assert_eq!(yp, before_y);
    }

    /// Integration smoke: rasterize a real FreeType glyph and blit
    /// it. Verifies the renderer accepts what the cache produces.
    #[test]
    fn real_freetype_glyph_blits_visible_pixels() {
        let Some(font) = probe_font() else {
            eprintln!("[skip] no system font");
            return;
        };
        const F: FontId = FontId(0);
        let mut cache = GlyphCache::new().unwrap();
        cache.register_font(F, FontStyle::Regular, &font).unwrap();
        let g = cache
            .get_or_rasterize(GlyphKey {
                font_id: F,
                size_px: 16,
                style: FontStyle::Regular,
                codepoint: 'O',
            })
            .unwrap()
            .clone();
        assert!(!g.is_empty());

        let (mut yp, mut uv) = black_frame(64, 32);
        {
            let mut frame = Nv12FrameMut::new(&mut yp, &mut uv, 64, 64, 64, 32).unwrap();
            // Place the glyph somewhere central - top-left anchor.
            blit_glyph(&mut frame, &g, 8, 8, Rgba::opaque(255, 255, 255));
        }
        // At least one Y plane byte should now exceed black (16).
        let any_painted = yp.iter().any(|&v| v > 16);
        assert!(any_painted, "real glyph blit produced no visible pixels");
    }
}
