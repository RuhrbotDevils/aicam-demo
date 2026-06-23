// RGBA to limited-range Rec. 709 YUV conversion and alpha-blend helpers.
// Author: Thomas Klute

//! RGBA -> YUV conversion + chroma helpers.
//!
//! Commits to **Rec. 709 limited range** (Y in `[16, 235]`, Cb/Cr in
//! `[16, 240]`). This is what the production camera path emits:
//! `nvarguscamerasrc` on Jetson defaults to Rec. 709 sYCC and
//! `libcamerasrc` on Pi 5 picks `Rec709` for the 1920×1080 NV12 case
//! (visible in the GStreamer caps trace: `1920x1080-NV12/Rec709`).
//! Drawing in any other space would tint the overlay.
//!
//! The conversion uses linear RGB -> YCbCr without an explicit sRGB
//! gamma decode step. For overlay rectangles and antialiased text on
//! contrasting backgrounds the visible difference is negligible
//! against the gamma overhead - same shortcut the existing
//! `cairooverlay` path takes (Cairo writes BGRx in display space and
//! the downstream `videoconvert` performs the matching shortcut).
//!
//! Implementation: fixed-point integer math with coefficients scaled
//! by 1024 (`>> 10` at the end). Avoids floating point on the hot
//! path and matches what an ARM NEON vectorisation would later do.

/// 32-bit RGBA color. Alpha is straight (non-premultiplied).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Rgba {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
}

impl Rgba {
    pub const fn new(r: u8, g: u8, b: u8, a: u8) -> Self {
        Self { r, g, b, a }
    }

    pub const fn opaque(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b, a: 255 }
    }

    /// Return black or white opaque, whichever gives better contrast
    /// against `self` as a background. Uses ITU-R BT.601 perceptual
    /// luminance with a 0.6 threshold - leans toward white text so the
    /// standard broadcast "white on dark jersey" look (red / blue /
    /// dark green) is preserved, flipping to black only on genuinely
    /// bright backgrounds (white, yellow, cyan).
    pub fn contrasting_text(self) -> Self {
        let r = self.r as f32 / 255.0;
        let g = self.g as f32 / 255.0;
        let b = self.b as f32 / 255.0;
        let l = 0.299 * r + 0.587 * g + 0.114 * b;
        if l > 0.6 {
            Self::opaque(0, 0, 0)
        } else {
            Self::opaque(255, 255, 255)
        }
    }
}

/// `(Y, Cb, Cr)` in limited-range Rec. 709 (Y ∈ [16, 235],
/// Cb/Cr ∈ [16, 240]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct YCbCr {
    pub y: u8,
    pub cb: u8,
    pub cr: u8,
}

/// Convert an sRGB triple to limited-range Rec. 709 `(Y, Cb, Cr)`.
///
/// Fixed-point coefficients (×1024) derived from:
/// `Y  = 16  + 219 / 255 * (0.2126 R + 0.7152 G + 0.0722 B)`
/// `Cb = 128 + 224 / 255 * (-0.1146 R - 0.3854 G + 0.5     B)`
/// `Cr = 128 + 224 / 255 * ( 0.5    R - 0.4542 G - 0.0458  B)`
///
/// Coefficients ×1024 (rounded to nearest integer):
/// `Y : 187, 629, 64;   +16384 (= 16 << 10)`
/// `Cb: -103, -347, 450; +131072 (= 128 << 10)`
/// `Cr: 450, -409, -41;  +131072`
///
/// Sanity-checked in the `tests` module against pure primaries.
#[inline]
pub fn rgb_to_ycbcr_rec709_limited(r: u8, g: u8, b: u8) -> YCbCr {
    let r = i32::from(r);
    let g = i32::from(g);
    let b = i32::from(b);

    let y = (187 * r + 629 * g + 64 * b + 16384) >> 10;
    let cb = (-103 * r - 347 * g + 450 * b + 131072) >> 10;
    let cr = (450 * r - 409 * g - 41 * b + 131072) >> 10;

    YCbCr {
        y: y.clamp(0, 255) as u8,
        cb: cb.clamp(0, 255) as u8,
        cr: cr.clamp(0, 255) as u8,
    }
}

impl From<Rgba> for YCbCr {
    fn from(c: Rgba) -> Self {
        rgb_to_ycbcr_rec709_limited(c.r, c.g, c.b)
    }
}

/// Straight-alpha blend: `dst = dst * (255 - a) + src * a`, then
/// divided by 255 with rounding.
///
/// Uses the classic exact reciprocal `(x + 1 + (x >> 8)) >> 8 ≈ x / 255`
/// - valid for `x ∈ [0, 65535]` and exact on the corner cases
/// `a = 0` (dst passthrough) and `a = 255` (src passthrough), unlike
/// the cheaper `(x * 257 + 32768) >> 16` shortcut which drifts by 1
/// near 65025. `acc` here maxes out at `255 * 255 = 65025`.
#[inline]
pub fn blend_u8(dst: u8, src: u8, alpha: u8) -> u8 {
    let inv = 255 - u32::from(alpha);
    let acc = u32::from(dst) * inv + u32::from(src) * u32::from(alpha);
    ((acc + 1 + (acc >> 8)) >> 8) as u8
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pure_white_maps_to_limited_range_white() {
        let YCbCr { y, cb, cr } = rgb_to_ycbcr_rec709_limited(255, 255, 255);
        // Limited-range white = 235; chroma centred at 128.
        assert_eq!(y, 235);
        assert!((cb as i32 - 128).abs() <= 1, "cb={}", cb);
        assert!((cr as i32 - 128).abs() <= 1, "cr={}", cr);
    }

    #[test]
    fn pure_black_maps_to_limited_range_black() {
        let YCbCr { y, cb, cr } = rgb_to_ycbcr_rec709_limited(0, 0, 0);
        assert_eq!(y, 16);
        assert_eq!(cb, 128);
        assert_eq!(cr, 128);
    }

    #[test]
    fn pure_red_pushes_cr_high() {
        let YCbCr { y, cb, cr } = rgb_to_ycbcr_rec709_limited(255, 0, 0);
        // Rec. 709 reference: pure red → Y=63, Cb=102, Cr=240.
        assert!((y as i32 - 63).abs() <= 2, "y={}", y);
        assert!((cb as i32 - 102).abs() <= 2, "cb={}", cb);
        assert!((cr as i32 - 240).abs() <= 2, "cr={}", cr);
    }

    #[test]
    fn pure_green_pushes_y_high_chroma_low() {
        let YCbCr { y, cb, cr } = rgb_to_ycbcr_rec709_limited(0, 255, 0);
        // Rec. 709 reference: pure green → Y=173, Cb=42, Cr=26.
        assert!((y as i32 - 173).abs() <= 2, "y={}", y);
        assert!((cb as i32 - 42).abs() <= 2, "cb={}", cb);
        assert!((cr as i32 - 26).abs() <= 2, "cr={}", cr);
    }

    #[test]
    fn pure_blue_pushes_cb_high() {
        let YCbCr { y, cb, cr } = rgb_to_ycbcr_rec709_limited(0, 0, 255);
        // Rec. 709 reference: pure blue → Y=32, Cb=240, Cr=118.
        assert!((y as i32 - 32).abs() <= 2, "y={}", y);
        assert!((cb as i32 - 240).abs() <= 2, "cb={}", cb);
        assert!((cr as i32 - 118).abs() <= 2, "cr={}", cr);
    }

    #[test]
    fn blend_alpha_zero_returns_dst() {
        assert_eq!(blend_u8(128, 200, 0), 128);
        assert_eq!(blend_u8(0, 255, 0), 0);
        assert_eq!(blend_u8(255, 0, 0), 255);
    }

    #[test]
    fn blend_alpha_full_returns_src() {
        assert_eq!(blend_u8(128, 200, 255), 200);
        assert_eq!(blend_u8(0, 255, 255), 255);
        assert_eq!(blend_u8(255, 0, 255), 0);
    }

    #[test]
    fn blend_half_alpha_is_midpoint() {
        // alpha=128 ≈ 50% → midpoint of dst and src, ±1.
        let mid = blend_u8(0, 200, 128);
        assert!((99..=101).contains(&mid), "mid={mid}");
    }

    // --- contrasting_text ---------------------------------------------

    #[test]
    fn contrasting_text_returns_black_on_bright_backgrounds() {
        // Pure white → black text.
        assert_eq!(
            Rgba::opaque(255, 255, 255).contrasting_text(),
            Rgba::opaque(0, 0, 0)
        );
        // RoboCup team-colour YELLOW (242, 217, 38) → black text.
        assert_eq!(
            Rgba::opaque(242, 217, 38).contrasting_text(),
            Rgba::opaque(0, 0, 0)
        );
        // Pure cyan → black text.
        assert_eq!(
            Rgba::opaque(0, 255, 255).contrasting_text(),
            Rgba::opaque(0, 0, 0)
        );
    }

    #[test]
    fn contrasting_text_returns_white_on_dark_backgrounds() {
        // Pure black → white text.
        assert_eq!(
            Rgba::opaque(0, 0, 0).contrasting_text(),
            Rgba::opaque(255, 255, 255)
        );
        // RoboCup team-colour RED (217, 38, 38) → white text;
        // luminance ≈ 0.28, well below the 0.6 threshold.
        assert_eq!(
            Rgba::opaque(217, 38, 38).contrasting_text(),
            Rgba::opaque(255, 255, 255)
        );
        // RoboCup team-colour BLUE (26, 102, 230) → white text.
        assert_eq!(
            Rgba::opaque(26, 102, 230).contrasting_text(),
            Rgba::opaque(255, 255, 255)
        );
    }
}
