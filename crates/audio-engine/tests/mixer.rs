//! Routing / level / metering behaviour of `MixerEngine`, driven through the public API only.

mod common;

use audio_types::{
    BusId, Compressor, EngineCommand as C, EqBand, Gate, StripDsp, StripId, load_f32,
};
use common::*;
use core::sync::atomic::Ordering::Relaxed;

fn s(i: u8) -> StripId {
    StripId(i)
}
fn b(i: u8) -> BusId {
    BusId(i)
}

// SECTION: Basic signal path
#[test]
fn unity_passthrough_is_bit_exact() {
    let mut r = Rig::unity();
    let sig = tone(0.5, 1000.0);
    let out = r.run(0, 0, SETTLE + 10, &sig);
    // Compare the settled tail with the generator.
    let frames = out.len() / 2;
    for f in frames - 10 * BLOCK..frames {
        for c in 0..2 {
            assert_eq!(out[f * 2 + c], sig(f, c), "frame {f} ch {c}");
        }
    }
}

#[test]
fn a_bus_with_no_sends_still_streams_silence() {
    // ? The output device needs a block every callback, even when nothing is routed to it.
    let mut r = Rig::new();
    r.attach_output(0, 2);
    let out = r.run_many(5, 0, &[]);
    assert_eq!(out.len(), 5 * BLOCK * 2);
    assert!(out.iter().all(|&v| v == 0.0));
}

#[test]
fn unattached_strip_is_silent_and_harmless() {
    let mut r = Rig::new();
    r.attach_output(0, 2);
    r.route(5, 0, 0.0, false); // strip 5 has no input
    let out = r.run_many(10, 0, &[]);
    assert!(out.iter().all(|&v| v == 0.0));
}
// !SECTION: Basic signal path

// SECTION: Fader / mute / solo
#[test]
fn fader_moves_smoothly_and_lands_exactly() {
    let mut r = Rig::unity();
    r.run(0, 0, SETTLE, dc(0.5));
    r.send(C::SetStripGain {
        strip: s(0),
        gain_db: -20.0,
    });
    let out = r.run(0, 0, SETTLE, dc(0.5));
    let l = chan(&out, 2, 0);
    for w in l.windows(2) {
        assert!(w[1] <= w[0] + 1.0e-7, "fader must fall monotonically");
        assert!(w[0] - w[1] < 0.003, "zipper step {} -> {}", w[0], w[1]);
    }
    assert!((l[l.len() - 1] - 0.05).abs() < 1.0e-5, "{}", l[l.len() - 1]);
    // The very first sample after the command is still ~old level: it ramps, it doesn't jump.
    assert!(l[0] > 0.45, "{}", l[0]);
}

#[test]
fn fader_at_minus_100_db_is_exact_silence() {
    let mut r = Rig::unity();
    r.send(C::SetStripGain {
        strip: s(0),
        gain_db: -100.0,
    });
    let out = r.run(0, 0, SETTLE, dc(0.5));
    assert!(tail(&out, 2, 0, BLOCK).iter().all(|&v| v == 0.0));
}

#[test]
fn mute_ramps_out_and_back() {
    let mut r = Rig::unity();
    r.run(0, 0, SETTLE, dc(0.5));
    r.send(C::SetStripMute {
        strip: s(0),
        mute: true,
    });
    let out = r.run(0, 0, SETTLE, dc(0.5));
    let l = chan(&out, 2, 0);
    assert!(l[0] > 0.45, "mute must ramp, not cut: {}", l[0]);
    assert!(l[l.len() - BLOCK..].iter().all(|&v| v == 0.0));
    r.send(C::SetStripMute {
        strip: s(0),
        mute: false,
    });
    let out = r.run(0, 0, SETTLE, dc(0.5));
    assert_eq!(*chan(&out, 2, 0).last().unwrap(), 0.5);
}

#[test]
fn solo_silences_everyone_else() {
    let mut r = Rig::unity();
    r.attach_input(1, 2);
    r.route(1, 0, 0.0, false);
    let (a, bb) = (dc(0.2), dc(0.4));
    let feeds: [(u8, Sig); 2] = [(0, &a), (1, &bb)];
    let level = |r: &mut Rig| *chan(&r.run_many(SETTLE, 0, &feeds), 2, 0).last().unwrap();

    assert!((level(&mut r) - 0.6).abs() < 1.0e-6);
    r.send(C::SetStripSolo {
        strip: s(1),
        solo: true,
    });
    assert!(
        (level(&mut r) - 0.4).abs() < 1.0e-6,
        "only the soloed strip is heard"
    );
    r.send(C::SetStripSolo {
        strip: s(0),
        solo: true,
    });
    assert!(
        (level(&mut r) - 0.6).abs() < 1.0e-6,
        "two soloed strips are both heard"
    );
    r.send(C::SetStripMute {
        strip: s(1),
        mute: true,
    });
    assert!((level(&mut r) - 0.2).abs() < 1.0e-6, "mute beats solo");
    r.send(C::SetStripSolo {
        strip: s(0),
        solo: false,
    });
    r.send(C::SetStripSolo {
        strip: s(1),
        solo: false,
    });
    r.send(C::SetStripMute {
        strip: s(1),
        mute: false,
    });
    assert!(
        (level(&mut r) - 0.6).abs() < 1.0e-6,
        "unsolo restores everything"
    );
}
// !SECTION: Fader / mute / solo

// SECTION: Sends
#[test]
fn pre_fader_send_ignores_the_fader_post_fader_follows_it() {
    let mut r = Rig::new();
    r.attach_input(0, 2);
    r.attach_output(0, 2);
    r.attach_output(1, 2);
    r.route(0, 0, 0.0, false); // bus 0: post-fader
    r.route(0, 1, 0.0, true); // bus 1: pre-fader
    r.send(C::SetStripGain {
        strip: s(0),
        gain_db: -20.0,
    });
    let sig = dc(0.5);
    let mut post = Vec::new();
    let mut pre = Vec::new();
    for _ in 0..SETTLE {
        post = r.step(0, &[(0, &sig)]);
        pre = r.drain(1);
    }
    assert!(
        (post[0] - 0.05).abs() < 1.0e-5,
        "post-fader follows the fader: {}",
        post[0]
    );
    assert!(
        (pre[0] - 0.5).abs() < 1.0e-5,
        "pre-fader ignores the fader: {}",
        pre[0]
    );
}

#[test]
fn pre_fader_send_still_obeys_mute() {
    let mut r = Rig::new();
    r.attach_input(0, 2);
    r.attach_output(0, 2);
    r.route(0, 0, 0.0, true);
    r.send(C::SetStripMute {
        strip: s(0),
        mute: true,
    });
    let out = r.run(0, 0, SETTLE, dc(0.5));
    assert!(tail(&out, 2, 0, BLOCK).iter().all(|&v| v == 0.0));
}

#[test]
fn send_gain_and_disable() {
    let mut r = Rig::unity();
    r.route(0, 0, -6.0206, false);
    let out = r.run(0, 0, SETTLE, dc(0.5));
    assert!((out[out.len() - 1] - 0.25).abs() < 1.0e-4);
    // Disabling ramps to exact silence.
    r.send(C::SetRouteSend {
        strip: s(0),
        bus: b(0),
        send: audio_types::RouteSend {
            enabled: false,
            gain_db: 0.0,
            pre_fader: false,
        },
    });
    let out = r.run(0, 0, SETTLE, dc(0.5));
    assert!(tail(&out, 2, 0, BLOCK).iter().all(|&v| v == 0.0));
}

#[test]
fn many_strips_sum_and_one_strip_fans_out() {
    let mut r = Rig::new();
    for i in 0..4 {
        r.attach_input(i, 2);
        r.route(i, 0, 0.0, false);
    }
    r.attach_output(0, 2);
    r.attach_output(1, 2);
    r.route(0, 1, -6.0206, false); // strip 0 also feeds bus 1 at -6 dB
    let (a, bb, c, d) = (dc(0.1), dc(0.2), dc(0.3), dc(0.05));
    let mut bus0 = Vec::new();
    let mut bus1 = Vec::new();
    for _ in 0..SETTLE {
        bus0 = r.step(0, &[(0, &a), (1, &bb), (2, &c), (3, &d)]);
        bus1 = r.drain(1);
    }
    assert!((bus0[0] - 0.65).abs() < 1.0e-5, "{}", bus0[0]);
    assert!((bus1[0] - 0.05).abs() < 1.0e-4, "{}", bus1[0]);
}

#[test]
fn bus_gain_and_mute() {
    let mut r = Rig::unity();
    r.send(C::SetBusGain {
        bus: b(0),
        gain_db: -6.0206,
    });
    let out = r.run(0, 0, SETTLE, dc(0.5));
    assert!((out[out.len() - 1] - 0.25).abs() < 1.0e-4);
    r.send(C::SetBusMute {
        bus: b(0),
        mute: true,
    });
    let out = r.run(0, 0, SETTLE, dc(0.5));
    assert!(tail(&out, 2, 0, BLOCK).iter().all(|&v| v == 0.0));
    r.send(C::SetBusMute {
        bus: b(0),
        mute: false,
    });
    let out = r.run(0, 0, SETTLE, dc(0.5));
    assert!(
        (out[out.len() - 1] - 0.25).abs() < 1.0e-4,
        "unmute returns to the fader level"
    );
}
// !SECTION: Sends

// SECTION: Pan and channel mapping
fn mono_to_stereo(pan: f32) -> (f32, f32) {
    let mut r = Rig::new();
    r.attach_input(0, 1);
    r.attach_output(0, 2);
    r.route(0, 0, 0.0, false);
    r.send(C::SetStripPan { strip: s(0), pan });
    let out = r.run(0, 0, SETTLE, dc(1.0));
    (
        *chan(&out, 2, 0).last().unwrap(),
        *chan(&out, 2, 1).last().unwrap(),
    )
}

#[test]
fn mono_strip_pans_with_constant_power() {
    let (l, r_) = mono_to_stereo(-1.0);
    assert!(
        (l - 1.0).abs() < 1.0e-5 && r_.abs() < 1.0e-5,
        "hard left: {l} {r_}"
    );
    let (l, r_) = mono_to_stereo(1.0);
    assert!(
        l.abs() < 1.0e-5 && (r_ - 1.0).abs() < 1.0e-5,
        "hard right: {l} {r_}"
    );
    let (l, r_) = mono_to_stereo(0.0);
    assert!(
        (db(l) + 3.0103).abs() < 0.01 && (l - r_).abs() < 1.0e-6,
        "centre: {l} {r_}"
    );
    for pan in [-0.75, -0.3, 0.2, 0.6, 0.9] {
        let (l, r_) = mono_to_stereo(pan);
        assert!(
            (l * l + r_ * r_ - 1.0).abs() < 1.0e-4,
            "power not constant at pan {pan}"
        );
    }
}

#[test]
fn stereo_strip_pan_is_a_balance() {
    let mut r = Rig::unity();
    r.send(C::SetStripPan {
        strip: s(0),
        pan: 1.0,
    });
    let out = r.run(0, 0, SETTLE, dc(0.5));
    assert!(chan(&out, 2, 0).last().unwrap().abs() < 1.0e-5);
    assert!((chan(&out, 2, 1).last().unwrap() - 0.5).abs() < 1.0e-5);
    r.send(C::SetStripPan {
        strip: s(0),
        pan: 0.0,
    });
    let out = r.run(0, 0, SETTLE, dc(0.5));
    assert_eq!(out[out.len() - 2..], [0.5, 0.5], "centre balance is unity");
}

#[test]
fn stereo_strip_to_mono_bus_averages() {
    let mut r = Rig::new();
    r.attach_input(0, 2);
    r.attach_output(0, 1);
    r.route(0, 0, 0.0, false);
    // L = 0.2, R = 0.6 -> mono 0.4
    let lr = |_t: usize, c: usize| if c == 0 { 0.2 } else { 0.6 };
    let out = r.run(0, 0, SETTLE, lr);
    assert_eq!(out.len(), SETTLE * BLOCK); // mono bus = 1 sample per frame
    assert!((out[out.len() - 1] - 0.4).abs() < 1.0e-5);
}

#[test]
fn eight_channels_to_stereo_keeps_the_first_two() {
    let mut r = Rig::new();
    r.attach_input(0, 8);
    r.attach_output(0, 2);
    r.route(0, 0, 0.0, false);
    let out = r.run(0, 0, SETTLE, |_t, c| (c + 1) as f32 * 0.1);
    assert_eq!(out[out.len() - 2..], [0.1, 0.2]);
}

#[test]
fn stereo_strip_to_eight_channel_bus_fills_only_l_and_r() {
    let mut r = Rig::new();
    r.attach_input(0, 2);
    r.attach_output(0, 8);
    r.route(0, 0, 0.0, false);
    let out = r.run(0, 0, SETTLE, |_t, c| (c + 1) as f32 * 0.25);
    let last = &out[out.len() - 8..];
    assert_eq!(last, [0.25, 0.5, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0]);
}
// !SECTION: Pan and channel mapping

// SECTION: Attach / detach
#[test]
fn detach_silences_and_reattach_discards_stale_audio() {
    let mut r = Rig::unity();
    r.run(0, 0, SETTLE, dc(0.5));
    r.send(C::DetachStripInput { strip: s(0) });
    r.process(1);
    let _ = r.drain(0);
    // The device keeps writing while detached; the engine must not read it.
    r.feed(0, &vec![0.9; BLOCK * 2 * 4]);
    r.process(2);
    assert!(
        r.drain(0).iter().all(|&v| v == 0.0),
        "detached strip must be silent"
    );

    // Re-attaching the same ring drops the stale 0.9 data.
    let slot = audio_engine::input_slot(0);
    r.send(C::AttachStripInput {
        strip: s(0),
        slot,
        channels: 2,
    });
    r.settle();
    let out = r.run(0, 0, 5, dc(0.25));
    assert!(
        out.iter().all(|&v| v == 0.25),
        "stale audio leaked after re-attach"
    );
}

#[test]
fn invalid_commands_are_rejected_and_change_nothing() {
    use audio_engine::{input_slot, output_slot};
    use audio_types::PortSlot;
    let mut r = Rig::unity();
    r.attach_input(1, 2); // slot 1 belongs to strip 1
    let nan = f32::NAN;
    let mut bad = vec![
        C::SetStripGain {
            strip: s(200),
            gain_db: 0.0,
        },
        C::SetStripGain {
            strip: s(0),
            gain_db: nan,
        },
        C::SetStripPan {
            strip: s(0),
            pan: f32::INFINITY,
        },
        C::SetBusGain {
            bus: b(99),
            gain_db: 0.0,
        },
        C::SetBusGain {
            bus: b(0),
            gain_db: nan,
        },
        C::SetStripMute {
            strip: s(16),
            mute: true,
        },
        C::SetRouteSend {
            strip: s(0),
            bus: b(99),
            send: audio_types::RouteSend {
                enabled: true,
                gain_db: 0.0,
                pre_fader: false,
            },
        },
        // wrong kind of slot
        C::AttachStripInput {
            strip: s(2),
            slot: output_slot(0),
            channels: 2,
        },
        C::AttachStripInput {
            strip: s(2),
            slot: PortSlot(48),
            channels: 2,
        },
        C::AttachBusOutput {
            bus: b(1),
            slot: input_slot(3),
            channels: 2,
        },
        // bad channel counts
        C::AttachStripInput {
            strip: s(2),
            slot: input_slot(4),
            channels: 0,
        },
        C::AttachStripInput {
            strip: s(2),
            slot: input_slot(4),
            channels: 9,
        },
        C::AttachBusOutput {
            bus: b(1),
            slot: output_slot(5),
            channels: 0,
        },
        // slot already owned by another strip / bus
        C::AttachStripInput {
            strip: s(2),
            slot: input_slot(1),
            channels: 2,
        },
        C::AttachBusOutput {
            bus: b(1),
            slot: output_slot(0),
            channels: 2,
        },
    ];
    let mut dsp = StripDsp::default();
    dsp.comp.ratio = nan;
    bad.push(C::SetStripDsp { strip: s(0), dsp });
    let mut eq = [EqBand::default(); 3];
    eq[1].freq_hz = nan;
    bad.push(C::SetBusEq { bus: b(0), eq });

    let before = r.h.stats.rejected_commands.load(Relaxed);
    let n = bad.len() as u32;
    for c in bad {
        r.send(c);
    }
    let out = r.run(0, 0, SETTLE, dc(0.5));
    assert_eq!(r.h.stats.rejected_commands.load(Relaxed) - before, n);
    assert_eq!(
        *out.last().unwrap(),
        0.5,
        "rejected commands must not disturb the audio"
    );
}
// !SECTION: Attach / detach

// SECTION: Meters and telemetry
#[test]
fn meters_report_peak_rms_and_activity() {
    let mut r = Rig::unity();
    // DC 0.5 on both channels for ~1.6 s so the 300 ms RMS integrator converges.
    r.run(0, 0, 600, dc(0.5));
    let t = r.tel();
    let want = db(0.5); // -6.02 dB
    for ch in 0..2 {
        assert!(
            (load_f32(&t.strips[0].peak_db[ch]) - want).abs() < 0.05,
            "strip peak"
        );
        assert!(
            (load_f32(&t.strips[0].rms_db[ch]) - want).abs() < 0.1,
            "strip rms"
        );
        assert!(
            (load_f32(&t.buses[0].peak_db[ch]) - want).abs() < 0.05,
            "bus peak"
        );
    }
    assert_eq!(
        load_f32(&t.strips[0].peak_db[2]),
        -120.0,
        "unused channel stays at the floor"
    );
    assert_eq!(
        load_f32(&t.strips[1].peak_db[0]),
        -120.0,
        "unattached strip stays at the floor"
    );
    assert!(t.any_audio_active.load(Relaxed));
    assert!(!t.strips[0].clipping.load(Relaxed));

    // A 0.5-amplitude sine has RMS 0.5/√2 = -9.03 dB.
    r.run(0, 0, 600, tone(0.5, 997.0));
    let rms = load_f32(&r.tel().strips[0].rms_db[0]);
    assert!((rms + 9.03).abs() < 0.15, "{rms}");

    // Silence: activity flag drops and the peak meter falls.
    r.run(0, 0, 10, dc(0.0));
    assert!(!r.tel().any_audio_active.load(Relaxed));
    r.run(0, 0, 3000, dc(0.0));
    assert_eq!(load_f32(&r.tel().strips[0].peak_db[0]), -120.0);
    assert_eq!(load_f32(&r.tel().buses[0].rms_db[0]), -120.0);
}

#[test]
fn clipping_flag_is_set_by_overs_and_cleared_only_by_the_ui() {
    let mut r = Rig::unity();
    r.run(0, 0, SETTLE, dc(0.5));
    assert!(!r.tel().buses[0].clipping.load(Relaxed));
    r.send(C::SetBusGain {
        bus: b(0),
        gain_db: 12.0,
    }); // 0.5 * 4 = 2.0 -> over
    r.run(0, 0, SETTLE, dc(0.5));
    assert!(r.tel().buses[0].clipping.load(Relaxed));
    assert!(
        !r.tel().strips[0].clipping.load(Relaxed),
        "the strip itself stayed below full scale"
    );
    r.send(C::SetBusGain {
        bus: b(0),
        gain_db: 0.0,
    });
    r.run(0, 0, SETTLE, dc(0.5));
    assert!(
        r.tel().buses[0].clipping.load(Relaxed),
        "sticky until the UI clears it"
    );
    assert!(r.tel().buses[0].clipping.swap(false, Relaxed));
    r.run(0, 0, SETTLE, dc(0.5));
    assert!(!r.tel().buses[0].clipping.load(Relaxed));
}

#[test]
fn load_meter_reports_work_done() {
    let mut r = Rig::new();
    r.attach_output(0, 2);
    let mut dsp = StripDsp::default();
    dsp.comp.enabled = true;
    dsp.eq[1] = EqBand {
        enabled: true,
        freq_hz: 1000.0,
        gain_db: 6.0,
        q: 1.0,
    };
    for i in 0..8 {
        r.attach_input(i, 2);
        r.route(i, 0, 0.0, false);
        r.send(C::SetStripDsp { strip: s(i), dsp });
    }
    let g = tone(0.05, 440.0);
    let feeds: Vec<(u8, Sig)> = (0..8).map(|i| (i, &g as Sig)).collect();
    r.run_many(1000, 0, &feeds);
    let permille = r.tel().rt_load_permille.load(Relaxed);
    assert!(permille >= 1, "load meter never moved");
    println!("8 strips with DSP: rt_load_permille = {permille}");
}
// !SECTION: Meters and telemetry

// SECTION: Rings: recorder, overrun, underrun, odd sizes
#[test]
fn recorder_ring_follows_the_record_flag() {
    let mut r = Rig::unity();
    r.attach_recorder(0);
    r.run(0, 0, SETTLE, tone(0.5, 440.0));
    assert!(
        r.drain_recorder(0).is_empty(),
        "record is off: nothing may be written"
    );

    r.send(C::SetBusRecord {
        bus: b(0),
        enabled: true,
    });
    let mut played = Vec::new();
    for _ in 0..10 {
        played.extend(r.run(0, 0, 1, tone(0.5, 440.0)));
    }
    let recorded = r.drain_recorder(0);
    assert_eq!(
        recorded, played,
        "recorder must receive exactly what the output got"
    );

    r.send(C::SetBusRecord {
        bus: b(0),
        enabled: false,
    });
    r.run(0, 0, 5, tone(0.5, 440.0));
    assert!(r.drain_recorder(0).is_empty());
}

#[test]
fn record_only_bus_works_without_an_output_device() {
    let mut r = Rig::new();
    r.attach_input(0, 2);
    r.route(0, 3, 0.0, false);
    r.attach_recorder(3);
    r.send(C::SetBusRecord {
        bus: b(3),
        enabled: true,
    });
    for _ in 0..SETTLE {
        r.run_many(1, 3, &[(0, &dc(0.5))]);
    }
    let rec = r.drain_recorder(3);
    assert_eq!(
        rec.len(),
        SETTLE * BLOCK * 2,
        "one block per call, none lost"
    );
    assert_eq!(*rec.last().unwrap(), 0.5);
}

#[test]
fn full_output_ring_drops_blocks_and_counts_them() {
    let mut r = Rig::unity();
    // Never drain bus 0: it holds 32768 samples = 128 stereo blocks.
    for _ in 0..300 {
        r.feed(0, &vec![0.5; BLOCK * 2]);
        r.process(1);
    }
    let dropped = r.h.stats.output_overruns.load(Relaxed);
    assert_eq!(
        dropped,
        300 - 128,
        "everything beyond the ring's capacity is counted"
    );
    // The engine kept running and recovers as soon as the device drains the ring.
    let out = r.drain(0);
    assert_eq!(out.len(), 128 * BLOCK * 2);
    let out = r.run(0, 0, 3, dc(0.5));
    assert_eq!(out.len(), 3 * BLOCK * 2);
    assert_eq!(r.h.stats.output_overruns.load(Relaxed), dropped);
}

#[test]
fn starved_input_is_zero_filled_and_counted() {
    let mut r = Rig::unity();
    r.run(0, 0, SETTLE, dc(0.5));
    let before = r.h.stats.input_underruns.load(Relaxed);

    // Nothing fed for one block: silence out, one underrun.
    r.process(1);
    let out = r.drain(0);
    assert_eq!(out.len(), BLOCK * 2);
    assert!(out.iter().all(|&v| v == 0.0));
    assert_eq!(r.h.stats.input_underruns.load(Relaxed), before + 1);

    // Half a block fed: 64 frames of signal then 64 of zeros.
    r.feed(0, &vec![0.5; 64 * 2]);
    r.process(1);
    let out = chan(&r.drain(0), 2, 0);
    assert!(out[..64].iter().all(|&v| v == 0.5));
    assert!(out[64..].iter().all(|&v| v == 0.0));
    assert_eq!(r.h.stats.input_underruns.load(Relaxed), before + 2);
}

#[test]
fn any_block_size_is_accepted() {
    let mut r = Rig::unity();
    r.run(0, 0, SETTLE, dc(0.5)); // settle ramps
    // 300 frames in a single call (chunked 128 + 128 + 44 internally).
    let input = sine(2, 0.4, 500.0, 0, 300);
    r.feed(0, &input);
    r.engine.process_block(300);
    let out = r.drain(0);
    assert_eq!(out.len(), 300 * 2);
    assert_eq!(out, input);
    // 0 frames is a no-op; 1 frame at a time still works.
    r.engine.process_block(0);
    r.feed(0, &sine(2, 0.4, 500.0, 300, 5));
    for _ in 0..5 {
        r.engine.process_block(1);
    }
    assert_eq!(r.drain(0).len(), 5 * 2);
}

#[test]
fn command_drain_is_bounded_per_block() {
    let mut r = Rig::unity();
    let cap = audio_engine::DEFAULT_COMMAND_CAPACITY;
    for _ in 0..cap - 24 {
        r.send(C::SetStripGain {
            strip: s(0),
            gain_db: 0.0,
        });
    }
    let free_before = r.h.commands.slots();
    r.process(1);
    let drained = r.h.commands.slots() - free_before;
    assert_eq!(drained, audio_engine::MAX_COMMANDS_PER_BLOCK);
}
// !SECTION: Rings: recorder, overrun, underrun, odd sizes

// SECTION: Strip DSP through the engine
fn set_dsp(r: &mut Rig, f: impl FnOnce(&mut StripDsp)) {
    let mut dsp = StripDsp::default();
    f(&mut dsp);
    r.send(C::SetStripDsp { strip: s(0), dsp });
}

#[test]
fn eq_band_changes_the_level_at_its_frequency() {
    let mut r = Rig::unity();
    set_dsp(&mut r, |d| {
        d.eq[1] = EqBand {
            enabled: true,
            freq_hz: 1000.0,
            gain_db: 12.0,
            q: 1.0,
        };
    });
    let out = r.run(0, 0, 100, tone(0.1, 1000.0));
    let l = tail(&out, 2, 0, 4800);
    let gain = db(rms(&l)) - db(0.1 / 2.0_f32.sqrt());
    assert!(
        (gain - 12.0).abs() < 0.2,
        "EQ gain at centre = {gain:.2} dB"
    );
    // Disabling restores bit-exact pass-through.
    set_dsp(&mut r, |_| {});
    let g = tone(0.1, 1000.0);
    let out = r.run(0, 0, 60, &g);
    let frames = out.len() / 2;
    assert_eq!(out[(frames - 1) * 2], g(frames + 100 * BLOCK - 1, 0));
}

#[test]
fn gate_blocks_quiet_signal_and_passes_loud() {
    let mut r = Rig::unity();
    set_dsp(&mut r, |d| {
        d.gate = Gate {
            enabled: true,
            threshold_db: -30.0,
            attack_ms: 2.0,
            release_ms: 50.0,
        };
    });
    let out = r.run(0, 0, 100, tone(db_lin(-50.0), 500.0));
    assert!(
        tail(&out, 2, 0, BLOCK).iter().all(|&v| v == 0.0),
        "quiet signal must be gated off"
    );
    let out = r.run(0, 0, 40, tone(db_lin(-10.0), 500.0));
    assert!(
        (peak(&tail(&out, 2, 0, 2 * BLOCK)) - db_lin(-10.0)).abs() < 0.005,
        "loud signal passes"
    );
}

fn db_lin(db: f32) -> f32 {
    10f32.powf(db / 20.0)
}

#[test]
fn compressor_follows_its_static_curve_inside_the_engine() {
    let mut r = Rig::unity();
    set_dsp(&mut r, |d| {
        d.comp = Compressor {
            enabled: true,
            threshold_db: -20.0,
            ratio: 4.0,
            attack_ms: 5.0,
            release_ms: 100.0,
            makeup_db: 0.0,
        };
    });
    // -10 dBFS square -> -20 + 10/4 = -17.5 dBFS.
    let out = r.run(0, 0, 300, square(db_lin(-10.0)));
    let level = db(peak(&tail(&out, 2, 0, 2 * BLOCK)));
    assert!((level + 17.5).abs() < 0.1, "{level:.2} dBFS");
}
// !SECTION: Strip DSP through the engine
