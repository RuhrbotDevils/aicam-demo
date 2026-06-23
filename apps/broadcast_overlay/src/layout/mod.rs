// Domain-aware overlay layouts that turn game state into overlay commands.
// Author: Thomas Klute

//! Domain-aware overlay layouts.
//!
//! These modules translate semantic game state into the generic
//! [`crate::commands::OverlayCommand`] list the renderer consumes.
//! Keeps RoboCup-specific concepts (teams, scores, clocks,
//! penalties) out of the renderer and the GStreamer element.

pub mod scoreboard;

pub use scoreboard::{
    scoreboard_commands, LayoutParams, LayoutSizes, PenaltyTile, ScoreboardState, ShootoutState,
};
