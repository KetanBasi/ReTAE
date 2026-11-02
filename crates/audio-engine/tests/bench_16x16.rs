//! RT budget: 16 strips x 16 buses, stereo, every DSP stage on (PLAN.md §11 S1:
//! "< 50 µs per block in release").
//!
//! * Run with `cargo test -p audio-engine --release --test bench_16x16 -- --nocapture`.
//! * The < 50 µs assertion only exists in release builds. A debug build is ~20x slower and
//!   only prints its numbers.

mod common;

use audio_engine::{EngineHandles, create_engine};
use audio_types::{
    BusId, Compressor, ENGINE_BLOCK_FRAMES, EngineCommand as C, EqBand, Gate, RouteSend, StripDsp,
    StripId,
};
use common::{BLOCK, sine};
use std::time::Instant;

/// Worst-case configuration: 256 enabled sends (half pre-fader), gate + 3-band EQ +
/// compressor on every strip, 3-band EQ on every bus, and every bus also recorded.
fn build_full_rig() -> (
    audio_engine::MixerEngine,
    Vec<audio_engine::InputPort>,
    Vec<audio_engine::OutputPort>,
    Vec<audio_engine::RecorderPort>,
) {
    let (mut engine, h) = create_engine();
    let EngineHandles {
        mut commands,
        mut ports,
        ..
    } = h;
    let dsp = StripDsp {
        gate: Gate {
            enabled: true,
            threshold_db: -50.0,
            attack_ms: 2.0,
            release_ms: 80.0,
        },
        eq: [
            EqBand {
                enabled: true,
                freq_hz: 120.0,
                gain_db: 3.0,
                q: 0.707,
            },
            EqBand {
                enabled: true,
                freq_hz: 1500.0,
                gain_db: -4.0,
                q: 1.2,
            },
            EqBand {
                enabled: true,
                freq_hz: 7000.0,
                gain_db: 2.5,
                q: 0.707,
            },
        ],
        comp: Compressor {
            enabled: true,
            threshold_db: -24.0,
            ratio: 4.0,
            attack_ms: 5.0,
            release_ms: 100.0,
            makeup_db: 3.0,
        },
    };
    let bus_eq = [
        EqBand {
            enabled: true,
            freq_hz: 100.0,
            gain_db: 2.0,
            q: 0.707,
        },
        EqBand {
            enabled: true,
            freq_hz: 2000.0,
            gain_db: -2.0,
            q: 1.0,
        },
        EqBand {
            enabled: true,
            freq_hz: 9000.0,
            gain_db: 1.5,
            q: 0.707,
        },
    ];
    let mut ins = Vec::new();
    let mut outs = Vec::new();
    let mut recs = Vec::new();
    for i in 0..16u8 {
        let p = ports.acquire_input().unwrap();
        commands
            .push(C::AttachStripInput {
                strip: StripId(i),
                slot: p.slot(),
                channels: 2,
            })
            .unwrap();
        commands
            .push(C::SetStripDsp {
                strip: StripId(i),
                dsp,
            })
            .unwrap();
        commands
            .push(C::SetStripPan {
                strip: StripId(i),
                pan: (i as f32 - 7.5) / 8.0,
            })
            .unwrap();
        ins.push(p);
    }
    for j in 0..16u8 {
        let p = ports.acquire_output().unwrap();
        commands
            .push(C::AttachBusOutput {
                bus: BusId(j),
                slot: p.slot(),
                channels: 2,
            })
            .unwrap();
        commands
            .push(C::SetBusEq {
                bus: BusId(j),
                eq: bus_eq,
            })
            .unwrap();
        commands
            .push(C::SetBusRecord {
                bus: BusId(j),
                enabled: true,
            })
            .unwrap();
        outs.push(p);
        recs.push(ports.acquire_recorder(BusId(j)).unwrap());
    }
    engine.process_block(BLOCK); // apply attaches + bus config (queue holds 1024)
    for i in 0..16u8 {
        for j in 0..16u8 {
            // 1/16 per send keeps the sum around full scale instead of clipping.
            let send = RouteSend {
                enabled: true,
                gain_db: -12.0,
                pre_fader: (i + j) % 2 == 0,
            };
            commands
                .push(C::SetRouteSend {
                    strip: StripId(i),
                    bus: BusId(j),
                    send,
                })
                .unwrap();
        }
        if i % 4 == 3 {
            engine.process_block(BLOCK); // keep the 1024-slot queue from filling
        }
    }
    engine.process_block(BLOCK);
    (engine, ins, outs, recs)
}

struct Stats {
    median: f64,
    p99: f64,
    mean: f64,
    max: f64,
}

fn run_phase(
    engine: &mut audio_engine::MixerEngine,
    ins: &mut [audio_engine::InputPort],
    outs: &mut [audio_engine::OutputPort],
    recs: &mut [audio_engine::RecorderPort],
    blocks: usize,
    input: impl Fn(usize) -> Vec<f32>,
) -> Stats {
    let feed: Vec<Vec<f32>> = (0..ins.len()).map(&input).collect();
    let mut sink = vec![0.0_f32; BLOCK * 2 * 2];
    let mut times = Vec::with_capacity(blocks);
    for _ in 0..blocks {
        for (p, f) in ins.iter_mut().zip(&feed) {
            p.push(f);
        }
        let t0 = Instant::now();
        engine.process_block(ENGINE_BLOCK_FRAMES);
        times.push(t0.elapsed().as_secs_f64() * 1.0e6);
        for p in outs.iter_mut() {
            while p.pop(&mut sink) > 0 {}
        }
        for p in recs.iter_mut() {
            while p.pop(&mut sink) > 0 {}
        }
    }
    times.sort_by(|a, b| a.partial_cmp(b).unwrap());
    Stats {
        median: times[times.len() / 2],
        p99: times[times.len() * 99 / 100],
        mean: times.iter().sum::<f64>() / times.len() as f64,
        max: *times.last().unwrap(),
    }
}

/// Trials per phase. This runs on shared CI/cloud machines where a noisy neighbour can slow
/// any one trial down but can never make the code faster, so the assertion uses the BEST
/// trial's median. Every trial is printed so a flaky machine is visible, not hidden.
#[cfg(not(debug_assertions))]
const TRIALS: usize = 5;
#[cfg(not(debug_assertions))]
const BLOCKS_PER_TRIAL: usize = 6_000;
#[cfg(not(debug_assertions))]
const WARMUP_BLOCKS: usize = 2_000;
// ? A debug build is ~20x slower and asserts nothing here, so keep `cargo test --workspace`
// ? quick: just enough blocks to prove the rig runs.
#[cfg(debug_assertions)]
const TRIALS: usize = 2;
#[cfg(debug_assertions)]
const BLOCKS_PER_TRIAL: usize = 200;
#[cfg(debug_assertions)]
const WARMUP_BLOCKS: usize = 100;

fn best_of(
    engine: &mut audio_engine::MixerEngine,
    ins: &mut [audio_engine::InputPort],
    outs: &mut [audio_engine::OutputPort],
    recs: &mut [audio_engine::RecorderPort],
    label: &str,
    input: impl Fn(usize) -> Vec<f32> + Copy,
) -> Stats {
    let mut best: Option<Stats> = None;
    let mut medians = Vec::new();
    for _ in 0..TRIALS {
        let s = run_phase(engine, ins, outs, recs, BLOCKS_PER_TRIAL, input);
        medians.push(s.median);
        if best.as_ref().is_none_or(|b| s.median < b.median) {
            best = Some(s);
        }
    }
    let b = best.expect("at least one trial");
    println!(
        "16x16 {label:<8}: best median {:6.1} µs  (mean {:6.1}, p99 {:6.1}, max {:7.1})   all trial medians: {}",
        b.median,
        b.mean,
        b.p99,
        b.max,
        medians
            .iter()
            .map(|m| format!("{m:.1}"))
            .collect::<Vec<_>>()
            .join(" / ")
    );
    b
}

#[test]
fn full_16x16_stays_under_the_block_budget() {
    let (mut engine, mut ins, mut outs, mut recs) = build_full_rig();
    let loud = |i: usize| sine(2, 0.25, 220.0 * (1.0 + i as f32 * 0.37), 0, BLOCK);
    let budget_us = BLOCK as f64 / 48_000.0 * 1.0e6;

    // Warm caches and let every smoother / detector settle.
    run_phase(
        &mut engine,
        &mut ins,
        &mut outs,
        &mut recs,
        WARMUP_BLOCKS,
        loud,
    );
    let s = best_of(&mut engine, &mut ins, &mut outs, &mut recs, "loud", loud);
    println!(
        "              = {:.2} % of the {:.0} µs block, {:.2} % of one core",
        s.median / budget_us * 100.0,
        budget_us,
        s.median / budget_us * 100.0
    );

    // Silence after loud audio: filter / envelope tails decay through the denormal range.
    // Any denormal slowdown would show up here as a median far above the loud case.
    let quiet = best_of(
        &mut engine,
        &mut ins,
        &mut outs,
        &mut recs,
        "silence",
        |_| vec![0.0; BLOCK * 2],
    );
    // ? Both results are only *asserted* in release builds; in debug they are just printed.
    #[cfg(debug_assertions)]
    let _ = (&s, &quiet);

    #[cfg(not(debug_assertions))]
    {
        assert!(
            s.median < 50.0,
            "16x16 median {:.1} µs exceeds the 50 µs budget",
            s.median
        );
        assert!(
            quiet.median < 50.0,
            "silence median {:.1} µs exceeds the 50 µs budget",
            quiet.median
        );
        assert!(
            quiet.median < s.median * 2.0 + 5.0,
            "silence is much slower than loud audio ({:.1} vs {:.1} µs): denormals?",
            quiet.median,
            s.median
        );
    }
    #[cfg(debug_assertions)]
    println!("(debug build: budget assertion skipped, run with --release)");
}
