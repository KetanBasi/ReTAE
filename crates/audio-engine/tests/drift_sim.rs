//! Clock-drift simulations (PLAN.md §11 S2).
//!
//! A producer (device/network thread) and a consumer (the mixer) run on *different* sample
//! clocks. Virtual time is advanced event by event, so a "10 minute" run takes seconds. The
//! producer pushes chunks into an `rtrb` ring at its own rate; the consumer pulls 128-frame
//! blocks at its own rate through a `ClockDomainAdapter`.
//!
//! Pass criteria (S2): after a 5 s settle the fill stays within ±1 ms of the target and there
//! are 0 underruns, for the plan's 48 005 Hz -> 47 995 Hz case, its mirror image and ±100 ppm.
//! On top of that: the learned drift matches the true offset, and the output never clicks.

use audio_engine::ClockDomainAdapter;
use audio_engine::dsp::Planar;
use audio_types::{ENGINE_BLOCK_FRAMES, MAX_CHANNELS};
use rtrb::RingBuffer;

const NOMINAL: f64 = 48_000.0;
const BLOCK: usize = ENGINE_BLOCK_FRAMES;
/// 750 Hz at 48 kHz = exactly 64 samples per period, so the tone comes from a 64-entry table.
const TONE_PERIOD: usize = 64;
const TONE_AMP: f32 = 0.5;

#[derive(Clone, Copy)]
struct Scenario {
    name: &'static str,
    /// Producer sample rate (Hz).
    fp: f64,
    /// Consumer (mixer) sample rate (Hz).
    fc: f64,
    /// Frames per producer delivery.
    chunk: usize,
    /// Extra random arrival delay per delivery, up to this many ms (arrivals stay in order).
    jitter_ms: f64,
    target_ms: f32,
    seconds: f64,
}

#[derive(Debug)]
struct Report {
    true_ppm: f64,
    learned_ppm: f32,
    /// |EMA fill - target| worst case after settle (ms).
    ema_err_ms: f32,
    /// Worst |mean fill over a 5 s window - target| after settle (ms).
    window_mean_err_ms: f32,
    /// Worst |instantaneous fill - target| after settle (ms).
    inst_err_ms: f32,
    underruns: u32,
    overruns: u32,
    /// Largest step between consecutive output samples after settle, relative to the tone's.
    step_ratio: f32,
}

fn tone_table() -> [f32; TONE_PERIOD] {
    core::array::from_fn(|i| {
        TONE_AMP * (core::f64::consts::TAU * i as f64 / TONE_PERIOD as f64).sin() as f32
    })
}

fn simulate(s: Scenario) -> Report {
    let table = tone_table();
    let (mut prod, mut cons) = RingBuffer::<f32>::new(MAX_CHANNELS * 4096);
    let mut adapter = ClockDomainAdapter::default();
    adapter.configure(2, s.target_ms);

    let mut rng = 0x2545_F491_4F6C_DD1D_u64;
    let mut jitter = move || {
        rng ^= rng << 13;
        rng ^= rng >> 7;
        rng ^= rng << 17;
        (rng >> 11) as f64 / (1u64 << 53) as f64
    };

    let chunk_s = s.chunk as f64 / s.fp;
    let block_s = BLOCK as f64 / s.fc;
    let mut sent_chunks = 0usize;
    let mut tone_idx = 0usize;
    let mut last_arrival = 0.0_f64;
    let arrival = |j: usize, jit: &mut dyn FnMut() -> f64, last: &mut f64| {
        let t = (j + 1) as f64 * chunk_s + jit() * s.jitter_ms * 1e-3;
        *last = t.max(*last); // deliveries never overtake each other
        *last
    };
    let mut next_p = arrival(0, &mut jitter, &mut last_arrival);
    let mut blocks = 0usize;
    let mut next_c = block_s;

    let settle_s = 5.0;
    let window_s = 5.0;
    // ? The applied ppm is `integral + Kp·error`, so a single reading wobbles with the fill
    // ? error. Averaged over the last minute it must equal the true offset: a bounded fill
    // ? means the long-run production and consumption rates are equal.
    let mut ppm_sum = 0.0_f64;
    let mut ppm_n = 0usize;
    let target = f64::from(s.target_ms);
    let (mut ema_err, mut window_err, mut inst_err) = (0.0_f64, 0.0_f64, 0.0_f64);
    let (mut win_sum, mut win_n, mut win_start) = (0.0_f64, 0usize, settle_s);
    let mut worst_step = 0.0_f32;
    let mut prev_sample: Option<f32> = None;
    let max_tone_step = TONE_AMP * 2.0 * (core::f32::consts::PI / TONE_PERIOD as f32).sin();

    let mut out: Planar = [[0.0; BLOCK]; MAX_CHANNELS];
    let mut chunk_buf = vec![0.0_f32; s.chunk * 2];
    loop {
        if next_p <= next_c {
            // Producer delivery.
            for f in 0..s.chunk {
                let v = table[tone_idx % TONE_PERIOD];
                tone_idx += 1;
                chunk_buf[2 * f] = v;
                chunk_buf[2 * f + 1] = v;
            }
            // A full ring would mean the producer outran everything; the adapter's overrun
            // handling keeps the fill far below capacity, so this must never fail.
            prod.push_entire_slice(&chunk_buf).expect("ring overflowed");
            sent_chunks += 1;
            next_p = arrival(sent_chunks, &mut jitter, &mut last_arrival);
        } else {
            // Consumer block.
            let t = next_c;
            let fill_ms = cons.slots() as f64 / 2.0 / NOMINAL * 1000.0;
            let report = adapter.pull(&mut cons, &mut out, BLOCK);
            blocks += 1;
            next_c = (blocks + 1) as f64 * block_s;
            if t >= s.seconds - 60.0 && report.running {
                ppm_sum += f64::from(adapter.drift_ppm());
                ppm_n += 1;
            }
            if t >= settle_s && report.running {
                inst_err = inst_err.max((fill_ms - target).abs());
                ema_err = ema_err.max((f64::from(adapter.fill_ms()) - target).abs());
                win_sum += fill_ms;
                win_n += 1;
                if t - win_start >= window_s {
                    window_err = window_err.max((win_sum / win_n as f64 - target).abs());
                    (win_sum, win_n, win_start) = (0.0, 0, t);
                }
                for &v in &out[0][..BLOCK] {
                    if let Some(p) = prev_sample {
                        worst_step = worst_step.max((v - p).abs());
                    }
                    prev_sample = Some(v);
                }
            }
            if t >= s.seconds {
                break;
            }
        }
    }

    Report {
        true_ppm: (s.fp / s.fc - 1.0) * 1.0e6,
        learned_ppm: (ppm_sum / ppm_n.max(1) as f64) as f32,
        ema_err_ms: ema_err as f32,
        window_mean_err_ms: window_err as f32,
        inst_err_ms: inst_err as f32,
        underruns: adapter.underruns(),
        overruns: adapter.overruns(),
        step_ratio: worst_step / max_tone_step,
    }
}

/// Run a scenario and assert the S2 criteria.
///
/// * Always: 0 underruns, 0 overruns, the mean applied ppm over the last minute equals the
///   true clock offset, and the output never steps beyond the tone's own step.
/// * Fill accuracy: for a smooth producer (<= 32-frame chunks, no jitter) the S2 spec applies
///   literally: the smoothed fill is within ±1 ms of the target.
///
/// ? Chunky producers get a looser, *justified* bound. The fill sampled at block starts is a
/// ? staircase with steps of one chunk; when the chunk is about a block long (the VBAN default
/// ? of 128 frames!) a 208 ppm offset slides that staircase through a full chunk every 12.8 s.
/// ? That is slower than the 1 s EMA can smooth, so the loop partly follows it and the smoothed
/// ? fill swings by up to half a chunk. Removing it needs packet arrival timestamps (S3's RX
/// ? thread), not a better filter, so the allowance here is ±(1 ms + ½ chunk + ½ jitter).
fn check(s: Scenario) -> Report {
    let r = simulate(s);
    println!(
        "{:<34} true {:>7.1} ppm mean-applied {:>7.1} ppm | err ms: ema {:.2} win {:.2} inst {:.2} | underruns {} overruns {} | step x{:.4}",
        s.name,
        r.true_ppm,
        r.learned_ppm,
        r.ema_err_ms,
        r.window_mean_err_ms,
        r.inst_err_ms,
        r.underruns,
        r.overruns,
        r.step_ratio
    );
    let chunk_ms = s.chunk as f32 / 48.0;
    let smooth_producer = s.chunk <= 32 && s.jitter_ms == 0.0;
    let fill_tol = if smooth_producer {
        1.0
    } else {
        1.0 + 0.5 * chunk_ms + 0.5 * s.jitter_ms as f32
    };
    let inst_tol = 1.0
        + 0.5 * chunk_ms
        + if smooth_producer {
            0.0
        } else {
            0.5 * chunk_ms + s.jitter_ms as f32
        };
    let ppm_tol = if smooth_producer { 15.0 } else { 40.0 };

    assert_eq!(r.underruns, 0, "{}: underruns", s.name);
    assert_eq!(r.overruns, 0, "{}: overruns", s.name);
    assert!(
        r.ema_err_ms <= fill_tol,
        "{}: smoothed fill off target by {} ms (allowed {fill_tol})",
        s.name,
        r.ema_err_ms
    );
    assert!(
        r.window_mean_err_ms <= fill_tol,
        "{}: 5 s mean fill off target by {} ms (allowed {fill_tol})",
        s.name,
        r.window_mean_err_ms
    );
    assert!(
        r.inst_err_ms <= inst_tol,
        "{}: instantaneous fill off target by {} ms (allowed {inst_tol})",
        s.name,
        r.inst_err_ms
    );
    assert!(
        (f64::from(r.learned_ppm) - r.true_ppm).abs() < ppm_tol,
        "{}: mean applied {} ppm, true {} ppm",
        s.name,
        r.learned_ppm,
        r.true_ppm
    );
    // The ratio moves by at most ~1000 ppm and slowly: steps stay at the tone's own size.
    assert!(
        r.step_ratio < 1.003,
        "{}: output stepped x{} the tone's own step",
        s.name,
        r.step_ratio
    );
    r
}

const TEN_MIN: f64 = 600.0;

fn scenario(name: &'static str, fp: f64, fc: f64, chunk: usize) -> Scenario {
    Scenario {
        name,
        fp,
        fc,
        chunk,
        jitter_ms: 0.0,
        target_ms: 12.0,
        seconds: TEN_MIN,
    }
}

// SECTION: The S2 acceptance scenarios
// * Each runs 10 simulated minutes.
#[test]
fn drift_48005_to_47995_hz_fill_holds_for_ten_minutes() {
    // Producer faster than consumer by 208 ppm: the ring wants to fill up.
    // 32-frame producer chunks (0.67 ms): near-continuous delivery, so the S2 spec applies as
    // written (smoothed fill within ±1 ms of target, 0 underruns).
    check(scenario(
        "48005 -> 47995, 32-frame chunks",
        48_005.0,
        47_995.0,
        32,
    ));
}

#[test]
fn drift_47995_to_48005_hz_fill_holds_for_ten_minutes() {
    // The mirror image: producer slower than consumer, the ring wants to drain.
    check(scenario(
        "47995 -> 48005, 32-frame chunks",
        47_995.0,
        48_005.0,
        32,
    ));
}

#[test]
fn drift_plus_and_minus_100_ppm_against_a_nominal_consumer() {
    check(scenario(
        "+100 ppm producer",
        48_000.0 * 1.000_1,
        48_000.0,
        32,
    ));
    check(scenario(
        "-100 ppm producer",
        48_000.0 * 0.999_9,
        48_000.0,
        32,
    ));
}

#[test]
fn drift_a_perfectly_matched_clock_learns_nothing_and_stays_put() {
    let r = check(scenario("matched 48000 -> 48000", 48_000.0, 48_000.0, 32));
    assert!(
        r.learned_ppm.abs() < 10.0,
        "invented a drift of {} ppm",
        r.learned_ppm
    );
}
// !SECTION: The S2 acceptance scenarios

// SECTION: Chunky and jittery producers
// ? Real producers deliver audio in bursts (a WASAPI callback is 480 frames = 10 ms; a VBAN
// ? packet is 128 frames), so the fill level sampled at block starts is a sawtooth. The
// ? controller works on the EMA of it, so the *mean* must hit the target even though the
// ? instantaneous value cannot: it is allowed to swing by one chunk.
#[test]
fn drift_128_frame_packets_fill_mean_holds_and_never_underruns() {
    check(scenario(
        "+208 ppm, 128-frame packets",
        48_005.0,
        47_995.0,
        128,
    ));
    check(scenario(
        "-208 ppm, 128-frame packets",
        47_995.0,
        48_005.0,
        128,
    ));
}

#[test]
fn drift_480_frame_device_callbacks_fill_mean_holds_and_never_underruns() {
    // 480 frames = 10 ms bursts; target 24 ms leaves room for the 10 ms swing.
    let s = |name, fp, fc| Scenario {
        target_ms: 24.0,
        ..scenario(name, fp, fc, 480)
    };
    check(s("+208 ppm, 480-frame callbacks", 48_005.0, 47_995.0));
    check(s("-208 ppm, 480-frame callbacks", 47_995.0, 48_005.0));
}

#[test]
fn drift_optimal_preset_6_ms_with_default_128_frame_packets_survives_208_ppm() {
    // The tightest shipped combination: `NetworkBufferPreset::Optimal` (6 ms) with the default
    // 128-frame packets (`effective_target_ms` = max(6, 2 x 2.67) = 6 ms). The staircase swing
    // described above is up to ±2 ms here, so this is the case most likely to underrun.
    for (name, fp, fc) in [
        ("Optimal 6 ms, +208 ppm", 48_005.0, 47_995.0),
        ("Optimal 6 ms, -208 ppm", 47_995.0, 48_005.0),
    ] {
        let s = Scenario {
            target_ms: 6.0,
            ..scenario(name, fp, fc, 128)
        };
        check(s);
    }
}

#[test]
fn drift_network_jitter_up_to_6_ms_is_absorbed_without_underruns() {
    // Packets arrive up to 6 ms late (in order). With a 20 ms target the buffer never runs dry.
    let s = Scenario {
        jitter_ms: 6.0,
        target_ms: 20.0,
        ..scenario("jittery LAN, +150 ppm", 48_007.2, 48_000.0, 128)
    };
    check(s);
}
// !SECTION: Chunky and jittery producers

// SECTION: Recovery
#[test]
fn drift_a_producer_stall_rebuffers_once_and_the_drift_estimate_survives_it() {
    // Run 60 s at +208 ppm, stop the producer for 200 ms (a dropout), then carry on.
    let table = tone_table();
    let (mut prod, mut cons) = RingBuffer::<f32>::new(MAX_CHANNELS * 4096);
    let mut adapter = ClockDomainAdapter::default();
    adapter.configure(2, 12.0);
    let (fp, fc, chunk) = (48_005.0_f64, 47_995.0_f64, 64usize);
    let (mut next_p, mut next_c) = (chunk as f64 / fp, BLOCK as f64 / fc);
    let (mut sent, mut blocks, mut idx) = (0usize, 0usize, 0usize);
    let mut out: Planar = [[0.0; BLOCK]; MAX_CHANNELS];
    let mut buf = vec![0.0_f32; chunk * 2];
    // Mean applied ppm in the 20 s before the dropout and the last 30 s of the run (a single
    // reading includes the P-term and wobbles with the fill error).
    let (mut before_sum, mut before_n, mut after_sum, mut after_n) =
        (0.0_f64, 0usize, 0.0_f64, 0usize);
    let mut running_after_stall = false;
    let mut worst_step = 0.0_f32;
    let mut prev = 0.0_f32;
    loop {
        let t_now = next_p.min(next_c);
        let stalled = (60.0..60.2).contains(&t_now);
        if next_p <= next_c {
            if !stalled {
                for f in 0..chunk {
                    let v = table[idx % TONE_PERIOD];
                    idx += 1;
                    buf[2 * f] = v;
                    buf[2 * f + 1] = v;
                }
                prod.push_entire_slice(&buf).expect("ring overflowed");
            }
            sent += 1;
            next_p = (sent + 1) as f64 * chunk as f64 / fp;
        } else {
            let r = adapter.pull(&mut cons, &mut out, BLOCK);
            blocks += 1;
            if (40.0..60.0).contains(&next_c) && r.running {
                before_sum += f64::from(adapter.drift_ppm());
                before_n += 1;
            }
            if next_c > 150.0 && r.running {
                after_sum += f64::from(adapter.drift_ppm());
                after_n += 1;
            }
            if next_c > 62.0 {
                running_after_stall |= r.running;
            }
            if next_c > 5.0 && r.running {
                for &v in &out[0][..BLOCK] {
                    worst_step = worst_step.max((v - prev).abs());
                    prev = v;
                }
            } else {
                prev = out[0][BLOCK - 1];
            }
            next_c = (blocks + 1) as f64 * BLOCK as f64 / fc;
            if next_c > 180.0 {
                break;
            }
        }
    }
    let before = before_sum / before_n.max(1) as f64;
    let after = after_sum / after_n.max(1) as f64;
    println!(
        "stall: underruns {} overruns {} mean applied before {before:.1} after {after:.1} ppm, worst step {worst_step:.4}",
        adapter.underruns(),
        adapter.overruns(),
    );
    assert_eq!(
        adapter.underruns(),
        1,
        "a single dropout must be one underrun event"
    );
    assert!(running_after_stall, "never resumed after the dropout");
    assert!(
        (before - 208.4).abs() < 25.0,
        "had not learned the drift before the stall: {before}"
    );
    assert!(
        (after - 208.4).abs() < 25.0,
        "drift estimate lost after the stall: {after} ppm"
    );
    // Fade-out/in around the dropout keeps even the worst step well below a full-scale click.
    let tone_step = TONE_AMP * 2.0 * (core::f32::consts::PI / TONE_PERIOD as f32).sin();
    assert!(
        worst_step <= tone_step * 1.01 + 1.0e-3,
        "dropout clicked: step {worst_step}"
    );
}
// !SECTION: Recovery
