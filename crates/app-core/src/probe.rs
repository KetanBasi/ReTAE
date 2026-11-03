//! `router-cli probe`: the measuring parts that do not need the daemon's state.
//!
//! * RTT: percentiles of VBAN-SERVICE ping round trips.
//! * Click-loop: push single-frame clicks into a strip's input ring at the real sample rate
//!   and time when each one comes out of a bus's output ring. That covers the whole mixer path
//!   (input FIFO and drift resampler, block alignment, mixing, output ring) but not a device
//!   or a network hop, which need real hardware (PLAN.md §12).

use crate::types::{ClickLoopReport, RttReport};
use audio_engine::{InputPort, OutputPort};
use audio_types::{ENGINE_BLOCK_FRAMES, ENGINE_SAMPLE_RATE};
use platform_io::RateClock;
use std::time::{Duration, Instant};

pub const BLOCK_MS: f32 = ENGINE_BLOCK_FRAMES as f32 * 1000.0 / ENGINE_SAMPLE_RATE as f32;

/// Silence before the first click, so the drift controller has settled on its target fill.
const WARMUP: Duration = Duration::from_millis(600);
const CLICK_SPACING: Duration = Duration::from_millis(125);
/// How long to keep listening after the last click.
const TAIL: Duration = Duration::from_millis(400);
const CLICK_AMPLITUDE: f32 = 0.5;
/// A click is spread and attenuated by pan and the resampler; anything above this counts.
const DETECT_THRESHOLD: f32 = 0.05;
/// After a detection ignore the output for this long, so one click is one detection.
const DEAD_TIME: Duration = Duration::from_millis(50);
const TICK: Duration = Duration::from_millis(1);
const CHANNELS: usize = 2;

// SECTION: Round-trip statistics
/// Nearest-rank percentile of an ascending list.
fn percentile(sorted: &[f32], p: f32) -> f32 {
    if sorted.is_empty() {
        return 0.0;
    }
    let rank = (p * sorted.len() as f32).ceil() as usize;
    sorted[rank.clamp(1, sorted.len()) - 1]
}

pub fn rtt_report(sent: u32, samples_ms: Vec<f32>) -> RttReport {
    let mut sorted = samples_ms.clone();
    sorted.sort_by(f32::total_cmp);
    let jitter_ms = if samples_ms.len() > 1 {
        samples_ms
            .windows(2)
            .map(|w| (w[1] - w[0]).abs())
            .sum::<f32>()
            / (samples_ms.len() - 1) as f32
    } else {
        0.0
    };
    RttReport {
        sent,
        received: samples_ms.len() as u32,
        min_ms: sorted.first().copied().unwrap_or(0.0),
        p50_ms: percentile(&sorted, 0.50),
        p99_ms: percentile(&sorted, 0.99),
        max_ms: sorted.last().copied().unwrap_or(0.0),
        jitter_ms,
        samples_ms,
    }
}
// !SECTION: Round-trip statistics

// SECTION: Click-loop
/// What one run saw.
#[derive(Debug, Default)]
pub struct ClickRun {
    /// When each click was handed to the input ring.
    pub sent: Vec<Instant>,
    /// When each click was seen leaving the output ring.
    pub found: Vec<Instant>,
    /// The worst time the loop slept longer than asked, in ms.
    pub host_noise_ms: f32,
    /// The input ring stopped accepting audio (nothing is running the mixer).
    pub stalled: bool,
}

/// Feed `clicks` clicks into `input` and watch `output`. Takes about 1.5 s plus 125 ms per click.
/// Both ports must already be attached to an unmuted strip and a bus that are routed together.
pub fn run_click_loop(input: &mut InputPort, output: &mut OutputPort, clicks: usize) -> ClickRun {
    let mut run = ClickRun::default();
    let mut clock = RateClock::new(f64::from(ENGINE_SAMPLE_RATE));
    let mut silence = vec![0.0_f32; 256 * CHANNELS];
    let mut popped = vec![0.0_f32; ENGINE_BLOCK_FRAMES * CHANNELS * 4];

    let start = Instant::now();
    let total = WARMUP + CLICK_SPACING * clicks as u32 + TAIL;
    let mut next_click = start + WARMUP;
    let mut clicks_left = clicks;
    let mut last_loop = start;
    let mut quiet_until = start;

    while start.elapsed() < total {
        std::thread::sleep(TICK);
        let now = Instant::now();
        let overshoot = now.duration_since(last_loop).saturating_sub(TICK);
        run.host_noise_ms = run.host_noise_ms.max(overshoot.as_secs_f32() * 1000.0);
        last_loop = now;

        // Feed: every frame that has become due, with the click placed in the first chunk
        // written once its time has come.
        let mut due = clock.due();
        while due > 0 {
            let n = due.min(silence.len() / CHANNELS);
            let click_now = clicks_left > 0 && Instant::now() >= next_click;
            let chunk = &mut silence[..n * CHANNELS];
            chunk.fill(0.0);
            if click_now {
                chunk[..CHANNELS].fill(CLICK_AMPLITUDE);
            }
            if input.push(chunk) < chunk.len() {
                run.stalled = true;
                return run;
            }
            if click_now {
                run.sent.push(Instant::now());
                clicks_left -= 1;
                next_click += CLICK_SPACING;
            }
            due -= n;
        }

        // Listen.
        loop {
            let got = output.pop(&mut popped);
            if got == 0 {
                break;
            }
            let seen = Instant::now();
            if seen >= quiet_until && popped[..got].iter().any(|v| v.abs() > DETECT_THRESHOLD) {
                run.found.push(seen);
                quiet_until = seen + DEAD_TIME;
            }
        }
    }
    run
}

/// Turn a run into numbers. `expected_ms` is what the configured pipeline should give.
pub fn click_report(run: &ClickRun, expected_ms: f32) -> ClickLoopReport {
    // Pair each detection with the latest click sent before it that has no detection yet.
    let mut paired = vec![false; run.sent.len()];
    let mut latencies: Vec<f32> = Vec::new();
    for &seen in &run.found {
        let Some(i) = run.sent.iter().rposition(|&s| s <= seen) else {
            continue;
        };
        if !paired[i] {
            paired[i] = true;
            latencies.push(seen.duration_since(run.sent[i]).as_secs_f32() * 1000.0);
        }
    }
    latencies.sort_by(f32::total_cmp);
    let p50_ms = percentile(&latencies, 0.50);
    ClickLoopReport {
        clicks_sent: run.sent.len() as u32,
        clicks_found: latencies.len() as u32,
        expected_ms,
        min_ms: latencies.first().copied().unwrap_or(0.0),
        p50_ms,
        max_ms: latencies.last().copied().unwrap_or(0.0),
        block_ms: BLOCK_MS,
        within_one_block: !latencies.is_empty() && (p50_ms - expected_ms).abs() <= BLOCK_MS,
        host_noise_ms: run.host_noise_ms,
        scope: "input FIFO + drift resampler + one mixer block + output ring on this machine; \
                device and network legs are not included"
            .to_owned(),
    }
}
// !SECTION: Click-loop

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percentiles_use_the_nearest_rank() {
        let v = [1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0, 9.0, 10.0];
        assert_eq!(percentile(&v, 0.5), 5.0);
        assert_eq!(percentile(&v, 0.99), 10.0);
        assert_eq!(percentile(&v, 0.0), 1.0);
        assert_eq!(percentile(&[], 0.5), 0.0);
        assert_eq!(percentile(&[7.0], 0.99), 7.0);
    }

    #[test]
    fn the_rtt_report_summarises_and_keeps_the_raw_samples() {
        let r = rtt_report(5, vec![0.4, 0.2, 0.3, 0.2]);
        assert_eq!((r.sent, r.received), (5, 4));
        assert_eq!(r.min_ms, 0.2);
        assert_eq!(r.max_ms, 0.4);
        assert_eq!(r.p50_ms, 0.2);
        assert_eq!(r.samples_ms, vec![0.4, 0.2, 0.3, 0.2]);
        // |0.2-0.4| + |0.3-0.2| + |0.2-0.3| over 3 steps
        assert!((r.jitter_ms - 0.4 / 3.0).abs() < 1e-6);

        let empty = rtt_report(3, vec![]);
        assert_eq!((empty.received, empty.min_ms, empty.max_ms), (0, 0.0, 0.0));
    }

    #[test]
    fn detections_pair_with_the_click_that_caused_them() {
        let t0 = Instant::now();
        let at = |ms: u64| t0 + Duration::from_millis(ms);
        let run = ClickRun {
            sent: vec![at(0), at(100), at(200)],
            // The second click was lost; the third arrives twice (the repeat must be ignored).
            found: vec![at(10), at(215), at(220)],
            host_noise_ms: 0.0,
            stalled: false,
        };
        let r = click_report(&run, 10.0);
        assert_eq!((r.clicks_sent, r.clicks_found), (3, 2));
        assert!((r.min_ms - 10.0).abs() < 0.01 && (r.max_ms - 15.0).abs() < 0.01);
        assert!(r.within_one_block);

        let none = click_report(&ClickRun::default(), 10.0);
        assert!(!none.within_one_block, "no data is not a pass");
    }
}
