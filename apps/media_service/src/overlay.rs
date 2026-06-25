// Defines Rust configuration and serialization logic for the media service.
// Author: Thomas Klute

//! Broadcast overlay - structured game state for the cairooverlay element.
//!
//! The streaming branch uses a `cairooverlay` element that calls our
//! [`draw_overlay`] function on every frame. The overlay reads the shared
//! [`OverlayState`] (populated from ZMQ game state messages) and draws
//! a broadcast-quality scoreboard:
//!
//! - Top-left:  field name pill
//! - Top-right: wall-clock time pill
//! - Bottom-center: 3-row scoreboard (packets, score+clock, game state)
//!
//! Team numbers are resolved to names via `config/teams.json`.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use serde::{Deserialize, Serialize};
use tracing::{info, warn};

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
    /// Optional jersey colour for team 1's score box; `None` means use
    /// the scoreboard's red fallback.
    pub team1_color: Option<(f64, f64, f64)>,
    /// Optional jersey colour for team 2's score box; `None` means use
    /// the scoreboard's blue fallback.
    pub team2_color: Option<(f64, f64, f64)>,
    pub team1_score: u32,
    pub team2_score: u32,
    pub team1_number: u32,
    pub team2_number: u32,
    /// true = 1st half, false = 2nd half
    pub first_half: bool,
    pub secs_remaining: i32,
    pub game_phase: u32,
    /// Legacy GC top-level packet counter (incrementing per broadcast).
    /// Kept for backwards compat with older publishers; the scoreboard
    /// uses the per-team `team{1,2}_packet_number` values below.
    pub packet_number: u32,
    /// Per-team `messageBudget` from the HSL TeamInfo block -
    /// the "packets remaining" int16 each team's robots draw down as
    /// they transmit (~1200 at match start). Rendered on row 1 of the
    /// scoreboard with a small "msg" prefix.
    pub team1_message_budget: u32,
    pub team2_message_budget: u32,
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
    /// GC's `stopped` flag. True during referee timeouts
    /// and the GC stop button. The scoreboard dims the clock text
    /// and prefixes a pause glyph so operators see play is paused
    /// instead of believing the clock is still ticking.
    pub stopped: bool,
    /// GC secondary clock (ready countdown, free-kick wait,
    /// half-time break). 0 when no secondary timer is active.
    pub secondary_time: i32,
    /// Per-team goalkeeper player number (1-based). 0
    /// when the GC hasn't assigned one yet (pre-game or in INITIAL).
    pub team1_goalkeeper: u32,
    pub team2_goalkeeper: u32,
    /// Optional jersey colour for each team's goalkeeper,
    /// rendered as a thin strip adjacent to the row-2 outer team-colour
    /// block. `None` when the publisher doesn't carry the field or when
    /// it matches the field-player colour (no useful indicator).
    pub team1_goalkeeper_color: Option<(f64, f64, f64)>,
    pub team2_goalkeeper_color: Option<(f64, f64, f64)>,
    /// Shootout state - current shot index (1-based) and
    /// per-attempt outcome bitmask (bit i = i-th shot scored). Both
    /// 0 outside the penalty-shootout phase.
    pub team1_penalty_shot: u32,
    pub team1_single_shots: u32,
    pub team2_penalty_shot: u32,
    pub team2_single_shots: u32,
    /// Active penalty cards for the home and away teams,
    /// populated from the `telemetry.penalties` topic. Empty when no
    /// robots are penalised.
    pub team1_penalties: Vec<PenaltyCard>,
    pub team2_penalties: Vec<PenaltyCard>,
}

impl Default for GameOverlayData {
    fn default() -> Self {
        Self {
            team1_name: "Home".into(),
            team2_name: "Away".into(),
            team1_color: None,
            team2_color: None,
            team1_score: 0,
            team2_score: 0,
            team1_number: 0,
            team2_number: 0,
            first_half: true,
            secs_remaining: 600,
            game_phase: 0,
            packet_number: 0,
            team1_message_budget: 0,
            team2_message_budget: 0,
            state: "INITIAL".into(),
            set_play: "NONE".into(),
            kicking_team: 0,
            field_name: "FIELD A".into(),
            is_live: false,
            stopped: false,
            secondary_time: 0,
            team1_goalkeeper: 0,
            team2_goalkeeper: 0,
            team1_goalkeeper_color: None,
            team2_goalkeeper_color: None,
            team1_penalty_shot: 0,
            team1_single_shots: 0,
            team2_penalty_shot: 0,
            team2_single_shots: 0,
            team1_penalties: Vec::new(),
            team2_penalties: Vec::new(),
        }
    }
}

/// Sentinel value the GC broadcasts in `kickingTeam` when no team is
/// the next kicker (e.g. between phases, after a goal before the
/// kickoff is re-set). Normalised to `0` in `parse_game_state` so
/// downstream rendering can treat "no kicker" uniformly.
const KICKING_TEAM_NONE: u32 = 255;

// HSL `gamePhase` byte values, mirroring
// `apps/schemas/game_state.py::GamePhase`. Module-level so the row-1
// rendering branch and `format_game_phase` agree on what
// `1 == PENALTY_SHOOTOUT` means.
const GAME_PHASE_NORMAL: u32 = 0;
const GAME_PHASE_PENALTY_SHOOTOUT: u32 = 1;
const GAME_PHASE_OVERTIME: u32 = 2;
const GAME_PHASE_TIMEOUT: u32 = 3;

/// Maximum number of shoot-out dots we ever draw per team
/// on row 1. Standard SPL/HSL shoot-outs settle in 3-5 attempts; the
/// cap exists so a pathological `penaltyShot` value doesn't cause the
/// dot row to overflow the scoreboard width. If a real shoot-out ever
/// exceeds this, the cap should be revisited rather than silently
/// truncating useful information.
pub const SHOOTOUT_DOT_CAP: u32 = 10;

pub type OverlayState = Arc<RwLock<GameOverlayData>>;

/// Create a new overlay state with defaults.
pub fn new_overlay_state() -> OverlayState {
    Arc::new(RwLock::new(GameOverlayData::default()))
}

// ---------------------------------------------------------------------------
// Penalty cards
// ---------------------------------------------------------------------------

/// One active penalty card shown at the bottom-left (home) or
/// bottom-right (away) of the broadcast overlay. Built from the
/// `telemetry.penalties` topic (see `apps/schemas/penalties.py`),
/// after filtering out no-penalty entries.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PenaltyCard {
    /// 1-based player number within the team.
    pub player_number: u32,
    /// Decoded penalty label (e.g. "Pushing", "Substitute") per the
    /// Python `gamecontroller.PENALTY_LABELS` table, or the fallback
    /// "code=<n>" for codes outside the HSL v20 spec.
    pub penalty_reason: String,
    /// Countdown timer in seconds. 0 means the penalty is active but
    /// the timer is stopped (e.g. waiting for a manual unpenalise).
    pub secs_remaining: u32,
}

// ---------------------------------------------------------------------------
// Teams map
// ---------------------------------------------------------------------------

/// Per-team info: human-readable name + optional jersey colour for the
/// scoreboard's score-box background. Colour is stored as (r, g, b) in
/// the [0.0, 1.0] range, matching Cairo's `set_source_rgb` API.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TeamInfo {
    pub name: String,
    /// Optional jersey colour. When absent, the score box uses the
    /// fallback red/blue defaults in `draw_scoreboard`.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub color: Option<(f64, f64, f64)>,
}

impl TeamInfo {
    /// Convenience constructor for tests and call sites that only have a
    /// name (no jersey colour).
    pub fn from_name<S: Into<String>>(name: S) -> Self {
        Self {
            name: name.into(),
            color: None,
        }
    }
}

/// Team number → `TeamInfo` map loaded from `config/teams.json`.
///
/// JSON format (each entry is either a plain string or an object):
///
/// ```json
/// {
///   "5":  "B-Human",
///   "14": {"name": "HTWK Robots", "color": "#cc0000"}
/// }
/// ```
///
/// The plain-string form is the original schema and continues to work
/// (the team has no jersey colour override and falls back to red/blue
/// in the scoreboard).
pub type TeamsMap = HashMap<u32, TeamInfo>;

/// Parse a `#RRGGBB` (or `RRGGBB`) hex string into a normalised
/// `(r, g, b)` tuple. Returns `None` for anything that doesn't match.
fn parse_hex_color(s: &str) -> Option<(f64, f64, f64)> {
    let hex = s.trim().trim_start_matches('#');
    if hex.len() != 6 {
        return None;
    }
    let r = u8::from_str_radix(&hex[0..2], 16).ok()?;
    let g = u8::from_str_radix(&hex[2..4], 16).ok()?;
    let b = u8::from_str_radix(&hex[4..6], 16).ok()?;
    Some((r as f64 / 255.0, g as f64 / 255.0, b as f64 / 255.0))
}

/// Load teams map from a JSON file. Each value can be either a plain
/// string (legacy schema) or an object `{"name": str, "color": "#hex"}`.
/// Unparseable entries are dropped with a warning; the rest are kept.
pub fn load_teams_map(path: &str) -> TeamsMap {
    let mut map = TeamsMap::new();
    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(e) => {
            warn!(error = %e, path, "Failed to read teams.json");
            return map;
        }
    };
    let raw: HashMap<String, serde_json::Value> = match serde_json::from_str(&content) {
        Ok(v) => v,
        Err(e) => {
            warn!(error = %e, path, "Failed to parse teams.json");
            return map;
        }
    };
    for (k, v) in raw {
        let Ok(num) = k.parse::<u32>() else {
            warn!(key = %k, "teams.json: non-integer team key, skipping");
            continue;
        };
        let info = match &v {
            serde_json::Value::String(s) => TeamInfo::from_name(s.clone()),
            serde_json::Value::Object(_) => {
                let name = v
                    .get("name")
                    .and_then(|n| n.as_str())
                    .map(|s| s.to_string());
                let Some(name) = name else {
                    warn!(
                        team = num,
                        "teams.json: object entry missing `name`, skipping"
                    );
                    continue;
                };
                let color = v
                    .get("color")
                    .and_then(|c| c.as_str())
                    .and_then(parse_hex_color);
                TeamInfo { name, color }
            }
            other => {
                warn!(team = num, value = %other, "teams.json: unexpected entry type, skipping");
                continue;
            }
        };
        map.insert(num, info);
    }
    info!(count = map.len(), path, "Loaded teams map");
    map
}

/// Resolve a team number to a name using the teams map.
pub fn resolve_team_name(teams: &TeamsMap, number: u32) -> String {
    teams
        .get(&number)
        .map(|t| t.name.clone())
        .unwrap_or_else(|| format!("Team {}", number))
}

/// Resolve a team number to a jersey colour, if configured.
pub fn resolve_team_color(teams: &TeamsMap, number: u32) -> Option<(f64, f64, f64)> {
    teams.get(&number).and_then(|t| t.color)
}

/// Map the HSL TeamInfo `fieldPlayerColour` enum byte to an
/// approximate (r, g, b). Mirrors :py:meth:`apps.schemas.game_state.TeamColour.to_rgb`.
/// Used as the packet-driven fallback when `config/teams.json` has no
/// entry for the team number. Returns `None` for bytes outside the
/// spec (0..9), so an unexpected wire value falls through to the
/// scoreboard's hard-coded red/blue defaults rather than rendering
/// something arbitrary.
pub fn team_colour_byte_to_rgb(byte: u32) -> Option<(f64, f64, f64)> {
    match byte {
        0 => Some((0.10, 0.40, 0.90)), // BLUE
        1 => Some((0.85, 0.15, 0.15)), // RED
        2 => Some((0.95, 0.85, 0.15)), // YELLOW
        3 => Some((0.10, 0.10, 0.10)), // BLACK
        4 => Some((0.95, 0.95, 0.95)), // WHITE
        5 => Some((0.15, 0.70, 0.30)), // GREEN
        6 => Some((0.95, 0.55, 0.10)), // ORANGE
        7 => Some((0.55, 0.20, 0.70)), // PURPLE
        8 => Some((0.45, 0.30, 0.15)), // BROWN
        9 => Some((0.55, 0.55, 0.55)), // GRAY
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// ZMQ subscriber
// ---------------------------------------------------------------------------

/// Merge a freshly-parsed game state into the existing overlay,
/// preserving the fields the GameController packet does not carry:
/// the per-robot penalty cards (their own publisher cadence, updated
/// by the `telemetry.penalties` branch) and the operator-configured
/// field name (seeded from `video.streaming.field_name` at startup,
/// live-editable via `PUT /overlay/text`).
fn apply_game_state(overlay: &mut GameOverlayData, mut data: GameOverlayData) {
    data.team1_penalties = std::mem::take(&mut overlay.team1_penalties);
    data.team2_penalties = std::mem::take(&mut overlay.team2_penalties);
    data.field_name = std::mem::take(&mut overlay.field_name);
    *overlay = data;
}

/// Start a background ZMQ subscriber that listens for game state and
/// penalty updates and populates the structured overlay data.
///
/// Two topics are subscribed:
/// - `telemetry.game_state` - scoreboard / clock / state fields.
/// - `telemetry.penalties` - per-robot cards. Penalty arrays
///   are written in-place so a state-change-gated game_state event
///   doesn't clobber the latest penalty snapshot.
pub fn start_overlay_subscriber(state: OverlayState, zmq_endpoint: &str, teams: TeamsMap) {
    let endpoint = zmq_endpoint.to_string();

    std::thread::spawn(move || {
        let ctx = zmq::Context::new();
        let socket = match ctx.socket(zmq::SUB) {
            Ok(s) => s,
            Err(e) => {
                warn!(error = %e, "Overlay: failed to create ZMQ SUB socket");
                return;
            }
        };
        if let Err(e) = socket.connect(&endpoint) {
            warn!(error = %e, endpoint, "Overlay: failed to connect to ZMQ broker");
            return;
        }
        let _ = socket.set_subscribe(b"telemetry.game_state");
        let _ = socket.set_subscribe(b"telemetry.penalties");
        info!(endpoint, "Overlay: ZMQ subscriber connected");

        loop {
            let topic = match socket.recv_string(0) {
                Ok(Ok(t)) => t,
                _ => continue,
            };
            let payload = match socket.recv_string(0) {
                Ok(Ok(p)) => p,
                _ => continue,
            };

            let Ok(json) = serde_json::from_str::<serde_json::Value>(&payload) else {
                continue;
            };

            match topic.as_str() {
                "telemetry.game_state" => {
                    let data = parse_game_state(&json, &teams);
                    if let Ok(mut overlay) = state.write() {
                        apply_game_state(&mut overlay, data);
                    }
                }
                "telemetry.penalties" => {
                    let (team1, team2) = parse_penalties(&json);
                    if let Ok(mut overlay) = state.write() {
                        overlay.team1_penalties = team1;
                        overlay.team2_penalties = team2;
                    }
                }
                _ => {}
            }
        }
    });
}

/// Parse a `telemetry.penalties` JSON payload into per-team
/// `PenaltyCard` lists. Filters out no-penalty entries
/// (`penalty_code == 0`) so the drawing code can iterate without a
/// guard, and sorts the surviving cards by `secs_remaining` descending
/// so the longest-pending penalty sits at the top of the stack
/// (matches the long-standing broadcast scoreboard convention).
fn parse_penalties(p: &serde_json::Value) -> (Vec<PenaltyCard>, Vec<PenaltyCard>) {
    fn collect(arr: &serde_json::Value) -> Vec<PenaltyCard> {
        let Some(items) = arr.as_array() else {
            return Vec::new();
        };
        let mut out: Vec<PenaltyCard> = items
            .iter()
            .filter(|e| {
                e.get("penalty_code")
                    .and_then(|v| v.as_u64())
                    .map(|c| c != 0)
                    .unwrap_or(false)
            })
            .map(|e| PenaltyCard {
                player_number: e.get("player_number").and_then(|v| v.as_u64()).unwrap_or(0) as u32,
                penalty_reason: e
                    .get("penalty_reason")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
                secs_remaining: e
                    .get("secs_remaining")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0) as u32,
            })
            .collect();
        // Stable sort by secs_remaining desc; player_number breaks ties
        // deterministically so the on-screen order doesn't flicker when
        // two robots happen to share a remaining time.
        out.sort_by(|a, b| {
            b.secs_remaining
                .cmp(&a.secs_remaining)
                .then(a.player_number.cmp(&b.player_number))
        });
        out
    }
    (
        collect(p.get("team1_penalties").unwrap_or(&serde_json::Value::Null)),
        collect(p.get("team2_penalties").unwrap_or(&serde_json::Value::Null)),
    )
}

/// Parse a game state JSON into structured overlay data.
fn parse_game_state(gs: &serde_json::Value, teams: &TeamsMap) -> GameOverlayData {
    let team1_number = gs.get("team1_number").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
    let team2_number = gs.get("team2_number").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
    // Packet-driven jersey colour fallback. Only consult it
    // when `teams.json` has no override for the number AND the
    // publisher actually emitted the field (Some on the JSON get).
    let team1_field_colour = gs
        .get("team1_field_player_colour")
        .and_then(|v| v.as_u64())
        .map(|n| n as u32);
    let team2_field_colour = gs
        .get("team2_field_player_colour")
        .and_then(|v| v.as_u64())
        .map(|n| n as u32);
    let team1_color = resolve_team_color(teams, team1_number)
        .or_else(|| team1_field_colour.and_then(team_colour_byte_to_rgb));
    let team2_color = resolve_team_color(teams, team2_number)
        .or_else(|| team2_field_colour.and_then(team_colour_byte_to_rgb));

    // Goalkeeper-colour strip. Only meaningful when the
    // publisher carries a value AND the byte differs from the
    // field-player colour byte. Two strips of the same colour look
    // like one wider strip - surface them only when they actually
    // tell the operator something.
    let team1_gk_colour_byte = gs
        .get("team1_goalkeeper_colour")
        .and_then(|v| v.as_u64())
        .map(|n| n as u32);
    let team2_gk_colour_byte = gs
        .get("team2_goalkeeper_colour")
        .and_then(|v| v.as_u64())
        .map(|n| n as u32);
    let team1_goalkeeper_color = match (team1_gk_colour_byte, team1_field_colour) {
        (Some(gk), Some(field)) if gk == field => None,
        (Some(gk), _) => team_colour_byte_to_rgb(gk),
        _ => None,
    };
    let team2_goalkeeper_color = match (team2_gk_colour_byte, team2_field_colour) {
        (Some(gk), Some(field)) if gk == field => None,
        (Some(gk), _) => team_colour_byte_to_rgb(gk),
        _ => None,
    };

    GameOverlayData {
        team1_name: resolve_team_name(teams, team1_number),
        team2_name: resolve_team_name(teams, team2_number),
        team1_color,
        team2_color,
        team1_score: gs.get("team1_score").and_then(|v| v.as_u64()).unwrap_or(0) as u32,
        team2_score: gs.get("team2_score").and_then(|v| v.as_u64()).unwrap_or(0) as u32,
        team1_number,
        team2_number,
        first_half: gs
            .get("first_half")
            .and_then(|v| v.as_bool())
            .unwrap_or(true),
        secs_remaining: gs
            .get("secs_remaining")
            .and_then(|v| v.as_i64())
            .unwrap_or(600) as i32,
        game_phase: gs.get("game_phase").and_then(|v| v.as_u64()).unwrap_or(0) as u32,
        packet_number: gs
            .get("packet_number")
            .and_then(|v| v.as_u64())
            .unwrap_or(0) as u32,
        // Per-team message budget. The publisher emits the
        // current field name (`team{1,2}_message_budget`); for
        // backwards-compat we still accept the historical
        // `team{1,2}_packet_number` key so a downgraded
        // or out-of-sync publisher still feeds the scoreboard, and
        // ultimately fall back to the legacy top-level `packet_number`
        // so the cell never renders blank.
        team1_message_budget: gs
            .get("team1_message_budget")
            .and_then(|v| v.as_u64())
            .or_else(|| gs.get("team1_packet_number").and_then(|v| v.as_u64()))
            .or_else(|| gs.get("packet_number").and_then(|v| v.as_u64()))
            .unwrap_or(0) as u32,
        team2_message_budget: gs
            .get("team2_message_budget")
            .and_then(|v| v.as_u64())
            .or_else(|| gs.get("team2_packet_number").and_then(|v| v.as_u64()))
            .or_else(|| gs.get("packet_number").and_then(|v| v.as_u64()))
            .unwrap_or(0) as u32,
        state: gs
            .get("state")
            .and_then(|v| v.as_str())
            .unwrap_or("INITIAL")
            .to_string(),
        set_play: gs
            .get("set_play")
            .and_then(|v| v.as_str())
            .unwrap_or("NONE")
            .to_string(),
        kicking_team: {
            // The wire packet uses 255 (KICKING_TEAM_NONE) for
            // "no team is the next kicker yet". Normalise to 0 so the
            // downstream "kicking_team == team{1,2}_number" branches
            // can't accidentally render "Team 255" if a future schema
            // change ever allows team numbers > 100.
            let raw = gs.get("kicking_team").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
            if raw == KICKING_TEAM_NONE {
                0
            } else {
                raw
            }
        },
        field_name: "FIELD A".into(), // overridden from config at init
        is_live: true,
        stopped: gs.get("stopped").and_then(|v| v.as_bool()).unwrap_or(false),
        secondary_time: gs
            .get("secondary_time")
            .and_then(|v| v.as_i64())
            .unwrap_or(0) as i32,
        team1_goalkeeper: gs
            .get("team1_goalkeeper")
            .and_then(|v| v.as_u64())
            .unwrap_or(0) as u32,
        team2_goalkeeper: gs
            .get("team2_goalkeeper")
            .and_then(|v| v.as_u64())
            .unwrap_or(0) as u32,
        team1_goalkeeper_color,
        team2_goalkeeper_color,
        team1_penalty_shot: gs
            .get("team1_penalty_shot")
            .and_then(|v| v.as_u64())
            .unwrap_or(0) as u32,
        team1_single_shots: gs
            .get("team1_single_shots")
            .and_then(|v| v.as_u64())
            .unwrap_or(0) as u32,
        team2_penalty_shot: gs
            .get("team2_penalty_shot")
            .and_then(|v| v.as_u64())
            .unwrap_or(0) as u32,
        team2_single_shots: gs
            .get("team2_single_shots")
            .and_then(|v| v.as_u64())
            .unwrap_or(0) as u32,
        // Penalty arrays are populated by the `telemetry.penalties`
        // branch of the subscriber. When parse_game_state
        // is called from that subscriber, the caller restores the
        // existing values in-place before the swap.
        team1_penalties: Vec::new(),
        team2_penalties: Vec::new(),
    }
}

// ---------------------------------------------------------------------------
// Translate `GameOverlayData` into the layered renderer's
// `ScoreboardState` for the NV12-native overlay plugin. Lives here
// (rather than in the broadcast_overlay crate) because the
// translation is media-service-specific: `GameOverlayData` is the
// existing ZMQ-fed shape, and the cairo path is the producer-side
// source of truth.
// ---------------------------------------------------------------------------

/// Convert game state into the layered renderer's input shape. The
/// caller (the streaming session's overlay publisher) writes this into
/// the linked `RwLock<ScoreboardState>` whenever
/// [`OverlayState`] changes, and the in-pipeline
/// `aicamnv12overlay` reads it on the next frame.
pub fn scoreboard_state_from_game(
    data: &GameOverlayData,
) -> aicam_broadcast_overlay::layout::ScoreboardState {
    use aicam_broadcast_overlay::layout::{PenaltyTile, ScoreboardState, ShootoutState};
    use aicam_broadcast_overlay::renderer::color::Rgba;

    fn to_rgba(rgb: Option<(f64, f64, f64)>) -> Option<Rgba> {
        rgb.map(|(r, g, b)| {
            Rgba::opaque(
                (r.clamp(0.0, 1.0) * 255.0).round() as u8,
                (g.clamp(0.0, 1.0) * 255.0).round() as u8,
                (b.clamp(0.0, 1.0) * 255.0).round() as u8,
            )
        })
    }

    let now = chrono::Local::now();
    let clock_text = now.format("%H:%M:%S").to_string();

    let game_clock_text = format_mm_ss(data.secs_remaining);
    let phase_text = format_game_phase(data);
    let state_text = format_state_line(data);

    let penalty_tile = |p: &PenaltyCard, gk_number: u32| PenaltyTile {
        player_number: p.player_number,
        secs_remaining: p.secs_remaining,
        penalty_reason: p.penalty_reason.clone(),
        is_goalkeeper: gk_number != 0 && p.player_number == gk_number,
    };
    let home_penalty_timers = data
        .team1_penalties
        .iter()
        .map(|p| penalty_tile(p, data.team1_goalkeeper))
        .collect();
    let away_penalty_timers = data
        .team2_penalties
        .iter()
        .map(|p| penalty_tile(p, data.team2_goalkeeper))
        .collect();

    let shootout = if data.game_phase == GAME_PHASE_PENALTY_SHOOTOUT {
        Some(ShootoutState {
            home_penalty_shot: data.team1_penalty_shot,
            home_single_shots: data.team1_single_shots,
            away_penalty_shot: data.team2_penalty_shot,
            away_single_shots: data.team2_single_shots,
        })
    } else {
        None
    };

    ScoreboardState {
        field_name: data.field_name.clone(),
        clock_text,
        home_team_name: data.team1_name.clone(),
        away_team_name: data.team2_name.clone(),
        home_team_color: to_rgba(data.team1_color),
        away_team_color: to_rgba(data.team2_color),
        home_team_goalkeeper_color: to_rgba(data.team1_goalkeeper_color),
        away_team_goalkeeper_color: to_rgba(data.team2_goalkeeper_color),
        home_score: data.team1_score,
        away_score: data.team2_score,
        game_clock_text,
        clock_stopped: data.stopped,
        phase_text,
        state_text,
        home_message_budget: data.team1_message_budget,
        away_message_budget: data.team2_message_budget,
        shootout,
        home_penalty_timers,
        away_penalty_timers,
    }
}

fn format_mm_ss(secs: i32) -> String {
    if secs < 0 {
        format!("-{:02}:{:02}", -secs / 60, (-secs) % 60)
    } else {
        format!("{:02}:{:02}", secs / 60, secs % 60)
    }
}

/// Return black (0,0,0) or white (1,1,1) RGB depending on
/// which gives better contrast against `bg`. Uses ITU-R BT.601
/// perceptual luminance - keeps yellow / cyan / white team jerseys
/// readable, where the previous hardcoded-white text was invisible.
/// Threshold 0.6 leans toward white text to preserve the broadcast
/// "white on team colour" look for the typical dark-jersey case
/// (red, blue, dark green) and only flips to black for genuinely
/// bright backgrounds.
pub fn contrasting_text_rgb(bg: (f64, f64, f64)) -> (f64, f64, f64) {
    let (r, g, b) = bg;
    let l = 0.299 * r + 0.587 * g + 0.114 * b;
    if l > 0.6 {
        (0.0, 0.0, 0.0)
    } else {
        (1.0, 1.0, 1.0)
    }
}

// ---------------------------------------------------------------------------
// Cairo drawing
// ---------------------------------------------------------------------------

/// Draw the broadcast overlay onto a Cairo context.
///
/// Called from the cairooverlay `draw` signal on every frame.
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

    // ---- Bottom-left / bottom-right: penalty cards ----
    // Drawn AFTER the scoreboard so the side columns sit visually
    // above its background pill, matching the reference design.
    draw_penalty_cards(
        cr,
        height,
        margin,
        scale,
        font_sm,
        font_md,
        Side::Left,
        &data.team1_penalties,
        data.team1_color.unwrap_or((0.85, 0.1, 0.1)),
        margin,
        data.team1_goalkeeper,
    );
    draw_penalty_cards(
        cr,
        height,
        margin,
        scale,
        font_sm,
        font_md,
        Side::Right,
        &data.team2_penalties,
        data.team2_color.unwrap_or((0.1, 0.2, 0.85)),
        width - margin,
        data.team2_goalkeeper,
    );
}

/// Side of the frame a penalty-card column anchors to.
#[derive(Copy, Clone)]
enum Side {
    Left,
    Right,
}

/// Draw a stack of penalty cards. Cards stack upward from the bottom
/// margin, oldest at top. Empty lists draw nothing.
///
/// Each card has three rows:
/// 1. Player number on a team-colour background.
/// 2. Countdown timer ("MM:SS") on a light-grey background.
/// 3. Penalty reason text in a smaller font.
///
/// Sized off the reference layout: card
/// width ≈ 80 px and total height ≈ 60 px at the 960 px reference
/// resolution, scaled with `scale`.
#[allow(clippy::too_many_arguments)]
fn draw_penalty_cards(
    cr: &cairo::Context,
    height: f64,
    margin: f64,
    scale: f64,
    font_sm: f64,
    font_md: f64,
    side: Side,
    cards: &[PenaltyCard],
    team_color: (f64, f64, f64),
    x_anchor: f64,
    goalkeeper_number: u32,
) {
    if cards.is_empty() {
        return;
    }

    let card_w = 80.0 * scale;
    let row_h = 18.0 * scale;
    let card_h = row_h * 3.0;
    let gap = 4.0 * scale;
    let radius = 3.0 * scale;

    // Bottom-most card touches the bottom margin; each preceding card
    // stacks above it. Iterate the list in render order so index 0
    // ends up nearest the bottom.
    let total_h = (card_h + gap) * cards.len() as f64 - gap;
    let mut y = height - margin - total_h;

    for card in cards {
        let x = match side {
            Side::Left => x_anchor,
            Side::Right => x_anchor - card_w,
        };
        let is_goalkeeper = goalkeeper_number != 0 && card.player_number == goalkeeper_number;
        draw_one_penalty_card(
            cr,
            x,
            y,
            card_w,
            row_h,
            radius,
            font_sm,
            font_md,
            card,
            team_color,
            is_goalkeeper,
        );
        y += card_h + gap;
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_one_penalty_card(
    cr: &cairo::Context,
    x: f64,
    y: f64,
    w: f64,
    row_h: f64,
    radius: f64,
    font_sm: f64,
    font_md: f64,
    card: &PenaltyCard,
    team_color: (f64, f64, f64),
    is_goalkeeper: bool,
) {
    let card_h = row_h * 3.0;

    // Card-wide rounded background to round the outer corners
    // cleanly. The team-colour and timer rows then paint over the
    // top two thirds; the reason row sits on this dark backing.
    draw_rounded_rect(cr, x, y, w, card_h, radius);
    cr.set_source_rgba(0.05, 0.05, 0.05, 0.85);
    let _ = cr.fill();

    // Row 1 - player number on team colour. Pick black
    // text for bright team-colour backgrounds (white / yellow /
    // cyan jerseys) so the digit doesn't render invisible.
    draw_rounded_rect(cr, x, y, w, row_h, radius);
    let (r, g, b) = team_color;
    cr.set_source_rgb(r, g, b);
    let _ = cr.fill();
    cr.set_font_size(font_md);
    let (tr, tg, tb) = contrasting_text_rgb(team_color);
    cr.set_source_rgb(tr, tg, tb);
    let n = card.player_number.to_string();
    let ext = cr.text_extents(&n).unwrap();
    cr.move_to(
        x + w / 2.0 - ext.width() / 2.0,
        y + row_h - (row_h - ext.height()) / 2.0,
    );
    let _ = cr.show_text(&n);

    // Small "GK" badge on the top-left of row 1 when the
    // penalised player is the team's goalkeeper. The badge sits inside
    // the row so it doesn't push the player-number off-centre.
    if is_goalkeeper {
        let badge_font = font_sm * 0.85;
        cr.set_font_size(badge_font);
        let badge_text = "GK";
        let badge_ext = cr.text_extents(badge_text).unwrap();
        let pad_x = row_h * 0.18;
        let pad_y = row_h * 0.12;
        let badge_w = badge_ext.width() + pad_x * 2.0;
        let badge_h = badge_ext.height() + pad_y * 2.0;
        let bx = x + row_h * 0.15;
        let by = y + row_h * 0.15;
        draw_rounded_rect(cr, bx, by, badge_w, badge_h, radius * 0.6);
        cr.set_source_rgba(0.05, 0.05, 0.05, 0.85);
        let _ = cr.fill();
        cr.set_source_rgb(1.0, 1.0, 1.0);
        cr.move_to(bx + pad_x, by + pad_y + badge_ext.height());
        let _ = cr.show_text(badge_text);
        // Restore the row-1 font size for any later draws on this card.
        cr.set_font_size(font_md);
    }

    // Row 2 - timer on light grey
    let row2_y = y + row_h;
    draw_rounded_rect(cr, x, row2_y, w, row_h, radius * 0.3);
    cr.set_source_rgb(0.85, 0.85, 0.85);
    let _ = cr.fill();
    cr.set_font_size(font_md);
    cr.set_source_rgb(0.05, 0.05, 0.05);
    let mins = card.secs_remaining / 60;
    let secs = card.secs_remaining % 60;
    let t = format!("{:02}:{:02}", mins, secs);
    let ext = cr.text_extents(&t).unwrap();
    cr.move_to(
        x + w / 2.0 - ext.width() / 2.0,
        row2_y + row_h - (row_h - ext.height()) / 2.0,
    );
    let _ = cr.show_text(&t);

    // Row 3 - penalty reason text
    let row3_y = y + 2.0 * row_h;
    cr.set_font_size(font_sm);
    cr.set_source_rgb(1.0, 1.0, 1.0);
    let label = card.penalty_reason.as_str();
    let ext = cr.text_extents(label).unwrap();
    cr.move_to(
        x + w / 2.0 - ext.width() / 2.0,
        row3_y + row_h - (row_h - ext.height()) / 2.0,
    );
    let _ = cr.show_text(label);
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

    // Row-2 outer team-colour blocks. The reference design shows a
    // coloured rectangle at each row-2
    // edge separate from the score box. Width is a small chunky
    // square (~half row height); GK strip is a thinner adjacent
    // indicator when the GK colour byte differs from the field-player
    // byte (suppressed otherwise so two equal-coloured strips don't
    // read as one wider block).
    let outer_color_w = 10.0 * scale;
    let gk_strip_w = 5.0 * scale;
    let team1_has_gk = data.team1_goalkeeper_color.is_some();
    let team2_has_gk = data.team2_goalkeeper_color.is_some();
    let team1_gk_segment = if team1_has_gk {
        gk_strip_w + 2.0 * scale
    } else {
        0.0
    };
    let team2_gk_segment = if team2_has_gk {
        gk_strip_w + 2.0 * scale
    } else {
        0.0
    };

    // Total scoreboard width: outer | gk? | name | score | clock | score | name | gk? | outer + gaps
    let gap = 2.0 * scale;
    let total_w = outer_color_w
        + gap
        + team1_gk_segment
        + name_w
        + score_box_w
        + gap
        + clock_box_w
        + gap
        + score_box_w
        + name_w
        + team2_gk_segment
        + gap
        + outer_color_w;

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

    // Pre-compute the row-2 column anchors so row-1 packet counts can
    // line up under the team-name columns even after the outer colour
    // blocks + optional GK strips widen total_w.
    let x_name1_left = sb_x + outer_color_w + gap + team1_gk_segment;
    let x_name2_right =
        x_name1_left + name_w + score_box_w + gap + clock_box_w + gap + score_box_w + name_w;
    let name1_center = x_name1_left + name_w / 2.0;
    let name2_center = x_name2_right - name_w / 2.0;

    // --- Row 1: packet counts + game phase ---
    // During PENALTY_SHOOTOUT the left/right cells render
    // shoot-out shot dots instead of packet counts. Centre cell (phase
    // label) is unchanged either way.
    {
        cr.set_font_size(font_sm);
        let phase_str = format_game_phase(data);
        let y_base = sb_y + font_sm;
        let total_dots = if data.game_phase == GAME_PHASE_PENALTY_SHOOTOUT {
            shootout_total_dots(data.team1_penalty_shot, data.team2_penalty_shot)
        } else {
            0
        };

        // Left cell: dots (during shoot-out with at least one attempt
        // taken) or packet count.
        if total_dots > 0 {
            let dots =
                shootout_dot_states(data.team1_penalty_shot, data.team1_single_shots, total_dots);
            draw_shootout_dots(
                cr,
                name1_center,
                sb_y + font_sm * 0.5,
                scale,
                &dots,
                data.team1_color.unwrap_or((0.85, 0.1, 0.1)),
            );
        } else {
            // Prefix with "msg " so operators read the
            // number as the team's message-budget counter (~1200 at
            // match start) instead of a generic counter. Folded into
            // the existing row-1 cell.
            cr.set_source_rgb(0.8, 0.8, 0.8);
            let mb1 = format_message_budget(data.team1_message_budget);
            let mb1_ext = cr.text_extents(&mb1).unwrap();
            cr.move_to(name1_center - mb1_ext.width() / 2.0, y_base);
            let _ = cr.show_text(&mb1);
        }

        // Centre phase indicator (unchanged).
        cr.set_source_rgb(0.8, 0.8, 0.8);
        let phase_ext = cr.text_extents(&phase_str).unwrap();
        cr.move_to(width / 2.0 - phase_ext.width() / 2.0, y_base);
        let _ = cr.show_text(&phase_str);

        // Right cell: dots or message budget.
        if total_dots > 0 {
            let dots =
                shootout_dot_states(data.team2_penalty_shot, data.team2_single_shots, total_dots);
            draw_shootout_dots(
                cr,
                name2_center,
                sb_y + font_sm * 0.5,
                scale,
                &dots,
                data.team2_color.unwrap_or((0.1, 0.2, 0.85)),
            );
        } else {
            cr.set_source_rgb(0.8, 0.8, 0.8);
            let mb2 = format_message_budget(data.team2_message_budget);
            let mb2_ext = cr.text_extents(&mb2).unwrap();
            cr.move_to(name2_center - mb2_ext.width() / 2.0, y_base);
            let _ = cr.show_text(&mb2);
        }
    }

    // --- Row 2: outer color | gk? | name | score | clock | score | name | gk? | outer color ---
    {
        let row_y = sb_y + row_height;
        let y_text = row_y + font_lg * 0.85;
        cr.set_font_size(font_md);

        // Left outer team-colour block (chunky square).
        let x_outer1 = sb_x;
        let (r, g, b) = data.team1_color.unwrap_or((0.85, 0.1, 0.1));
        draw_rounded_rect(cr, x_outer1, row_y, outer_color_w, row_height, radius);
        cr.set_source_rgb(r, g, b);
        let _ = cr.fill();

        // Left GK strip, only when distinct from field colour.
        if let Some((gr, gg, gb)) = data.team1_goalkeeper_color {
            let x_gk1 = x_outer1 + outer_color_w + gap;
            draw_rounded_rect(cr, x_gk1, row_y, gk_strip_w, row_height, radius);
            cr.set_source_rgb(gr, gg, gb);
            let _ = cr.fill();
        }

        // Team 1 name (right-aligned before score)
        cr.set_source_rgb(1.0, 1.0, 1.0);
        let t1_ext = cr.text_extents(&data.team1_name).unwrap();
        let x_name1_right = x_name1_left + name_w;
        cr.move_to(x_name1_right - t1_ext.width() - pad, y_text);
        let _ = cr.show_text(&data.team1_name);

        // Score 1 box (team 1 jersey colour, or red fallback).
        // Score-digit colour adapts to the team-colour
        // background's luminance so bright jerseys (white, yellow,
        // cyan) don't render an invisible white-on-white digit.
        let x_score1 = x_name1_right;
        draw_rounded_rect(cr, x_score1, row_y, score_box_w, row_height, radius);
        let team1_bg = data.team1_color.unwrap_or((0.85, 0.1, 0.1));
        cr.set_source_rgb(team1_bg.0, team1_bg.1, team1_bg.2);
        let _ = cr.fill();

        cr.set_font_size(font_lg);
        let (tr, tg, tb) = contrasting_text_rgb(team1_bg);
        cr.set_source_rgb(tr, tg, tb);
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
        // Drop the pause-glyph prefix from the
        // stopped state - the system fonts on Jetson Nano + Pi 5
        // don't have a glyph for it and Cairo renders an empty
        // rectangle ("tofu box"). The 60%-alpha dim on the digit
        // text already conveys "play paused" without an extra
        // glyph. Also: show ASCII `-` when the clock has gone
        // negative (overtime / past final whistle); the previous
        // `unsigned_abs()`-only path silently displayed the
        // overtime delta as a positive number.
        let sign = if data.secs_remaining < 0 { "-" } else { "" };
        let clock_str = format!("{sign}{:02}:{:02}", mins, secs);
        cr.set_font_size(font_lg);
        if data.stopped {
            cr.set_source_rgba(1.0, 1.0, 1.0, 0.6);
        } else {
            cr.set_source_rgb(1.0, 1.0, 1.0);
        }
        let clk_ext = cr.text_extents(&clock_str).unwrap();
        cr.move_to(x_clock + clock_box_w / 2.0 - clk_ext.width() / 2.0, y_text);
        let _ = cr.show_text(&clock_str);

        // Score 2 box (team 2 jersey colour, or blue fallback).
        // Contrast-adaptive digit colour, same as score 1.
        let x_score2 = x_clock + clock_box_w + gap;
        draw_rounded_rect(cr, x_score2, row_y, score_box_w, row_height, radius);
        let team2_bg = data.team2_color.unwrap_or((0.1, 0.2, 0.85));
        cr.set_source_rgb(team2_bg.0, team2_bg.1, team2_bg.2);
        let _ = cr.fill();

        cr.set_font_size(font_lg);
        let (tr, tg, tb) = contrasting_text_rgb(team2_bg);
        cr.set_source_rgb(tr, tg, tb);
        let s2 = data.team2_score.to_string();
        let s2_ext = cr.text_extents(&s2).unwrap();
        cr.move_to(x_score2 + score_box_w / 2.0 - s2_ext.width() / 2.0, y_text);
        let _ = cr.show_text(&s2);

        // Team 2 name (left-aligned after score)
        cr.set_font_size(font_md);
        cr.set_source_rgb(1.0, 1.0, 1.0);
        let x_name2_left = x_score2 + score_box_w;
        cr.move_to(x_name2_left + pad, y_text);
        let _ = cr.show_text(&data.team2_name);

        // Right GK strip (mirror of left), only when distinct.
        if let Some((gr, gg, gb)) = data.team2_goalkeeper_color {
            let x_gk2 = x_name2_left + name_w;
            draw_rounded_rect(cr, x_gk2, row_y, gk_strip_w, row_height, radius);
            cr.set_source_rgb(gr, gg, gb);
            let _ = cr.fill();
        }

        // Right outer team-colour block (mirror of left).
        let x_outer2 = sb_x + total_w - outer_color_w;
        let (r, g, b) = data.team2_color.unwrap_or((0.1, 0.2, 0.85));
        draw_rounded_rect(cr, x_outer2, row_y, outer_color_w, row_height, radius);
        cr.set_source_rgb(r, g, b);
        let _ = cr.fill();
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

// ---------------------------------------------------------------------------
// Shoot-out shot indicator (row 1, conditional on phase)
// ---------------------------------------------------------------------------

/// Visual state of one shoot-out dot. Computed from the GC's
/// `penaltyShot` index + `singleShots` bitmask; rendered by
/// `draw_shootout_dots`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DotState {
    /// Attempt taken and scored - filled team-colour dot.
    Scored,
    /// Attempt taken and missed - hollow team-colour outline.
    Missed,
    /// Attempt not yet taken - dim grey outline.
    Upcoming,
}

/// Compute the total number of dots to draw per team during a
/// shoot-out so both teams' rows span the same width (operator can
/// see at a glance who is ahead on attempts).
///
/// Caps at `SHOOTOUT_DOT_CAP` so a malformed `penaltyShot` byte
/// can't blow the row-1 width budget. `0` means "draw nothing" -
/// either we're not in shoot-out or no attempts have happened yet.
pub fn shootout_total_dots(team1_penalty_shot: u32, team2_penalty_shot: u32) -> u32 {
    team1_penalty_shot
        .max(team2_penalty_shot)
        .min(SHOOTOUT_DOT_CAP)
}

/// Compute the per-dot states for one team. `penalty_shot` is the
/// 1-based current-attempt index from the GC; `single_shots` is the
/// bitmask of per-attempt outcomes (bit `i` = i-th shot scored,
/// 0-indexed). `total` is the row width (same for both teams; see
/// `shootout_total_dots`).
pub fn shootout_dot_states(penalty_shot: u32, single_shots: u32, total: u32) -> Vec<DotState> {
    (0..total)
        .map(|i| {
            let taken = i < penalty_shot;
            let scored = (single_shots & (1u32 << i)) != 0;
            match (taken, scored) {
                (true, true) => DotState::Scored,
                (true, false) => DotState::Missed,
                (false, _) => DotState::Upcoming,
            }
        })
        .collect()
}

/// Render one team's shoot-out dot row centred at `(center_x, center_y)`.
/// The dot row is sized off `scale` so it matches row-1 font height.
/// Empty `dots` slices draw nothing.
fn draw_shootout_dots(
    cr: &cairo::Context,
    center_x: f64,
    center_y: f64,
    scale: f64,
    dots: &[DotState],
    team_color: (f64, f64, f64),
) {
    if dots.is_empty() {
        return;
    }
    let dot_r = 3.0 * scale;
    let gap = 3.0 * scale;
    let row_w = dots.len() as f64 * (dot_r * 2.0) + (dots.len().saturating_sub(1)) as f64 * gap;
    let mut x = center_x - row_w / 2.0 + dot_r;
    let (tr, tg, tb) = team_color;
    for state in dots {
        cr.new_path();
        cr.arc(x, center_y, dot_r, 0.0, std::f64::consts::TAU);
        match state {
            DotState::Scored => {
                cr.set_source_rgb(tr, tg, tb);
                let _ = cr.fill();
            }
            DotState::Missed => {
                cr.set_source_rgb(tr, tg, tb);
                cr.set_line_width(1.5 * scale);
                let _ = cr.stroke();
            }
            DotState::Upcoming => {
                cr.set_source_rgba(0.7, 0.7, 0.7, 0.3);
                cr.set_line_width(1.0 * scale);
                let _ = cr.stroke();
            }
        }
        x += dot_r * 2.0 + gap;
    }
}

/// Format the message-budget cell text, prefixing the value with
/// `"msg "` so operators read the number as the team's HSL
/// message-budget counter rather than a generic packet count.
fn format_message_budget(budget: u32) -> String {
    format!("msg {budget}")
}

/// Format the row-1 game-phase indicator.
///
/// Covers the full GC `gamePhase` enum (`apps/schemas/game_state.py::GamePhase`):
///
/// - `NORMAL` (0)           → "1st" / "2nd" by `first_half`
/// - `PENALTY_SHOOTOUT` (1) → "shootout"
/// - `OVERTIME` (2)         → "ET 1st" / "ET 2nd" by `first_half`
/// - `TIMEOUT` (3)          → behave as NORMAL (timeout is orthogonal
///   to the half indicator; the state row already shows "timeout")
fn format_game_phase(data: &GameOverlayData) -> String {
    match data.game_phase {
        GAME_PHASE_PENALTY_SHOOTOUT => "shootout".to_string(),
        GAME_PHASE_OVERTIME => {
            if data.first_half {
                "ET 1st".to_string()
            } else {
                "ET 2nd".to_string()
            }
        }
        GAME_PHASE_NORMAL | GAME_PHASE_TIMEOUT => {
            if data.first_half {
                "1st".to_string()
            } else {
                "2nd".to_string()
            }
        }
        _ => {
            // Future-proof: any unknown phase byte falls back to the
            // simple 1st/2nd indicator so the overlay never goes blank.
            if data.first_half {
                "1st".to_string()
            } else {
                "2nd".to_string()
            }
        }
    }
}

/// Format the state line (row 3): e.g. "playing", "ready, kickoff for B-Human - 00:21"
fn format_state_line(data: &GameOverlayData) -> String {
    let state_lower = data.state.to_lowercase();
    // When the GC reports no kicker (raw 255 → normalised 0
    // in parse_game_state, or genuinely 0 pre-kickoff), drop the
    // "for <kicker>" suffix entirely rather than rendering
    // "kickoff for unknown" / "Team 255".
    let resolved_kicker: Option<&str> = if data.kicking_team == 0 {
        None
    } else if data.kicking_team == data.team1_number {
        Some(&data.team1_name)
    } else if data.kicking_team == data.team2_number {
        Some(&data.team2_name)
    } else {
        Some("unknown")
    };
    let head = if data.set_play != "NONE" {
        let play = data.set_play.to_lowercase().replace('_', " ");
        match resolved_kicker {
            Some(k) => format!("{state_lower}, {play} for {k}"),
            None => format!("{state_lower}, {play}"),
        }
    } else if data.state == "READY" || data.state == "SET" {
        match resolved_kicker {
            Some(k) => format!("{state_lower}, kickoff for {k}"),
            None => state_lower,
        }
    } else {
        state_lower
    };
    // Append the GC secondary clock (ready countdown, free-kick wait,
    // half-time break) when it's running (e.g.
    // "ready, kickoff for <team> - 00:21"). 0 means no secondary timer
    // is active so we suppress the suffix entirely.
    if data.secondary_time > 0 {
        let mins = (data.secondary_time as u32) / 60;
        let secs = (data.secondary_time as u32) % 60;
        format!("{head} - {:02}:{:02}", mins, secs)
    } else {
        head
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
    fn test_load_teams_map() {
        // Plain-string entries (the legacy schema) still load correctly.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("teams.json");
        std::fs::write(
            &path,
            r#"{"0":"Invisibles","5":"B-Human","14":"HTWK Robots"}"#,
        )
        .unwrap();
        let map = load_teams_map(path.to_str().unwrap());
        assert_eq!(map.len(), 3);
        assert_eq!(map.get(&5).unwrap().name, "B-Human");
        assert_eq!(map.get(&5).unwrap().color, None);
        assert_eq!(map.get(&14).unwrap().name, "HTWK Robots");
    }

    #[test]
    fn test_load_teams_map_mixed_schema() {
        // Mixed schema: some plain strings, some objects with colour.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("teams.json");
        std::fs::write(
            &path,
            r##"{
                "5":  "B-Human",
                "14": {"name": "HTWK Robots", "color": "#cc0000"},
                "20": {"name": "Greens",      "color": "00aa55"}
            }"##,
        )
        .unwrap();
        let map = load_teams_map(path.to_str().unwrap());
        assert_eq!(map.len(), 3);
        // Plain string: no colour.
        assert_eq!(map.get(&5).unwrap().name, "B-Human");
        assert_eq!(map.get(&5).unwrap().color, None);
        // Object with leading-# hex.
        let htwk = map.get(&14).unwrap();
        assert_eq!(htwk.name, "HTWK Robots");
        let (r, g, b) = htwk.color.unwrap();
        assert!((r - 0xcc as f64 / 255.0).abs() < 1e-6);
        assert!(g.abs() < 1e-6);
        assert!(b.abs() < 1e-6);
        // Object with bare hex (no leading #).
        let greens = map.get(&20).unwrap();
        assert_eq!(greens.name, "Greens");
        let (r, g, b) = greens.color.unwrap();
        assert!(r.abs() < 1e-6);
        assert!((g - 0xaa as f64 / 255.0).abs() < 1e-6);
        assert!((b - 0x55 as f64 / 255.0).abs() < 1e-6);
    }

    #[test]
    fn test_load_teams_map_bad_entries_skipped() {
        // Object missing `name`, non-integer key, bad colour, junk type -
        // each dropped with a warning, valid neighbours still loaded.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("teams.json");
        std::fs::write(
            &path,
            r##"{
                "5":   {"name": "B-Human"},
                "6":   {"color": "#ffffff"},
                "abc": "Not A Team Number",
                "9":   42,
                "10":  {"name": "Bad Colour", "color": "not-a-hex"}
            }"##,
        )
        .unwrap();
        let map = load_teams_map(path.to_str().unwrap());
        // Only #5 (valid name-only object) and #10 (valid name, color
        // silently dropped) survive.
        assert!(map.contains_key(&5));
        assert!(!map.contains_key(&6));
        assert!(!map.contains_key(&9));
        assert_eq!(map.get(&10).unwrap().name, "Bad Colour");
        assert_eq!(map.get(&10).unwrap().color, None);
    }

    #[test]
    fn test_parse_hex_color_variants() {
        assert_eq!(parse_hex_color("#000000"), Some((0.0, 0.0, 0.0)));
        let (r, g, b) = parse_hex_color("#ffffff").unwrap();
        assert!((r - 1.0).abs() < 1e-6 && (g - 1.0).abs() < 1e-6 && (b - 1.0).abs() < 1e-6);
        // bare hex (no #) also accepted
        assert!(parse_hex_color("00aa55").is_some());
        // wrong length / non-hex chars rejected
        assert_eq!(parse_hex_color("#abc"), None);
        assert_eq!(parse_hex_color("rgb(0,0,0)"), None);
        assert_eq!(parse_hex_color(""), None);
    }

    #[test]
    fn test_resolve_team_name_known() {
        let mut map = TeamsMap::new();
        map.insert(5, TeamInfo::from_name("B-Human"));
        assert_eq!(resolve_team_name(&map, 5), "B-Human");
    }

    #[test]
    fn test_resolve_team_name_unknown() {
        let map = TeamsMap::new();
        assert_eq!(resolve_team_name(&map, 99), "Team 99");
    }

    #[test]
    fn test_resolve_team_color_returns_configured_or_none() {
        let mut map = TeamsMap::new();
        map.insert(
            14,
            TeamInfo {
                name: "HTWK Robots".into(),
                color: Some((0.8, 0.0, 0.0)),
            },
        );
        map.insert(5, TeamInfo::from_name("B-Human"));
        assert_eq!(resolve_team_color(&map, 14), Some((0.8, 0.0, 0.0)));
        // Name-only entry → no colour.
        assert_eq!(resolve_team_color(&map, 5), None);
        // Unknown team → no colour.
        assert_eq!(resolve_team_color(&map, 999), None);
    }

    #[test]
    fn test_parse_game_state_uses_per_team_message_budgets_when_present() {
        // When the publisher carries explicit per-team message
        // budgets, those win over
        // the legacy top-level packet_number fallback.
        let teams = TeamsMap::new();
        let gs = serde_json::json!({
            "team1_number": 5,
            "team2_number": 14,
            "state": "PLAYING",
            "set_play": "NONE",
            "first_half": true,
            "secs_remaining": 600,
            "game_phase": 0,
            "kicking_team": 0,
            "packet_number": 1042,
            "team1_message_budget": 555,
            "team2_message_budget": 614,
        });
        let data = parse_game_state(&gs, &teams);
        assert_eq!(data.team1_message_budget, 555);
        assert_eq!(data.team2_message_budget, 614);
        // legacy top-level packet_number still preserved.
        assert_eq!(data.packet_number, 1042);
    }

    #[test]
    fn test_parse_game_state_accepts_legacy_packet_number_keys() {
        // Backward compat: an out-of-sync publisher that
        // still emits the historical `team{1,2}_packet_number` keys
        // is honoured before falling back to the top-level
        // packet_number. Lets the bus stay live across a rolling
        // upgrade.
        let teams = TeamsMap::new();
        let gs = serde_json::json!({
            "team1_number": 5,
            "team2_number": 14,
            "state": "PLAYING",
            "set_play": "NONE",
            "first_half": true,
            "secs_remaining": 600,
            "game_phase": 0,
            "kicking_team": 0,
            "packet_number": 999,
            "team1_packet_number": 555,
            "team2_packet_number": 614,
        });
        let data = parse_game_state(&gs, &teams);
        assert_eq!(data.team1_message_budget, 555);
        assert_eq!(data.team2_message_budget, 614);
    }

    #[test]
    fn test_format_message_budget_prefix() {
        // Row-1 cell text is prefixed with "msg " so the
        // value reads as the HSL team message budget rather than as
        // a generic counter.
        assert_eq!(format_message_budget(1200), "msg 1200");
        assert_eq!(format_message_budget(0), "msg 0");
    }

    #[test]
    fn test_parse_game_state() {
        let mut teams = TeamsMap::new();
        teams.insert(5, TeamInfo::from_name("B-Human"));
        teams.insert(
            14,
            TeamInfo {
                name: "HTWK Robots".into(),
                color: Some((0.8, 0.0, 0.0)),
            },
        );
        let gs = serde_json::json!({
            "team1_number": 5,
            "team2_number": 14,
            "team1_score": 2,
            "team2_score": 1,
            "first_half": false,
            "secs_remaining": 300,
            "state": "PLAYING",
            "set_play": "NONE",
            "kicking_team": 5,
            "packet_number": 1042,
            "game_phase": 1,
        });
        let data = parse_game_state(&gs, &teams);
        assert_eq!(data.team1_name, "B-Human");
        assert_eq!(data.team2_name, "HTWK Robots");
        // team1 is name-only (no colour configured) → None
        assert_eq!(data.team1_color, None);
        // team2 has a configured colour → propagated through
        assert_eq!(data.team2_color, Some((0.8, 0.0, 0.0)));
        // Per-team message budgets absent → fall back to
        // the top-level packet_number so the scoreboard row-1 cell
        // still renders something (last-resort behaviour).
        assert_eq!(data.team1_message_budget, 1042);
        assert_eq!(data.team2_message_budget, 1042);
        assert_eq!(data.team1_score, 2);
        assert_eq!(data.team2_score, 1);
        assert!(!data.first_half);
        assert_eq!(data.secs_remaining, 300);
        assert_eq!(data.state, "PLAYING");
        assert!(data.is_live);
    }

    #[test]
    fn test_parse_game_state_surfaces_task481_fields() {
        // secondaryTime + TeamInfo extras (goalkeeper,
        // penaltyShot, singleShots) flow through parse_game_state
        // when the publisher emits them.
        let teams = TeamsMap::new();
        let gs = serde_json::json!({
            "team1_number": 5,
            "team2_number": 14,
            "state": "PLAYING",
            "set_play": "NONE",
            "first_half": true,
            "secs_remaining": 600,
            "game_phase": 1,
            "kicking_team": 0,
            "secondary_time": 45,
            "team1_goalkeeper": 1,
            "team1_penalty_shot": 2,
            "team1_single_shots": 0b0011, // shots 1 & 2 scored
            "team2_goalkeeper": 7,
            "team2_penalty_shot": 1,
            "team2_single_shots": 0b0001,
        });
        let data = parse_game_state(&gs, &teams);
        assert_eq!(data.secondary_time, 45);
        assert_eq!(data.team1_goalkeeper, 1);
        assert_eq!(data.team2_goalkeeper, 7);
        assert_eq!(data.team1_penalty_shot, 2);
        assert_eq!(data.team1_single_shots, 0b0011);
        assert_eq!(data.team2_penalty_shot, 1);
        assert_eq!(data.team2_single_shots, 0b0001);
    }

    #[test]
    fn test_parse_game_state_packet_colour_fallback() {
        // When teams.json has no override for a team
        // number, the publisher's fieldPlayerColour byte falls
        // through team_colour_byte_to_rgb to give the score box
        // a non-default colour.
        let teams = TeamsMap::new(); // empty map → no overrides
        let gs = serde_json::json!({
            "team1_number": 99,
            "team2_number": 100,
            "state": "PLAYING",
            "set_play": "NONE",
            "first_half": true,
            "secs_remaining": 600,
            "game_phase": 0,
            "kicking_team": 0,
            "team1_field_player_colour": 1,  // RED
            "team2_field_player_colour": 5,  // GREEN
        });
        let data = parse_game_state(&gs, &teams);
        assert_eq!(data.team1_color, Some((0.85, 0.15, 0.15)));
        assert_eq!(data.team2_color, Some((0.15, 0.70, 0.30)));
    }

    #[test]
    fn test_parse_game_state_teams_json_wins_over_packet_colour() {
        // teams.json overrides remain authoritative - the
        // packet colour only fills the gap where no override exists.
        let mut teams = TeamsMap::new();
        teams.insert(
            5,
            TeamInfo {
                name: "B-Human".into(),
                color: Some((0.10, 0.20, 0.30)),
            },
        );
        let gs = serde_json::json!({
            "team1_number": 5,
            "team2_number": 99, // not in teams.json
            "state": "PLAYING",
            "set_play": "NONE",
            "first_half": true,
            "secs_remaining": 600,
            "game_phase": 0,
            "kicking_team": 0,
            "team1_field_player_colour": 1,  // would be RED - but override wins
            "team2_field_player_colour": 5,  // GREEN - no override, this wins
        });
        let data = parse_game_state(&gs, &teams);
        assert_eq!(data.team1_color, Some((0.10, 0.20, 0.30)));
        assert_eq!(data.team2_color, Some((0.15, 0.70, 0.30)));
    }

    #[test]
    fn test_team_colour_byte_to_rgb_full_table() {
        // All 10 spec values map to Some; anything else is None.
        for byte in 0..=9_u32 {
            assert!(
                team_colour_byte_to_rgb(byte).is_some(),
                "byte {byte} should map to Some(rgb)"
            );
        }
        assert!(team_colour_byte_to_rgb(10).is_none());
        assert!(team_colour_byte_to_rgb(255).is_none());
    }

    #[test]
    fn test_parse_game_state_normalises_kicking_team_none_sentinel() {
        // GC wire byte 255 == KICKING_TEAM_NONE. Normalise
        // to 0 so the renderer can use the existing "0 means no
        // kicker" rule rather than special-casing the sentinel.
        let teams = TeamsMap::new();
        let gs = serde_json::json!({
            "team1_number": 5,
            "team2_number": 14,
            "state": "READY",
            "set_play": "NONE",
            "first_half": true,
            "secs_remaining": 45,
            "game_phase": 0,
            "kicking_team": 255,
        });
        let data = parse_game_state(&gs, &teams);
        assert_eq!(data.kicking_team, 0);
    }

    #[test]
    fn test_parse_game_state_reads_stopped_flag() {
        // `stopped` rides on the existing game_state
        // payload and surfaces on GameOverlayData so the
        // scoreboard can dim the clock during referee timeouts.
        let teams = TeamsMap::new();
        let gs_stopped = serde_json::json!({
            "team1_number": 5,
            "team2_number": 14,
            "state": "PLAYING",
            "set_play": "NONE",
            "first_half": true,
            "secs_remaining": 300,
            "game_phase": 0,
            "kicking_team": 0,
            "stopped": true,
        });
        assert!(parse_game_state(&gs_stopped, &teams).stopped);

        // Field absent → defaults to false (older publishers).
        let gs_default = serde_json::json!({
            "team1_number": 5,
            "team2_number": 14,
            "state": "PLAYING",
            "set_play": "NONE",
            "first_half": true,
            "secs_remaining": 300,
            "game_phase": 0,
            "kicking_team": 0,
        });
        assert!(!parse_game_state(&gs_default, &teams).stopped);
    }

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
    fn test_format_state_line_ready_without_kicker() {
        // kicking_team == 0 (post-normalisation from the
        // wire's KICKING_TEAM_NONE) drops the "for <kicker>" suffix
        // rather than rendering "kickoff for unknown".
        let data = GameOverlayData {
            state: "READY".into(),
            set_play: "NONE".into(),
            kicking_team: 0,
            team1_number: 5,
            team1_name: "B-Human".into(),
            ..Default::default()
        };
        assert_eq!(format_state_line(&data), "ready");
    }

    #[test]
    fn test_format_state_line_set_play_without_kicker() {
        // Same suffix-drop applies inside set-play
        // descriptions - "playing, penalty kick" not
        // "playing, penalty kick for unknown".
        let data = GameOverlayData {
            state: "PLAYING".into(),
            set_play: "PENALTY_KICK".into(),
            kicking_team: 0,
            team2_number: 14,
            team2_name: "HTWK Robots".into(),
            ..Default::default()
        };
        assert_eq!(format_state_line(&data), "playing, penalty kick");
    }

    #[test]
    fn test_format_state_line_appends_secondary_time() {
        // When GC reports a non-zero secondary clock (ready
        // countdown, free-kick wait, half-time break), append " - MM:SS"
        // to the state line (e.g. "ready, kickoff for <team> - 00:21").
        let data = GameOverlayData {
            state: "READY".into(),
            set_play: "NONE".into(),
            kicking_team: 14,
            team2_number: 14,
            team2_name: "HTWK Robots".into(),
            secondary_time: 21,
            ..Default::default()
        };
        assert_eq!(
            format_state_line(&data),
            "ready, kickoff for HTWK Robots - 00:21"
        );

        // Non-trivial mm:ss.
        let data = GameOverlayData {
            state: "FINISHED".into(),
            set_play: "NONE".into(),
            secondary_time: 645, // 10:45
            ..Default::default()
        };
        assert_eq!(format_state_line(&data), "finished - 10:45");
    }

    #[test]
    fn test_format_state_line_omits_zero_secondary_time() {
        // secondary_time == 0 means "no secondary clock active";
        // the trailing " - MM:SS" suffix must be suppressed entirely.
        let data = GameOverlayData {
            state: "PLAYING".into(),
            set_play: "NONE".into(),
            secondary_time: 0,
            ..Default::default()
        };
        assert_eq!(format_state_line(&data), "playing");
    }

    #[test]
    fn test_format_game_phase_covers_all_phases() {
        // Full GamePhase enum coverage. Numeric values
        // mirror apps/schemas/game_state.py::GamePhase.
        let make = |phase: u32, first_half: bool| GameOverlayData {
            game_phase: phase,
            first_half,
            ..Default::default()
        };
        // NORMAL × {1st, 2nd}
        assert_eq!(format_game_phase(&make(0, true)), "1st");
        assert_eq!(format_game_phase(&make(0, false)), "2nd");
        // PENALTY_SHOOTOUT
        assert_eq!(format_game_phase(&make(1, true)), "shootout");
        assert_eq!(format_game_phase(&make(1, false)), "shootout");
        // OVERTIME × {1st, 2nd}
        assert_eq!(format_game_phase(&make(2, true)), "ET 1st");
        assert_eq!(format_game_phase(&make(2, false)), "ET 2nd");
        // TIMEOUT - orthogonal to half indicator; the state row already
        // shows "timeout" so the phase pill keeps the half label.
        assert_eq!(format_game_phase(&make(3, true)), "1st");
        assert_eq!(format_game_phase(&make(3, false)), "2nd");
        // Unknown phase byte → safe fallback to 1st/2nd.
        assert_eq!(format_game_phase(&make(99, true)), "1st");
    }

    #[test]
    fn test_parse_game_state_surfaces_goalkeeper_colour() {
        // parse_game_state must surface team{1,2}_goalkeeper_colour
        // (published by gamecontroller.py) so the row-2
        // GK strip renders. Distinct field/GK bytes → Some(rgb).
        let teams = TeamsMap::new();
        let gs = serde_json::json!({
            "team1_number": 5,
            "team2_number": 14,
            "state": "PLAYING",
            "set_play": "NONE",
            "first_half": true,
            "secs_remaining": 600,
            "game_phase": 0,
            "kicking_team": 0,
            "team1_field_player_colour": 1, // RED
            "team1_goalkeeper_colour": 0,   // BLUE
            "team2_field_player_colour": 0, // BLUE
            "team2_goalkeeper_colour": 2,   // YELLOW
        });
        let data = parse_game_state(&gs, &teams);
        assert!(data.team1_goalkeeper_color.is_some());
        assert!(data.team2_goalkeeper_color.is_some());
    }

    #[test]
    fn test_parse_game_state_suppresses_goalkeeper_colour_when_equal() {
        // When field-player and goalkeeper bytes match the strip would
        // be visually indistinguishable from the outer block; the
        // parser surfaces None instead of a redundant value.
        let teams = TeamsMap::new();
        let gs = serde_json::json!({
            "team1_number": 5,
            "team2_number": 14,
            "state": "PLAYING",
            "set_play": "NONE",
            "first_half": true,
            "secs_remaining": 600,
            "game_phase": 0,
            "kicking_team": 0,
            "team1_field_player_colour": 1,
            "team1_goalkeeper_colour": 1, // same as field
            "team2_field_player_colour": 0,
            "team2_goalkeeper_colour": 0, // same as field
        });
        let data = parse_game_state(&gs, &teams);
        assert!(data.team1_goalkeeper_color.is_none());
        assert!(data.team2_goalkeeper_color.is_none());
    }

    #[test]
    fn test_shootout_total_dots_takes_max_and_caps() {
        // Both teams render the same number of dots so the
        // operator sees who is ahead on attempts. Cap at SHOOTOUT_DOT_CAP.
        assert_eq!(shootout_total_dots(0, 0), 0);
        assert_eq!(shootout_total_dots(3, 4), 4);
        assert_eq!(shootout_total_dots(4, 3), 4);
        assert_eq!(shootout_total_dots(5, 5), 5);
        // Cap honoured even when one side is wildly higher.
        assert_eq!(
            shootout_total_dots(0, SHOOTOUT_DOT_CAP + 5),
            SHOOTOUT_DOT_CAP
        );
        assert_eq!(shootout_total_dots(99, 99), SHOOTOUT_DOT_CAP);
    }

    #[test]
    fn test_shootout_dot_states_classifies_each_attempt() {
        // bit i of single_shots == 1 → Scored;
        // bit i == 0 and i < penalty_shot → Missed;
        // i >= penalty_shot → Upcoming.
        use DotState::*;

        // penalty_shot=3, single_shots=0b101 → scored / missed / scored.
        // Row of 3 attempts taken, no upcoming.
        assert_eq!(
            shootout_dot_states(3, 0b101, 3),
            vec![Scored, Missed, Scored],
        );

        // Same shot history but the dot row is widened to 5 (because
        // the opponent has taken more attempts): trailing two are
        // Upcoming.
        assert_eq!(
            shootout_dot_states(3, 0b101, 5),
            vec![Scored, Missed, Scored, Upcoming, Upcoming],
        );

        // No attempts yet → all Upcoming.
        assert_eq!(
            shootout_dot_states(0, 0, 3),
            vec![Upcoming, Upcoming, Upcoming],
        );

        // total == 0 → empty vec, no panic.
        assert_eq!(shootout_dot_states(2, 0b11, 0), Vec::<DotState>::new());

        // Mask bits beyond `total` are silently ignored (they describe
        // attempts the row doesn't render).
        assert_eq!(
            shootout_dot_states(3, 0b11111, 3),
            vec![Scored, Scored, Scored],
        );
    }

    #[test]
    fn test_shootout_total_dots_zero_when_no_attempts() {
        // Pre-shootout (penalty_shot == 0 for both teams) draws no
        // dots so the row-1 left/right cells fall back to the packet
        // counts. The phase indicator already shows "shootout".
        assert_eq!(shootout_total_dots(0, 0), 0);
    }

    #[test]
    fn test_parse_penalties_sorts_by_secs_remaining_desc() {
        // Longest-pending penalty at index 0 so it sits at
        // the top of the bottom-anchored stack. Player-number ties
        // break deterministically.
        let p = serde_json::json!({
            "team1_penalties": [
                {"player_number": 4, "penalty_code": 7,
                 "penalty_reason": "Ball Holding", "secs_remaining": 10},
                {"player_number": 2, "penalty_code": 7,
                 "penalty_reason": "Ball Holding", "secs_remaining": 60},
                {"player_number": 6, "penalty_code": 7,
                 "penalty_reason": "Ball Holding", "secs_remaining": 60},
                {"player_number": 1, "penalty_code": 7,
                 "penalty_reason": "Ball Holding", "secs_remaining": 30},
            ],
            "team2_penalties": [],
        });
        let (team1, _team2) = parse_penalties(&p);
        let order: Vec<u32> = team1.iter().map(|c| c.player_number).collect();
        // 60 (#2) → 60 (#6) → 30 (#1) → 10 (#4). Ties broken by
        // ascending player_number to keep the on-screen order stable.
        assert_eq!(order, vec![2, 6, 1, 4]);
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

    // -----------------------------------------------------------------
    // penalty card parsing
    // -----------------------------------------------------------------

    #[test]
    fn test_parse_penalties_filters_zero_codes() {
        // Mixed payload: two active penalties + one no-penalty entry
        // per team. The no-penalty entries (`penalty_code: 0`) must
        // be dropped so the draw loop only iterates active ones.
        // Label conventions: code 13 = Substitute,
        // code 7 = Ball Holding, code 0 = No Penalty.
        let p = serde_json::json!({
            "team1_penalties": [
                {"team_number": 5, "player_number": 1, "penalty_code": 13,
                 "penalty_reason": "Substitute", "secs_remaining": 0,
                 "cautions": 0},
                {"team_number": 5, "player_number": 2, "penalty_code": 0,
                 "penalty_reason": "No Penalty", "secs_remaining": 0,
                 "cautions": 0},
                {"team_number": 5, "player_number": 3, "penalty_code": 7,
                 "penalty_reason": "Ball Holding", "secs_remaining": 30,
                 "cautions": 1},
            ],
            "team2_penalties": [
                {"team_number": 14, "player_number": 1, "penalty_code": 0,
                 "penalty_reason": "No Penalty", "secs_remaining": 0,
                 "cautions": 0},
            ],
        });
        let (team1, team2) = parse_penalties(&p);
        // Two active for team1 (#1 Substitute + #3 Ball Holding), zero for team2.
        // parse_penalties now sorts by secs_remaining desc so
        // the longer-pending card sits at the top of the stack. #3 has
        // 30s left, #1 has 0s, so #3 comes first.
        assert_eq!(team1.len(), 2);
        assert_eq!(team1[0].player_number, 3);
        assert_eq!(team1[0].penalty_reason, "Ball Holding");
        assert_eq!(team1[0].secs_remaining, 30);
        assert_eq!(team1[1].player_number, 1);
        assert_eq!(team1[1].penalty_reason, "Substitute");
        assert_eq!(team1[1].secs_remaining, 0);
        assert!(team2.is_empty());
    }

    #[test]
    fn test_parse_penalties_handles_missing_or_malformed_payload() {
        // Missing arrays altogether → empty lists, no panic.
        let p = serde_json::json!({});
        let (team1, team2) = parse_penalties(&p);
        assert!(team1.is_empty());
        assert!(team2.is_empty());

        // Wrong type for the arrays → also empty, no panic.
        let p = serde_json::json!({
            "team1_penalties": "not an array",
            "team2_penalties": 42,
        });
        let (team1, team2) = parse_penalties(&p);
        assert!(team1.is_empty());
        assert!(team2.is_empty());

        // Missing fields inside an entry → safe defaults (player 0,
        // empty reason, 0 secs) - only filters on `penalty_code != 0`.
        let p = serde_json::json!({
            "team1_penalties": [
                {"penalty_code": 13},  // only the code; everything else missing
            ],
        });
        let (team1, _team2) = parse_penalties(&p);
        assert_eq!(team1.len(), 1);
        assert_eq!(team1[0].player_number, 0);
        assert_eq!(team1[0].penalty_reason, "");
        assert_eq!(team1[0].secs_remaining, 0);
    }

    #[test]
    fn test_default_overlay_state_has_no_penalty_cards() {
        let d = GameOverlayData::default();
        assert!(d.team1_penalties.is_empty());
        assert!(d.team2_penalties.is_empty());
    }

    // -----------------------------------------------------------------
    // scoreboard_state_from_game NV12 translation
    // -----------------------------------------------------------------

    #[test]
    fn scoreboard_state_from_game_copies_scores_and_names() {
        let d = GameOverlayData {
            team1_name: "Nao Devils".into(),
            team2_name: "B-Human".into(),
            team1_score: 3,
            team2_score: 1,
            team1_color: Some((1.0, 0.0, 0.0)),
            team2_color: Some((0.0, 0.0, 1.0)),
            field_name: "FIELD A".into(),
            ..GameOverlayData::default()
        };
        let s = scoreboard_state_from_game(&d);
        assert_eq!(s.home_team_name, "Nao Devils");
        assert_eq!(s.away_team_name, "B-Human");
        assert_eq!(s.home_score, 3);
        assert_eq!(s.away_score, 1);
        assert_eq!(s.field_name, "FIELD A");
        let home_rgba = s.home_team_color.expect("home color present");
        assert_eq!((home_rgba.r, home_rgba.g, home_rgba.b), (255, 0, 0));
        let away_rgba = s.away_team_color.expect("away color present");
        assert_eq!((away_rgba.r, away_rgba.g, away_rgba.b), (0, 0, 255));
    }

    #[test]
    fn apply_game_state_preserves_configured_field_name() {
        // The configured/seeded value lives in the existing overlay;
        // a GC packet parses to the placeholder "FIELD A". After the
        // merge the operator's value must survive.
        let mut overlay = GameOverlayData {
            field_name: "FIELD C".into(),
            team1_score: 0,
            ..GameOverlayData::default()
        };
        let incoming = GameOverlayData {
            field_name: "FIELD A".into(), // parse_game_state placeholder
            team1_score: 2,
            ..GameOverlayData::default()
        };
        apply_game_state(&mut overlay, incoming);
        // Field name preserved; live game data still applied.
        assert_eq!(overlay.field_name, "FIELD C");
        assert_eq!(overlay.team1_score, 2);
    }

    #[test]
    fn apply_game_state_preserves_penalty_cards() {
        let mut overlay = GameOverlayData {
            team1_penalties: vec![PenaltyCard {
                player_number: 3,
                penalty_reason: "Pushing".into(),
                secs_remaining: 30,
            }],
            ..GameOverlayData::default()
        };
        let incoming = GameOverlayData::default();
        apply_game_state(&mut overlay, incoming);
        assert_eq!(overlay.team1_penalties.len(), 1);
    }

    #[test]
    fn scoreboard_state_from_game_emits_shootout_only_in_phase() {
        let mut d = GameOverlayData {
            game_phase: 0,
            team1_penalty_shot: 2,
            team1_single_shots: 0b01,
            ..GameOverlayData::default()
        };
        assert!(scoreboard_state_from_game(&d).shootout.is_none());

        d.game_phase = GAME_PHASE_PENALTY_SHOOTOUT;
        let s = scoreboard_state_from_game(&d);
        let so = s.shootout.expect("shootout state present");
        assert_eq!(so.home_penalty_shot, 2);
        assert_eq!(so.home_single_shots, 0b01);
    }

    #[test]
    fn scoreboard_state_from_game_marks_goalkeeper_penalty_tile() {
        let d = GameOverlayData {
            team1_goalkeeper: 5,
            team1_penalties: vec![
                PenaltyCard {
                    player_number: 5,
                    penalty_reason: "Goalkeeper Holding".into(),
                    secs_remaining: 30,
                },
                PenaltyCard {
                    player_number: 2,
                    penalty_reason: "Ball Holding".into(),
                    secs_remaining: 20,
                },
            ],
            ..GameOverlayData::default()
        };
        let s = scoreboard_state_from_game(&d);
        assert_eq!(s.home_penalty_timers.len(), 2);
        assert!(s.home_penalty_timers[0].is_goalkeeper);
        assert!(!s.home_penalty_timers[1].is_goalkeeper);
    }

    #[test]
    fn scoreboard_state_from_game_formats_clock_text() {
        let mut d = GameOverlayData {
            secs_remaining: 305,
            ..GameOverlayData::default()
        };
        assert_eq!(scoreboard_state_from_game(&d).game_clock_text, "05:05");
        d.secs_remaining = -7;
        assert_eq!(scoreboard_state_from_game(&d).game_clock_text, "-00:07");
        d.secs_remaining = 0;
        assert_eq!(scoreboard_state_from_game(&d).game_clock_text, "00:00");
    }

    #[test]
    fn scoreboard_state_from_game_propagates_clock_stopped_flag() {
        let mut d = GameOverlayData {
            stopped: false,
            ..GameOverlayData::default()
        };
        assert!(!scoreboard_state_from_game(&d).clock_stopped);
        d.stopped = true;
        assert!(scoreboard_state_from_game(&d).clock_stopped);
    }

    // -----------------------------------------------------------------
    // contrasting_text_rgb + signed mm:ss + dropped pause glyph
    // -----------------------------------------------------------------

    #[test]
    fn contrasting_text_rgb_picks_black_on_bright_backgrounds() {
        // Pure white → black text.
        assert_eq!(contrasting_text_rgb((1.0, 1.0, 1.0)), (0.0, 0.0, 0.0));
        // Pure yellow → black text (RoboCup team-color enum 2).
        assert_eq!(contrasting_text_rgb((1.0, 1.0, 0.0)), (0.0, 0.0, 0.0));
        // Pure cyan (high green channel) → black text.
        assert_eq!(contrasting_text_rgb((0.0, 1.0, 1.0)), (0.0, 0.0, 0.0));
    }

    #[test]
    fn contrasting_text_rgb_picks_white_on_dark_backgrounds() {
        // Pure black → white text.
        assert_eq!(contrasting_text_rgb((0.0, 0.0, 0.0)), (1.0, 1.0, 1.0));
        // Pure red (RoboCup team-color enum 1) → white text;
        // luminance ≈ 0.299, well below the 0.6 threshold.
        assert_eq!(contrasting_text_rgb((1.0, 0.0, 0.0)), (1.0, 1.0, 1.0));
        // Pure blue (RoboCup team-color enum 0) → white text.
        assert_eq!(contrasting_text_rgb((0.0, 0.0, 1.0)), (1.0, 1.0, 1.0));
        // Pure green (saturated): luminance = 0.587, just under
        // the 0.6 threshold, still flips to white.
        assert_eq!(contrasting_text_rgb((0.0, 1.0, 0.0)), (1.0, 1.0, 1.0));
    }

    #[test]
    fn format_mm_ss_uses_ascii_hyphen_minus_for_negative() {
        // ASCII byte for hyphen-minus is 0x2D - must be exactly that
        // (some Unicode minus alternatives lack a glyph in cairo's
        // default fonts on Jetson L4T R32.7).
        let s = format_mm_ss(-65);
        assert!(s.starts_with('-'));
        assert_eq!(s.as_bytes()[0], 0x2D);
        assert_eq!(s, "-01:05");
    }

    #[test]
    fn format_mm_ss_no_sign_for_zero_or_positive() {
        assert_eq!(format_mm_ss(0), "00:00");
        assert_eq!(format_mm_ss(305), "05:05");
    }
}
