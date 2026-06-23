// FreeType-backed glyph cache: rasterize once per (font, size, style, char), reuse each frame.
// Author: Thomas Klute

//! FreeType-backed glyph cache.
//!
//! Cache key: `(FontId, size_px, FontStyle, char)`. Cached value:
//! [`GlyphBitmap`] (8-bit alpha mask + bearing/advance metrics).
//!
//! Coverage strategy:
//!  - [`GlyphCache::preload_ascii`] rasterizes the printable ASCII
//!    range `0x20`-`0x7E` (95 codepoints) at startup for a given
//!    `(font_id, size_px, style)`.
//!  - [`GlyphCache::get_or_rasterize`] lazily rasterizes anything
//!    else on first use (e.g. `ô` in `config/teams.json`).
//!  - Unknown codepoints (FreeType reports no glyph for them) fall
//!    back to the font's `.notdef` glyph; we log one warning per
//!    codepoint per process via the `notdef_seen` set.
//!
//! Threading: the cache is single-owner. The GStreamer element owns
//! the cache and calls it from its own task; concurrent access from a
//! state-update path is handled via swap-style command-list handoff,
//! not by sharing the cache.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use anyhow::{anyhow, Context, Result};
use tracing::warn;

/// Opaque font identifier defined by the caller. Pair it with
/// [`FontStyle`] to address a specific font face the cache loaded
/// via [`GlyphCache::register_font`].
#[derive(Debug, Clone, Copy, Hash, Eq, PartialEq, Ord, PartialOrd)]
pub struct FontId(pub u32);

/// Font face style. The cache holds an independent FreeType face per
/// `(FontId, FontStyle)` pair - bold means the bold *file*, not a
/// synthetic stroke transform.
#[derive(Debug, Clone, Copy, Hash, Eq, PartialEq)]
pub enum FontStyle {
    Regular,
    Bold,
}

/// Cache key for a single rasterized glyph.
#[derive(Debug, Clone, Copy, Hash, Eq, PartialEq)]
pub struct GlyphKey {
    pub font_id: FontId,
    pub size_px: u32,
    pub style: FontStyle,
    pub codepoint: char,
}

/// Rasterized glyph: 8-bit alpha mask + FreeType bearing / advance
/// metrics. All distance fields are in pixels (the cache rasterizes
/// at integer pixel sizes).
#[derive(Debug, Clone)]
pub struct GlyphBitmap {
    /// Bitmap width in pixels.
    pub width: u32,
    /// Bitmap height in pixels.
    pub height: u32,
    /// Horizontal offset from the pen position to the left edge of
    /// the bitmap. Positive = right.
    pub bearing_x: i32,
    /// Vertical offset from the baseline to the **top** edge of the
    /// bitmap. Positive = above the baseline (FreeType convention).
    pub bearing_y: i32,
    /// Pen advance after this glyph, in pixels.
    pub advance_x: i32,
    /// Row-major `width * height` 8-bit coverage. 0 = fully
    /// transparent, 255 = fully opaque. Empty for whitespace glyphs
    /// like `' '`.
    pub alpha: Vec<u8>,
}

impl GlyphBitmap {
    /// `true` if this glyph has no pixel coverage (e.g. the space
    /// character). The blit fast-path treats this as advance-only.
    pub fn is_empty(&self) -> bool {
        self.width == 0 || self.height == 0 || self.alpha.is_empty()
    }
}

/// FreeType-backed glyph cache. Single-owner; not `Send` because the
/// underlying `freetype::Library` handle isn't.
pub struct GlyphCache {
    library: freetype::Library,
    /// One face per (FontId, FontStyle). Sized on demand via
    /// `set_pixel_sizes` before rasterization.
    faces: HashMap<(FontId, FontStyle), freetype::Face>,
    cache: HashMap<GlyphKey, GlyphBitmap>,
    /// Per-process log-once set so unknown codepoints don't spam the
    /// streaming thread.
    notdef_seen: HashSet<char>,
}

impl GlyphCache {
    /// Construct an empty cache. Loads no fonts - call
    /// [`register_font`] for each `(FontId, FontStyle)` you intend to
    /// draw with.
    pub fn new() -> Result<Self> {
        let library = freetype::Library::init().context("freetype: Library::init failed")?;
        Ok(Self {
            library,
            faces: HashMap::new(),
            cache: HashMap::new(),
            notdef_seen: HashSet::new(),
        })
    }

    /// Register a font file (TTF/OTF) for the given `(FontId,
    /// FontStyle)`. Loads face 0 from the file. Re-registering the
    /// same key replaces the previous face and invalidates any
    /// already-cached glyphs for that key.
    pub fn register_font(&mut self, font_id: FontId, style: FontStyle, path: &Path) -> Result<()> {
        let face = self
            .library
            .new_face(path, 0)
            .with_context(|| format!("freetype: new_face({})", path.display()))?;
        // Invalidate stale glyphs for this (font, style) before
        // swapping the face in.
        self.cache
            .retain(|k, _| !(k.font_id == font_id && k.style == style));
        self.faces.insert((font_id, style), face);
        Ok(())
    }

    /// `true` if a face has been registered for this `(font_id,
    /// style)` pair.
    pub fn has_font(&self, font_id: FontId, style: FontStyle) -> bool {
        self.faces.contains_key(&(font_id, style))
    }

    /// Pre-rasterize the printable-ASCII range (`0x20`-`0x7E`, 95
    /// chars) for `(font_id, size_px, style)`. Called once at element
    /// startup for every `(font, size, style)` the layout uses, so
    /// the streaming hot path never hits FreeType for clocks, scores,
    /// or score names rendered in ASCII.
    ///
    /// Returns the number of glyphs successfully rasterized.
    pub fn preload_ascii(
        &mut self,
        font_id: FontId,
        size_px: u32,
        style: FontStyle,
    ) -> Result<usize> {
        if !self.has_font(font_id, style) {
            return Err(anyhow!(
                "preload_ascii: no font registered for FontId({}) {:?}",
                font_id.0,
                style
            ));
        }
        let mut n = 0;
        for code in 0x20u32..=0x7Eu32 {
            // Every `u32` in that range is a valid `char` (ASCII).
            let ch = char::from_u32(code).expect("printable-ASCII range is all valid chars");
            if self
                .get_or_rasterize(GlyphKey {
                    font_id,
                    size_px,
                    style,
                    codepoint: ch,
                })
                .is_some()
            {
                n += 1;
            }
        }
        Ok(n)
    }

    /// Look up a glyph, rasterizing if needed. Returns `None` only if
    /// the `(font_id, style)` pair isn't registered.
    ///
    /// For codepoints the font doesn't carry, returns the cached
    /// `.notdef` (tofu) glyph and logs a warning once per codepoint
    /// per process.
    pub fn get_or_rasterize(&mut self, key: GlyphKey) -> Option<&GlyphBitmap> {
        if self.cache.contains_key(&key) {
            return self.cache.get(&key);
        }
        let face = self.faces.get(&(key.font_id, key.style))?;

        // FreeType wants the size in *26.6 fixed-point pixels*; the
        // ergonomic helper `set_pixel_sizes(0, size_px)` does the
        // conversion.
        if let Err(e) = face.set_pixel_sizes(0, key.size_px) {
            warn!(
                error = ?e,
                font = key.font_id.0,
                size = key.size_px,
                "freetype: set_pixel_sizes failed"
            );
            return None;
        }

        // `Default` = LOAD_RENDER, which produces an 8-bit grayscale
        // alpha bitmap. No hinting tweaks for v1 - the FreeType
        // defaults give a reasonable result at scoreboard sizes.
        let glyph_index = face.get_char_index(key.codepoint as usize).unwrap_or(0);
        if glyph_index == 0 && !self.notdef_seen.insert(key.codepoint) {
            // First miss for this codepoint - also load the .notdef
            // (which is what `get_char_index == 0` maps to) and
            // proceed below. The `insert` returned `false` here means
            // we'd already logged, so silently fall through.
        } else if glyph_index == 0 {
            warn!(
                codepoint = key.codepoint as u32,
                font = key.font_id.0,
                style = ?key.style,
                "glyph cache: no glyph for codepoint, falling back to .notdef"
            );
        }

        if let Err(e) = face.load_char(key.codepoint as usize, freetype::face::LoadFlag::RENDER) {
            warn!(
                error = ?e,
                codepoint = key.codepoint as u32,
                "freetype: load_char failed"
            );
            return None;
        }

        let glyph = face.glyph();
        let bitmap = glyph.bitmap();
        let metrics = glyph;

        // FreeType returns advance in 26.6 fixed-point; >> 6 gives
        // integer pixels (good enough for v1 - scoreboard text is
        // pixel-snapped anyway).
        let advance_x = (metrics.advance().x >> 6) as i32;
        let bearing_x = metrics.bitmap_left();
        let bearing_y = metrics.bitmap_top();

        let w = bitmap.width().max(0) as u32;
        let h = bitmap.rows().max(0) as u32;
        let pitch = bitmap.pitch();
        let buf = bitmap.buffer();

        // The bitmap can come back with a negative pitch (rows are
        // stored bottom-up). For glyphs FreeType nearly always uses
        // a positive pitch in 8-bit mode, but defend against the
        // signed case to be safe.
        let mut alpha = Vec::with_capacity((w * h) as usize);
        if pitch >= 0 {
            let pitch = pitch as usize;
            for row in 0..h as usize {
                let off = row * pitch;
                alpha.extend_from_slice(&buf[off..off + w as usize]);
            }
        } else {
            let pitch = (-pitch) as usize;
            for row in (0..h as usize).rev() {
                let off = row * pitch;
                alpha.extend_from_slice(&buf[off..off + w as usize]);
            }
        }

        let bitmap_owned = GlyphBitmap {
            width: w,
            height: h,
            bearing_x,
            bearing_y,
            advance_x,
            alpha,
        };
        self.cache.insert(key, bitmap_owned);
        self.cache.get(&key)
    }

    /// Number of cached glyphs across all keys. Diagnostics only.
    pub fn len(&self) -> usize {
        self.cache.len()
    }

    /// `true` if the cache holds no glyphs.
    pub fn is_empty(&self) -> bool {
        self.cache.is_empty()
    }
}

/// Lay text out left-to-right at integer pixel positions and return
/// the on-screen placement of each glyph relative to `(baseline_x,
/// baseline_y)`.
///
/// Whitespace glyphs (empty `alpha`) are skipped from the returned
/// list - the caller doesn't need to draw them - but their advance
/// still contributes to the pen position so the following glyphs
/// land at the correct x.
///
/// Returns `None` if the requested font isn't registered for the
/// `(font_id, style)` pair. Returns an empty vec for empty input.
pub fn layout_left_to_right(
    cache: &mut GlyphCache,
    text: &str,
    font_id: FontId,
    size_px: u32,
    style: FontStyle,
    baseline_x: i32,
    baseline_y: i32,
) -> Option<Vec<Placement>> {
    if !cache.has_font(font_id, style) {
        return None;
    }
    let mut placements = Vec::with_capacity(text.len());
    let mut pen_x = baseline_x;
    for ch in text.chars() {
        let key = GlyphKey {
            font_id,
            size_px,
            style,
            codepoint: ch,
        };
        let Some(g) = cache.get_or_rasterize(key) else {
            continue;
        };
        let advance = g.advance_x;
        if !g.is_empty() {
            placements.push(Placement {
                key,
                x: pen_x + g.bearing_x,
                y: baseline_y - g.bearing_y,
            });
        }
        pen_x += advance;
    }
    Some(placements)
}

/// One laid-out glyph: the cache key to fetch the bitmap from, plus
/// the top-left frame coordinates the bitmap should blit to.
#[derive(Debug, Clone, Copy)]
pub struct Placement {
    pub key: GlyphKey,
    pub x: i32,
    pub y: i32,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// Return a TTF path that exists on this host, or `None` if none
    /// of the well-known font paths are present.
    fn probe_dejavu_regular() -> Option<PathBuf> {
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

    fn probe_dejavu_bold() -> Option<PathBuf> {
        let candidates = [
            "/usr/share/fonts/truetype/dejavu/DejaVuSans-Bold.ttf",
            "/usr/share/fonts/TTF/DejaVuSans-Bold.ttf",
            "/usr/share/fonts/dejavu/DejaVuSans-Bold.ttf",
            "/usr/share/fonts/truetype/liberation/LiberationSans-Bold.ttf",
        ];
        for c in candidates {
            let p = PathBuf::from(c);
            if p.exists() {
                return Some(p);
            }
        }
        None
    }

    const TEST_FONT: FontId = FontId(0);

    #[test]
    fn cache_with_no_font_returns_none_for_get() {
        let mut cache = GlyphCache::new().unwrap();
        assert!(cache
            .get_or_rasterize(GlyphKey {
                font_id: TEST_FONT,
                size_px: 16,
                style: FontStyle::Regular,
                codepoint: 'A',
            })
            .is_none());
        assert!(cache.is_empty());
    }

    #[test]
    fn cache_preload_ascii_fills_95_glyphs() {
        let Some(font) = probe_dejavu_regular() else {
            eprintln!("[skip] no DejaVu/Liberation regular font found on this host");
            return;
        };
        let mut cache = GlyphCache::new().unwrap();
        cache
            .register_font(TEST_FONT, FontStyle::Regular, &font)
            .unwrap();
        let n = cache
            .preload_ascii(TEST_FONT, 16, FontStyle::Regular)
            .unwrap();
        assert_eq!(n, 95, "expected printable-ASCII range = 95 glyphs");
        assert_eq!(cache.len(), 95);
    }

    #[test]
    fn cache_get_returns_cached_glyph_after_preload() {
        let Some(font) = probe_dejavu_regular() else {
            eprintln!("[skip] no font found");
            return;
        };
        let mut cache = GlyphCache::new().unwrap();
        cache
            .register_font(TEST_FONT, FontStyle::Regular, &font)
            .unwrap();
        cache
            .preload_ascii(TEST_FONT, 24, FontStyle::Regular)
            .unwrap();
        let len_before = cache.len();
        let g = cache
            .get_or_rasterize(GlyphKey {
                font_id: TEST_FONT,
                size_px: 24,
                style: FontStyle::Regular,
                codepoint: '5',
            })
            .expect("'5' must be in the printable-ASCII range");
        assert!(g.advance_x > 0, "digit 5 must have positive advance");
        assert!(!g.is_empty(), "'5' must produce a non-empty bitmap");
        // No new rasterization happened.
        assert_eq!(cache.len(), len_before);
    }

    #[test]
    fn cache_lazy_rasterizes_non_ascii() {
        let Some(font) = probe_dejavu_regular() else {
            eprintln!("[skip] no font found");
            return;
        };
        let mut cache = GlyphCache::new().unwrap();
        cache
            .register_font(TEST_FONT, FontStyle::Regular, &font)
            .unwrap();
        // ô is the one non-ASCII char in config/teams.json today.
        let key = GlyphKey {
            font_id: TEST_FONT,
            size_px: 20,
            style: FontStyle::Regular,
            codepoint: 'ô',
        };
        let g = cache
            .get_or_rasterize(key)
            .expect("DejaVu/Liberation carry ô");
        assert!(!g.is_empty());
        // Same key on second call returns the cached glyph (size stays at 1).
        assert_eq!(cache.len(), 1);
        let _ = cache.get_or_rasterize(key);
        assert_eq!(cache.len(), 1);
    }

    #[test]
    fn cache_space_glyph_has_advance_but_empty_bitmap() {
        let Some(font) = probe_dejavu_regular() else {
            eprintln!("[skip] no font found");
            return;
        };
        let mut cache = GlyphCache::new().unwrap();
        cache
            .register_font(TEST_FONT, FontStyle::Regular, &font)
            .unwrap();
        let g = cache
            .get_or_rasterize(GlyphKey {
                font_id: TEST_FONT,
                size_px: 16,
                style: FontStyle::Regular,
                codepoint: ' ',
            })
            .unwrap();
        assert!(g.advance_x > 0, "space must advance the pen");
        assert!(g.is_empty(), "space must produce no pixels");
    }

    #[test]
    fn cache_distinct_sizes_dont_clobber_each_other() {
        let Some(font) = probe_dejavu_regular() else {
            eprintln!("[skip] no font found");
            return;
        };
        let mut cache = GlyphCache::new().unwrap();
        cache
            .register_font(TEST_FONT, FontStyle::Regular, &font)
            .unwrap();
        let g16 = cache
            .get_or_rasterize(GlyphKey {
                font_id: TEST_FONT,
                size_px: 16,
                style: FontStyle::Regular,
                codepoint: '0',
            })
            .unwrap()
            .clone();
        let g32 = cache
            .get_or_rasterize(GlyphKey {
                font_id: TEST_FONT,
                size_px: 32,
                style: FontStyle::Regular,
                codepoint: '0',
            })
            .unwrap()
            .clone();
        assert!(g32.advance_x > g16.advance_x, "32px '0' must be wider");
        assert_eq!(cache.len(), 2);
    }

    #[test]
    fn cache_distinct_styles_load_distinct_faces() {
        let (Some(regular), Some(bold)) = (probe_dejavu_regular(), probe_dejavu_bold()) else {
            eprintln!("[skip] need both regular and bold faces");
            return;
        };
        let mut cache = GlyphCache::new().unwrap();
        cache
            .register_font(TEST_FONT, FontStyle::Regular, &regular)
            .unwrap();
        cache
            .register_font(TEST_FONT, FontStyle::Bold, &bold)
            .unwrap();
        let r = cache
            .get_or_rasterize(GlyphKey {
                font_id: TEST_FONT,
                size_px: 24,
                style: FontStyle::Regular,
                codepoint: 'M',
            })
            .unwrap()
            .clone();
        let b = cache
            .get_or_rasterize(GlyphKey {
                font_id: TEST_FONT,
                size_px: 24,
                style: FontStyle::Bold,
                codepoint: 'M',
            })
            .unwrap()
            .clone();
        // Bold 'M' is virtually always at least as wide as regular at
        // the same size. Use a soft assertion rather than strict
        // inequality in case the font's bold weight is unusual.
        assert!(
            b.advance_x >= r.advance_x,
            "bold M advance ({}) should be ≥ regular ({})",
            b.advance_x,
            r.advance_x
        );
    }

    #[test]
    fn layout_text_pen_advances_through_string() {
        let Some(font) = probe_dejavu_regular() else {
            eprintln!("[skip] no font found");
            return;
        };
        let mut cache = GlyphCache::new().unwrap();
        cache
            .register_font(TEST_FONT, FontStyle::Regular, &font)
            .unwrap();
        let placements = layout_left_to_right(
            &mut cache,
            "10:49",
            TEST_FONT,
            24,
            FontStyle::Regular,
            100,
            200,
        )
        .expect("font registered");
        // 5 chars, all non-empty in DejaVu, so 5 placements.
        assert_eq!(placements.len(), 5);
        // Strictly increasing x (allowing for kern-style overlap with
        // wide bearings, but for digits it's safely increasing).
        for w in placements.windows(2) {
            assert!(
                w[1].x > w[0].x,
                "placement x must advance: {:?} → {:?}",
                w[0],
                w[1]
            );
        }
    }

    #[test]
    fn layout_text_skips_whitespace_placement_but_advances_pen() {
        let Some(font) = probe_dejavu_regular() else {
            eprintln!("[skip] no font found");
            return;
        };
        let mut cache = GlyphCache::new().unwrap();
        cache
            .register_font(TEST_FONT, FontStyle::Regular, &font)
            .unwrap();
        let no_space =
            layout_left_to_right(&mut cache, "ABC", TEST_FONT, 20, FontStyle::Regular, 0, 0)
                .unwrap();
        let with_space =
            layout_left_to_right(&mut cache, "A B C", TEST_FONT, 20, FontStyle::Regular, 0, 0)
                .unwrap();
        // Same number of *visible* placements.
        assert_eq!(no_space.len(), 3);
        assert_eq!(with_space.len(), 3);
        // But the trailing C is pushed further right in the spaced
        // version.
        assert!(
            with_space[2].x > no_space[2].x,
            "spaced 'C' must land further right"
        );
    }

    #[test]
    fn layout_text_with_no_font_returns_none() {
        let mut cache = GlyphCache::new().unwrap();
        assert!(
            layout_left_to_right(&mut cache, "hi", TEST_FONT, 16, FontStyle::Regular, 0, 0)
                .is_none()
        );
    }
}
