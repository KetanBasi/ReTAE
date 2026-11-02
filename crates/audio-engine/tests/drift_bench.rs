//! S2 benchmark: our cubic Hermite resampler vs rubato's `Async::new_poly(Cubic)`
//! (PLAN.md §5.1, §11 S2: "benchmark both resamplers and keep the cheaper one that passes").
//!
//! "Passes" means: THD+N of a 1 kHz sine < -80 dB, algorithmic latency < 0.1 ms, and the full
//! `ClockDomainAdapter` holds a 208 ppm offset with no underruns when driven by it.
//!
//! Run: `cargo test -p audio-engine --release --test drift_bench -- --nocapture`
//! (The speed comparison only makes sense in a release build; debug just checks correctness.)

use audio_engine::drift::{WORK_FRAMES, Work};
use audio_engine::dsp::Planar;
use audio_engine::{ClockDomainAdapter, DriftResampler, HermiteResampler};
use audio_types::{ENGINE_BLOCK_FRAMES, MAX_CHANNELS};
use rtrb::RingBuffer;
use rubato::audioadapter_buffers::direct::SequentialSliceOfSlices;
use rubato::{Adjustable, Async, FixedAsync, PolynomialDegree, Resampler};
use std::time::Instant;

const FS: f64 = 48_000.0;

// SECTION: rubato behind the same trait
/// `Async::new_poly(.., PolynomialDegree::Cubic, 128, 2, FixedAsync::Output)` driven through
/// `process_into_buffer`, exactly as PLAN.md §5.1 describes. Stereo only (benchmark helper).
struct RubatoCubic {
    rs: Async<f32>,
}

impl RubatoCubic {
    fn new() -> Self {
        let rs = Async::<f32>::new_poly(
            1.0,
            1.01,
            PolynomialDegree::Cubic,
            ENGINE_BLOCK_FRAMES,
            2,
            FixedAsync::Output,
        )
        .expect("rubato constructor");
        Self { rs }
    }
}

impl DriftResampler for RubatoCubic {
    fn reset(&mut self, channels: usize) {
        assert_eq!(channels, 2, "benchmark wrapper is stereo only");
        self.rs.reset();
        self.rs.set_resample_ratio(1.0, false).expect("ratio");
    }
    fn latency_frames(&self) -> f32 {
        self.rs.output_delay() as f32
    }
    fn input_frames_needed(&mut self, ratio: f64, out_frames: usize) -> usize {
        assert_eq!(
            out_frames, ENGINE_BLOCK_FRAMES,
            "rubato's chunk size is fixed at 128 outputs"
        );
        // rubato's ratio is output/input; ours is input/output.
        self.rs
            .set_resample_ratio(1.0 / ratio, false)
            .expect("ratio in range");
        self.rs.input_frames_next()
    }
    fn process(
        &mut self,
        _ratio: f64,
        input: &Work,
        in_frames: usize,
        output: &mut Planar,
        _out_frames: usize,
    ) {
        let ins: [&[f32]; 2] = [&input[0][..in_frames], &input[1][..in_frames]];
        let buf_in = SequentialSliceOfSlices::new(&ins, 2, in_frames).expect("input adapter");
        let [o0, o1, ..] = output;
        let mut outs: [&mut [f32]; 2] = [&mut o0[..], &mut o1[..]];
        let mut buf_out = SequentialSliceOfSlices::new_mut(&mut outs, 2, ENGINE_BLOCK_FRAMES)
            .expect("output adapter");
        self.rs
            .process_into_buffer(&buf_in, &mut buf_out, None)
            .expect("rubato process");
    }
}
// !SECTION: rubato behind the same trait

// SECTION: Quality
/// THD+N (dB) of a 1 kHz sine through a resampler pulled directly at a fixed `ratio`.
fn thd_n_db<R: DriftResampler>(rs: &mut R, ratio: f64) -> f64 {
    rs.reset(2);
    let mut out_all = Vec::new();
    let mut handed = 0usize;
    for _ in 0..400 {
        let need = rs.input_frames_needed(ratio, ENGINE_BLOCK_FRAMES);
        let mut work: Work = [[0.0; WORK_FRAMES]; MAX_CHANNELS];
        let [left, right, ..] = &mut work;
        for (i, (l, r)) in left.iter_mut().zip(right.iter_mut()).enumerate().take(need) {
            let s =
                (0.5 * (core::f64::consts::TAU * 1000.0 * (handed + i) as f64 / FS).sin()) as f32;
            *l = s;
            *r = s;
        }
        handed += need;
        let mut out: Planar = [[0.0; ENGINE_BLOCK_FRAMES]; MAX_CHANNELS];
        rs.process(ratio, &work, need, &mut out, ENGINE_BLOCK_FRAMES);
        out_all.extend_from_slice(&out[0]);
    }
    let w = core::f64::consts::TAU * 1000.0 * ratio / FS; // rad per output sample
    let skip = 2048; // generous start-up skip: rubato's filter warm-up is longer than ours
    let (mut cc, mut cs, mut ss, mut yc, mut ys, mut yy) = (0.0, 0.0, 0.0, 0.0, 0.0, 0.0);
    for (i, &y) in out_all[skip..].iter().enumerate() {
        let (s, c) = (w * (i + skip) as f64).sin_cos();
        let y = f64::from(y);
        cc += c * c;
        cs += c * s;
        ss += s * s;
        yc += y * c;
        ys += y * s;
        yy += y * y;
    }
    let det = cc * ss - cs * cs;
    let (a, b) = ((yc * ss - ys * cs) / det, (ys * cc - yc * cs) / det);
    let fundamental = a * yc + b * ys;
    10.0 * ((yy - fundamental).max(1e-30) / fundamental).log10()
}
// !SECTION: Quality

// SECTION: Cost
/// Median wall time (ns) of one stereo 128-frame pull, alternating the two resamplers in one
/// process so both see the same machine noise.
fn pull_cost_ns() -> (f64, f64) {
    let mut ours = HermiteResampler::new();
    let mut theirs = RubatoCubic::new();
    ours.reset(2);
    theirs.reset(2);
    let mut work: Work = [[0.0; WORK_FRAMES]; MAX_CHANNELS];
    for (c, row) in work.iter_mut().take(2).enumerate() {
        for (i, v) in row.iter_mut().enumerate() {
            *v = ((i * 31 + c * 7) % 100) as f32 * 0.01 - 0.5;
        }
    }
    let mut out: Planar = [[0.0; ENGINE_BLOCK_FRAMES]; MAX_CHANNELS];
    let (mut a, mut b) = (Vec::new(), Vec::new());
    for round in 0..6000 {
        // Walk the ratio like the controller does so neither side sits on a special case.
        let ratio = 1.0 + 0.0008 * (round as f64 * 0.01).sin();

        let t = Instant::now();
        let need = ours.input_frames_needed(ratio, ENGINE_BLOCK_FRAMES);
        ours.process(ratio, &work, need, &mut out, ENGINE_BLOCK_FRAMES);
        std::hint::black_box(&out);
        let ns_ours = t.elapsed().as_nanos() as f64;

        let t = Instant::now();
        let need = theirs.input_frames_needed(ratio, ENGINE_BLOCK_FRAMES);
        theirs.process(ratio, &work, need, &mut out, ENGINE_BLOCK_FRAMES);
        std::hint::black_box(&out);
        let ns_theirs = t.elapsed().as_nanos() as f64;

        if round >= 500 {
            a.push(ns_ours);
            b.push(ns_theirs);
        }
    }
    a.sort_by(|x, y| x.partial_cmp(y).unwrap());
    b.sort_by(|x, y| x.partial_cmp(y).unwrap());
    (a[a.len() / 2], b[b.len() / 2])
}
// !SECTION: Cost

// SECTION: Behaviour inside the real adapter
/// 5 simulated minutes: producer 208 ppm fast, 32-frame chunks. Returns (underruns, mean
/// applied ppm over the last 60 s, worst |smoothed fill - target| in ms after 5 s).
fn adapter_208_ppm<R: DriftResampler>(resampler: R) -> (u32, f64, f64) {
    let (mut prod, mut cons) = RingBuffer::<f32>::new(MAX_CHANNELS * 4096);
    let mut adapter = ClockDomainAdapter::new(resampler);
    adapter.configure(2, 12.0);
    let (fp, fc, chunk) = (48_005.0_f64, 47_995.0_f64, 32usize);
    let (mut next_p, mut next_c) = (chunk as f64 / fp, ENGINE_BLOCK_FRAMES as f64 / fc);
    let (mut sent, mut blocks, mut idx) = (0usize, 0usize, 0usize);
    let table: Vec<f32> = (0..64)
        .map(|i| 0.5 * (core::f64::consts::TAU * i as f64 / 64.0).sin() as f32)
        .collect();
    let mut buf = vec![0.0_f32; chunk * 2];
    let mut out: Planar = [[0.0; ENGINE_BLOCK_FRAMES]; MAX_CHANNELS];
    let (mut ppm_sum, mut ppm_n, mut worst) = (0.0_f64, 0usize, 0.0_f64);
    loop {
        if next_p <= next_c {
            for f in 0..chunk {
                let v = table[idx % 64];
                idx += 1;
                buf[2 * f] = v;
                buf[2 * f + 1] = v;
            }
            prod.push_entire_slice(&buf).expect("ring overflow");
            sent += 1;
            next_p = (sent + 1) as f64 * chunk as f64 / fp;
        } else {
            let t = next_c;
            let r = adapter.pull(&mut cons, &mut out, ENGINE_BLOCK_FRAMES);
            blocks += 1;
            next_c = (blocks + 1) as f64 * ENGINE_BLOCK_FRAMES as f64 / fc;
            if r.running && t >= 5.0 {
                worst = worst.max((f64::from(adapter.fill_ms()) - 12.0).abs());
            }
            if r.running && t >= 240.0 {
                ppm_sum += f64::from(adapter.drift_ppm());
                ppm_n += 1;
            }
            if t >= 300.0 {
                break;
            }
        }
    }
    (adapter.underruns(), ppm_sum / ppm_n.max(1) as f64, worst)
}
// !SECTION: Behaviour inside the real adapter

#[test]
fn drift_resampler_comparison_hermite_vs_rubato_cubic() {
    // ---- quality ----
    let ratios = [1.0, 1.0005, 0.9995, 1.001];
    let ours: Vec<f64> = ratios
        .iter()
        .map(|&r| thd_n_db(&mut HermiteResampler::new(), r))
        .collect();
    let theirs: Vec<f64> = ratios
        .iter()
        .map(|&r| thd_n_db(&mut RubatoCubic::new(), r))
        .collect();
    let worst_ours = ours.iter().cloned().fold(f64::MIN, f64::max);
    let worst_theirs = theirs.iter().cloned().fold(f64::MIN, f64::max);

    // ---- latency ----
    let lat_ours = HermiteResampler::new().latency_frames();
    let mut rb = RubatoCubic::new();
    rb.reset(2);
    let lat_theirs = rb.latency_frames();

    // ---- cost ----
    let (ns_ours, ns_theirs) = pull_cost_ns();

    // ---- behaviour in the adapter ----
    let (u_ours, ppm_ours, fill_ours) = adapter_208_ppm(HermiteResampler::new());
    let (u_theirs, ppm_theirs, fill_theirs) = adapter_208_ppm(RubatoCubic::new());

    println!();
    println!("                      | cubic Hermite (ours) | rubato Async poly Cubic");
    println!("----------------------+----------------------+-------------------------");
    println!("THD+N 1 kHz, worst    | {worst_ours:>17.1} dB | {worst_theirs:>20.1} dB");
    println!(
        "latency               | {:>10.0} fr {:>5.3} ms | {:>13.0} fr {:>5.3} ms",
        lat_ours,
        f64::from(lat_ours) / FS * 1e3,
        lat_theirs,
        f64::from(lat_theirs) / FS * 1e3
    );
    println!("cost / stereo pull    | {ns_ours:>15.0} ns   | {ns_theirs:>18.0} ns");
    println!(
        "  = % of one 2.67 ms block | {:>8.3} %           | {:>11.3} %",
        ns_ours / 2_666_667.0 * 100.0,
        ns_theirs / 2_666_667.0 * 100.0
    );
    println!(
        "adapter @208 ppm, 5 min: underruns {u_ours} vs {u_theirs}; mean ppm {ppm_ours:.1} vs {ppm_theirs:.1}; worst fill err {fill_ours:.2} vs {fill_theirs:.2} ms"
    );
    println!("cost ratio (rubato / ours): {:.1}x", ns_theirs / ns_ours);

    // Ours must pass every criterion ...
    assert!(worst_ours < -80.0, "Hermite THD+N {worst_ours} dB");
    assert!(
        f64::from(lat_ours) / FS * 1e3 < 0.1,
        "Hermite latency {lat_ours} frames"
    );
    assert_eq!(u_ours, 0);
    assert!(
        (ppm_ours - 208.4).abs() < 15.0,
        "Hermite mean ppm {ppm_ours}"
    );
    assert!(fill_ours <= 1.0, "Hermite fill error {fill_ours} ms");
    // ... and, in a release build, be at least as cheap as the alternative (that's the S2
    // decision rule: keep the cheaper one that passes).
    #[cfg(not(debug_assertions))]
    assert!(
        ns_ours <= ns_theirs,
        "rubato ({ns_theirs} ns) is cheaper than Hermite ({ns_ours} ns): revisit the S2 decision"
    );
}
