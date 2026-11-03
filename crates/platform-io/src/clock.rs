//! A software sample clock for threads that have no hardware to pace them (the timer master
//! and the mock devices).

use std::time::Instant;

/// Longest pause that is turned into frames. A laptop that slept for an hour must not come
/// back and be handed 170 million frames of "catch-up".
const MAX_CATCH_UP_S: f64 = 0.25;

/// Counts how many frames have become due since the last call, at a rate that can be nudged
/// by a few ppm to imitate a crystal that is slightly off.
// ? Why credit instead of "sleep one period": a thread that wakes 5 ms late must deliver 5 ms
// ? of frames on its next turn, exactly like a device whose DMA keeps running. Sleeping a fixed
// ? period would stretch every late wake into a permanently slower clock.
#[derive(Debug)]
pub struct RateClock {
    last: Instant,
    credit: f64,
    rate_hz: f64,
    nominal_hz: f64,
}

impl RateClock {
    pub fn new(rate_hz: f64) -> Self {
        Self {
            last: Instant::now(),
            credit: 0.0,
            rate_hz,
            nominal_hz: rate_hz,
        }
    }

    /// Run `ppm` parts per million fast (negative = slow) against the nominal rate.
    pub fn set_ppm(&mut self, ppm: f64) {
        self.rate_hz = self.nominal_hz * (1.0 + ppm * 1e-6);
    }

    /// Whole frames that became due since the last call.
    pub fn due(&mut self) -> usize {
        let now = Instant::now();
        let dt = now
            .duration_since(self.last)
            .as_secs_f64()
            .min(MAX_CATCH_UP_S);
        self.last = now;
        self.credit += dt * self.rate_hz;
        let whole = self.credit.floor();
        self.credit -= whole;
        whole as usize
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn frames_follow_wall_time_and_never_get_lost_between_calls() {
        let mut c = RateClock::new(48_000.0);
        let start = Instant::now();
        let mut total = 0usize;
        while start.elapsed() < Duration::from_millis(100) {
            std::thread::sleep(Duration::from_millis(3));
            total += c.due();
        }
        total += c.due();
        let expect = start.elapsed().as_secs_f64() * 48_000.0;
        assert!(
            (total as f64 - expect).abs() < 48.0 + expect * 0.01,
            "{total} frames against {expect:.0} expected"
        );
    }

    #[test]
    fn ppm_changes_the_rate() {
        let mut slow = RateClock::new(48_000.0);
        slow.set_ppm(-1_000_000.0 / 2.0); // half speed, exaggerated so a short test can see it
        let mut fast = RateClock::new(48_000.0);
        std::thread::sleep(Duration::from_millis(50));
        let (s, f) = (slow.due(), fast.due());
        assert!(s * 3 < f * 2, "slow {s} vs fast {f}");
    }

    #[test]
    fn a_long_pause_is_capped() {
        let mut c = RateClock::new(48_000.0);
        c.last -= Duration::from_secs(3600);
        assert!(c.due() <= (MAX_CATCH_UP_S * 48_000.0) as usize + 1);
    }
}
