// Wall-clock benchmark for the fill-rect renderer hot path.
// Author: Thomas Klute

//! Simple wall-clock benchmark for the renderer hot path.
//!
//! Stable Rust doesn't ship `#[bench]`, and pulling in `criterion`
//! for a single timing number is overkill. This is a regular binary
//! that runs each scenario a fixed number of times and prints
//! per-frame nanoseconds.
//!
//! Run with:
//!
//! ```text
//! cargo bench -p aicam_broadcast_overlay --bench fill_rect
//! ```
//!
//! On a dev machine, expect numbers in the 50-500 µs range per
//! 1080p frame for the scoreboard-sized regions (a few hundred
//! square pixels). The real targets - Pi 5 A76 and Jetson Nano A57 -
//! are measured separately during validation and benchmarking.

use std::time::Instant;

use aicam_broadcast_overlay::renderer::color::Rgba;
use aicam_broadcast_overlay::renderer::fill_rect::{fill_rect_alpha, fill_rect_opaque};
use aicam_broadcast_overlay::renderer::frame::{Nv12FrameMut, Rect};

const W: u32 = 1920;
const H: u32 = 1080;
const ITERATIONS: usize = 1000;

fn bench<F: FnMut(&mut Nv12FrameMut<'_>)>(label: &str, mut op: F) {
    let mut y = vec![16u8; (W as usize) * (H as usize)];
    let mut uv = vec![128u8; (W as usize) * (H as usize) / 2];

    let start = Instant::now();
    for _ in 0..ITERATIONS {
        let mut frame = Nv12FrameMut::new(&mut y, &mut uv, W as usize, W as usize, W, H).unwrap();
        op(&mut frame);
    }
    let elapsed = start.elapsed();
    let ns_per = elapsed.as_nanos() / ITERATIONS as u128;
    let us_per = ns_per as f64 / 1000.0;
    println!("  {label:<45}  {us_per:>10.2} µs/iter  ({ITERATIONS} iters)");
}

fn main() {
    println!("aicam_broadcast_overlay fill_rect bench  ({W}×{H} NV12)");
    println!();

    // Scoreboard-ish regions: small (a name pill) and large (the
    // bottom 3-row scoreboard background).
    let pill = Rect {
        x: 10,
        y: 10,
        width: 220,
        height: 36,
    };
    let scoreboard = Rect {
        x: 400,
        y: 900,
        width: 1120,
        height: 160,
    };
    let full_frame = Rect {
        x: 0,
        y: 0,
        width: W as i32,
        height: H as i32,
    };
    let white = Rgba::opaque(255, 255, 255);
    let translucent_black = Rgba::new(0, 0, 0, 178); // ~70 % alpha

    bench("opaque pill (220×36)", |f| {
        fill_rect_opaque(f, pill, white);
    });
    bench("opaque scoreboard (1120×160)", |f| {
        fill_rect_opaque(f, scoreboard, white);
    });
    bench("opaque full frame (1920×1080)", |f| {
        fill_rect_opaque(f, full_frame, white);
    });
    bench("alpha pill (220×36, a=178)", |f| {
        fill_rect_alpha(f, pill, translucent_black);
    });
    bench("alpha scoreboard (1120×160, a=178)", |f| {
        fill_rect_alpha(f, scoreboard, translucent_black);
    });
    bench("alpha full frame (1920×1080, a=178)", |f| {
        fill_rect_alpha(f, full_frame, translucent_black);
    });
}
