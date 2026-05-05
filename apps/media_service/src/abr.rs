// Implements Rust media pipeline logic for streaming and camera processing.
// Author: Thomas Klute

//! Adaptive-bitrate controller for the streaming consumer pipeline.
//!
//! Reads `stream_queue.current-level-time` at 1 Hz and adjusts
//! `stream_encoder.bitrate` based on sustained queue fullness. The
//! decision logic is pure - see [`AbrController::tick`] - so it
//! can be unit-tested without a real GStreamer pipeline. The poll
//! loop and property I/O live in `main.rs`.

/// Configuration for the ABR controller. The defaults are tuned for
/// a 2-second `stream_queue` (the streaming consumer pipeline's
/// `max-size-time`) and a 1 Hz tick.
#[derive(Debug, Clone, Copy)]
pub struct AbrConfig {
    /// Lowest bitrate the controller will set, in kbps.
    pub floor_kbps: u32,
    /// Highest bitrate (the configured streaming target).
    pub ceiling_kbps: u32,
    /// Queue-level fraction (0.0..=1.0) above which the controller
    /// counts a "high" tick.
    pub high_level_ratio: f64,
    /// Queue-level fraction below which the controller counts a
    /// "low" tick.
    pub low_level_ratio: f64,
    /// Consecutive high ticks needed before stepping down.
    pub high_ticks_to_downstep: u32,
    /// Consecutive low ticks needed before stepping up.
    pub low_ticks_to_upstep: u32,
    /// Multiplier applied on a downstep (e.g. 0.8 = 20% reduction).
    pub downstep_factor: f64,
    /// Multiplier applied on an upstep.
    pub upstep_factor: f64,
}

impl AbrConfig {
    /// Build an `AbrConfig` for a given configured streaming bitrate
    /// (the ceiling). Uses the tuned defaults for everything else.
    pub fn from_ceiling(ceiling_kbps: u32) -> Self {
        Self {
            floor_kbps: 500,
            ceiling_kbps,
            high_level_ratio: 0.75,
            low_level_ratio: 0.10,
            high_ticks_to_downstep: 3,
            low_ticks_to_upstep: 10,
            downstep_factor: 0.80,
            upstep_factor: 1.10,
        }
    }
}

/// Decision-making half of the ABR controller. The poll loop in
/// `main.rs` drives [`tick`] with the most recent queue level and
/// applies any returned bitrate change to `stream_encoder`.
#[derive(Debug, Clone)]
pub struct AbrController {
    cfg: AbrConfig,
    current_kbps: u32,
    consecutive_high: u32,
    consecutive_low: u32,
}

impl AbrController {
    pub fn new(cfg: AbrConfig) -> Self {
        let ceiling = cfg.ceiling_kbps;
        Self {
            cfg,
            current_kbps: ceiling,
            consecutive_high: 0,
            consecutive_low: 0,
        }
    }

    #[allow(dead_code)]
    pub fn current_kbps(&self) -> u32 {
        self.current_kbps
    }

    /// Drive one tick with the observed queue level (0.0 = empty,
    /// 1.0 = full). Returns `Some(new_kbps)` when the controller
    /// decided to change the bitrate; `None` otherwise.
    pub fn tick(&mut self, queue_level_ratio: f64) -> Option<u32> {
        let lvl = queue_level_ratio.clamp(0.0, 1.0);

        if lvl >= self.cfg.high_level_ratio {
            self.consecutive_high = self.consecutive_high.saturating_add(1);
            self.consecutive_low = 0;
        } else if lvl <= self.cfg.low_level_ratio {
            self.consecutive_low = self.consecutive_low.saturating_add(1);
            self.consecutive_high = 0;
        } else {
            // Mid-band: reset both counters. The controller only
            // moves on sustained extremes.
            self.consecutive_high = 0;
            self.consecutive_low = 0;
        }

        if self.consecutive_high >= self.cfg.high_ticks_to_downstep {
            self.consecutive_high = 0;
            return self.try_downstep();
        }
        if self.consecutive_low >= self.cfg.low_ticks_to_upstep {
            self.consecutive_low = 0;
            return self.try_upstep();
        }
        None
    }

    fn try_downstep(&mut self) -> Option<u32> {
        let proposed = ((self.current_kbps as f64) * self.cfg.downstep_factor).round() as u32;
        let new_kbps = proposed.max(self.cfg.floor_kbps);
        if new_kbps == self.current_kbps {
            None
        } else {
            self.current_kbps = new_kbps;
            Some(new_kbps)
        }
    }

    fn try_upstep(&mut self) -> Option<u32> {
        let proposed = ((self.current_kbps as f64) * self.cfg.upstep_factor).round() as u32;
        let new_kbps = proposed.min(self.cfg.ceiling_kbps);
        if new_kbps == self.current_kbps {
            None
        } else {
            self.current_kbps = new_kbps;
            Some(new_kbps)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fresh(ceiling: u32) -> AbrController {
        AbrController::new(AbrConfig::from_ceiling(ceiling))
    }

    #[test]
    fn starts_at_ceiling() {
        let c = fresh(8000);
        assert_eq!(c.current_kbps(), 8000);
    }

    #[test]
    fn mid_level_does_not_change_bitrate() {
        let mut c = fresh(8000);
        for _ in 0..30 {
            assert_eq!(c.tick(0.5), None);
        }
        assert_eq!(c.current_kbps(), 8000);
    }

    #[test]
    fn high_for_three_ticks_steps_down() {
        let mut c = fresh(8000);
        assert_eq!(c.tick(0.9), None);
        assert_eq!(c.tick(0.9), None);
        let new = c.tick(0.9).expect("third high tick should downstep");
        assert_eq!(new, 6400); // 8000 * 0.8
        assert_eq!(c.current_kbps(), 6400);
    }

    #[test]
    fn high_runs_step_down_repeatedly_until_floor() {
        // 8000 → 6400 → 5120 → 4096 → 3277 → 2622 → 2098 → 1678 →
        //   1342 → 1074 → 859 → 687 → 550 → floor (500)
        let mut c = fresh(8000);
        let mut history = vec![c.current_kbps()];
        for _ in 0..50 {
            // Every tick is high → every 3 ticks produces a downstep.
            for _ in 0..3 {
                let _ = c.tick(0.95);
            }
            history.push(c.current_kbps());
        }
        // After enough downsteps, we hit the floor.
        assert_eq!(c.current_kbps(), 500);
        // The first step was 8000 → 6400.
        assert_eq!(history[1], 6400);
    }

    #[test]
    fn low_for_ten_ticks_steps_up_from_reduced_bitrate() {
        let mut c = fresh(8000);
        // Force a downstep first so there's headroom for upstep.
        for _ in 0..3 {
            let _ = c.tick(0.95);
        }
        assert_eq!(c.current_kbps(), 6400);

        // Nine low ticks: no upstep yet.
        for _ in 0..9 {
            assert_eq!(c.tick(0.05), None);
        }
        // Tenth low tick fires the upstep.
        let new = c.tick(0.05).expect("tenth low tick should upstep");
        assert_eq!(new, 7040); // 6400 * 1.10
    }

    #[test]
    fn upstep_does_not_exceed_ceiling() {
        let mut c = fresh(8000);
        // Force one downstep to 6400.
        for _ in 0..3 {
            let _ = c.tick(0.95);
        }
        // Pile on low ticks - should rise back to 8000 and stop.
        for _ in 0..200 {
            let _ = c.tick(0.05);
        }
        assert_eq!(c.current_kbps(), 8000);
    }

    #[test]
    fn downstep_does_not_go_below_floor() {
        let mut c = fresh(8000);
        // Pile on high ticks - should reach the 500 floor and stop.
        for _ in 0..200 {
            let _ = c.tick(0.95);
        }
        assert_eq!(c.current_kbps(), 500);
    }

    #[test]
    fn alternating_extremes_keep_counters_zeroed() {
        // High-then-low alternation should never trigger either
        // extreme; both counters get reset by the opposite reading.
        let mut c = fresh(8000);
        for _ in 0..100 {
            let _ = c.tick(0.9);
            let _ = c.tick(0.05);
        }
        assert_eq!(c.current_kbps(), 8000);
    }

    #[test]
    fn mid_band_resets_counters_too() {
        let mut c = fresh(8000);
        // Two highs, then a mid resets - third high should not yet fire.
        let _ = c.tick(0.9);
        let _ = c.tick(0.9);
        let _ = c.tick(0.5);
        assert_eq!(c.tick(0.9), None); // counter was reset; this is tick 1 again
    }

    #[test]
    fn ratio_is_clamped() {
        let mut c = fresh(8000);
        // Negative or > 1 ratios should be clamped; no panic.
        // Three high ticks (each ratio clamped to 1.0) → downstep.
        let _ = c.tick(2.0);
        let _ = c.tick(1.5);
        let _ = c.tick(2.0);
        assert_eq!(c.current_kbps(), 6400);
        // A nonsense negative ratio is clamped to 0.0, which counts as low.
        let _ = c.tick(-1.0);
        // No panic so far. The state is now (high=0, low=1).
    }
}
