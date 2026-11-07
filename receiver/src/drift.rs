//! Clock-drift correction: nudge the playout rate so the buffer level stays
//! at the jitter target without audible pitch changes.
//!
//! The phone's oscillator and the PC sound card's oscillator run at slightly
//! different true rates (typically tens of ppm). If we played stream samples
//! at a fixed rate, the buffer would drift to empty (dropouts) or full
//! (overruns) over minutes. The controller watches the buffer level and
//! reports a playback ratio: > 1 consumes slightly faster (level falls),
//! < 1 slightly slower (level rises).
//!
//! The ratio is capped at ±`max_deviation` (project rule: < 0.1 %, and this
//! default is 0.05 % so it is far below anything audible as pitch change)
//! and slew-limited so it never jumps.

#[derive(Debug, Clone)]
pub struct DriftConfig {
    /// Deadband around the target level, in ms: no correction while the
    /// error stays inside it (prevents constant micro-adjustment noise).
    pub deadband_ms: f64,
    /// Proportional gain: ratio contribution per ms of level error.
    pub kp: f64,
    /// Integral gain per ms-seconds of accumulated error.
    pub ki: f64,
    /// Max |ratio - 1|. 0.0005 = 0.05 %.
    pub max_deviation: f64,
    /// Max |ratio change| applied in one update call (slew limit).
    pub max_step: f64,
}

impl Default for DriftConfig {
    fn default() -> Self {
        Self {
            deadband_ms: 2.0,
            // With levels in ms and targets of tens of ms: a 20 ms excess
            // level gives p = 20 * 2.5e-5 = 0.0005 (the full cap) — the
            // proportional term alone maxes out at a one-second excursion.
            kp: 2.5e-5,
            // Integral: 1000 ms·s of error adds ~0.02 % ... slow but it is
            // the term that removes steady-state error over long sessions.
            ki: 2.0e-8,
            max_deviation: 0.0005,
            max_step: 5.0e-5,
        }
    }
}

pub struct DriftController {
    cfg: DriftConfig,
    integral: f64,
    ratio: f64,
}

impl DriftController {
    pub fn new(cfg: DriftConfig) -> Self {
        Self {
            cfg,
            integral: 0.0,
            ratio: 1.0,
        }
    }

    /// Current playback ratio (1.0 = nominal). Used by tests; kept public as
    /// the read-side accessor of this controller.
    #[allow(dead_code)]
    pub fn ratio(&self) -> f64 {
        self.ratio
    }

    /// One control step. `error_ms` = buffered_ms - target_ms (positive:
    /// too much buffered, speed up). `dt_secs` = time since last call.
    pub fn update(&mut self, error_ms: f64, dt_secs: f64) -> f64 {
        let deadband_err = if error_ms.abs() <= self.cfg.deadband_ms {
            0.0
        } else {
            error_ms - error_ms.signum() * self.cfg.deadband_ms
        };

        // Integrate with two defenses against windup:
        // 1) hard clamp at the deviation cap;
        // 2) a leak (time constant ~30 s) so the integral relaxes toward
        //    zero when the error disappears instead of pinning forever.
        const LEAK_TAU_SECS: f64 = 30.0;
        self.integral = (self.integral + deadband_err * dt_secs * self.cfg.ki)
            / (1.0 + dt_secs / LEAK_TAU_SECS);
        self.integral = self
            .integral
            .clamp(-self.cfg.max_deviation, self.cfg.max_deviation);

        let desired = 1.0 + deadband_err * self.cfg.kp + self.integral;
        let desired = desired.clamp(1.0 - self.cfg.max_deviation, 1.0 + self.cfg.max_deviation);

        // Slew-limit movement of the actual ratio.
        let step = (desired - self.ratio).clamp(-self.cfg.max_step, self.cfg.max_step);
        self.ratio += step;
        self.ratio
    }

    /// Forget history (used when a session restarts or after a big flush).
    #[allow(dead_code)]
    pub fn reset(&mut self) {
        self.integral = 0.0;
        self.ratio = 1.0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctrl() -> DriftController {
        DriftController::new(DriftConfig::default())
    }

    #[test]
    fn deadband_produces_no_correction() {
        let mut c = ctrl();
        for _ in 0..50 {
            c.update(1.0, 0.01); // 1 ms over target, inside deadband
        }
        assert_eq!(c.ratio(), 1.0);
    }

    #[test]
    fn excess_level_speeds_up_within_bounds() {
        let mut c = ctrl();
        for _ in 0..100 {
            let r = c.update(20.0, 0.01); // 20 ms over target
            assert!(r >= 1.0);
            assert!(r <= 1.0005);
        }
        assert!(
            c.ratio() > 1.0001,
            "should push meaningfully: {}",
            c.ratio()
        );
    }

    #[test]
    fn starvation_slows_down() {
        let mut c = ctrl();
        for _ in 0..100 {
            let r = c.update(-20.0, 0.01);
            assert!(r < 1.0);
            assert!(r >= 0.9995);
        }
    }

    #[test]
    fn ratio_is_slew_limited_per_step() {
        let cfg = DriftConfig::default();
        let mut c = DriftController::new(cfg.clone());
        let r1 = c.update(500.0, 0.01); // demand far beyond one step
        assert!((r1 - 1.0).abs() <= cfg.max_step + f64::EPSILON);
    }

    #[test]
    fn converges_back_to_nominal_when_on_target() {
        let mut c = ctrl();
        for _ in 0..200 {
            c.update(30.0, 0.01); // heavily over target: ratio high
        }
        assert!(c.ratio() > 1.0);
        // Now simulate the corrected world: error goes to zero gradually.
        let mut error = 30.0;
        for _ in 0..2000 {
            let r = c.update(error, 0.01);
            // level drains proportionally to how much we sped up
            error -= (r - 1.0) * 1000.0 * 0.01 * 48.0; // arbitrary plausible coupling
            if error < 0.0 {
                error = 0.0;
            }
        }
        assert!(
            (c.ratio() - 1.0).abs() < 0.0001,
            "ratio should relax back toward 1, got {}",
            c.ratio()
        );
    }

    #[test]
    fn integral_anti_windup_stays_bounded() {
        let mut c = ctrl();
        for _ in 0..100_000 {
            c.update(1000.0, 0.01); // sustained huge error
        }
        assert!(c.ratio() <= 1.0005);
        // After the error stops, it must come back down at slew speed, not
        // stay pinned by a wound-up integral.
        for _ in 0..200_000 {
            c.update(0.0, 0.01);
        }
        assert!((c.ratio() - 1.0).abs() < 1e-9, "{}", c.ratio());
    }

    #[test]
    fn reset_clears_state() {
        let mut c = ctrl();
        for _ in 0..10 {
            c.update(100.0, 0.01);
        }
        c.reset();
        assert_eq!(c.ratio(), 1.0);
    }
}
