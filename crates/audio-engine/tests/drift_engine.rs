//! Drift correction inside the real `MixerEngine` (strip input on an external clock).
//!
//! * The mixer is the consumer and calls `process_block(128)` exactly on its own clock; the
//!   producer is simulated at a slightly different rate by pushing a fractional number of
//!   frames per block. Everything runs under the `assert_no_alloc` guard from `common`.

mod common;

use audio_types::{ClockDomain, EngineCommand as C, StripId, load_f32};
use common::*;
use core::sync::atomic::Ordering::Relaxed;

const TABLE: usize = 64; // 750 Hz at 48 kHz
const AMP: f32 = 0.5;

fn tone(i: usize) -> f32 {
    AMP * (core::f64::consts::TAU * (i % TABLE) as f64 / TABLE as f64).sin() as f32
}

/// A producer running `offset_ppm` faster than the mixer: pushes `128 * (1 + ppm)` frames per
/// block, carrying the fractional remainder so the long-run rate is exact.
struct Producer {
    owed: f64,
    next_idx: usize,
    per_block: f64,
}

impl Producer {
    fn new(offset_ppm: f64) -> Self {
        Self {
            owed: 0.0,
            next_idx: 0,
            per_block: BLOCK as f64 * (1.0 + offset_ppm * 1.0e-6),
        }
    }
    fn frames_this_block(&mut self) -> usize {
        self.owed += self.per_block;
        let n = self.owed.floor();
        self.owed -= n;
        n as usize
    }
    fn push(&mut self, rig: &mut Rig, strip: u8, frames: usize) {
        let mut v = Vec::with_capacity(frames * 2);
        for _ in 0..frames {
            let s = tone(self.next_idx);
            self.next_idx += 1;
            v.push(s);
            v.push(s);
        }
        rig.feed(strip, &v);
    }
}

fn external_rig(target_ms: f32) -> Rig {
    let mut r = Rig::unity();
    r.send(C::SetStripClock {
        strip: StripId(0),
        clock: ClockDomain::External { target_ms },
    });
    r
}

#[test]
fn drift_engine_strip_on_an_external_clock_follows_a_fast_and_a_slow_producer() {
    for offset_ppm in [300.0_f64, -300.0, 0.0] {
        let mut rig = external_rig(12.0);
        // ? `Rig` setup runs a couple of blocks on the still-empty Master strip, which the
        // ? engine counts as underruns. Measure everything relative to after the setup.
        let underruns0 = rig.h.stats.input_underruns.load(Relaxed);
        let mut prod = Producer::new(offset_ppm);
        // Prefill a little so priming completes in the first few blocks.
        let n = (12.0 * 48.0) as usize;
        prod.push(&mut rig, 0, n);

        let max_tone_step = AMP * 2.0 * (core::f32::consts::PI / TABLE as f32).sin();
        let (mut worst_step, mut prev) = (0.0_f32, 0.0_f32);
        let (mut ppm_sum, mut ppm_n) = (0.0_f64, 0usize);
        let mut worst_ema = 0.0_f32;
        let blocks = 60 * 375; // 60 s
        for b in 0..blocks {
            let frames = prod.frames_this_block();
            prod.push(&mut rig, 0, frames);
            rig.process(1);
            let out = rig.drain(0);
            let t = b as f32 * BLOCK as f32 / FS;
            let lan = &rig.tel().lan_rx[0];
            if t > 20.0 {
                for &v in chan(&out, 2, 0).iter() {
                    worst_step = worst_step.max((v - prev).abs());
                    prev = v;
                }
                worst_ema = worst_ema.max((load_f32(&lan.buffer_ms) - 12.0).abs());
            } else {
                prev = out[out.len() - 2];
            }
            if t > 50.0 {
                ppm_sum += f64::from(load_f32(&lan.drift_ppm));
                ppm_n += 1;
            }
        }
        let learned = ppm_sum / ppm_n as f64;
        let lan = &rig.tel().lan_rx[0];
        println!(
            "engine, producer {offset_ppm:+.0} ppm: mean drift_ppm {learned:.1}, worst |buffer_ms - 12| = {worst_ema:.2}, \
             worst step x{:.4}, underruns {}, overruns {}",
            worst_step / max_tone_step,
            rig.h.stats.input_underruns.load(Relaxed) - underruns0,
            rig.h.stats.input_overruns.load(Relaxed),
        );
        assert!(
            (learned - offset_ppm).abs() < 20.0,
            "{offset_ppm}: telemetry drift_ppm {learned}"
        );
        assert!(
            worst_ema < 1.5,
            "{offset_ppm}: buffer_ms strayed {worst_ema} ms from target"
        );
        assert_eq!(load_f32(&lan.target_ms), 12.0);
        assert_eq!(lan.underruns.load(Relaxed), 0);
        assert_eq!(
            rig.h.stats.input_underruns.load(Relaxed) - underruns0,
            0,
            "{offset_ppm}: underruns"
        );
        assert_eq!(
            rig.h.stats.input_overruns.load(Relaxed),
            0,
            "{offset_ppm}: overruns"
        );
        // Through send, fader, bus and the whole chain the tone stays click-free.
        assert!(
            worst_step / max_tone_step < 1.005,
            "{offset_ppm}: output clicked"
        );
    }
}

#[test]
fn drift_engine_control_experiment_an_uncorrected_strip_drifts_away() {
    // The same +300 ppm producer into a strip on the MASTER clock: the ring's fill grows
    // without bound (14 frames per second). This proves the test above passes because of the
    // adapter and not by accident; the adapted strip holds its fill at the 12 ms target.
    let fill_after_two_minutes = |external: bool| {
        let mut rig = if external {
            external_rig(12.0)
        } else {
            Rig::unity()
        };
        let mut prod = Producer::new(300.0);
        prod.push(&mut rig, 0, 600);
        for _ in 0..(120 * 375) {
            let n = prod.frames_this_block();
            prod.push(&mut rig, 0, n);
            rig.process(1);
            let _ = rig.drain(0);
        }
        rig.input_fill_frames(0)
    };
    let uncorrected = fill_after_two_minutes(false);
    let corrected = fill_after_two_minutes(true);
    // ? The controller regulates the fill seen *before* a pull (12 ms = 576 frames, including
    // ? the block just pushed). Reading the ring after the pull has consumed one block gives
    // ? 576 - 128 = 448 frames.
    println!(
        "ring fill after 2 min at +300 ppm: master clock {uncorrected} frames, external clock {corrected} frames (576 before / 448 after a pull)"
    );
    assert!(
        uncorrected > 600 + 1500,
        "uncorrected fill only {uncorrected} frames"
    );
    assert!(
        (400..500).contains(&corrected),
        "corrected fill {corrected} frames, expected ~448"
    );
}

#[test]
fn drift_engine_switching_the_clock_back_to_master_restores_exact_passthrough() {
    let mut rig = external_rig(12.0);
    let mut prod = Producer::new(0.0);
    prod.push(&mut rig, 0, 600);
    for _ in 0..2000 {
        let n = prod.frames_this_block();
        prod.push(&mut rig, 0, n);
        rig.process(1);
        let _ = rig.drain(0);
    }
    let lan = &rig.tel().lan_rx[0];
    assert_eq!(load_f32(&lan.target_ms), 12.0);
    assert!(load_f32(&lan.buffer_ms) > 5.0);

    // Back to the master clock: stale telemetry is cleared immediately.
    rig.send(C::SetStripClock {
        strip: StripId(0),
        clock: ClockDomain::Master,
    });
    rig.process(1);
    let _ = rig.drain(0);
    let lan = &rig.tel().lan_rx[0];
    assert_eq!(load_f32(&lan.target_ms), 0.0);
    assert_eq!(load_f32(&lan.buffer_ms), 0.0);
    assert_eq!(load_f32(&lan.drift_ppm), 0.0);

    // ... and the strip reads the ring directly again: bit-exact pass-through of DC.
    // (Drain whatever the old external clock left in the ring first.)
    rig.send(C::DetachStripInput { strip: StripId(0) });
    rig.process(1);
    rig.send(C::AttachStripInput {
        strip: StripId(0),
        slot: audio_engine::input_slot(0),
        channels: 2,
    });
    rig.settle();
    let out = rig.run(0, 0, SETTLE, dc(0.25));
    assert!(
        tail(&out, 2, 0, BLOCK).iter().all(|&v| v == 0.25),
        "Master clock must pass audio through bit-exactly"
    );
}

#[test]
fn drift_engine_a_non_finite_target_is_rejected_and_changes_nothing() {
    let mut rig = Rig::unity();
    let before = rig.h.stats.rejected_commands.load(Relaxed);
    for bad in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
        rig.send(C::SetStripClock {
            strip: StripId(0),
            clock: ClockDomain::External { target_ms: bad },
        });
    }
    // Invalid strip id as well.
    rig.send(C::SetStripClock {
        strip: StripId(99),
        clock: ClockDomain::Master,
    });
    let out = rig.run(0, 0, SETTLE, dc(0.5));
    assert_eq!(rig.h.stats.rejected_commands.load(Relaxed) - before, 4);
    assert_eq!(
        *out.last().unwrap(),
        0.5,
        "a rejected clock command must leave the strip on the master clock"
    );
}

#[test]
fn drift_engine_silent_producer_gives_silence_then_recovers_without_noise() {
    let mut rig = external_rig(12.0);
    rig.process(1); // apply SetStripClock before taking the baseline
    let underruns0 = rig.h.stats.input_underruns.load(Relaxed);
    let mut prod = Producer::new(0.0);
    // Nothing fed for a while: the strip stays silent (priming), no underrun is counted.
    let mut quiet = Vec::new();
    for _ in 0..50 {
        rig.process(1);
        quiet.extend(rig.drain(0));
    }
    assert!(quiet.iter().all(|&v| v == 0.0));
    assert_eq!(
        rig.h.stats.input_underruns.load(Relaxed) - underruns0,
        0,
        "priming is not an underrun"
    );
    // The producer starts: audio appears once the FIFO reaches its target, and is clean.
    prod.push(&mut rig, 0, 700);
    let mut heard = Vec::new();
    for _ in 0..400 {
        let n = prod.frames_this_block();
        prod.push(&mut rig, 0, n);
        rig.process(1);
        heard.extend(chan(&rig.drain(0), 2, 0));
    }
    let peak_level = peak(&heard);
    assert!((peak_level - AMP).abs() < 0.02, "level {peak_level}");
    assert!(heard.iter().all(|v| v.is_finite()));
    assert_eq!(rig.h.stats.input_underruns.load(Relaxed) - underruns0, 0);
}
