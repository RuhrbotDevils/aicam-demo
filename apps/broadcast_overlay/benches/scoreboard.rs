// End-to-end benchmark for the scoreboard layout and dispatch pipeline.
// Author: Thomas Klute

//! End-to-end bench for the scoreboard pipeline:
//! `ScoreboardState -> scoreboard_commands -> dispatch_commands -> NV12`.
//!
//! Measures the per-frame cost the GStreamer element actually pays.
//! Skips silently if no system font is found.
//!
//! Run with:
//!
//! ```text
//! cargo bench -p aicam_broadcast_overlay --bench scoreboard
//! ```

use std::path::PathBuf;
use std::time::Instant;

use aicam_broadcast_overlay::commands::{dispatch_commands, OverlayCommand};
use aicam_broadcast_overlay::glyphs::{FontId, FontStyle, GlyphCache};
use aicam_broadcast_overlay::layout::{
    scoreboard_commands, LayoutParams, LayoutSizes, PenaltyTile, ScoreboardState,
};
use aicam_broadcast_overlay::renderer::color::Rgba;
use aicam_broadcast_overlay::renderer::frame::Nv12FrameMut;

const W: u32 = 1920;
const H: u32 = 1080;
const ITERATIONS: usize = 1000;
const REGULAR: FontId = FontId(0);
const BOLD: FontId = FontId(1);

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

fn probe_bold() -> Option<PathBuf> {
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

fn build_state(secs: u32) -> ScoreboardState {
    let mm = secs / 60;
    let ss = secs % 60;
    ScoreboardState {
        field_name: "FIELD E".into(),
        clock_text: format!("10:49:{:02}", secs % 60),
        home_team_name: "B-Human".into(),
        away_team_name: "NAO Devils".into(),
        home_team_color: Some(Rgba::opaque(217, 25, 25)),
        away_team_color: Some(Rgba::opaque(25, 50, 217)),
        home_score: 3,
        away_score: 1,
        game_clock_text: format!("{mm:02}:{ss:02}"),
        phase_text: "FIRST HALF".into(),
        state_text: "PLAYING".into(),
        home_penalty_timers: vec![PenaltyTile {
            player_number: 4,
            secs_remaining: 30,
            penalty_reason: "PUSHING".into(),
            is_goalkeeper: false,
        }],
        away_penalty_timers: vec![
            PenaltyTile {
                player_number: 2,
                secs_remaining: 12,
                penalty_reason: "FOUL".into(),
                is_goalkeeper: false,
            },
            PenaltyTile {
                player_number: 6,
                secs_remaining: 43,
                penalty_reason: "BALL HOLDING".into(),
                is_goalkeeper: true,
            },
        ],
        ..Default::default()
    }
}

fn main() {
    let Some(regular) = probe_font() else {
        eprintln!("[skip] no regular system font");
        return;
    };
    let Some(bold) = probe_bold() else {
        eprintln!("[skip] no bold system font");
        return;
    };

    let mut cache = GlyphCache::new().expect("freetype init");
    cache
        .register_font(REGULAR, FontStyle::Regular, &regular)
        .unwrap();
    cache.register_font(BOLD, FontStyle::Bold, &bold).unwrap();

    let params = LayoutParams {
        frame_width: W,
        frame_height: H,
        regular_font: REGULAR,
        bold_font: BOLD,
        sizes: LayoutSizes::default(),
    };

    // Pre-rasterize every font/size the layout uses so the hot-path
    // measurement reflects cache-hit-only cost.
    let warmup_start = Instant::now();
    for size in [18u32, 22, 32] {
        let _ = cache.preload_ascii(REGULAR, size, FontStyle::Regular);
        let _ = cache.preload_ascii(BOLD, size, FontStyle::Bold);
    }
    println!("aicam_broadcast_overlay scoreboard bench  ({W}×{H} NV12)");
    println!(
        "  preload (3 sizes × 2 styles): {:.2} ms",
        warmup_start.elapsed().as_micros() as f64 / 1000.0
    );

    let mut y = vec![16u8; (W as usize) * (H as usize)];
    let mut uv = vec![128u8; (W as usize) * (H as usize) / 2];

    // -- layout-only --
    let start = Instant::now();
    let mut total_cmds = 0usize;
    for i in 0..ITERATIONS {
        let state = build_state(i as u32);
        let cmds = scoreboard_commands(&state, &params);
        total_cmds = cmds.len();
        std::hint::black_box(&cmds);
    }
    let elapsed = start.elapsed();
    println!(
        "  layout only (per frame, {total_cmds} commands):   {:>8.2} µs/iter",
        elapsed.as_nanos() as f64 / ITERATIONS as f64 / 1000.0
    );

    // -- layout + dispatch (real per-frame cost) --
    // Fill the Y plane with a uniform "background" each iteration so
    // measurements aren't dominated by accumulated alpha-blending
    // saturation.
    let start = Instant::now();
    for i in 0..ITERATIONS {
        y.fill(80);
        uv.fill(128);
        let mut frame = Nv12FrameMut::new(&mut y, &mut uv, W as usize, W as usize, W, H).unwrap();
        let state = build_state(i as u32);
        let cmds = scoreboard_commands(&state, &params);
        let stats = dispatch_commands(&mut frame, &mut cache, &cmds);
        std::hint::black_box(stats);
    }
    let elapsed = start.elapsed();
    println!(
        "  layout + dispatch (per frame):                    {:>8.2} µs/iter",
        elapsed.as_nanos() as f64 / ITERATIONS as f64 / 1000.0
    );

    // -- single "validate" pass cost - the plugin will run this
    // once per state update, not per frame --
    let state = build_state(0);
    let cmds = scoreboard_commands(&state, &params);
    let start = Instant::now();
    for _ in 0..ITERATIONS {
        let _ = aicam_broadcast_overlay::commands::validate_commands(&cache, &cmds);
    }
    let elapsed = start.elapsed();
    println!(
        "  validate_commands per call:                       {:>8.2} µs/iter",
        elapsed.as_nanos() as f64 / ITERATIONS as f64 / 1000.0
    );

    // -- frame count sanity --
    let stats = {
        let mut frame = Nv12FrameMut::new(&mut y, &mut uv, W as usize, W as usize, W, H).unwrap();
        dispatch_commands(
            &mut frame,
            &mut cache,
            &scoreboard_commands(&state, &params),
        )
    };
    println!(
        "\n  dispatch stats: fill_rect={} draw_text={} skipped_no_op={} skipped_invalid={}",
        stats.fill_rect_executed,
        stats.draw_text_executed,
        stats.skipped_no_op,
        stats.skipped_invalid,
    );

    // Manual sanity: should we have any FillRect commands too?
    let fill_count = scoreboard_commands(&state, &params)
        .iter()
        .filter(|c| matches!(c, OverlayCommand::FillRect { .. }))
        .count();
    println!("  total FillRect commands per frame: {fill_count}");
}
