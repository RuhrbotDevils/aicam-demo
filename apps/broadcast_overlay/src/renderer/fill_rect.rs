// Opaque and alpha-blended rectangle fills into NV12 Y and UV planes.
// Author: Thomas Klute

//! Rectangle blit - opaque and alpha-blended.
//!
//! Touches only the affected region. Off-screen rectangles are
//! clipped against the frame bounds via [`super::frame::Rect::clip_to_frame`]
//! before any plane access.
//!
//! ## Chroma model - per-2×2 block update
//!
//! NV12's UV plane has half the spatial resolution of Y, so each
//! `(U, V)` pair covers a 2×2 Y block. The renderer commits to
//! **per-2×2 block** chroma updates:
//!
//! - When the clipped rectangle fully covers a 2×2 Y block, the
//!   chroma pair for that block is rewritten (opaque) or blended
//!   (alpha).
//! - When the clipped rectangle *partially* covers a 2×2 Y block
//!   (an edge bisects it), the chroma pair is **also** updated -
//!   this is the trade-off: rectangle edges may show ~1-px color
//!   bleed onto pixels that the Y plane left untouched. For 1-px
//!   strokes around scoreboard pills the bleed is hard to spot at
//!   1080p; for solid 50×50+ fill rectangles it's invisible. This
//!   keeps the hot path branch-free vs the "only update fully
//!   covered blocks" alternative.

use super::color::{blend_u8, Rgba, YCbCr};
use super::frame::{Nv12FrameMut, Rect};

/// Fill a fully opaque rectangle in `color` over the clipped region.
///
/// `color.a` is **ignored** - call [`fill_rect_alpha`] if you want
/// blending. This is a separate entry point because the opaque path
/// is meaningfully cheaper (memset for the Y plane, two memsets for
/// the chroma pair).
pub fn fill_rect_opaque(frame: &mut Nv12FrameMut<'_>, rect: Rect, color: Rgba) {
    let Some(clipped) = rect.clip_to_frame(frame.width, frame.height) else {
        return;
    };
    let YCbCr { y, cb, cr } = color.into();

    // --- Y plane ---
    let y_stride = frame.y_stride;
    for row in clipped.y..(clipped.y + clipped.height) {
        let off = row as usize * y_stride + clipped.x as usize;
        let end = off + clipped.width as usize;
        frame.y_plane[off..end].fill(y);
    }

    // --- UV plane (interleaved U V pairs, half-resolution) ---
    // Clip the rectangle to the chroma grid: any pixel of the rect
    // that lands in a given 2×2 block makes that block "covered".
    let (cx0, cy0, cx1, cy1) = covered_chroma_blocks(&clipped, frame.width, frame.height);
    let uv_stride = frame.uv_stride;
    for cy in cy0..cy1 {
        // Each chroma row covers `frame.width / 2` (U, V) pairs.
        // Byte offset of the first covered pair in this row:
        let row_base = cy as usize * uv_stride + cx0 as usize * 2;
        let pair_count = (cx1 - cx0) as usize;
        for i in 0..pair_count {
            frame.uv_plane[row_base + 2 * i] = cb;
            frame.uv_plane[row_base + 2 * i + 1] = cr;
        }
    }
}

/// Fill an alpha-blended rectangle. `color.a` is the blend weight
/// (0 = no effect, 255 = same as `fill_rect_opaque`).
pub fn fill_rect_alpha(frame: &mut Nv12FrameMut<'_>, rect: Rect, color: Rgba) {
    let Some(clipped) = rect.clip_to_frame(frame.width, frame.height) else {
        return;
    };
    let alpha = color.a;
    if alpha == 0 {
        return;
    }
    if alpha == 255 {
        fill_rect_opaque(frame, rect, color);
        return;
    }
    let YCbCr {
        y: y_src,
        cb: cb_src,
        cr: cr_src,
    } = color.into();

    // --- Y plane ---
    let y_stride = frame.y_stride;
    for row in clipped.y..(clipped.y + clipped.height) {
        let off = row as usize * y_stride + clipped.x as usize;
        let end = off + clipped.width as usize;
        for byte in &mut frame.y_plane[off..end] {
            *byte = blend_u8(*byte, y_src, alpha);
        }
    }

    // --- UV plane ---
    let (cx0, cy0, cx1, cy1) = covered_chroma_blocks(&clipped, frame.width, frame.height);
    let uv_stride = frame.uv_stride;
    for cy in cy0..cy1 {
        let row_base = cy as usize * uv_stride + cx0 as usize * 2;
        let pair_count = (cx1 - cx0) as usize;
        for i in 0..pair_count {
            let u_off = row_base + 2 * i;
            let v_off = u_off + 1;
            frame.uv_plane[u_off] = blend_u8(frame.uv_plane[u_off], cb_src, alpha);
            frame.uv_plane[v_off] = blend_u8(frame.uv_plane[v_off], cr_src, alpha);
        }
    }
}

/// Map a clipped pixel-space rectangle to the half-resolution chroma
/// grid, returning the *inclusive-exclusive* range of covered chroma
/// blocks `(cx0, cy0, cx1, cy1)` where `cx*` indexes the (U,V) pair
/// column and `cy*` the chroma row.
///
/// Uses the per-2×2 model: any pixel touching a chroma block makes
/// the whole block covered. Floors the top-left corner; ceils the
/// bottom-right.
fn covered_chroma_blocks(
    rect: &super::frame::ClippedRect,
    frame_width: u32,
    frame_height: u32,
) -> (u32, u32, u32, u32) {
    let cx0 = rect.x / 2;
    let cy0 = rect.y / 2;
    let right = rect.x + rect.width; // exclusive in pixel space
    let bottom = rect.y + rect.height;
    // Ceil-divide so any pixel that straddles a chroma block makes
    // that block "covered".
    let cx1 = right.div_ceil(2);
    let cy1 = bottom.div_ceil(2);
    // Bound to the chroma grid for safety.
    (
        cx0,
        cy0,
        cx1.min(frame_width / 2),
        cy1.min(frame_height / 2),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::renderer::color::{rgb_to_ycbcr_rec709_limited, Rgba};

    /// Allocate a tightly-strided 64×32 NV12 buffer pre-filled with
    /// limited-range black so we can spot writes against the
    /// background.
    fn black_frame_64x32(y: &mut Vec<u8>, uv: &mut Vec<u8>) {
        y.clear();
        y.resize(64 * 32, 16);
        uv.clear();
        uv.resize(64 * 16, 128); // U V U V … chroma centre = 128
    }

    /// Helper that wraps a plane pair into an Nv12FrameMut.
    fn view<'a>(y: &'a mut [u8], uv: &'a mut [u8], w: u32, h: u32) -> Nv12FrameMut<'a> {
        let stride = w as usize;
        Nv12FrameMut::new(y, uv, stride, stride, w, h).unwrap()
    }

    #[test]
    fn opaque_fill_writes_solid_y_and_uv() {
        let mut yp = Vec::new();
        let mut uv = Vec::new();
        black_frame_64x32(&mut yp, &mut uv);
        {
            let mut frame = view(&mut yp, &mut uv, 64, 32);
            fill_rect_opaque(
                &mut frame,
                Rect {
                    x: 10,
                    y: 4,
                    width: 8,
                    height: 6,
                },
                Rgba::opaque(255, 0, 0), // pure red
            );
        }
        let YCbCr { y, cb, cr } = rgb_to_ycbcr_rec709_limited(255, 0, 0);

        // Y plane: rows 4..10, cols 10..18 should be red.Y.
        for row in 4..10 {
            for col in 10..18 {
                let v = yp[row * 64 + col];
                assert_eq!(v, y, "y@({},{})", col, row);
            }
        }
        // Outside the rect the Y plane should still be 16 (black).
        assert_eq!(yp[0], 16);
        assert_eq!(yp[3 * 64 + 10], 16); // row above
        assert_eq!(yp[10 * 64 + 10], 16); // row below

        // UV plane: pixel (10..18, 4..10) maps to chroma (5..9, 2..5).
        // Sample one pair: cx=6, cy=3 → byte 3*64 + 6*2 = 204.
        assert_eq!(uv[204], cb);
        assert_eq!(uv[205], cr);
    }

    #[test]
    fn alpha_zero_is_noop() {
        let mut yp = Vec::new();
        let mut uv = Vec::new();
        black_frame_64x32(&mut yp, &mut uv);
        let before = yp.clone();
        let uv_before = uv.clone();
        {
            let mut frame = view(&mut yp, &mut uv, 64, 32);
            fill_rect_alpha(
                &mut frame,
                Rect {
                    x: 10,
                    y: 10,
                    width: 10,
                    height: 10,
                },
                Rgba::new(255, 0, 0, 0),
            );
        }
        assert_eq!(yp, before);
        assert_eq!(uv, uv_before);
    }

    #[test]
    fn alpha_full_matches_opaque() {
        let mut a_y = Vec::new();
        let mut a_uv = Vec::new();
        let mut b_y = Vec::new();
        let mut b_uv = Vec::new();
        black_frame_64x32(&mut a_y, &mut a_uv);
        black_frame_64x32(&mut b_y, &mut b_uv);
        let rect = Rect {
            x: 4,
            y: 2,
            width: 10,
            height: 6,
        };
        {
            let mut frame_a = view(&mut a_y, &mut a_uv, 64, 32);
            fill_rect_opaque(&mut frame_a, rect, Rgba::opaque(0, 255, 0));
        }
        {
            let mut frame_b = view(&mut b_y, &mut b_uv, 64, 32);
            fill_rect_alpha(&mut frame_b, rect, Rgba::new(0, 255, 0, 255));
        }
        assert_eq!(a_y, b_y);
        assert_eq!(a_uv, b_uv);
    }

    #[test]
    fn alpha_half_is_midpoint() {
        // Y of pure green is ~173 (Rec.709 limited). Black background
        // is Y=16. 50% blend should land near (173 + 16) / 2 ≈ 95.
        let mut yp = Vec::new();
        let mut uv = Vec::new();
        black_frame_64x32(&mut yp, &mut uv);
        {
            let mut frame = view(&mut yp, &mut uv, 64, 32);
            fill_rect_alpha(
                &mut frame,
                Rect {
                    x: 4,
                    y: 4,
                    width: 4,
                    height: 4,
                },
                Rgba::new(0, 255, 0, 128),
            );
        }
        let v = yp[4 * 64 + 4];
        assert!((93..=97).contains(&v), "midpoint Y was {v}");
    }

    #[test]
    fn fully_offscreen_is_noop() {
        let mut yp = Vec::new();
        let mut uv = Vec::new();
        black_frame_64x32(&mut yp, &mut uv);
        let before_y = yp.clone();
        let before_uv = uv.clone();
        {
            let mut frame = view(&mut yp, &mut uv, 64, 32);
            fill_rect_opaque(
                &mut frame,
                Rect {
                    x: 200,
                    y: 200,
                    width: 50,
                    height: 50,
                },
                Rgba::opaque(255, 255, 255),
            );
            fill_rect_alpha(
                &mut frame,
                Rect {
                    x: -100,
                    y: -100,
                    width: 50,
                    height: 50,
                },
                Rgba::new(255, 255, 255, 200),
            );
        }
        assert_eq!(yp, before_y);
        assert_eq!(uv, before_uv);
    }

    #[test]
    fn partially_offscreen_writes_only_visible_part() {
        let mut yp = Vec::new();
        let mut uv = Vec::new();
        black_frame_64x32(&mut yp, &mut uv);
        // Half off the left edge.
        {
            let mut frame = view(&mut yp, &mut uv, 64, 32);
            fill_rect_opaque(
                &mut frame,
                Rect {
                    x: -10,
                    y: -5,
                    width: 20,
                    height: 10,
                },
                Rgba::opaque(255, 255, 255),
            );
        }
        // (0..10, 0..5) should be the white luma; pixel (10, 0) is
        // just outside.
        let YCbCr { y: y_white, .. } = rgb_to_ycbcr_rec709_limited(255, 255, 255);
        for row in 0..5 {
            for col in 0..10 {
                assert_eq!(
                    yp[row * 64 + col],
                    y_white,
                    "expected white at ({},{})",
                    col,
                    row
                );
            }
        }
        // Just outside the clipped rect: row=5 (below) or col=10 (right).
        assert_eq!(yp[5 * 64], 16, "row below clipped rect must stay black");
        assert_eq!(yp[10], 16, "col right of clipped rect must stay black");
    }

    #[test]
    fn padded_stride_does_not_leak_outside_visible_pixels() {
        // 64×32 visible pixels but with padded stride 80 on both planes.
        let mut yp = vec![16; 80 * 32];
        let mut uv = vec![128; 80 * 16];
        let frame = Nv12FrameMut::new(&mut yp, &mut uv, 80, 80, 64, 32);
        assert!(frame.is_some());
        let mut frame = frame.unwrap();
        fill_rect_opaque(
            &mut frame,
            Rect {
                x: 0,
                y: 0,
                width: 64,
                height: 32,
            },
            Rgba::opaque(255, 255, 255),
        );
        // Pixel (64, 0) is *outside* the declared width - even though
        // the Y plane has room for it. Must remain Y=16.
        for row in 0..32 {
            assert_eq!(yp[row * 80 + 64], 16, "padding leaked at row {}", row);
            assert_eq!(
                yp[row * 80 + 79],
                16,
                "padding leaked at row {} col 79",
                row
            );
        }
    }
}
