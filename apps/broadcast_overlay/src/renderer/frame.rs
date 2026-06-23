// Borrowed NV12 frame view plus rectangle clipping against frame bounds.
// Author: Thomas Klute

//! NV12 frame view + rectangle clipping helpers.
//!
//! NV12 is YUV 4:2:0 with two planes:
//!   - Y plane: `height × y_stride` bytes (one byte per pixel).
//!   - UV plane: `(height / 2) × uv_stride` bytes, with U and V
//!     samples *interleaved* (`U V U V U V …`). Each `(U, V)` pair
//!     covers a 2×2 block of Y pixels, i.e. chroma is half resolution
//!     in both axes.
//!
//! GStreamer's `gst_video_frame_map` can hand us a buffer where
//! `y_stride > width` and `uv_stride > width`. Every plane access in
//! this module must go through the stride, never raw `width`.

/// Mutable view into an NV12 buffer.
///
/// The two slice references are split out by the caller (typically
/// from `gst_video::VideoFrame::data_mut`). Width and height are in
/// pixels; both strides are in bytes.
///
/// Invariants enforced at construction:
/// - `width > 0`, `height > 0`
/// - `width % 2 == 0`, `height % 2 == 0` (NV12 chroma sub-sampling)
/// - `y_stride >= width as usize`
/// - `uv_stride >= width as usize` (one UV pair per 2 Y columns,
///   so the UV plane needs at least `width` bytes per row)
/// - `y_plane.len() >= height * y_stride`
/// - `uv_plane.len() >= (height / 2) * uv_stride`
pub struct Nv12FrameMut<'a> {
    pub y_plane: &'a mut [u8],
    pub uv_plane: &'a mut [u8],
    pub y_stride: usize,
    pub uv_stride: usize,
    pub width: u32,
    pub height: u32,
}

impl<'a> Nv12FrameMut<'a> {
    /// Construct a frame view, checking the invariants. Returns
    /// `None` if any invariant is violated; the renderer treats this
    /// as a "skip the frame" condition rather than crashing the
    /// pipeline.
    pub fn new(
        y_plane: &'a mut [u8],
        uv_plane: &'a mut [u8],
        y_stride: usize,
        uv_stride: usize,
        width: u32,
        height: u32,
    ) -> Option<Self> {
        if width == 0 || height == 0 {
            return None;
        }
        if !width.is_multiple_of(2) || !height.is_multiple_of(2) {
            return None;
        }
        if y_stride < width as usize || uv_stride < width as usize {
            return None;
        }
        if y_plane.len() < height as usize * y_stride {
            return None;
        }
        if uv_plane.len() < (height as usize / 2) * uv_stride {
            return None;
        }
        Some(Self {
            y_plane,
            uv_plane,
            y_stride,
            uv_stride,
            width,
            height,
        })
    }
}

/// Unclipped rectangle in pixel coordinates. May extend off-screen on
/// any side; the renderer clips against the frame before writing.
#[derive(Debug, Clone, Copy)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

/// On-frame rectangle, guaranteed `x + width ≤ frame_width` and
/// `y + height ≤ frame_height`. All four fields are `u32`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClippedRect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

impl Rect {
    /// Intersect with the frame bounds. Returns `None` if the
    /// rectangle is fully off-screen or has zero area after
    /// clipping. Negative `width`/`height` collapse to zero (the
    /// pixel-command-spec allows negative coordinates, not negative
    /// dimensions, but we defend against both).
    pub fn clip_to_frame(&self, frame_width: u32, frame_height: u32) -> Option<ClippedRect> {
        if self.width <= 0 || self.height <= 0 {
            return None;
        }
        // Compute the original right/bottom edges in i64 to avoid the
        // signed-overflow case where x or y is near i32::MIN.
        let left = i64::from(self.x);
        let top = i64::from(self.y);
        let right = left + i64::from(self.width);
        let bottom = top + i64::from(self.height);

        let frame_w = i64::from(frame_width);
        let frame_h = i64::from(frame_height);

        let clipped_left = left.max(0);
        let clipped_top = top.max(0);
        let clipped_right = right.min(frame_w);
        let clipped_bottom = bottom.min(frame_h);

        if clipped_right <= clipped_left || clipped_bottom <= clipped_top {
            return None;
        }

        Some(ClippedRect {
            x: clipped_left as u32,
            y: clipped_top as u32,
            width: (clipped_right - clipped_left) as u32,
            height: (clipped_bottom - clipped_top) as u32,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dummy_y(stride: usize, height: usize) -> Vec<u8> {
        vec![0; stride * height]
    }
    fn dummy_uv(stride: usize, height: usize) -> Vec<u8> {
        vec![0; stride * (height / 2)]
    }

    #[test]
    fn frame_construction_accepts_tight_stride() {
        let mut y = dummy_y(1920, 1080);
        let mut uv = dummy_uv(1920, 1080);
        assert!(Nv12FrameMut::new(&mut y, &mut uv, 1920, 1920, 1920, 1080).is_some());
    }

    #[test]
    fn frame_construction_accepts_padded_stride() {
        let mut y = dummy_y(2048, 1080);
        let mut uv = dummy_uv(2048, 1080);
        assert!(Nv12FrameMut::new(&mut y, &mut uv, 2048, 2048, 1920, 1080).is_some());
    }

    #[test]
    fn frame_construction_rejects_zero_dim() {
        let mut y = vec![0; 0];
        let mut uv = vec![0; 0];
        assert!(Nv12FrameMut::new(&mut y, &mut uv, 0, 0, 0, 0).is_none());
    }

    #[test]
    fn frame_construction_rejects_odd_dim() {
        // Odd width breaks NV12 chroma sub-sampling.
        let mut y = dummy_y(1921, 1080);
        let mut uv = dummy_uv(1921, 1080);
        assert!(Nv12FrameMut::new(&mut y, &mut uv, 1921, 1921, 1921, 1080).is_none());
    }

    #[test]
    fn frame_construction_rejects_short_planes() {
        // Y plane too short for the declared height.
        let mut y = vec![0; 1920 * 100];
        let mut uv = dummy_uv(1920, 1080);
        assert!(Nv12FrameMut::new(&mut y, &mut uv, 1920, 1920, 1920, 1080).is_none());
    }

    #[test]
    fn clip_fully_inside_passes_through() {
        let r = Rect {
            x: 10,
            y: 20,
            width: 100,
            height: 80,
        };
        assert_eq!(
            r.clip_to_frame(1920, 1080),
            Some(ClippedRect {
                x: 10,
                y: 20,
                width: 100,
                height: 80,
            })
        );
    }

    #[test]
    fn clip_fully_offscreen_returns_none() {
        // Right of frame
        assert!(Rect {
            x: 2000,
            y: 0,
            width: 50,
            height: 50
        }
        .clip_to_frame(1920, 1080)
        .is_none());
        // Below frame
        assert!(Rect {
            x: 0,
            y: 1200,
            width: 50,
            height: 50
        }
        .clip_to_frame(1920, 1080)
        .is_none());
        // Left of frame
        assert!(Rect {
            x: -100,
            y: 0,
            width: 50,
            height: 50
        }
        .clip_to_frame(1920, 1080)
        .is_none());
        // Above frame
        assert!(Rect {
            x: 0,
            y: -100,
            width: 50,
            height: 50
        }
        .clip_to_frame(1920, 1080)
        .is_none());
    }

    #[test]
    fn clip_partially_offscreen_top_left() {
        // Half off the top-left corner.
        let r = Rect {
            x: -10,
            y: -20,
            width: 100,
            height: 80,
        };
        assert_eq!(
            r.clip_to_frame(1920, 1080),
            Some(ClippedRect {
                x: 0,
                y: 0,
                width: 90,
                height: 60,
            })
        );
    }

    #[test]
    fn clip_partially_offscreen_bottom_right() {
        // Half off the bottom-right corner.
        let r = Rect {
            x: 1880,
            y: 1040,
            width: 100,
            height: 80,
        };
        assert_eq!(
            r.clip_to_frame(1920, 1080),
            Some(ClippedRect {
                x: 1880,
                y: 1040,
                width: 40,
                height: 40,
            })
        );
    }

    #[test]
    fn clip_zero_or_negative_dimensions_return_none() {
        assert!(Rect {
            x: 0,
            y: 0,
            width: 0,
            height: 50
        }
        .clip_to_frame(1920, 1080)
        .is_none());
        assert!(Rect {
            x: 0,
            y: 0,
            width: 50,
            height: 0
        }
        .clip_to_frame(1920, 1080)
        .is_none());
        assert!(Rect {
            x: 0,
            y: 0,
            width: -10,
            height: 50
        }
        .clip_to_frame(1920, 1080)
        .is_none());
    }

    #[test]
    fn clip_extreme_coordinates_dont_overflow() {
        // i32::MIN as x would overflow naïve `x + width` arithmetic.
        let r = Rect {
            x: i32::MIN,
            y: 0,
            width: 100,
            height: 50,
        };
        assert!(r.clip_to_frame(1920, 1080).is_none());
        // i32::MAX similar.
        let r = Rect {
            x: i32::MAX - 10,
            y: 0,
            width: 100,
            height: 50,
        };
        assert!(r.clip_to_frame(1920, 1080).is_none());
    }
}
