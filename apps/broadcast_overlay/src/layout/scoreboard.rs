// Scoreboard layout: turns semantic game state into a paint-ordered command list.
// Author: Thomas Klute

//! Scoreboard layout - semantic state -> `Vec<OverlayCommand>`.
//!
//! Mirrors the visual shape of the existing cairo overlay in
//! `apps/media_service/src/overlay.rs::draw_overlay`, simplified for
//! v1:
//!
//!  - top-left field-name pill
//!  - top-right clock pill
//!  - bottom-centre 3-row scoreboard:
//!    row 1 - game-phase label (centred);
//!    row 2 - `[colour] name  score  CLOCK  score  name [colour]`;
//!    row 3 - state line (centred, e.g. `PLAYING`)
//!  - bottom-left / bottom-right penalty-card columns (one tile per
//!    penalised robot - number + countdown; the cairo overlay's
//!    reason text + GK strip + shootout dots are deferred to a
//!    follow-up iteration)
//!
//! The layout function returns commands in **paint order**: backings
//! first, foregrounds and text last. Downstream `dispatch_commands`
//! preserves that order so text always sits above its pill.
//!
//! ## Text width estimation
//!
//! The layout doesn't query FreeType - it would couple the layout to
//! the renderer and force a `&mut GlyphCache` through every layout
//! call. Instead we estimate per-glyph advance from `size_px` using
//! the common proportional-Sans ratio (≈0.55) and a slightly wider
//! ratio (≈0.6) for digit-heavy strings. The pill widths are
//! conservative - they over-size by ~10 % rather than clip text,
//! prioritising readability over exact reproduction.

use crate::commands::OverlayCommand;
use crate::glyphs::{FontId, FontStyle};
use crate::renderer::color::Rgba;

/// Layout target. The crate doesn't read the producer-side YAML; the
/// plugin builds this struct from `MediaConfig` +
/// `GameOverlayData` + the registered `FontId`s.
#[derive(Debug, Clone, Copy)]
pub struct LayoutParams {
    pub frame_width: u32,
    pub frame_height: u32,
    pub regular_font: FontId,
    pub bold_font: FontId,
    /// Per-element sizing knobs. Defaults match the values tuned at
    /// the 1920×1080 reference resolution; operators override via
    /// `video.streaming.overlay_layout` in `config.yaml`.
    pub sizes: LayoutSizes,
}

/// Tunable per-element layout sizing, in reference-resolution pixels
/// (1920×1080). Every value scales linearly with `frame_width /
/// 1920.0`. Default = the values tuned in `scoreboard_commands` at
/// crate-shipped resolution; operators override individual fields
/// via the producer-side config.
///
/// `Copy` so it fits inside [`LayoutParams`] without lifetimes; the
/// struct is tiny (≈11 × `f32`).
#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct LayoutSizes {
    /// Top-left field-name + top-right clock pill text size.
    pub font_pill: f32,
    /// Centre-scoreboard team-name text size.
    pub font_sb_name: f32,
    /// Centre-scoreboard score + game-clock text size.
    pub font_sb_score: f32,
    /// Row 1 of the centre scoreboard: phase label + message budget.
    pub font_sb_phase: f32,
    /// Row 3 of the centre scoreboard: state text.
    pub font_sb_state: f32,
    /// Penalty tile row 1: player number.
    pub font_penalty_num: f32,
    /// Penalty tile row 2: MM:SS countdown.
    pub font_penalty_time: f32,
    /// Penalty tile row 3: penalty reason text.
    pub font_penalty_reason: f32,
    /// Centre-scoreboard row height (single row of the 3-row stack).
    pub sb_row_h: f32,
    /// Penalty tile width.
    pub penalty_tile_w: f32,
    /// Penalty tile total height (3 rows).
    pub penalty_tile_h: f32,
}

impl Default for LayoutSizes {
    fn default() -> Self {
        // Centre-scoreboard fonts use the +1/3 bump requested in
        // review; the row height grows in step so the 48 px scores
        // still fit. The pill font (44) and penalty
        // tile sizes are deliberately *not* bumped - those already
        // matched the reference image.
        Self {
            font_pill: 44.0,
            font_sb_name: 37.0,
            font_sb_score: 48.0,
            font_sb_phase: 35.0,
            font_sb_state: 35.0,
            font_penalty_num: 44.0,
            font_penalty_time: 44.0,
            font_penalty_reason: 22.0,
            sb_row_h: 60.0,
            penalty_tile_w: 220.0,
            penalty_tile_h: 110.0,
        }
    }
}

/// Semantic scoreboard state, intentionally narrower than
/// `apps/media_service/src/overlay.rs::GameOverlayData`. The plugin
/// fills the gap with a simple borrowing conversion at its boundary.
///
/// `Serialize`/`Deserialize` are derived so the GStreamer plugin's
/// `scoreboard-state-json` property can carry the full state across
/// the cdylib/main-binary boundary - producer (media_service) writes
/// JSON via `gst::Element::set_property`, plugin parses it on the
/// other side. This avoids registering the `AicamNv12Overlay` GType
/// twice (once via the dlopen-loaded `.so`, once via media_service's
/// linked-in Cargo dep), which is otherwise fatal: glib's subclass
/// registration asserts the name is unique.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct ScoreboardState {
    /// Top-left pill text.
    pub field_name: String,
    /// Top-right pill text. The plugin formats this (e.g. with
    /// `chrono::Local::now().format("%H:%M:%S")`) so the layout
    /// stays domain- and time-independent.
    pub clock_text: String,

    pub home_team_name: String,
    pub away_team_name: String,
    /// Display colour of the home team's score block. `None` falls
    /// back to a default red.
    pub home_team_color: Option<Rgba>,
    /// `None` falls back to a default blue.
    pub away_team_color: Option<Rgba>,
    /// Goalkeeper jersey colour for the home team. When `Some` AND
    /// distinct from `home_team_color`, the renderer adds a thinner
    /// GK strip next to the field-player block. Same convention as
    /// the cairo overlay.
    #[serde(default)]
    pub home_team_goalkeeper_color: Option<Rgba>,
    #[serde(default)]
    pub away_team_goalkeeper_color: Option<Rgba>,

    pub home_score: u32,
    pub away_score: u32,

    /// Centre scoreboard clock (e.g. `"10:00"` or `"00:23"`).
    pub game_clock_text: String,
    /// When `true`, the renderer prefixes the clock with a pause
    /// glyph (U+23F8) and dims the digits - matches the cairo
    /// overlay's "GC reports stopped" indicator.
    #[serde(default)]
    pub clock_stopped: bool,
    /// Row 1 label - `"FIRST HALF"`, `"PENALTY SHOOTOUT"`,
    /// `"HALF TIME"`, etc. Caller formats.
    pub phase_text: String,
    /// Row 3 line - `"PLAYING"`, `"SET PLAY: …"`, `"FINISHED"`.
    pub state_text: String,

    /// Message budget counters rendered to the left/right cells of
    /// row 1. When the row is a shoot-out, dot
    /// indicators replace these - see [`shootout`].
    #[serde(default)]
    pub home_message_budget: u32,
    #[serde(default)]
    pub away_message_budget: u32,

    /// When `Some`, row 1's side cells render shoot-out dots instead
    /// of message budgets. See [`ShootoutState`].
    #[serde(default)]
    pub shootout: Option<ShootoutState>,

    pub home_penalty_timers: Vec<PenaltyTile>,
    pub away_penalty_timers: Vec<PenaltyTile>,
}

/// Shoot-out indicator data for scoreboard row 1.
///
/// Shoot-out model: GC reports a 1-based shot index per team
/// (`penalty_shot`) and a bitmask of which shots were successful
/// (`single_shots`). The renderer draws one dot per shot taken, filled
/// in the team colour for makes and as an empty ring for misses.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct ShootoutState {
    pub home_penalty_shot: u32,
    pub home_single_shots: u32,
    pub away_penalty_shot: u32,
    pub away_single_shots: u32,
}

/// One penalised robot.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct PenaltyTile {
    pub player_number: u32,
    pub secs_remaining: u32,
    /// Penalty reason text (cairo's row 3 of each card). Short labels
    /// e.g. `"PUSHING"`, `"BALL HOLDING"`. Empty string suppresses
    /// the row to keep the tile compact for unknown reasons.
    #[serde(default)]
    pub penalty_reason: String,
    /// When `true`, the renderer adds a `"GK"` badge on the top-left
    /// of the tile's row 1.
    #[serde(default)]
    pub is_goalkeeper: bool,
}

// ---------------------------------------------------------------------------
// Default colours (Rec. 709 limited-range numbers come out of YUV at
// blit time; the layout works in straight RGBA).
// ---------------------------------------------------------------------------
/// Pills (top-left field name, top-right wall clock): light gray at
/// 50 % alpha. Sits over the camera image with black text on top.
const PILL_BG: Rgba = Rgba::new(217, 217, 217, 128);
/// Per-label backings inside the centre scoreboard and on the
/// penalty tile timer / reason rows. Opaque so the text reads at
/// full contrast regardless of what's behind in the video.
const LABEL_BG: Rgba = Rgba::opaque(217, 217, 217);
/// Black text used on the light-gray backings (pills, scoreboard
/// labels, penalty timer + reason). Slightly above pure black so
/// FreeType subpixel rendering still anti-aliases cleanly.
const TEXT_FG_DARK: Rgba = Rgba::opaque(12, 12, 12);
/// White text used on the team-colour score boxes and on the
/// player-number cell of the penalty tile (which sits on the team
/// colour, not on light gray).
const TEXT_FG_LIGHT: Rgba = Rgba::opaque(255, 255, 255);

/// Shadow `Rgba::contrasting_text` as a free function so
/// the call sites read like a small library function rather than a
/// type-method chain. No behaviour change beyond the namespacing.
fn contrasting_text_color(bg: Rgba) -> Rgba {
    bg.contrasting_text()
}
/// Dimmed clock text when GC reports `stopped`. Mid-gray so the
/// stopped-state is recognisable but the digits still read.
const CLOCK_FG_STOPPED: Rgba = Rgba::opaque(102, 102, 102);
const DEFAULT_HOME: Rgba = Rgba::opaque(217, 25, 25); // red
const DEFAULT_AWAY: Rgba = Rgba::opaque(25, 50, 217); // blue

// ---------------------------------------------------------------------------
// Reference layout - picked at 1920×1080 and linearly scaled. Numbers
// were transcribed from the existing cairo overlay so v1 lands at the
// same on-screen positions. The *user-tunable* sizes live on
// [`LayoutSizes`]; the constants below are only the structural
// values that don't make sense to expose (margins / paddings / inter-
// element gaps).
// ---------------------------------------------------------------------------
const REF_W: f32 = 1920.0;
const MARGIN_REF: f32 = 16.0;
const PILL_PAD_X_REF: f32 = 12.0;
const PILL_PAD_Y_REF: f32 = 6.0;
const SB_PAD_REF: f32 = 10.0;
const TEAM_COLOR_W_REF: f32 = 16.0;
/// Width of the optional goalkeeper-colour strip drawn next to the
/// team-colour block when GK colour ≠ field colour. Cairo uses 5 px
/// at 960 px reference (10 px at 1920 reference).
const GK_STRIP_W_REF: f32 = 10.0;
const GK_STRIP_GAP_REF: f32 = 4.0;
const SCORE_BOX_W_REF: f32 = 64.0;
/// Clock box width is computed at render time from the real font
/// size (`MM:SS` plus an optional pause-glyph prefix), so a font
/// bump no longer silently overflows into the score-box on the
/// right. This constant is only a *minimum*.
const CLOCK_BOX_MIN_W_REF: f32 = 140.0;
/// Inner right-padding inside the team-name cells (and any other
/// text field that needs a visible gap between text edge and the
/// next element). Operators asked for "some fixed padding" so the
/// names don't kiss the score boxes. Generous enough to read
/// clearly even when the image is downscaled for review.
const SB_TEXT_PAD_REF: f32 = 18.0;
const PENALTY_GAP_REF: f32 = 6.0;
/// Penalty tile inner padding - kept off `LayoutSizes` because it's
/// derived from the tile dimensions and serves the visual "row
/// rounding" rather than being a content size operators want to
/// tweak.
const PENALTY_ROW_PAD_REF: f32 = 4.0;
const SHOOTOUT_DOT_RADIUS_REF: f32 = 4.0;
const SHOOTOUT_DOT_GAP_REF: f32 = 8.0;

/// Build the scoreboard command list. Result paints back-to-front.
pub fn scoreboard_commands(state: &ScoreboardState, params: &LayoutParams) -> Vec<OverlayCommand> {
    let scale = params.frame_width as f32 / REF_W;
    let s = |v: f32| -> i32 { (v * scale).round() as i32 };
    let margin = s(MARGIN_REF);

    let pill_pad_x = s(PILL_PAD_X_REF);
    let pill_pad_y = s(PILL_PAD_Y_REF);
    let font_pill = (params.sizes.font_pill * scale).round() as u32;
    let pill_h = font_pill as i32 + pill_pad_y * 2;

    let mut cmds = Vec::with_capacity(64);

    // ---- Top-left: field name pill ----
    if !state.field_name.is_empty() {
        let text_w = estimate_text_width(&state.field_name, font_pill);
        let pill_w = text_w as i32 + pill_pad_x * 2;
        cmds.push(OverlayCommand::FillRect {
            x: margin,
            y: margin,
            width: pill_w,
            height: pill_h,
            color: PILL_BG,
        });
        cmds.push(OverlayCommand::DrawText {
            x: margin + pill_pad_x,
            y: margin + pill_pad_y + font_pill as i32, // baseline = top + pad + size
            text: state.field_name.clone(),
            font_id: params.bold_font,
            size_px: font_pill,
            style: FontStyle::Bold,
            color: TEXT_FG_DARK,
        });
    }

    // ---- Top-right: clock pill ----
    if !state.clock_text.is_empty() {
        let text_w = estimate_text_width_digits(&state.clock_text, font_pill);
        let pill_w = text_w as i32 + pill_pad_x * 2;
        let pill_x = params.frame_width as i32 - margin - pill_w;
        cmds.push(OverlayCommand::FillRect {
            x: pill_x,
            y: margin,
            width: pill_w,
            height: pill_h,
            color: PILL_BG,
        });
        cmds.push(OverlayCommand::DrawText {
            x: pill_x + pill_pad_x,
            y: margin + pill_pad_y + font_pill as i32,
            text: state.clock_text.clone(),
            font_id: params.bold_font,
            size_px: font_pill,
            style: FontStyle::Bold,
            color: TEXT_FG_DARK,
        });
    }

    // ---- Bottom-center scoreboard ----
    push_scoreboard(&mut cmds, state, params, scale);

    // ---- Bottom-left / bottom-right penalty columns ----
    push_penalty_column(
        &mut cmds,
        params,
        scale,
        Side::Left,
        &state.home_penalty_timers,
        state.home_team_color.unwrap_or(DEFAULT_HOME),
    );
    push_penalty_column(
        &mut cmds,
        params,
        scale,
        Side::Right,
        &state.away_penalty_timers,
        state.away_team_color.unwrap_or(DEFAULT_AWAY),
    );

    cmds
}

// ---------------------------------------------------------------------------

fn push_scoreboard(
    cmds: &mut Vec<OverlayCommand>,
    state: &ScoreboardState,
    params: &LayoutParams,
    scale: f32,
) {
    let s = |v: f32| -> i32 { (v * scale).round() as i32 };

    let row_h = (params.sizes.sb_row_h * scale).round() as i32;
    let sb_pad = s(SB_PAD_REF);
    let team_color_w = s(TEAM_COLOR_W_REF);
    let gk_strip_w = s(GK_STRIP_W_REF);
    let gk_strip_gap = s(GK_STRIP_GAP_REF);

    let font_name = (params.sizes.font_sb_name * scale).round() as u32;
    let font_score = (params.sizes.font_sb_score * scale).round() as u32;
    let font_phase = (params.sizes.font_sb_phase * scale).round() as u32;
    let font_state = (params.sizes.font_sb_state * scale).round() as u32;

    // Clock + score cells are sized at render time from the font
    // they actually use, so a font bump no longer silently
    // overflows neighbouring cells. Both reserve the same fixed
    // padding on each side as the team-name cells.
    //
    // Clock: "-MM:SS" - 6 chars covers both running ("MM:SS") and
    // overtime ("-MM:SS"). The pause glyph was dropped (the glyph
    // cache can't render codepoints the font lacks), so this is the
    // widest content we ever render.
    let clock_box_w = (estimate_text_width_digits("-00:00", font_score) as i32
        + 2 * s(SB_TEXT_PAD_REF))
    .max(s(CLOCK_BOX_MIN_W_REF));
    // Score: two-digit max for the foreseeable future
    // (RoboCup HSL scores cap around the teens).
    let score_box_w = (estimate_text_width_digits("99", font_score) as i32
        + 2 * s(SB_TEXT_PAD_REF))
    .max(s(SCORE_BOX_W_REF));

    // Goalkeeper strips appear only when GK colour ≠ field colour
    // (cairo overlay rule). Width is added to the scoreboard total when
    // present so the strip doesn't overlap the team name.
    let home_gk_extra =
        if gk_strip_distinct(state.home_team_goalkeeper_color, state.home_team_color) {
            gk_strip_w + gk_strip_gap
        } else {
            0
        };
    let away_gk_extra =
        if gk_strip_distinct(state.away_team_goalkeeper_color, state.away_team_color) {
            gk_strip_w + gk_strip_gap
        } else {
            0
        };

    // Estimate name column width from the longer of the two names,
    // bounded to keep the overall scoreboard inside the frame even
    // for absurd inputs. The GK strips eat into the budget when
    // present.
    //
    // Names are rendered in bold; use the bold-aware estimate so we
    // don't under-allocate and let the team name overflow into the
    // score box. The cell reserves `SB_TEXT_PAD_REF` on both sides
    // of the text so there's always visible breathing room between
    // name edge and score-box / team-colour edge.
    let text_pad = s(SB_TEXT_PAD_REF);
    let max_name_w_px = ((params.frame_width as i32 - 2 * s(MARGIN_REF) - 4 * sb_pad) / 2
        - 2 * team_color_w
        - home_gk_extra
        - away_gk_extra
        - 2 * score_box_w
        - clock_box_w)
        .max(s(120.0)) as u32;
    let raw_name_w = estimate_text_width_bold(&state.home_team_name, font_name)
        .max(estimate_text_width_bold(&state.away_team_name, font_name));
    let name_w = (raw_name_w + 2 * text_pad as u32).min(max_name_w_px) as i32;

    let total_w = team_color_w
        + home_gk_extra
        + name_w
        + score_box_w
        + clock_box_w
        + score_box_w
        + name_w
        + away_gk_extra
        + team_color_w;
    let sb_x = (params.frame_width as i32 - total_w) / 2;
    // Three rows abut without an outer panel; we keep a small inter-
    // row gap so the lightgray strips read as separate bars rather
    // than one continuous block.
    let row_gap = s(2.0);
    let sb_h = row_h * 3 + row_gap * 2;
    let sb_y = params.frame_height as i32 - s(MARGIN_REF) - sb_h;

    // No outer FillRect - the reference design uses per-label
    // opaque-lightgray backings instead of one full panel.

    // Row 2 sits in the middle.
    let row2_y = sb_y + row_h + row_gap;
    let home_color = state.home_team_color.unwrap_or(DEFAULT_HOME);
    let away_color = state.away_team_color.unwrap_or(DEFAULT_AWAY);

    // Geometry for inner columns (used by row 1, row 2 and the
    // shoot-out / msg-budget side cells).
    let name1_x = sb_x + team_color_w + home_gk_extra;
    let score1_x = name1_x + name_w;
    let clock_x = score1_x + score_box_w;
    let score2_x = clock_x + clock_box_w;
    let name2_x = score2_x + score_box_w;

    // Row 2 backings: [team_color][gk?][name lightgray][score team_color][clock lightgray][score team_color][name lightgray][gk?][team_color]
    // Left team-colour edge block
    cmds.push(OverlayCommand::FillRect {
        x: sb_x,
        y: row2_y,
        width: team_color_w,
        height: row_h,
        color: home_color,
    });
    if home_gk_extra > 0 {
        if let Some(gk_c) = state.home_team_goalkeeper_color {
            cmds.push(OverlayCommand::FillRect {
                x: sb_x + team_color_w + gk_strip_gap,
                y: row2_y,
                width: gk_strip_w,
                height: row_h,
                color: gk_c,
            });
        }
    }
    // Home team-name backing (lightgray)
    cmds.push(OverlayCommand::FillRect {
        x: name1_x,
        y: row2_y,
        width: name_w,
        height: row_h,
        color: LABEL_BG,
    });
    // Home score backing (team colour)
    cmds.push(OverlayCommand::FillRect {
        x: score1_x,
        y: row2_y,
        width: score_box_w,
        height: row_h,
        color: home_color,
    });
    // Clock backing (lightgray)
    cmds.push(OverlayCommand::FillRect {
        x: clock_x,
        y: row2_y,
        width: clock_box_w,
        height: row_h,
        color: LABEL_BG,
    });
    // Away score backing (team colour)
    cmds.push(OverlayCommand::FillRect {
        x: score2_x,
        y: row2_y,
        width: score_box_w,
        height: row_h,
        color: away_color,
    });
    // Away team-name backing (lightgray)
    cmds.push(OverlayCommand::FillRect {
        x: name2_x,
        y: row2_y,
        width: name_w,
        height: row_h,
        color: LABEL_BG,
    });
    if away_gk_extra > 0 {
        if let Some(gk_c) = state.away_team_goalkeeper_color {
            cmds.push(OverlayCommand::FillRect {
                x: sb_x + total_w - team_color_w - gk_strip_gap - gk_strip_w,
                y: row2_y,
                width: gk_strip_w,
                height: row_h,
                color: gk_c,
            });
        }
    }
    // Right team-colour edge block
    cmds.push(OverlayCommand::FillRect {
        x: sb_x + total_w - team_color_w,
        y: row2_y,
        width: team_color_w,
        height: row_h,
        color: away_color,
    });

    // 3) row 1: phase label centred, msg-budget OR shoot-out dots on each side
    let row1_baseline = sb_y + (row_h + font_phase as i32) / 2 - s(2.0);
    // Phase label backing (centre) sized to the text + the same fixed
    // padding the team-name cells use so all row-1 cells read alike.
    if !state.phase_text.is_empty() {
        let phase_w = estimate_text_width_bold(&state.phase_text, font_phase) as i32;
        let pad = text_pad;
        let bg_x = sb_x + (total_w - phase_w - 2 * pad) / 2;
        cmds.push(OverlayCommand::FillRect {
            x: bg_x,
            y: sb_y,
            width: phase_w + 2 * pad,
            height: row_h,
            color: LABEL_BG,
        });
        cmds.push(OverlayCommand::DrawText {
            x: bg_x + pad,
            y: row1_baseline,
            text: state.phase_text.clone(),
            font_id: params.bold_font,
            size_px: font_phase,
            style: FontStyle::Bold,
            color: TEXT_FG_DARK,
        });
    }
    push_row1_side(
        cmds,
        params,
        scale,
        state.shootout.as_ref(),
        Side::Left,
        state.home_message_budget,
        home_color,
        sb_y,
        name1_x + name_w / 2,
        row_h,
        row1_baseline,
        font_phase,
    );
    push_row1_side(
        cmds,
        params,
        scale,
        state.shootout.as_ref(),
        Side::Right,
        state.away_message_budget,
        away_color,
        sb_y,
        name2_x + name_w / 2,
        row_h,
        row1_baseline,
        font_phase,
    );

    let row2_baseline = row2_y + (row_h + font_name as i32) / 2 - s(2.0);
    let row2_score_baseline = row2_y + (row_h + font_score as i32) / 2 - s(2.0);

    // Home team name (right-aligned to score-block left edge). Truncate
    // FIRST so the anchor uses the displayed width, not the original.
    // Use the bold-aware width estimate so we don't undershoot and let
    // the text overflow into the score box.
    let home_display = truncate_for_display(
        &state.home_team_name,
        (name_w - 2 * text_pad).max(0) as u32,
        font_name,
    );
    cmds.push(OverlayCommand::DrawText {
        x: score1_x - estimate_text_width_bold(&home_display, font_name) as i32 - text_pad,
        y: row2_baseline,
        text: home_display,
        font_id: params.bold_font,
        size_px: font_name,
        style: FontStyle::Bold,
        color: TEXT_FG_DARK,
    });
    // Home score (centred in its team-colour box):
    // contrast-adaptive text colour against the team-colour
    // background so white / yellow / cyan jerseys don't render an
    // invisible white digit.
    let home_score_str = state.home_score.to_string();
    let home_score_w = estimate_text_width_digits(&home_score_str, font_score);
    cmds.push(OverlayCommand::DrawText {
        x: score1_x + (score_box_w - home_score_w as i32) / 2,
        y: row2_score_baseline,
        text: home_score_str,
        font_id: params.bold_font,
        size_px: font_score,
        style: FontStyle::Bold,
        color: contrasting_text_color(home_color),
    });
    // Clock (centred on lightgray) - black text, gray when stopped.
    // Dropped the U+23F8 PAUSE prefix; the glyph cache can't
    // rasterize codepoints the registered font doesn't have, and
    // DejaVu / Liberation lack U+23F8 (operator saw an empty box
    // where they expected a minus). The `CLOCK_FG_STOPPED` gray-out
    // already conveys "paused" without a glyph that may not exist.
    if !state.game_clock_text.is_empty() {
        let clock_text = state.game_clock_text.clone();
        let clock_color = if state.clock_stopped {
            CLOCK_FG_STOPPED
        } else {
            TEXT_FG_DARK
        };
        let clock_w = estimate_text_width_digits(&clock_text, font_score);
        cmds.push(OverlayCommand::DrawText {
            x: clock_x + (clock_box_w - clock_w as i32) / 2,
            y: row2_score_baseline,
            text: clock_text,
            font_id: params.bold_font,
            size_px: font_score,
            style: FontStyle::Bold,
            color: clock_color,
        });
    }
    // Away score: same contrast-adaptive treatment as home.
    let away_score_str = state.away_score.to_string();
    let away_score_w = estimate_text_width_digits(&away_score_str, font_score);
    cmds.push(OverlayCommand::DrawText {
        x: score2_x + (score_box_w - away_score_w as i32) / 2,
        y: row2_score_baseline,
        text: away_score_str,
        font_id: params.bold_font,
        size_px: font_score,
        style: FontStyle::Bold,
        color: contrasting_text_color(away_color),
    });
    // Away team name (left-aligned to score-block right edge)
    cmds.push(OverlayCommand::DrawText {
        x: name2_x + text_pad,
        y: row2_baseline,
        text: truncate_for_display(
            &state.away_team_name,
            (name_w - 2 * text_pad).max(0) as u32,
            font_name,
        ),
        font_id: params.bold_font,
        size_px: font_name,
        style: FontStyle::Bold,
        color: TEXT_FG_DARK,
    });

    // 5) row 3: state text on lightgray strip, sized to text + padding
    if !state.state_text.is_empty() {
        let row3_y = sb_y + 2 * row_h + 2 * row_gap;
        let baseline = row3_y + (row_h + font_state as i32) / 2 - s(2.0);
        let txt_w = estimate_text_width(&state.state_text, font_state) as i32;
        let pad = text_pad;
        let bg_x = sb_x + (total_w - txt_w - 2 * pad) / 2;
        cmds.push(OverlayCommand::FillRect {
            x: bg_x,
            y: row3_y,
            width: txt_w + 2 * pad,
            height: row_h,
            color: LABEL_BG,
        });
        cmds.push(OverlayCommand::DrawText {
            x: bg_x + pad,
            y: baseline,
            text: state.state_text.clone(),
            font_id: params.regular_font,
            size_px: font_state,
            style: FontStyle::Regular,
            color: TEXT_FG_DARK,
        });
    }
}

#[derive(Copy, Clone, Eq, PartialEq)]
enum Side {
    Left,
    Right,
}

/// Row-1 side cell on the centre scoreboard: shoot-out dots when the
/// match is in shoot-out, otherwise a bare message-budget number on
/// a lightgray strip.
#[allow(clippy::too_many_arguments)]
fn push_row1_side(
    cmds: &mut Vec<OverlayCommand>,
    params: &LayoutParams,
    scale: f32,
    shootout: Option<&ShootoutState>,
    side: Side,
    message_budget: u32,
    team_color: Rgba,
    row_y: i32,
    center_x: i32,
    row_h: i32,
    baseline_y: i32,
    font_phase: u32,
) {
    let s = |v: f32| -> i32 { (v * scale).round() as i32 };
    if let Some(so) = shootout {
        let (mine_shot, mine_mask, theirs_shot) = match side {
            Side::Left => (
                so.home_penalty_shot,
                so.home_single_shots,
                so.away_penalty_shot,
            ),
            Side::Right => (
                so.away_penalty_shot,
                so.away_single_shots,
                so.home_penalty_shot,
            ),
        };
        let total = mine_shot.max(theirs_shot);
        if total == 0 || mine_shot == 0 {
            return;
        }
        let dot_r = s(SHOOTOUT_DOT_RADIUS_REF);
        let dot_d = dot_r * 2;
        let gap = s(SHOOTOUT_DOT_GAP_REF);
        let total_w = total as i32 * dot_d + (total as i32 - 1).max(0) * gap;
        let top_y = baseline_y - dot_d - s(2.0);
        let mut x = center_x - total_w / 2;
        for i in 0..mine_shot {
            let made = (mine_mask >> i) & 1 == 1;
            let color = if made {
                team_color
            } else {
                Rgba::opaque(102, 102, 102) // empty / miss
            };
            cmds.push(OverlayCommand::FillRect {
                x,
                y: top_y,
                width: dot_d,
                height: dot_d,
                color,
            });
            x += dot_d + gap;
        }
        return;
    }

    // Message-budget readout when no shoot-out. Bare digits on a
    // lightgray strip - operator wanted the "msg " prefix dropped.
    // Uses the same fixed padding as the phase label / team-name
    // cells so all row-1 backings look consistent.
    if message_budget == 0 {
        return;
    }
    let label = message_budget.to_string();
    let w = estimate_text_width_digits(&label, font_phase) as i32;
    let pad = s(SB_TEXT_PAD_REF);
    let bg_x = center_x - (w + 2 * pad) / 2;
    cmds.push(OverlayCommand::FillRect {
        x: bg_x,
        y: row_y,
        width: w + 2 * pad,
        height: row_h,
        color: LABEL_BG,
    });
    cmds.push(OverlayCommand::DrawText {
        x: bg_x + pad,
        y: baseline_y,
        text: label,
        font_id: params.bold_font,
        size_px: font_phase,
        style: FontStyle::Bold,
        color: TEXT_FG_DARK,
    });
}

fn push_penalty_column(
    cmds: &mut Vec<OverlayCommand>,
    params: &LayoutParams,
    scale: f32,
    side: Side,
    tiles: &[PenaltyTile],
    team_color: Rgba,
) {
    if tiles.is_empty() {
        return;
    }
    let s = |v: f32| -> i32 { (v * scale).round() as i32 };
    let tile_w = (params.sizes.penalty_tile_w * scale).round() as i32;
    let tile_h = (params.sizes.penalty_tile_h * scale).round() as i32;
    let gap = s(PENALTY_GAP_REF);
    let margin = s(MARGIN_REF);
    let row_pad = s(PENALTY_ROW_PAD_REF);
    let font_num = (params.sizes.font_penalty_num * scale).round() as u32;
    let font_time = (params.sizes.font_penalty_time * scale).round() as u32;
    let font_reason = (params.sizes.font_penalty_reason * scale).round() as u32;

    // 2-row layout:
    //   row 1 = max(font_num, font_time) + padding (≈ 2/3 of tile_h)
    //   row 2 = remainder (the reason row)
    let row1_h = (tile_h * 2 / 3).max(font_num.max(font_time) as i32 + 2 * row_pad);
    let row2_h = tile_h - row1_h;

    // Player-number box: fixed width sized for "88" at bold + a bit
    // of padding. Stays constant for both single- and double-digit
    // numbers so the timer cell width doesn't jitter.
    let player_num_box_w = estimate_text_width_digits("88", font_num) as i32 + 2 * row_pad;
    let timer_cell_w = (tile_w - player_num_box_w).max(0);

    // Anchor: bottom-left or bottom-right corner of the frame, stacked upward.
    let tile_x = match side {
        Side::Left => margin,
        Side::Right => params.frame_width as i32 - margin - tile_w,
    };
    let mut tile_y = params.frame_height as i32 - margin - tile_h;
    let bottom_limit = margin + s(120.0); // don't crowd the top

    for tile in tiles {
        if tile_y < bottom_limit {
            break;
        }
        // Row 1 left cell: team-colour box, fixed width, white bold
        // player number.
        cmds.push(OverlayCommand::FillRect {
            x: tile_x,
            y: tile_y,
            width: player_num_box_w,
            height: row1_h,
            color: team_color,
        });
        // Row 1 right cell: opaque lightgray, black non-bold timer.
        cmds.push(OverlayCommand::FillRect {
            x: tile_x + player_num_box_w,
            y: tile_y,
            width: timer_cell_w,
            height: row1_h,
            color: LABEL_BG,
        });

        let row1_baseline = tile_y + (row1_h + font_num as i32) / 2 - s(2.0);

        // GK badge: small bold "GK" stuck in the top-left of the
        // player-number box. Doesn't push the number off-centre
        // because the number is centred in the whole cell anyway.
        if tile.is_goalkeeper {
            let badge_font = (font_reason as f32 * 0.85).max(10.0).round() as u32;
            let badge_text = "GK";
            let bw = estimate_text_width(badge_text, badge_font) as i32 + s(6.0);
            let bh = badge_font as i32 + s(4.0);
            let bx = tile_x + s(3.0);
            let by = tile_y + s(3.0);
            cmds.push(OverlayCommand::FillRect {
                x: bx,
                y: by,
                width: bw,
                height: bh,
                color: Rgba::opaque(12, 12, 12),
            });
            cmds.push(OverlayCommand::DrawText {
                x: bx + s(3.0),
                y: by + badge_font as i32 + s(1.0),
                text: badge_text.into(),
                font_id: params.bold_font,
                size_px: badge_font,
                style: FontStyle::Bold,
                color: TEXT_FG_LIGHT,
            });
        }

        // Player number bold, centred in the team-colour box.
        // Contrast-adaptive text colour so bright jerseys
        // (white / yellow / cyan) don't render an invisible white
        // digit.
        let num_str = tile.player_number.to_string();
        let num_w = estimate_text_width_digits(&num_str, font_num);
        cmds.push(OverlayCommand::DrawText {
            x: tile_x + (player_num_box_w - num_w as i32) / 2,
            y: row1_baseline,
            text: num_str,
            font_id: params.bold_font,
            size_px: font_num,
            style: FontStyle::Bold,
            color: contrasting_text_color(team_color),
        });
        // Timer - black non-bold, centred in the lightgray cell.
        let mm = tile.secs_remaining / 60;
        let ss = tile.secs_remaining % 60;
        let countdown = format!("{mm:02}:{ss:02}");
        let cd_w = estimate_text_width_digits(&countdown, font_time);
        cmds.push(OverlayCommand::DrawText {
            x: tile_x + player_num_box_w + (timer_cell_w - cd_w as i32) / 2,
            y: row1_baseline,
            text: countdown,
            font_id: params.regular_font,
            size_px: font_time,
            style: FontStyle::Regular,
            color: TEXT_FG_DARK,
        });

        // Row 2: opaque lightgray, reason left-aligned with the
        // player-number box left edge (= tile_x + row_pad).
        let row2_y = tile_y + row1_h;
        cmds.push(OverlayCommand::FillRect {
            x: tile_x,
            y: row2_y,
            width: tile_w,
            height: row2_h,
            color: LABEL_BG,
        });
        if !tile.penalty_reason.is_empty() {
            let reason = truncate_for_display(
                &tile.penalty_reason,
                (tile_w - 2 * row_pad).max(0) as u32,
                font_reason,
            );
            let row2_baseline = row2_y + (row2_h + font_reason as i32) / 2 - s(2.0);
            cmds.push(OverlayCommand::DrawText {
                x: tile_x + row_pad,
                y: row2_baseline,
                text: reason,
                font_id: params.regular_font,
                size_px: font_reason,
                style: FontStyle::Regular,
                color: TEXT_FG_DARK,
            });
        }

        tile_y -= tile_h + gap;
    }
}

/// Whether a goalkeeper colour is present AND distinct from the
/// field-player colour. Cairo overlay rule: when GK matches the
/// field colour, the strip is suppressed so it doesn't read as a
/// wider field-colour block.
fn gk_strip_distinct(gk: Option<Rgba>, field: Option<Rgba>) -> bool {
    match (gk, field) {
        (Some(g), Some(f)) => g != f,
        (Some(_), None) => true,
        _ => false,
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Rough text width estimate for proportional Sans. The renderer
/// clips to the frame, so over-estimating is benign; under-estimating
/// would cause clipping. We err on the wide side.
fn estimate_text_width(text: &str, size_px: u32) -> u32 {
    // ~0.55 of size per character is the standard "ex-height" rough
    // estimate for Latin proportional Sans; bumping to 0.62 leaves
    // generous room so the layout pre-allocations don't clip the
    // rendered glyphs.
    let chars = text.chars().count() as f32;
    (chars * size_px as f32 * 0.62).ceil() as u32
}

/// Bold-Sans width estimate. Bold glyphs render ~10 % wider than
/// regular at the same point size, so reserving 0.62 (as we do for
/// regular) under-allocates and the team-name text overflows into
/// the score box. 0.7 buys back the margin.
fn estimate_text_width_bold(text: &str, size_px: u32) -> u32 {
    let chars = text.chars().count() as f32;
    (chars * size_px as f32 * 0.7).ceil() as u32
}

/// Digit-heavy strings (scores, clocks, countdowns) are typically
/// slightly wider per glyph than mixed-case strings.
fn estimate_text_width_digits(text: &str, size_px: u32) -> u32 {
    let chars = text.chars().count() as f32;
    (chars * size_px as f32 * 0.65).ceil() as u32
}

/// Trim a string so its estimated width is ≤ `max_width_px`. Adds an
/// ellipsis (`…`) when truncated. v1 doesn't measure precise glyph
/// widths, so the truncation point is approximate - same conservative
/// estimate the layout uses elsewhere.
fn truncate_for_display(text: &str, max_width_px: u32, size_px: u32) -> String {
    if estimate_text_width(text, size_px) <= max_width_px {
        return text.to_string();
    }
    let ellipsis_w = estimate_text_width("…", size_px);
    if ellipsis_w >= max_width_px {
        return String::new();
    }
    let mut out = String::new();
    for ch in text.chars() {
        out.push(ch);
        let proposed_width = estimate_text_width(&out, size_px) + ellipsis_w;
        if proposed_width > max_width_px {
            out.pop();
            out.push('…');
            return out;
        }
    }
    text.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::{validate_commands, OverlayCommand};
    use crate::glyphs::GlyphCache;

    const REGULAR: FontId = FontId(0);
    const BOLD: FontId = FontId(1);

    fn params_1080() -> LayoutParams {
        LayoutParams {
            frame_width: 1920,
            frame_height: 1080,
            regular_font: REGULAR,
            bold_font: BOLD,
            sizes: LayoutSizes::default(),
        }
    }

    fn params_720() -> LayoutParams {
        LayoutParams {
            frame_width: 1280,
            frame_height: 720,
            regular_font: REGULAR,
            bold_font: BOLD,
            sizes: LayoutSizes::default(),
        }
    }

    fn realistic_state() -> ScoreboardState {
        ScoreboardState {
            field_name: "FIELD E".into(),
            clock_text: "10:49:05".into(),
            home_team_name: "B-Human".into(),
            away_team_name: "Côme & Mars".into(),
            home_team_color: Some(Rgba::opaque(217, 25, 25)),
            away_team_color: Some(Rgba::opaque(25, 50, 217)),
            home_score: 3,
            away_score: 1,
            game_clock_text: "07:42".into(),
            phase_text: "FIRST HALF".into(),
            state_text: "PLAYING".into(),
            home_penalty_timers: vec![PenaltyTile {
                player_number: 4,
                secs_remaining: 27,
                penalty_reason: "PUSHING".into(),
                is_goalkeeper: false,
            }],
            away_penalty_timers: vec![
                PenaltyTile {
                    player_number: 2,
                    secs_remaining: 12,
                    penalty_reason: "BALL HOLDING".into(),
                    is_goalkeeper: true,
                },
                PenaltyTile {
                    player_number: 6,
                    secs_remaining: 43,
                    penalty_reason: "FOUL".into(),
                    is_goalkeeper: false,
                },
            ],
            ..Default::default()
        }
    }

    // -------- structural assertions --------

    #[test]
    fn layout_produces_commands_for_realistic_state() {
        let cmds = scoreboard_commands(&realistic_state(), &params_1080());
        // ~rough sanity: field pill + clock pill + scoreboard panel +
        // 2 team-colour blocks + ~5 row-2 texts + phase + state + 3
        // penalty tiles × 4 commands each. Should be well over a
        // dozen.
        assert!(cmds.len() > 14, "got {} commands", cmds.len());
    }

    #[test]
    fn layout_default_state_does_not_panic() {
        let cmds = scoreboard_commands(&ScoreboardState::default(), &params_1080());
        // No field name, no clock, no penalties - just the scoreboard
        // panel + 2 colour blocks + 4 row-2 score/name texts.
        assert!(!cmds.is_empty());
    }

    #[test]
    fn layout_validates_clean_against_a_two_font_cache() {
        let cache = {
            let mut c = GlyphCache::new().unwrap();
            // We don't need to actually rasterize - validate_commands
            // only checks `has_font(font_id, style)`. Register both
            // pairs against the same file probe.
            let candidates = [
                "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
                "/usr/share/fonts/truetype/liberation/LiberationSans-Regular.ttf",
            ];
            let mut path = None;
            for p in candidates {
                if std::path::Path::new(p).exists() {
                    path = Some(p);
                    break;
                }
            }
            let Some(path) = path else {
                eprintln!("[skip] no system font");
                return;
            };
            c.register_font(REGULAR, FontStyle::Regular, std::path::Path::new(path))
                .unwrap();
            c.register_font(BOLD, FontStyle::Bold, std::path::Path::new(path))
                .unwrap();
            c
        };
        let cmds = scoreboard_commands(&realistic_state(), &params_1080());
        let failures = validate_commands(&cache, &cmds);
        assert!(failures.is_empty(), "{failures:?}");
    }

    // -------- frame-bound containment --------

    /// Every command's *anchor* point sits inside the frame, plus its
    /// extent (for fill rects) stays within ~5 % overshoot - the
    /// renderer clips, but a layout that anchors texts off-screen is
    /// almost always a layout bug.
    fn assert_commands_mostly_in_frame(cmds: &[OverlayCommand], params: &LayoutParams) {
        let fw = params.frame_width as i32;
        let fh = params.frame_height as i32;
        for (i, cmd) in cmds.iter().enumerate() {
            match cmd {
                OverlayCommand::FillRect {
                    x,
                    y,
                    width,
                    height,
                    ..
                } => {
                    assert!(
                        *x >= 0 && *y >= 0,
                        "cmd #{i}: FillRect anchored off-screen at ({x},{y})"
                    );
                    // Width / height can overshoot by clipping; the
                    // assertion is that *most* of the rect lies in
                    // frame.
                    assert!(
                        *x < fw && *y < fh,
                        "cmd #{i}: FillRect origin past frame far edge"
                    );
                    assert!(*x + *width <= fw + fw / 20);
                    assert!(*y + *height <= fh + fh / 20);
                }
                OverlayCommand::DrawText { x, y, .. } => {
                    assert!(
                        *y >= 0 && *y < fh,
                        "cmd #{i}: text baseline @ y={y} out of frame"
                    );
                    assert!(
                        *x >= 0 && *x < fw,
                        "cmd #{i}: text origin @ x={x} out of frame"
                    );
                }
            }
        }
    }

    #[test]
    fn realistic_state_stays_in_frame_at_1080p() {
        let cmds = scoreboard_commands(&realistic_state(), &params_1080());
        assert_commands_mostly_in_frame(&cmds, &params_1080());
    }

    #[test]
    fn realistic_state_stays_in_frame_at_720p() {
        let cmds = scoreboard_commands(&realistic_state(), &params_720());
        assert_commands_mostly_in_frame(&cmds, &params_720());
    }

    // -------- long-name handling --------

    #[test]
    fn long_team_names_get_truncated_to_fit() {
        let mut state = realistic_state();
        state.home_team_name = "AldebaranSuperLongTeamName".repeat(3);
        state.away_team_name = "AnotherSuperLongTeamName".repeat(3);
        let cmds = scoreboard_commands(&state, &params_1080());
        assert_commands_mostly_in_frame(&cmds, &params_1080());

        // Find the row-2 name DrawText commands and confirm their
        // displayed text was truncated (ends with the ellipsis).
        let mut truncated_seen = false;
        for cmd in &cmds {
            if let OverlayCommand::DrawText { text, .. } = cmd {
                if text.ends_with('…') {
                    truncated_seen = true;
                }
            }
        }
        assert!(
            truncated_seen,
            "expected at least one DrawText command to be truncated to '…'"
        );
    }

    #[test]
    fn truncate_for_display_uses_ellipsis_and_fits() {
        let t = truncate_for_display("Aldebaran NAO Devils", 80, 22);
        assert!(
            t.ends_with('…'),
            "expected truncation to add ellipsis, got {t:?}"
        );
        assert!(estimate_text_width(&t, 22) <= 80);
    }

    #[test]
    fn truncate_for_display_passes_through_short_text() {
        let t = truncate_for_display("OK", 200, 22);
        assert_eq!(t, "OK");
    }

    // -------- penalty column overflow --------

    #[test]
    fn penalty_column_caps_at_frame_top() {
        let mut state = ScoreboardState {
            field_name: "F".into(),
            home_team_name: "H".into(),
            away_team_name: "A".into(),
            ..Default::default()
        };
        // 50 penalised robots - far more than will fit in the column.
        // Layout must NOT panic and must NOT push tiles off the top.
        state.home_penalty_timers = (0..50)
            .map(|i| PenaltyTile {
                player_number: i,
                secs_remaining: 30,
                ..Default::default()
            })
            .collect();
        let cmds = scoreboard_commands(&state, &params_1080());
        assert_commands_mostly_in_frame(&cmds, &params_1080());
    }

    // -------- ordering of paint commands --------

    #[test]
    fn paint_order_puts_backings_before_their_text() {
        // The pill background must precede the pill text in the
        // returned list, otherwise dispatch_commands would paint the
        // pill on top of its text.
        let state = realistic_state();
        let cmds = scoreboard_commands(&state, &params_1080());
        let field_text_idx = cmds
            .iter()
            .position(|c| matches!(c, OverlayCommand::DrawText { text, .. } if text == "FIELD E"));
        let mut field_rect_idx = None;
        for (i, c) in cmds.iter().enumerate() {
            if let OverlayCommand::FillRect { color, .. } = c {
                if *color == PILL_BG {
                    field_rect_idx = Some(i);
                    break;
                }
            }
        }
        let Some(text_idx) = field_text_idx else {
            panic!("field-name DrawText not emitted");
        };
        let Some(rect_idx) = field_rect_idx else {
            panic!("field-name pill FillRect not emitted");
        };
        assert!(
            rect_idx < text_idx,
            "field pill background (#{rect_idx}) must precede text (#{text_idx})"
        );
    }
}
