//! RT-safety: no allocation in `process_block`, no poisoning by hostile input or commands.
//! (CLAUDE.md law 1; PLAN.md §11 S1 "10 000 x process_block(128) inside assert_no_alloc while
//! another thread floods commands".)

mod common;

use assert_no_alloc::assert_no_alloc;
use audio_engine::{EngineHandles, create_engine, input_slot, output_slot};
use audio_types::{
    BusId, ClockDomain, Compressor, ENGINE_BLOCK_FRAMES, EngineCommand as C, EqBand, Gate,
    PortSlot, RouteSend, StripDsp, StripId,
};
use common::{BLOCK, sine};
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use std::sync::Arc;
use std::thread;

// SECTION: Proof that the guard works
/// If the checking allocator were silently not installed, every "no alloc" test in this
/// crate would pass vacuously. So prove it fires: a child copy of this test binary allocates
/// inside `assert_no_alloc` and must die.
#[cfg(debug_assertions)]
#[test]
fn the_allocation_guard_is_armed() {
    const CHILD: &str = "AUDIO_ENGINE_GUARD_CHILD";
    if std::env::var_os(CHILD).is_some() {
        assert_no_alloc(|| {
            let v = vec![1_u8; 64];
            std::hint::black_box(&v);
        });
        // Only reachable if the allocator did not abort us.
        std::process::exit(42);
    }
    let status = std::process::Command::new(std::env::current_exe().expect("test exe path"))
        .args([
            "the_allocation_guard_is_armed",
            "--exact",
            "--test-threads=1",
        ])
        .env(CHILD, "1")
        .stderr(std::process::Stdio::null())
        .status()
        .expect("spawn child test");
    assert_ne!(
        status.code(),
        Some(42),
        "allocation inside assert_no_alloc was NOT caught"
    );
    assert!(
        !status.success(),
        "child should have been aborted by the allocator"
    );
}
// !SECTION: Proof that the guard works

// SECTION: Random command generator
// * xorshift, so the test needs no external RNG crate.
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
    /// Mostly sane values, sometimes extreme or non-finite.
    fn float(&mut self, lo: f32, hi: f32) -> f32 {
        match self.below(20) {
            0 => f32::NAN,
            1 => f32::INFINITY,
            2 => f32::NEG_INFINITY,
            3 => f32::MAX,
            4 => f32::MIN,
            5 => 0.0,
            _ => lo + (hi - lo) * (self.below(10_001) as f32 / 10_000.0),
        }
    }
    fn strip(&mut self) -> StripId {
        StripId(self.below(20) as u8) // 16..20 are invalid on purpose
    }
    fn bus(&mut self) -> BusId {
        BusId(self.below(20) as u8)
    }
}

fn random_command(r: &mut Rng) -> C {
    match r.below(18) {
        // Clock-domain churn: strips flip between the master clock and drift-corrected
        // external clocks (with hostile targets) while audio is flowing through the adapters.
        16 | 17 => C::SetStripClock {
            strip: r.strip(),
            clock: if r.below(3) == 0 {
                ClockDomain::Master
            } else {
                ClockDomain::External {
                    target_ms: r.float(0.0, 600.0),
                }
            },
        },
        0 => C::SetStripGain {
            strip: r.strip(),
            gain_db: r.float(-90.0, 24.0),
        },
        1 => C::SetStripPan {
            strip: r.strip(),
            pan: r.float(-1.0, 1.0),
        },
        2 => C::SetStripMute {
            strip: r.strip(),
            mute: r.below(4) == 0,
        },
        3 => C::SetStripSolo {
            strip: r.strip(),
            solo: r.below(6) == 0,
        },
        4 | 5 => C::SetStripDsp {
            strip: r.strip(),
            dsp: StripDsp {
                gate: Gate {
                    enabled: r.below(2) == 0,
                    threshold_db: r.float(-90.0, -10.0),
                    attack_ms: r.float(0.0, 50.0),
                    release_ms: r.float(0.0, 500.0),
                },
                eq: core::array::from_fn(|_| EqBand {
                    enabled: r.below(3) != 0,
                    freq_hz: r.float(1.0, 30_000.0),
                    gain_db: r.float(-30.0, 30.0),
                    q: r.float(0.0, 30.0),
                }),
                comp: Compressor {
                    enabled: r.below(2) == 0,
                    threshold_db: r.float(-60.0, 0.0),
                    ratio: r.float(0.0, 30.0),
                    attack_ms: r.float(0.0, 100.0),
                    release_ms: r.float(0.0, 1000.0),
                    makeup_db: r.float(-10.0, 30.0),
                },
            },
        },
        6 | 7 => C::SetRouteSend {
            strip: r.strip(),
            bus: r.bus(),
            send: RouteSend {
                enabled: r.below(3) != 0,
                gain_db: r.float(-60.0, 12.0),
                pre_fader: r.below(2) == 0,
            },
        },
        8 => C::SetBusGain {
            bus: r.bus(),
            gain_db: r.float(-60.0, 12.0),
        },
        9 => C::SetBusMute {
            bus: r.bus(),
            mute: r.below(5) == 0,
        },
        10 => C::SetBusEq {
            bus: r.bus(),
            eq: core::array::from_fn(|_| EqBand {
                enabled: r.below(2) == 0,
                freq_hz: r.float(10.0, 24_000.0),
                gain_db: r.float(-30.0, 30.0),
                q: r.float(0.0, 30.0),
            }),
        },
        11 => C::SetBusRecord {
            bus: r.bus(),
            enabled: r.below(2) == 0,
        },
        // Attach/detach churn, with valid and invalid slots and channel counts.
        12 => C::AttachStripInput {
            strip: r.strip(),
            slot: PortSlot(r.below(52) as u8),
            channels: r.below(11) as u8,
        },
        13 => C::DetachStripInput { strip: r.strip() },
        14 => C::AttachBusOutput {
            bus: r.bus(),
            slot: PortSlot(r.below(52) as u8),
            channels: r.below(11) as u8,
        },
        _ => C::DetachBusOutput { bus: r.bus() },
    }
}
// !SECTION: Random command generator

/// Ordinary commands that put the engine back into a known-good state: everything detached,
/// all parameters at defaults, strips `0..n_strips` -> bus 0 post-fader at unity, then
/// strips `0..n_strips` re-attached to their own input slots and buses `0..n_buses` to theirs.
fn reset_commands(n_strips: u8, n_buses: u8) -> Vec<C> {
    let mut v = Vec::new();
    for i in 0..16u8 {
        v.push(C::DetachStripInput { strip: StripId(i) });
        v.push(C::DetachBusOutput { bus: BusId(i) });
    }
    for i in 0..16u8 {
        let s = StripId(i);
        let b = BusId(i);
        v.push(C::SetStripGain {
            strip: s,
            gain_db: 0.0,
        });
        v.push(C::SetStripPan { strip: s, pan: 0.0 });
        v.push(C::SetStripMute {
            strip: s,
            mute: false,
        });
        v.push(C::SetStripSolo {
            strip: s,
            solo: false,
        });
        v.push(C::SetStripClock {
            strip: s,
            clock: ClockDomain::Master,
        });
        v.push(C::SetStripDsp {
            strip: s,
            dsp: StripDsp::default(),
        });
        for j in 0..16u8 {
            v.push(C::SetRouteSend {
                strip: s,
                bus: BusId(j),
                send: RouteSend {
                    enabled: i < n_strips && j == 0,
                    gain_db: 0.0,
                    pre_fader: false,
                },
            });
        }
        v.push(C::SetBusGain {
            bus: b,
            gain_db: 0.0,
        });
        v.push(C::SetBusMute {
            bus: b,
            mute: false,
        });
        v.push(C::SetBusEq {
            bus: b,
            eq: [EqBand::default(); 3],
        });
        v.push(C::SetBusRecord {
            bus: b,
            enabled: false,
        });
    }
    for i in 0..n_strips {
        v.push(C::AttachStripInput {
            strip: StripId(i),
            slot: input_slot(i as usize),
            channels: 2,
        });
    }
    for j in 0..n_buses {
        v.push(C::AttachBusOutput {
            bus: BusId(j),
            slot: output_slot(j as usize),
            channels: 2,
        });
    }
    v
}

#[test]
fn ten_thousand_blocks_under_a_command_flood_never_allocate() {
    let (mut engine, h) = create_engine();
    let EngineHandles {
        commands,
        mut ports,
        telemetry,
        stats,
    } = h;

    // ? `commands` is a `Producer`, which is single-owner: it moves into the flooder thread
    // ? below and comes back through `join()` when the flood is over.
    // 4 stereo strips -> bus 0, 2 stereo buses, recorder on bus 0.
    let mut ins: Vec<_> = (0..4).map(|_| ports.acquire_input().unwrap()).collect();
    let mut outs: Vec<_> = (0..2).map(|_| ports.acquire_output().unwrap()).collect();
    let mut rec = ports.acquire_recorder(BusId(0)).unwrap();
    let mut commands = commands;
    for (i, p) in ins.iter().enumerate() {
        commands
            .push(C::AttachStripInput {
                strip: StripId(i as u8),
                slot: p.slot(),
                channels: 2,
            })
            .unwrap();
        commands
            .push(C::SetRouteSend {
                strip: StripId(i as u8),
                bus: BusId(0),
                send: RouteSend {
                    enabled: true,
                    gain_db: 0.0,
                    pre_fader: i % 2 == 0,
                },
            })
            .unwrap();
    }
    for (j, p) in outs.iter().enumerate() {
        commands
            .push(C::AttachBusOutput {
                bus: BusId(j as u8),
                slot: p.slot(),
                channels: 2,
            })
            .unwrap();
    }
    commands
        .push(C::SetBusRecord {
            bus: BusId(0),
            enabled: true,
        })
        .unwrap();
    engine.process_block(BLOCK); // apply the setup before the flood starts
    let setup_rejected = stats.rejected_commands.load(Relaxed);
    assert_eq!(setup_rejected, 0, "setup commands must all be valid");

    // The flooder owns the producer half of the queue from now on.
    let stop = Arc::new(AtomicBool::new(false));
    let sent = Arc::new(AtomicU64::new(0));
    let flooder = {
        let (stop, sent) = (Arc::clone(&stop), Arc::clone(&sent));
        thread::spawn(move || {
            let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
            while !stop.load(Relaxed) {
                if commands.push(random_command(&mut rng)).is_ok() {
                    sent.fetch_add(1, Relaxed);
                } else {
                    std::hint::spin_loop(); // queue full: the RT thread is behind, keep pushing
                }
            }
            commands // hand the producer back so we can reset the engine afterwards
        })
    };

    let block = sine(2, 0.3, 440.0, 0, BLOCK);
    let mut sink = vec![0.0_f32; BLOCK * 2 * 4];
    let mut non_finite = 0_u64;
    for _ in 0..10_000 {
        for p in ins.iter_mut() {
            p.push(&block); // partial pushes are fine: a detached strip's ring just fills up
        }
        // Explicit guard in addition to the one inside process_block.
        assert_no_alloc(|| engine.process_block(ENGINE_BLOCK_FRAMES));
        for p in outs.iter_mut() {
            let n = p.pop(&mut sink);
            non_finite += sink[..n].iter().filter(|v| !v.is_finite()).count() as u64;
        }
        let n = rec.pop(&mut sink);
        non_finite += sink[..n].iter().filter(|v| !v.is_finite()).count() as u64;
    }
    stop.store(true, Relaxed);
    let mut commands = flooder.join().expect("flooder thread");

    let sent = sent.load(Relaxed);
    let rejected = stats.rejected_commands.load(Relaxed);
    println!("flood: {sent} commands sent, {rejected} rejected, {non_finite} non-finite samples");
    assert!(sent > 10_000, "flood too weak to mean anything: {sent}");
    assert!(
        rejected > 1_000,
        "garbage commands should have been rejected: {rejected}"
    );
    assert_eq!(non_finite, 0, "NaN/inf reached a ring");

    // ---- The engine must still work normally after all that abuse. ----
    for c in reset_commands(4, 2) {
        while commands.push(c).is_err() {
            engine.process_block(BLOCK); // queue full: let the engine drain some
        }
    }
    for _ in 0..4 {
        engine.process_block(BLOCK); // 4 x 256 = 1024 commands per drain cycle
    }
    // Clear what's left in the output rings, then feed DC and check the exact mix.
    for p in outs.iter_mut() {
        while p.pop(&mut sink) > 0 {}
    }
    while rec.pop(&mut sink) > 0 {}
    let dc: Vec<f32> = vec![0.1; BLOCK * 2];
    let mut last = [0.0_f32; 2];
    for _ in 0..common::SETTLE * 3 {
        for p in ins.iter_mut() {
            p.push(&dc);
        }
        engine.process_block(BLOCK);
        while outs[1].pop(&mut sink) > 0 {}
        let n = outs[0].pop(&mut sink);
        last = [sink[n - 2], sink[n - 1]];
    }
    for v in last {
        assert!((v - 0.4).abs() < 1.0e-5, "engine did not recover: {v}");
    }
    assert!(telemetry.rt_load_permille.load(Relaxed) < u32::MAX);
}

#[test]
fn hostile_audio_at_maximum_gain_cannot_poison_the_engine() {
    let (mut engine, h) = create_engine();
    let EngineHandles {
        mut commands,
        mut ports,
        telemetry,
        ..
    } = h;
    let mut ins: Vec<_> = (0..2).map(|_| ports.acquire_input().unwrap()).collect();
    let mut out = ports.acquire_output().unwrap();

    let max_dsp = StripDsp {
        gate: Gate {
            enabled: true,
            threshold_db: -100.0,
            attack_ms: 0.1,
            release_ms: 1.0,
        },
        eq: [EqBand {
            enabled: true,
            freq_hz: 100.0,
            gain_db: 24.0,
            q: 18.0,
        }; 3],
        comp: Compressor {
            enabled: true,
            threshold_db: 0.0,
            ratio: 100.0,
            attack_ms: 0.1,
            release_ms: 5.0,
            makeup_db: 24.0,
        },
    };
    for (i, p) in ins.iter().enumerate() {
        let s = StripId(i as u8);
        commands
            .push(C::AttachStripInput {
                strip: s,
                slot: p.slot(),
                channels: 2,
            })
            .unwrap();
        commands
            .push(C::SetStripGain {
                strip: s,
                gain_db: 24.0,
            })
            .unwrap();
        commands
            .push(C::SetStripDsp {
                strip: s,
                dsp: max_dsp,
            })
            .unwrap();
        commands
            .push(C::SetRouteSend {
                strip: s,
                bus: BusId(0),
                send: RouteSend {
                    enabled: true,
                    gain_db: 24.0,
                    pre_fader: false,
                },
            })
            .unwrap();
    }
    commands
        .push(C::AttachBusOutput {
            bus: BusId(0),
            slot: out.slot(),
            channels: 2,
        })
        .unwrap();
    commands
        .push(C::SetBusGain {
            bus: BusId(0),
            gain_db: 24.0,
        })
        .unwrap();
    commands
        .push(C::SetBusEq {
            bus: BusId(0),
            eq: [EqBand {
                enabled: true,
                freq_hz: 80.0,
                gain_db: 24.0,
                q: 18.0,
            }; 3],
        })
        .unwrap();
    engine.process_block(BLOCK);

    // Hostile input: the worst finite values, NaN and infinity, in a rotating pattern.
    let poison = [
        f32::MAX,
        f32::MIN,
        f32::NAN,
        f32::INFINITY,
        f32::NEG_INFINITY,
        3.0e38,
        -1.0e30,
        0.5,
    ];
    let mut sink = vec![0.0_f32; BLOCK * 2 * 2];
    let mut bad = 0;
    for blk in 0..300 {
        let samples: Vec<f32> = (0..BLOCK * 2)
            .map(|i| poison[(i / 2 + blk) % poison.len()])
            .collect();
        for p in ins.iter_mut() {
            p.push(&samples);
        }
        engine.process_block(BLOCK);
        let n = out.pop(&mut sink);
        bad += sink[..n].iter().filter(|v| !v.is_finite()).count();
    }
    assert_eq!(bad, 0, "non-finite samples reached the output ring");
    for s in 0..2 {
        for ch in 0..2 {
            assert!(audio_types::load_f32(&telemetry.strips[s].peak_db[ch]).is_finite());
            assert!(audio_types::load_f32(&telemetry.strips[s].rms_db[ch]).is_finite());
            assert!(audio_types::load_f32(&telemetry.buses[0].peak_db[ch]).is_finite());
        }
    }

    // Back to normal parameters and normal audio: the output must be exactly clean again.
    for i in 0..2u8 {
        let s = StripId(i);
        commands
            .push(C::SetStripGain {
                strip: s,
                gain_db: 0.0,
            })
            .unwrap();
        commands
            .push(C::SetStripDsp {
                strip: s,
                dsp: StripDsp::default(),
            })
            .unwrap();
        commands
            .push(C::SetRouteSend {
                strip: s,
                bus: BusId(0),
                send: RouteSend {
                    enabled: true,
                    gain_db: 0.0,
                    pre_fader: false,
                },
            })
            .unwrap();
    }
    commands
        .push(C::SetBusGain {
            bus: BusId(0),
            gain_db: 0.0,
        })
        .unwrap();
    commands
        .push(C::SetBusEq {
            bus: BusId(0),
            eq: [EqBand::default(); 3],
        })
        .unwrap();
    let dc = vec![0.25_f32; BLOCK * 2];
    let mut last = 0.0;
    for _ in 0..common::SETTLE * 2 {
        for p in ins.iter_mut() {
            p.push(&dc);
        }
        engine.process_block(BLOCK);
        let n = out.pop(&mut sink);
        last = sink[n - 1];
    }
    assert!(
        (last - 0.5).abs() < 1.0e-5,
        "engine stayed poisoned: {last}"
    );
}
