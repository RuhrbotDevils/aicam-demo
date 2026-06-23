// Wall-clock benchmark for glyph-cache text layout and blitting.
// Author: Thomas Klute

//! Wall-clock benchmark for laying out + blitting changing timer
//! strings - the workload the glyph cache was designed for.
//!
//! Verifies the central claim: caching individual glyphs (not full
//! strings) is fast enough that we can render a fresh
//! `HH:MM:SS`-style clock every frame without touching FreeType after
//! the warmup pass.
//!
//! Run with:
//!
//! ```text
//! cargo bench -p aicam_broadcast_overlay --bench text
//! ```
//!
//! Skips silently with a printed message if no system font is found.

use std::path::PathBuf;
use std::time::Instant;

use aicam_broadcast_overlay::glyphs::{
    layout_left_to_right, FontId, FontStyle, GlyphCache, GlyphKey,
};
use aicam_broadcast_overlay::renderer::color::Rgba;
use aicam_broadcast_overlay::renderer::frame::Nv12FrameMut;
use aicam_broadcast_overlay::renderer::glyph_blit::blit_glyph;

const W: u32 = 1920;
const H: u32 = 1080;
const SIZE_PX: u32 = 24;
const ITERATIONS: usize = 1000;
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

fn bench_string(cache: &mut GlyphCache, label: &str, sample: &str) {
    let mut y = vec![16u8; (W as usize) * (H as usize)];
    let mut uv = vec![128u8; (W as usize) * (H as usize) / 2];
    let color = Rgba::opaque(255, 255, 255);

    // Warmup: this pre-rasterizes any glyph in `sample` that wasn't
    // already covered by `preload_ascii`, so the timed loop only
    // exercises lookup + blit.
    {
        let mut frame = Nv12FrameMut::new(&mut y, &mut uv, W as usize, W as usize, W, H).unwrap();
        let placements =
            layout_left_to_right(cache, sample, F, SIZE_PX, FontStyle::Regular, 100, 100).unwrap();
        for p in &placements {
            if let Some(g) = cache.get_or_rasterize(p.key) {
                let g = g.clone();
                blit_glyph(&mut frame, &g, p.x, p.y, color);
            }
        }
    }

    let start = Instant::now();
    for _ in 0..ITERATIONS {
        let mut frame = Nv12FrameMut::new(&mut y, &mut uv, W as usize, W as usize, W, H).unwrap();
        let placements =
            layout_left_to_right(cache, sample, F, SIZE_PX, FontStyle::Regular, 100, 100).unwrap();
        // Pull each placement's bitmap by key and blit. Cloning the
        // bitmap each iteration is what the GStreamer element will
        // do anyway (the borrow checker won't let us hold a
        // &GlyphBitmap into the cache across the blit, since the
        // blit takes &mut frame and the cache may need &mut for
        // lazy rasterization).
        for p in &placements {
            let g = cache.get_or_rasterize(p.key).cloned().expect("preloaded");
            blit_glyph(&mut frame, &g, p.x, p.y, color);
        }
    }
    let elapsed = start.elapsed();
    let us_per = elapsed.as_nanos() as f64 / ITERATIONS as f64 / 1000.0;
    println!(
        "  {label:<45}  {us_per:>10.2} µs/iter  ({} chars, {} iters)",
        sample.chars().count(),
        ITERATIONS
    );
}

fn touch_blit(cache: &mut GlyphCache, key: GlyphKey, frame: &mut Nv12FrameMut<'_>) {
    let g = cache.get_or_rasterize(key).cloned();
    if let Some(g) = g {
        blit_glyph(frame, &g, 0, 0, Rgba::opaque(255, 255, 255));
    }
}

fn main() {
    let Some(font) = probe_font() else {
        eprintln!("[skip] no system font found");
        return;
    };
    let mut cache = GlyphCache::new().expect("freetype init");
    cache
        .register_font(F, FontStyle::Regular, &font)
        .expect("register font");

    let warmup_start = Instant::now();
    let n = cache
        .preload_ascii(F, SIZE_PX, FontStyle::Regular)
        .expect("preload");
    println!("aicam_broadcast_overlay text bench (1920×1080 NV12, {SIZE_PX}px regular)");
    println!(
        "  preload_ascii: {n} glyphs in {:.2} ms",
        warmup_start.elapsed().as_micros() as f64 / 1000.0
    );
    println!();

    // Clock-style strings - every digit changes per frame, but
    // they're all in the preloaded ASCII range.
    bench_string(&mut cache, "clock HH:MM:SS", "10:49:05");
    bench_string(&mut cache, "countdown MM:SS", "00:07");
    bench_string(&mut cache, "score block", "5 - 3");
    bench_string(&mut cache, "long string (status line)", "half time - 09:46");
    bench_string(
        &mut cache,
        "team name (Aldebaran NAO Devils)",
        "Aldebaran NAO Devils",
    );

    // One-off: cold lookup of a non-ASCII codepoint after warmup.
    // Measures the lazy-rasterize cost on the first frame a new
    // codepoint appears.
    {
        let mut y = vec![16u8; (W as usize) * (H as usize)];
        let mut uv = vec![128u8; (W as usize) * (H as usize) / 2];
        let mut frame = Nv12FrameMut::new(&mut y, &mut uv, W as usize, W as usize, W, H).unwrap();
        let key = GlyphKey {
            font_id: F,
            size_px: SIZE_PX,
            style: FontStyle::Regular,
            codepoint: 'ô',
        };
        let start = Instant::now();
        touch_blit(&mut cache, key, &mut frame);
        let cold = start.elapsed();
        let start = Instant::now();
        touch_blit(&mut cache, key, &mut frame);
        let warm = start.elapsed();
        println!();
        println!(
            "  lazy first-use 'ô':  cold {:.2} µs / warm {:.2} µs",
            cold.as_nanos() as f64 / 1000.0,
            warm.as_nanos() as f64 / 1000.0,
        );
    }
}
