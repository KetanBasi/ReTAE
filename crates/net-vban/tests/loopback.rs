//! Real sockets, real threads, real mixer: the whole LAN path on the loopback interface.
//!
//! ```text
//!  test ring --> TxStream --UDP--> [proxy: drops] --> RxSocket --> StreamRx --> test ring
//!
//!  test tone --> TxStream --UDP--> RxSocket --> StreamRx == strip input ring
//!                   --> MixerEngine (External clock, assert_no_alloc) --> bus output ring
//!                   --> TxStream --UDP--> RxSocket --> StreamRx --> test ring
//! ```
//!
//! * Everything here is wall-clock driven, so the tests take a lock and run one at a time: two
//!   real-time loops competing for the same cores would measure each other.
//! * Loopback has no jitter and no loss of its own. Loss is added by a small UDP proxy that
//!   drops chosen datagrams, so the expected results are exact, not statistical.
//! * What this cannot show: Wi-Fi behaviour, device clocks, VoiceMeeter interop (PLAN.md §12).

use audio_engine::create_engine;
use audio_types::{
    BusId, ClockDomain, ENGINE_BLOCK_FRAMES, EngineCommand, FixedStr, LanRxConfig, LanTxConfig,
    NetworkBufferPreset, RouteSend, StripId, VbanDataType, WireCodec,
};
use net_vban::jitter::{CONCEAL_FADE_FRAMES, FADE_IN_FRAMES};
use net_vban::rx::{RxPolicy, RxSocket, StreamKey};
use net_vban::stream::{StreamCounters, StreamRx};
use net_vban::tx::TxStream;
use rtrb::{Consumer, Producer, RingBuffer};
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering::Relaxed};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

// * With the checking allocator installed, any alloc/free inside `process_block` aborts the
// * test binary. Compiled out in release.
#[cfg(debug_assertions)]
#[global_allocator]
static ALLOC: assert_no_alloc::AllocDisabler = assert_no_alloc::AllocDisabler;

const BLOCK: usize = ENGINE_BLOCK_FRAMES;
const FS: f32 = 48_000.0;
const CH: usize = 2;
const LOOPBACK: Ipv4Addr = Ipv4Addr::LOCALHOST;
/// What the mixer-less link tests leave in the ring while they "play": 40 ms. Larger than the
/// worst sender wake-up delay seen in the dev container (30 ms), so a late packet is never
/// mistaken for a lost one (predictive concealment only steps in when the ring is low).
/// ? These tests check the link (bit-exact audio, loss accounting, duplication), not latency,
/// ? so they use the Slow preset (48 ms target, ring cap 192 ms) to have room for that cushion.
const CUSHION_SAMPLES: usize = 1920 * CH;

// ? Wall-clock tests must not overlap (see the module docs). A test that failed while holding
// ? the lock poisons it; the next test still wants the lock, not the panic.
static SERIAL: Mutex<()> = Mutex::new(());
fn serial() -> MutexGuard<'static, ()> {
    SERIAL.lock().unwrap_or_else(|e| e.into_inner())
}

fn name(s: &str) -> FixedStr<16> {
    FixedStr::from_str_trunc(s)
}

fn pcm_f32() -> WireCodec {
    WireCodec::Pcm {
        data_type: VbanDataType::Float32,
    }
}

fn tx_config(to: SocketAddrV4, stream: &str, duplicate: bool) -> LanTxConfig {
    LanTxConfig {
        peer_ipv4: to.ip().octets(),
        port: to.port(),
        stream_name: name(stream),
        channels: CH as u8,
        codec: pcm_f32(),
        frames_per_packet: BLOCK as u16,
        dscp: 46,
        duplicate,
    }
}

/// An RX socket with one stream wired to `producer`, keyed like the app does from a config.
fn rx_with_stream(
    stream: &str,
    preset: NetworkBufferPreset,
    producer: Producer<f32>,
) -> (RxSocket, Arc<StreamCounters>) {
    let rx = RxSocket::bind(SocketAddrV4::new(LOOPBACK, 0), RxPolicy::default(), None).unwrap();
    let counters = Arc::new(StreamCounters::default());
    let key = StreamKey::from_config(&LanRxConfig {
        source_ipv4: LOOPBACK.octets(),
        stream_name: name(stream),
        buffer: preset,
        adaptive: false,
    });
    let s = StreamRx::new(producer, CH, preset, counters.clone());
    if rx.add_stream(key, s).is_err() {
        panic!("add_stream refused");
    }
    (rx, counters)
}

fn sleep_until(t: Instant) {
    loop {
        let now = Instant::now();
        if now >= t {
            return;
        }
        let left = t - now;
        if left > Duration::from_micros(1500) {
            thread::sleep(left - Duration::from_micros(1000));
        } else {
            thread::yield_now();
        }
    }
}

fn pop_all(c: &mut Consumer<f32>, into: &mut Vec<f32>) -> usize {
    let n = c.slots();
    if n == 0 {
        return 0;
    }
    let chunk = c.read_chunk(n).unwrap();
    let (a, b) = chunk.as_slices();
    into.extend_from_slice(a);
    into.extend_from_slice(b);
    chunk.commit_all();
    n
}

/// Like [`pop_all`] but leaves `keep` samples in the ring, the way a playing mixer leaves its
/// jitter cushion there. Without it the ring of these mixer-less tests sits at ~0 fill all the
/// time, which is exactly the state in which predictive concealment (NOTES N1) rightly steps
/// in whenever a packet is a few ms late, and these tests want bit-exact audio.
fn pop_keep(c: &mut Consumer<f32>, into: &mut Vec<f32>, keep: usize) {
    let n = c.slots().saturating_sub(keep);
    if n == 0 {
        return;
    }
    let chunk = c.read_chunk(n).unwrap();
    let (a, b) = chunk.as_slices();
    into.extend_from_slice(a);
    into.extend_from_slice(b);
    chunk.commit_all();
}

fn push_all(p: &mut Producer<f32>, samples: &[f32]) {
    let mut chunk = p.write_chunk(samples.len()).expect("test ring has room");
    let (a, b) = chunk.as_mut_slices();
    let (sa, sb) = samples.split_at(a.len());
    a.copy_from_slice(sa);
    b.copy_from_slice(sb);
    chunk.commit_all();
}

// SECTION: A UDP proxy that loses chosen datagrams
struct Proxy {
    addr: SocketAddrV4,
    stop: Arc<AtomicBool>,
    seen: Arc<AtomicU32>,
    dropped: Arc<AtomicU32>,
    thread: Option<JoinHandle<()>>,
}

impl Proxy {
    /// Forward everything to `dest` except the datagrams (counted from 0) `drop_if` picks.
    fn start(dest: SocketAddrV4, drop_if: impl Fn(u32) -> bool + Send + 'static) -> Self {
        let socket = UdpSocket::bind((LOOPBACK, 0)).unwrap();
        socket
            .set_read_timeout(Some(Duration::from_millis(20)))
            .unwrap();
        let addr = match socket.local_addr().unwrap() {
            SocketAddr::V4(a) => a,
            SocketAddr::V6(_) => unreachable!(),
        };
        let stop = Arc::new(AtomicBool::new(false));
        let (seen, dropped) = (Arc::new(AtomicU32::new(0)), Arc::new(AtomicU32::new(0)));
        let thread = {
            let (stop, seen, dropped) = (stop.clone(), seen.clone(), dropped.clone());
            thread::spawn(move || {
                let mut buf = [0u8; 2048];
                while !stop.load(Relaxed) {
                    let Ok((n, _)) = socket.recv_from(&mut buf) else {
                        continue;
                    };
                    let idx = seen.fetch_add(1, Relaxed);
                    if drop_if(idx) {
                        dropped.fetch_add(1, Relaxed);
                    } else {
                        let _ = socket.send_to(&buf[..n], dest);
                    }
                }
            })
        };
        Self {
            addr,
            stop,
            seen,
            dropped,
            thread: Some(thread),
        }
    }
}

impl Drop for Proxy {
    fn drop(&mut self) {
        self.stop.store(true, Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}
// !SECTION: A UDP proxy that loses chosen datagrams

// SECTION: One TX -> RX link
/// Distinct, exactly representable test samples: frame `i`, channel `c`.
fn sample(i: usize, c: usize) -> f32 {
    let v = ((i * 7919) % 20011) as f32 / 20011.0 - 0.5;
    if c == 0 { v } else { -v }
}

struct Run {
    sent_frames: usize,
    out: Vec<f32>,
    counters: Arc<StreamCounters>,
    seen: u32,
    dropped: u32,
    duplicates_sent: u64,
}

/// Send `packets` packets of `BLOCK` frames, one per 3 ms, through an optional lossy proxy,
/// and collect everything that comes out of the receiving ring.
fn run_link(
    packets: usize,
    duplicate: bool,
    drop_if: impl Fn(u32) -> bool + Send + 'static,
) -> Run {
    let (prod_rx, mut cons_rx) = RingBuffer::<f32>::new(1 << 17);
    let (rx, counters) = rx_with_stream("Link", NetworkBufferPreset::Slow, prod_rx);
    let proxy = Proxy::start(rx.local_addr(), drop_if);
    let (mut prod_tx, cons_tx) = RingBuffer::<f32>::new(1 << 15);
    let tx = match TxStream::spawn(tx_config(proxy.addr, "Link", duplicate), cons_tx, CH) {
        Ok(tx) => tx,
        Err((e, _)) => panic!("TxStream::spawn: {e:?}"),
    };

    let mut out = Vec::new();
    let t0 = Instant::now();
    for p in 0..packets {
        sleep_until(t0 + Duration::from_millis(3) * p as u32);
        let block: Vec<f32> = (0..BLOCK * CH)
            .map(|k| sample(p * BLOCK + k / CH, k % CH))
            .collect();
        push_all(&mut prod_tx, &block);
        pop_keep(&mut cons_rx, &mut out, CUSHION_SAMPLES);
    }
    // Let the tail arrive: stop when nothing new has come out for 150 ms.
    let (mut quiet_since, mut last_len) = (Instant::now(), out.len());
    while quiet_since.elapsed() < Duration::from_millis(150) {
        thread::sleep(Duration::from_millis(5));
        pop_keep(&mut cons_rx, &mut out, CUSHION_SAMPLES);
        if out.len() != last_len {
            (quiet_since, last_len) = (Instant::now(), out.len());
        }
    }
    let duplicates_sent = tx.stats().duplicates.load(Relaxed);
    assert_eq!(tx.stats().send_errors.load(Relaxed), 0, "TX socket errors");
    drop(tx);
    drop(rx);
    // The receiver is stopped, so the cushion can be taken without prediction stepping in.
    pop_all(&mut cons_rx, &mut out);
    Run {
        sent_frames: packets * BLOCK,
        out,
        counters,
        seen: proxy.seen.load(Relaxed),
        dropped: proxy.dropped.load(Relaxed),
        duplicates_sent,
    }
}

/// What must come out at (packet `p`, frame `f`, channel `c`) when exactly the packets in
/// `holes` were lost. Spells out the concealer's contract (jitter.rs) with the very same f32
/// operations, so the comparison is bit-exact:
/// - inside a hole: the last played frame faded to zero over `CONCEAL_FADE_FRAMES` (or the
///   hole, if that is shorter);
/// - in the first `FADE_IN_FRAMES` frames of the packet after a hole: the real audio ramped in;
/// - everywhere else: the audio that was sent.
fn expected(p: usize, f: usize, c: usize, holes: &[usize]) -> f32 {
    if holes.contains(&p) {
        let fade = CONCEAL_FADE_FRAMES.clamp(1, BLOCK);
        let g = if f < fade {
            1.0 - (f + 1) as f32 / fade as f32
        } else {
            0.0
        };
        return sample(p * BLOCK - 1, c) * g;
    }
    let real = sample(p * BLOCK + f, c);
    if p > 0 && holes.contains(&(p - 1)) && f < FADE_IN_FRAMES {
        return real * ((f + 1) as f32 / FADE_IN_FRAMES as f32);
    }
    real
}

fn assert_frame_exact(run: &Run, holes: &[usize]) {
    assert_eq!(
        run.out.len(),
        run.sent_frames * CH,
        "frames out != frames in"
    );
    for p in 0..run.sent_frames / BLOCK {
        for f in 0..BLOCK {
            for c in 0..CH {
                let got = run.out[((p * BLOCK) + f) * CH + c];
                let want = expected(p, f, c, holes);
                assert_eq!(
                    got.to_bits(),
                    want.to_bits(),
                    "packet {p} frame {f} ch {c}: {got} vs {want}"
                );
            }
        }
    }
}
// !SECTION: One TX -> RX link

#[test]
fn a_clean_link_delivers_every_sample_bit_exact() {
    let _guard = serial();
    let run = run_link(300, false, |_| false);
    let c = &run.counters;
    println!(
        "clean link: {} packets sent, received {}, lost {}, late {}, duplicate {}, resyncs {}",
        run.seen,
        c.received.load(Relaxed),
        c.lost.load(Relaxed),
        c.late.load(Relaxed),
        c.duplicate.load(Relaxed),
        c.resyncs.load(Relaxed),
    );
    assert_eq!(run.seen, 300);
    assert_frame_exact(&run, &[]);
    assert_eq!(c.received.load(Relaxed), 300);
    for (what, n) in [
        ("lost", c.lost.load(Relaxed)),
        ("late", c.late.load(Relaxed)),
        ("duplicate", c.duplicate.load(Relaxed)),
        ("concealed", c.concealed_frames.load(Relaxed)),
        ("dropped", c.dropped_frames.load(Relaxed)),
        ("resyncs", c.resyncs.load(Relaxed)),
        ("wrong_rate", c.wrong_rate.load(Relaxed)),
        ("bad_payload", c.bad_payload.load(Relaxed)),
    ] {
        assert_eq!(n, 0, "{what}");
    }
    // What the receiver learned about the stream from its headers.
    assert_eq!(c.sample_rate.load(Relaxed), 48_000);
    assert_eq!(c.channels.load(Relaxed), 2);
    assert_eq!(c.frames_per_packet.load(Relaxed), BLOCK as u32);
    assert_eq!(c.format.load(Relaxed), VbanDataType::Float32 as u32);
}

#[test]
fn a_single_lost_packet_is_concealed_in_place_and_counted() {
    let _guard = serial();
    // One datagram in fifty vanishes (never the last ones, so the stream is still running when
    // the hole is noticed).
    let run = run_link(300, false, |i| i % 50 == 25);
    let c = &run.counters;
    let lost_packets: Vec<usize> = (0..300).filter(|&p| p % 50 == 25).collect();
    println!(
        "lossy link: dropped {} of {} datagrams, lost {}, concealed {} frames, received {}",
        run.dropped,
        run.seen,
        c.lost.load(Relaxed),
        c.concealed_frames.load(Relaxed),
        c.received.load(Relaxed),
    );
    assert_eq!(run.dropped as usize, lost_packets.len());
    assert_eq!(c.lost.load(Relaxed) as usize, lost_packets.len());
    assert_eq!(
        c.concealed_frames.load(Relaxed) as usize,
        lost_packets.len() * BLOCK
    );
    // Time did not slip: as many frames came out as went in, every received packet is exactly
    // where it was, each hole is the fade-out of the last played frame, and the packet after
    // it is faded in (see `expected`).
    assert_frame_exact(&run, &lost_packets);
}

#[test]
fn duplication_recovers_every_single_loss_with_no_concealment() {
    let _guard = serial();
    // The wire carries p0 p0' p1 p1' p2 p2' ... Losing every third datagram never takes both
    // copies of one packet.
    let run = run_link(300, true, |i| i % 3 == 2);
    let c = &run.counters;
    println!(
        "duplicated link: {} datagrams, dropped {}, TX duplicates {}, lost {}, duplicate {}, late {}, concealed {}",
        run.seen,
        run.dropped,
        run.duplicates_sent,
        c.lost.load(Relaxed),
        c.duplicate.load(Relaxed),
        c.late.load(Relaxed),
        c.concealed_frames.load(Relaxed),
    );
    assert!(
        run.seen >= 590,
        "about two datagrams per packet, got {}",
        run.seen
    );
    assert!(run.dropped > 150);
    assert!(run.duplicates_sent >= 298);
    assert_eq!(
        c.lost.load(Relaxed),
        0,
        "a copy was there for every lost original"
    );
    assert_eq!(c.concealed_frames.load(Relaxed), 0);
    assert!(
        c.duplicate.load(Relaxed) > 0,
        "surviving pairs are recognised as duplicates"
    );
    assert_frame_exact(&run, &[]);
}

// SECTION: Opus (feature `opus`)
// ? Opus rides in the VBAN `User` codec slot, so a VoiceMeeter never sees it. The codec's
// ? own behaviour is tested in `opus_codec.rs`; this checks that it survives real packets,
// ? the RX demux and the play-out ring, and that a PCM-only build is not affected.
#[cfg(feature = "opus")]
#[test]
fn opus_over_real_sockets_reproduces_the_tone() {
    use audio_types::OpusFrame;
    let _guard = serial();
    const N: usize = 240; // 5 ms
    const PACKETS: usize = 300;
    let tone =
        |i: usize| 0.5 * (core::f64::consts::TAU * 750.0 * i as f64 / f64::from(FS)).sin() as f32;

    let (prod_rx, mut cons_rx) = RingBuffer::<f32>::new(1 << 17);
    let (rx, counters) = rx_with_stream("Opus", NetworkBufferPreset::Slow, prod_rx);
    let (mut prod_tx, cons_tx) = RingBuffer::<f32>::new(1 << 15);
    let mut cfg = tx_config(rx.local_addr(), "Opus", false);
    cfg.codec = WireCodec::Opus {
        bitrate_kbps: 96,
        frame: OpusFrame::Ms5,
    };
    cfg.frames_per_packet = 0;
    let tx = match TxStream::spawn(cfg, cons_tx, CH) {
        Ok(tx) => tx,
        Err((e, _)) => panic!("TxStream::spawn: {e:?}"),
    };
    assert_eq!(tx.frames_per_packet(), N);

    let mut out = Vec::new();
    let t0 = Instant::now();
    for p in 0..PACKETS {
        sleep_until(t0 + Duration::from_millis(5) * p as u32);
        let block: Vec<f32> = (0..N * CH).map(|k| tone(p * N + k / CH)).collect();
        push_all(&mut prod_tx, &block);
        pop_keep(&mut cons_rx, &mut out, CUSHION_SAMPLES);
    }
    let (mut quiet_since, mut last_len) = (Instant::now(), out.len());
    while quiet_since.elapsed() < Duration::from_millis(150) {
        thread::sleep(Duration::from_millis(5));
        pop_keep(&mut cons_rx, &mut out, CUSHION_SAMPLES);
        if out.len() != last_len {
            (quiet_since, last_len) = (Instant::now(), out.len());
        }
    }
    // The receiver is stopped before the cushion is taken, so prediction cannot see an empty
    // ring and conceal packets of a sender that has finished.
    drop(tx);
    drop(rx);
    pop_all(&mut cons_rx, &mut out);

    // Codec delay shifts the tone by a few ms: find the best alignment within +-10 ms.
    let left: Vec<f32> = out.iter().step_by(CH).copied().collect();
    let (mut best, mut best_lag) = (-1.0_f64, 0_isize);
    for lag in -480_isize..=480 {
        let (mut dot, mut ea, mut eb) = (0.0_f64, 1e-12_f64, 1e-12_f64);
        for i in 4800..14_400 {
            let j = (i as isize + lag) as usize;
            let (a, b) = (f64::from(tone(i)), f64::from(left[j]));
            dot += a * b;
            ea += a * a;
            eb += b * b;
        }
        let r = dot / (ea * eb).sqrt();
        if r > best {
            (best, best_lag) = (r, lag);
        }
    }
    let c = &counters;
    // ? The lag is only defined modulo the tone's 64-frame period, so it is not printed as a
    // ? latency; the end-to-end figure comes from the click test below.
    println!(
        "opus link: {} packets, {} frames out, correlation {best:.4}, lost {}, late {}, format {:#x}",
        c.received.load(Relaxed),
        left.len(),
        c.lost.load(Relaxed),
        c.late.load(Relaxed),
        c.format.load(Relaxed),
    );
    assert_eq!(c.received.load(Relaxed) as usize, PACKETS);
    assert_eq!(
        left.len(),
        PACKETS * N,
        "every Opus packet decoded to its 240 frames"
    );
    assert_eq!(c.lost.load(Relaxed), 0);
    assert_eq!(c.late.load(Relaxed), 0);
    assert_eq!(c.unsupported.load(Relaxed), 0);
    assert_eq!(c.bad_payload.load(Relaxed), 0);
    assert_eq!(c.format.load(Relaxed), 0xF0, "reported as Opus");
    assert!(
        best > 0.99,
        "tone not reproduced: correlation {best} at lag {best_lag}"
    );
}
// !SECTION: Opus (feature `opus`)

// SECTION: Whole path through the mixer
#[test]
fn lan_in_mixer_lan_out_is_clean_and_inside_the_latency_budget() {
    let _guard = serial();
    const AMP: f32 = 0.25;
    const TABLE: usize = 64; // 750 Hz
    let tone =
        |i: usize| AMP * (core::f64::consts::TAU * (i % TABLE) as f64 / TABLE as f64).sin() as f32;

    let (mut engine, mut h) = create_engine();
    let in_port = h.ports.acquire_input().unwrap();
    let out_port = h.ports.acquire_output().unwrap();
    let (in_slot, out_slot) = (in_port.slot(), out_port.slot());

    // Receive side of the mixer: network --> strip 0's input ring (== the jitter buffer).
    let (rx_a, counters_a) = rx_with_stream("ToMixer", NetworkBufferPreset::Fast, in_port.producer);
    // Far end: bus 0's output ring --> network --> a receiver that plays the "other PC".
    let (prod_far, mut cons_far) = RingBuffer::<f32>::new(1 << 16);
    let (rx_b, counters_b) = rx_with_stream("FromMixer", NetworkBufferPreset::Fast, prod_far);
    let tx_b = match TxStream::spawn(
        tx_config(rx_b.local_addr(), "FromMixer", false),
        out_port.consumer,
        CH,
    ) {
        Ok(tx) => tx,
        Err((e, _)) => panic!("TxStream::spawn (bus out): {e:?}"),
    };
    // Sender side: a tone ring --> network --> rx_a.
    let (mut prod_tone, cons_tone) = RingBuffer::<f32>::new(1 << 14);
    let tx_a = match TxStream::spawn(
        tx_config(rx_a.local_addr(), "ToMixer", false),
        cons_tone,
        CH,
    ) {
        Ok(tx) => tx,
        Err((e, _)) => panic!("TxStream::spawn (tone): {e:?}"),
    };

    // Control thread wires the strip exactly as the app will.
    let target_ms = counters_a.target_ms();
    assert!(target_ms > 0.0);
    for cmd in [
        EngineCommand::AttachStripInput {
            strip: StripId(0),
            slot: in_slot,
            channels: CH as u8,
        },
        EngineCommand::SetStripClock {
            strip: StripId(0),
            clock: ClockDomain::External { target_ms },
        },
        EngineCommand::AttachBusOutput {
            bus: BusId(0),
            slot: out_slot,
            channels: CH as u8,
        },
        EngineCommand::SetRouteSend {
            strip: StripId(0),
            bus: BusId(0),
            send: RouteSend {
                enabled: true,
                gain_db: 0.0,
                pre_fader: false,
            },
        },
    ] {
        h.commands.push(cmd).expect("command queue full");
    }
    engine.process_block(BLOCK); // applies the commands; the rings are empty, so it is silence
    let underruns0 = h.stats.input_underruns.load(Relaxed);

    // The mixer's clock: one block every 128/48000 s. Send the tone, mix, collect.
    let block = Duration::from_secs_f64(BLOCK as f64 / f64::from(FS));
    let far_cushion = (counters_b.target_ms() * FS / 1000.0) as usize * CH;
    let blocks = 4 * 375; // 4 s
    let click_block = 3 * 375; // a burst after the tone has been running for 3 s
    let (mut out, mut sent_at) = (Vec::<f32>::new(), None::<Instant>);
    let mut click_seen_at = None::<Instant>;
    let t0 = Instant::now();
    let mut tone_idx = 0usize;
    // ? How late the pacing loop itself woke up. Together with the senders' tick gaps this says
    // ? how much scheduling noise the host added; see the assertions below.
    let mut max_late = Duration::ZERO;
    for b in 0..blocks {
        let due = t0 + block * b as u32;
        sleep_until(due);
        max_late = max_late.max(Instant::now().saturating_duration_since(due));
        let mut frames = Vec::with_capacity(BLOCK * CH);
        for f in 0..BLOCK {
            let v = if b == click_block && f < 16 {
                0.9
            } else {
                tone(tone_idx)
            };
            tone_idx += 1;
            frames.extend_from_slice(&[v, v]);
        }
        if b == click_block {
            sent_at = Some(Instant::now());
        }
        push_all(&mut prod_tone, &frames);
        engine.process_block(BLOCK);

        let before = out.len();
        // ? The far end plays like a device would: it leaves its own jitter cushion (B's target)
        // ? in the ring. A ring drained to zero is the one state in which predictive concealment
        // ? (NOTES N1) treats a packet that is a few ms late as lost. The click therefore needs
        // ? this cushion to pass too, so the latency below counts both receive buffers.
        pop_keep(&mut cons_far, &mut out, far_cushion);
        if click_seen_at.is_none() && sent_at.is_some() {
            let now = Instant::now();
            if out[before..].iter().any(|v| v.abs() > 0.6) {
                click_seen_at = Some(now);
            }
        }
    }

    let latency = click_seen_at
        .zip(sent_at)
        .map(|(seen, sent)| seen.duration_since(sent))
        .expect("the click never came out of the far end");
    let settled = (1.5 * f64::from(FS)) as usize * CH; // skip the first 1.5 s
    let click_at = out.len().min(click_block * BLOCK * CH);
    let body = &out[settled..click_at.saturating_sub(40 * BLOCK * CH)];

    // Continuity: the tone's own largest step, plus a margin for the resampler's ripple. A
    // dropout, a fade or a crossfade-drop shows up as a step of the order of the amplitude.
    let max_tone_step = AMP * 2.0 * (core::f32::consts::PI / TABLE as f32).sin();
    let mut worst = 0.0_f32;
    for w in body.chunks_exact(CH).collect::<Vec<_>>().windows(2) {
        worst = worst.max((w[1][0] - w[0][0]).abs());
        assert_eq!(
            w[0][0].to_bits(),
            w[0][1].to_bits(),
            "L and R must stay identical"
        );
    }
    let rms = (body
        .iter()
        .step_by(CH)
        .map(|v| f64::from(*v).powi(2))
        .sum::<f64>()
        / (body.len() / CH) as f64)
        .sqrt();

    println!(
        "lan->mixer->lan: latency {:.1} ms (two Fast receive buffers of {:.1} ms each), worst step x{:.3} of the tone's own, \
         rms {:.4} (want {:.4}), A: lost {} late {} concealed {}, B: lost {} late {} concealed {}, \
         engine underruns {} overruns {}, frames out {}, host noise: loop late {:.1} ms, \
         TX tick gap A {:.1} ms B {:.1} ms",
        latency.as_secs_f64() * 1e3,
        target_ms,
        worst / max_tone_step,
        rms,
        AMP / 2.0_f32.sqrt(),
        counters_a.lost.load(Relaxed),
        counters_a.late.load(Relaxed),
        counters_a.concealed_frames.load(Relaxed),
        counters_b.lost.load(Relaxed),
        counters_b.late.load(Relaxed),
        counters_b.concealed_frames.load(Relaxed),
        h.stats.input_underruns.load(Relaxed) - underruns0,
        h.stats.input_overruns.load(Relaxed),
        out.len() / CH,
        max_late.as_secs_f64() * 1e3,
        tx_a.stats().max_tick_gap_us.load(Relaxed) as f64 / 1e3,
        tx_b.stats().max_tick_gap_us.load(Relaxed) as f64 / 1e3,
    );

    // PLAN.md: 70 ms end to end is the budget; the tone is sent and received on this host, so
    // only the buffers, the packetisers and the mixer block are in the way.
    assert!(latency < Duration::from_millis(70), "latency {latency:?}");
    assert!(
        latency > Duration::from_millis(3),
        "latency {latency:?} is below one packet: the click was misread"
    );
    assert!(
        worst / max_tone_step < 1.05,
        "the tone has a step x{:.3} of its own",
        worst / max_tone_step
    );
    assert!(
        (rms - f64::from(AMP / 2.0_f32.sqrt())).abs() < 0.01,
        "level changed on the way: rms {rms}"
    );
    assert!(body.iter().all(|v| v.is_finite()));
    // ? Underruns and overruns depend on the host. The 12 ms Fast buffer absorbs about 9 ms of
    // ? delay (minus one packet); a sleeping 1 ms sender thread that wakes 20-30 ms late
    // ? drains it and then dumps a backlog burst that overruns it. That happened in the dev
    // ? container (a microVM whose idle vCPUs wake late): every run with an under/overrun had a
    // ? tick gap >= 8 ms, every run with gaps <= 6.2 ms was clean. Real-time thread priorities
    // ? (NOTES N4) are what fixes this on a real host. So: zero is required when the host was
    // ? quiet; on a noisy host the test says so and only requires that the stream recovered.
    let noise = max_late
        .max(Duration::from_micros(u64::from(
            tx_a.stats().max_tick_gap_us.load(Relaxed),
        )))
        .max(Duration::from_micros(u64::from(
            tx_b.stats().max_tick_gap_us.load(Relaxed),
        )));
    let (underruns, overruns) = (
        h.stats.input_underruns.load(Relaxed) - underruns0,
        h.stats.input_overruns.load(Relaxed),
    );
    // Loopback loses nothing. On a quiet host anything counted here is the code's doing. On a
    // noisy host a packet that is delayed by more than the buffer is concealed ahead of time
    // (predictive concealment, NOTES N1) and then counts as lost + late; that is the receiver
    // doing its job, so only a few of them are tolerated.
    let quiet = noise < Duration::from_millis(6);
    for (what, n) in [
        ("A lost", counters_a.lost.load(Relaxed)),
        ("A late", counters_a.late.load(Relaxed)),
        ("B lost", counters_b.lost.load(Relaxed)),
        ("B late", counters_b.late.load(Relaxed)),
    ] {
        if quiet {
            assert_eq!(n, 0, "{what} on a quiet host (wake-up delay {noise:?})");
        } else {
            assert!(n <= 20, "{what} = {n} is more than a few stalls explain");
        }
    }
    if quiet {
        assert_eq!(counters_a.concealed_frames.load(Relaxed), 0, "A concealed");
        assert_eq!(counters_b.concealed_frames.load(Relaxed), 0, "B concealed");
    }
    if quiet {
        assert_eq!(
            (underruns, overruns),
            (0, 0),
            "quiet host (worst wake-up delay {noise:?}) but the mixer ran dry or overflowed"
        );
    } else {
        println!(
            "NOTE: noisy host (worst wake-up delay {:.1} ms), {underruns} underruns / {overruns} overruns tolerated",
            noise.as_secs_f64() * 1e3
        );
        assert!(
            underruns + overruns <= 20,
            "noisy host, but {underruns} underruns / {overruns} overruns is more than a few stalls explain"
        );
    }
    assert_eq!(
        tx_a.stats().send_errors.load(Relaxed) + tx_b.stats().send_errors.load(Relaxed),
        0
    );
    // The mixer cannot wake a thread (it is the RT thread), so the bus-out sender runs off its
    // 1 ms tick; this is the longest gap it saw between two looks at the ring.
    println!(
        "bus-out TX: {} packets, longest tick gap {} us",
        tx_b.stats().packets.load(Relaxed),
        tx_b.stats().max_tick_gap_us.load(Relaxed)
    );
}
// !SECTION: Whole path through the mixer
