// Defines Rust configuration and serialization logic for the media service.
// Author: Thomas Klute

//! Broadcast overlay - structured game state for the cairooverlay element.
//!
//! The streaming branch uses a `cairooverlay` element that calls our
//! [`draw_overlay`] function on every frame. The overlay reads the shared
//! [`OverlayState`] and draws a broadcast-quality scoreboard:
//!
//! - Top-left:  field name pill
//! - Top-right: wall-clock time pill
//! - Bottom-center: 3-row scoreboard (packets, score+clock, game state)
//!
//! v1.0 has no live GameController source feeding the score / clock /
//! state fields; the cairo draw runs against `GameOverlayData::default()`
//! plus whatever the operator pushes into `field_name` via
//! `PUT /overlay/text`. A future task can wire in a GC subscriber that
//! mutates the same `OverlayState` (and resolves team numbers to names
//! - the browser-side preview already does this via `/config/teams.json`).

use std::sync::{Arc, RwLock};

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Structured game overlay data
// ---------------------------------------------------------------------------

/// Structured game state for the broadcast overlay.
///
/// Fields mirror the GameController `telemetry.game_state` schema with the
/// additions needed for rendering (team names resolved from teams.json,
/// field name from config).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GameOverlayData {
    /// Resolved team 1 name (from teams.json or fallback "Team <num>")
    pub team1_name: String,
    /// Resolved team 2 name
    pub team2_name: String,
    pub team1_score: u32,
    pub team2_score: u32,
    pub team1_number: u32,
    pub team2_number: u32,
    /// true = 1st half, false = 2nd half
    pub first_half: bool,
    pub secs_remaining: i32,
    pub game_phase: u32,
    pub packet_number: u32,
    /// e.g. "PLAYING", "READY", "SET", "FINISHED"
    pub state: String,
    /// e.g. "NONE", "PENALTY_KICK", etc.
    pub set_play: String,
    /// Team number of the kicking team (for set plays)
    pub kicking_team: u32,
    /// Field name (from config, e.g. "FIELD E")
    pub field_name: String,
    /// Whether game state is from live GC (vs dummy/stale)
    pub is_live: bool,
}

impl Default for GameOverlayData {
    fn default() -> Self {
        Self {
            team1_name: "Home".into(),
            team2_name: "Away".into(),
            team1_score: 0,
            team2_score: 0,
            team1_number: 0,
            team2_number: 0,
            first_half: true,
            secs_remaining: 600,
            game_phase: 0,
            packet_number: 0,
            state: "INITIAL".into(),
            set_play: "NONE".into(),
            kicking_team: 0,
            field_name: "FIELD A".into(),
            is_live: false,
        }
    }
}

pub type OverlayState = Arc<RwLock<GameOverlayData>>;

/// Create a new overlay state with defaults.
pub fn new_overlay_state() -> OverlayState {
    Arc::new(RwLock::new(GameOverlayData::default()))
}

// ---------------------------------------------------------------------------
// Cairo drawing
// ---------------------------------------------------------------------------

/// Draw the broadcast overlay onto a Cairo context.
///
/// Called from the cairooverlay `draw` signal on every frame.
/// Layout matches the reference screenshots in docs/examples/Overlay/.
pub fn draw_overlay(cr: &cairo::Context, width: f64, height: f64, data: &GameOverlayData) {
    let scale = width / 960.0; // reference resolution is 960px wide
    let margin = 10.0 * scale;
    let font_sm = 12.0 * scale;
    let font_md = 16.0 * scale;
    let font_lg = 20.0 * scale;
    let pill_pad_x = 8.0 * scale;
    let pill_pad_y = 4.0 * scale;
    let pill_radius = 4.0 * scale;

    cr.select_font_face(
        "Noto Sans",
        cairo::FontSlant::Normal,
        cairo::FontWeight::Bold,
    );

    // ---- Top-left: field name pill ----
    {
        cr.set_font_size(font_md);
        let ext = cr.text_extents(&data.field_name).unwrap();
        let px = margin;
        let py = margin;
        let pw = ext.width() + pill_pad_x * 2.0;
        let ph = ext.height() + pill_pad_y * 2.0;

        draw_rounded_rect(cr, px, py, pw, ph, pill_radius);
        cr.set_source_rgba(0.0, 0.0, 0.0, 0.7);
        let _ = cr.fill();

        cr.set_source_rgb(1.0, 1.0, 1.0);
        cr.move_to(px + pill_pad_x, py + pill_pad_y + ext.height());
        let _ = cr.show_text(&data.field_name);
    }

    // ---- Top-right: clock pill (HH:MM:SS) ----
    {
        let now = chrono::Local::now();
        let time_str = now.format("%H:%M:%S").to_string();

        cr.set_font_size(font_md);
        let ext = cr.text_extents(&time_str).unwrap();
        let pw = ext.width() + pill_pad_x * 2.0;
        let ph = ext.height() + pill_pad_y * 2.0;
        let px = width - margin - pw;
        let py = margin;

        draw_rounded_rect(cr, px, py, pw, ph, pill_radius);
        cr.set_source_rgba(0.0, 0.0, 0.0, 0.7);
        let _ = cr.fill();

        cr.set_source_rgb(1.0, 1.0, 1.0);
        cr.move_to(px + pill_pad_x, py + pill_pad_y + ext.height());
        let _ = cr.show_text(&time_str);
    }

    // ---- Bottom-center: 3-row scoreboard ----
    draw_scoreboard(
        cr, width, height, data, scale, margin, font_sm, font_md, font_lg,
    );
}

/// Draw the 3-row scoreboard at bottom center.
#[allow(clippy::too_many_arguments)]
fn draw_scoreboard(
    cr: &cairo::Context,
    width: f64,
    height: f64,
    data: &GameOverlayData,
    scale: f64,
    margin: f64,
    font_sm: f64,
    font_md: f64,
    font_lg: f64,
) {
    let row_height = 22.0 * scale;
    let score_box_w = 22.0 * scale;
    let clock_box_w = 60.0 * scale;
    let pad = 6.0 * scale;
    let radius = 3.0 * scale;

    // Compute widths for team names
    cr.set_font_size(font_md);
    let t1_ext = cr.text_extents(&data.team1_name).unwrap();
    let t2_ext = cr.text_extents(&data.team2_name).unwrap();
    let name_w = t1_ext.width().max(t2_ext.width()) + pad * 2.0;

    // Total scoreboard width: name + score + clock + score + name + gaps
    let gap = 2.0 * scale;
    let total_w = name_w + score_box_w + gap + clock_box_w + gap + score_box_w + name_w;

    // Scoreboard position
    let sb_x = (width - total_w) / 2.0;
    let sb_y = height - margin - row_height * 3.0 - pad * 2.0;

    // --- Background for entire scoreboard ---
    let sb_h = row_height * 3.0 + pad * 2.0;
    draw_rounded_rect(
        cr,
        sb_x - pad,
        sb_y - pad,
        total_w + pad * 2.0,
        sb_h + pad,
        radius,
    );
    cr.set_source_rgba(0.0, 0.0, 0.0, 0.65);
    let _ = cr.fill();

    // --- Row 1: packet counts + game phase ---
    {
        cr.set_font_size(font_sm);
        let phase_str = format_game_phase(data);
        let pkt1 = data.packet_number.to_string();
        let pkt2 = data.packet_number.to_string(); // GC broadcasts a single packet_number

        let y_base = sb_y + font_sm;

        // Left packet count
        cr.set_source_rgb(0.8, 0.8, 0.8);
        let pkt1_ext = cr.text_extents(&pkt1).unwrap();
        cr.move_to(sb_x + name_w / 2.0 - pkt1_ext.width() / 2.0, y_base);
        let _ = cr.show_text(&pkt1);

        // Center phase
        let phase_ext = cr.text_extents(&phase_str).unwrap();
        cr.move_to(width / 2.0 - phase_ext.width() / 2.0, y_base);
        let _ = cr.show_text(&phase_str);

        // Right packet count
        let pkt2_ext = cr.text_extents(&pkt2).unwrap();
        cr.move_to(
            sb_x + total_w - name_w / 2.0 - pkt2_ext.width() / 2.0,
            y_base,
        );
        let _ = cr.show_text(&pkt2);
    }

    // --- Row 2: team1 name | score1 (red) | clock | score2 (blue) | team2 name ---
    {
        let row_y = sb_y + row_height;
        let y_text = row_y + font_lg * 0.85;
        cr.set_font_size(font_md);

        // Team 1 name (right-aligned before score)
        cr.set_source_rgb(1.0, 1.0, 1.0);
        let t1_ext = cr.text_extents(&data.team1_name).unwrap();
        let x_name1_right = sb_x + name_w;
        cr.move_to(x_name1_right - t1_ext.width() - pad, y_text);
        let _ = cr.show_text(&data.team1_name);

        // Score 1 box (red background)
        let x_score1 = x_name1_right;
        draw_rounded_rect(cr, x_score1, row_y, score_box_w, row_height, radius);
        cr.set_source_rgb(0.85, 0.1, 0.1); // red
        let _ = cr.fill();

        cr.set_font_size(font_lg);
        cr.set_source_rgb(1.0, 1.0, 1.0);
        let s1 = data.team1_score.to_string();
        let s1_ext = cr.text_extents(&s1).unwrap();
        cr.move_to(x_score1 + score_box_w / 2.0 - s1_ext.width() / 2.0, y_text);
        let _ = cr.show_text(&s1);

        // Clock box (dark background)
        let x_clock = x_score1 + score_box_w + gap;
        draw_rounded_rect(cr, x_clock, row_y, clock_box_w, row_height, radius);
        cr.set_source_rgba(0.15, 0.15, 0.15, 0.9);
        let _ = cr.fill();

        let mins = data.secs_remaining.unsigned_abs() / 60;
        let secs = data.secs_remaining.unsigned_abs() % 60;
        let clock_str = format!("{:02}:{:02}", mins, secs);
        cr.set_font_size(font_lg);
        cr.set_source_rgb(1.0, 1.0, 1.0);
        let clk_ext = cr.text_extents(&clock_str).unwrap();
        cr.move_to(x_clock + clock_box_w / 2.0 - clk_ext.width() / 2.0, y_text);
        let _ = cr.show_text(&clock_str);

        // Score 2 box (blue background)
        let x_score2 = x_clock + clock_box_w + gap;
        draw_rounded_rect(cr, x_score2, row_y, score_box_w, row_height, radius);
        cr.set_source_rgb(0.1, 0.2, 0.85); // blue
        let _ = cr.fill();

        cr.set_font_size(font_lg);
        cr.set_source_rgb(1.0, 1.0, 1.0);
        let s2 = data.team2_score.to_string();
        let s2_ext = cr.text_extents(&s2).unwrap();
        cr.move_to(x_score2 + score_box_w / 2.0 - s2_ext.width() / 2.0, y_text);
        let _ = cr.show_text(&s2);

        // Team 2 name (left-aligned after score)
        cr.set_font_size(font_md);
        cr.set_source_rgb(1.0, 1.0, 1.0);
        cr.move_to(x_score2 + score_box_w + pad, y_text);
        let _ = cr.show_text(&data.team2_name);
    }

    // --- Row 3: game state + timer ---
    {
        let row_y = sb_y + row_height * 2.0;
        cr.set_font_size(font_sm);

        let state_str = format_state_line(data);
        let ext = cr.text_extents(&state_str).unwrap();
        cr.set_source_rgb(0.9, 0.9, 0.9);
        cr.move_to(
            width / 2.0 - ext.width() / 2.0,
            row_y + font_sm + 2.0 * scale,
        );
        let _ = cr.show_text(&state_str);
    }
}

/// Format game phase string (e.g. "1st", "2nd").
fn format_game_phase(data: &GameOverlayData) -> String {
    if data.first_half {
        "1st".to_string()
    } else {
        "2nd".to_string()
    }
}

/// Format the state line (row 3): e.g. "playing", "ready, kickoff for B-Human - 00:21"
fn format_state_line(data: &GameOverlayData) -> String {
    let state_lower = data.state.to_lowercase();
    if data.set_play != "NONE" {
        let play = data.set_play.to_lowercase().replace('_', " ");
        let kicking = if data.kicking_team == data.team1_number {
            &data.team1_name
        } else if data.kicking_team == data.team2_number {
            &data.team2_name
        } else {
            "unknown"
        };
        format!("{}, {} for {}", state_lower, play, kicking)
    } else if data.state == "READY" || data.state == "SET" {
        let kicking = if data.kicking_team == data.team1_number {
            &data.team1_name
        } else if data.kicking_team == data.team2_number {
            &data.team2_name
        } else {
            "unknown"
        };
        format!("{}, kickoff for {}", state_lower, kicking)
    } else {
        state_lower
    }
}

/// Draw a rounded rectangle path.
fn draw_rounded_rect(cr: &cairo::Context, x: f64, y: f64, w: f64, h: f64, r: f64) {
    let r = r.min(w / 2.0).min(h / 2.0);
    cr.new_path();
    cr.arc(x + w - r, y + r, r, -std::f64::consts::FRAC_PI_2, 0.0);
    cr.arc(x + w - r, y + h - r, r, 0.0, std::f64::consts::FRAC_PI_2);
    cr.arc(
        x + r,
        y + h - r,
        r,
        std::f64::consts::FRAC_PI_2,
        std::f64::consts::PI,
    );
    cr.arc(
        x + r,
        y + r,
        r,
        std::f64::consts::PI,
        3.0 * std::f64::consts::FRAC_PI_2,
    );
    cr.close_path();
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_state_line_playing() {
        let data = GameOverlayData {
            state: "PLAYING".into(),
            set_play: "NONE".into(),
            ..Default::default()
        };
        assert_eq!(format_state_line(&data), "playing");
    }

    #[test]
    fn test_format_state_line_ready_kickoff() {
        let data = GameOverlayData {
            state: "READY".into(),
            set_play: "NONE".into(),
            kicking_team: 5,
            team1_number: 5,
            team1_name: "B-Human".into(),
            ..Default::default()
        };
        assert_eq!(format_state_line(&data), "ready, kickoff for B-Human");
    }

    #[test]
    fn test_format_state_line_set_play() {
        let data = GameOverlayData {
            state: "PLAYING".into(),
            set_play: "PENALTY_KICK".into(),
            kicking_team: 14,
            team2_number: 14,
            team2_name: "HTWK Robots".into(),
            ..Default::default()
        };
        assert_eq!(
            format_state_line(&data),
            "playing, penalty kick for HTWK Robots"
        );
    }

    #[test]
    fn test_overlay_state_thread_safe() {
        let state = new_overlay_state();
        let state2 = state.clone();
        std::thread::spawn(move || {
            let mut data = state2.write().unwrap();
            data.team1_name = "Test".to_string();
            data.team1_score = 3;
        })
        .join()
        .unwrap();
        let data = state.read().unwrap();
        assert_eq!(data.team1_name, "Test");
        assert_eq!(data.team1_score, 3);
    }
}
