//! One mixer strip: input ring -> gate -> EQ -> compressor -> mute/solo -> sends -> fader -> sends -> meter.
//!
//! * "Pre-fader" sends tap the signal after DSP and mute/solo but before the fader;
//!   "post-fader" sends tap it after the fader. Pan applies to both.

use crate::block::{BlockConsts, BusInfo};
use crate::drift::ClockDomainAdapter;
use crate::dsp::Planar;
use crate::dsp::biquad::EqChain;
use crate::dsp::dynamics::{CompState, GateState};
use crate::dsp::smooth::{Ramp, apply_gain_ramp};
use crate::meter::MeterState;
use crate::mixmap::{SendGains, mix_into_bus, pan_gains};
use crate::ports::{InterleaveBuf, read_interleaved};
use crate::stats::EngineStats;
use audio_types::{ClockDomain, LanRxAtomics, MAX_BUSES, MeterAtomics, StripDsp, store_f32};
use core::sync::atomic::Ordering;
use rtrb::Consumer;

/// The two telemetry blocks a strip writes to.
// ? `lan` is indexed by the strip's own number: a strip has exactly one source, and
// ? `MAX_LAN_STREAMS == MAX_STRIPS`, so "strip i <-> lan_rx[i]" needs no extra mapping. The
// ? network side (S3) writes packets_lost / packets_late / last_packet_ms_ago to the same slot.
#[derive(Clone, Copy)]
pub(crate) struct StripTelemetry<'a> {
    pub meter: &'a MeterAtomics,
    pub lan: &'a LanRxAtomics,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct SendState {
    pub enabled: bool,
    pub pre_fader: bool,
    /// Linear send gain; the target is 0 while the send is disabled.
    pub gain: Ramp,
}

pub(crate) struct Strip {
    pub channels: usize,
    /// Index into `EnginePorts::inputs`; `None` = nothing attached (strip is silent).
    pub input_slot: Option<usize>,
    pub mute: bool,
    pub solo: bool,
    /// Fader (linear).
    pub gain: Ramp,
    /// 1.0 when the strip is audible, 0.0 when muted or another strip is soloed.
    pub audible: Ramp,
    pub pan: f32,
    pan_l: Ramp,
    pan_r: Ramp,
    pub dsp: StripDsp,
    gate: GateState,
    eq: EqChain,
    comp: CompState,
    pub sends: [SendState; MAX_BUSES],
    meter: MeterState,
    /// Set by `attach`: the next block starts with all ramps at their targets (no fade-in).
    fresh: bool,
    /// Which clock the input ring runs on.
    clock: ClockDomain,
    /// Drift adapter, used only while `clock` is `External`.
    // ? Boxed and created here (control thread, at engine construction) so that switching a
    // ? strip to an external clock later is just `configure`, with no allocation on the RT thread.
    adapter: Box<ClockDomainAdapter>,
}

impl Strip {
    pub fn new() -> Self {
        let mut s = Self {
            channels: 2,
            input_slot: None,
            mute: false,
            solo: false,
            gain: Ramp::new(1.0),
            audible: Ramp::new(1.0),
            pan: 0.0,
            pan_l: Ramp::new(1.0),
            pan_r: Ramp::new(1.0),
            dsp: StripDsp::default(),
            gate: GateState::new(),
            eq: EqChain::new(),
            comp: CompState::new(),
            sends: [SendState {
                enabled: false,
                pre_fader: false,
                gain: Ramp::new(0.0),
            }; MAX_BUSES],
            meter: MeterState::new(),
            fresh: false,
            clock: ClockDomain::Master,
            adapter: Box::default(),
        };
        s.update_pan_targets();
        s
    }

    /// Switch between the master clock and an external one. Resets the adapter (and with it
    /// the learned clock offset): a new clock means a new source.
    pub fn set_clock(&mut self, clock: ClockDomain) {
        self.clock = clock;
        if let ClockDomain::External { target_ms } = clock {
            self.adapter.configure(self.channels, target_ms);
        }
    }

    /// Follow a new jitter target without a reset (only meaningful on an external clock).
    pub fn set_target_ms(&mut self, target_ms: f32) {
        if let ClockDomain::External { .. } = self.clock {
            self.adapter.set_target_ms(target_ms);
            self.clock = ClockDomain::External {
                target_ms: self.adapter.target_ms(),
            };
        }
    }

    /// Mirror the adapter into the strip's LAN telemetry slot (4 relaxed stores).
    fn publish_clock(&self, lan: &LanRxAtomics) {
        store_f32(&lan.buffer_ms, self.adapter.fill_ms());
        store_f32(&lan.target_ms, self.adapter.target_ms());
        store_f32(&lan.drift_ppm, self.adapter.drift_ppm());
        lan.underruns
            .store(self.adapter.underruns(), Ordering::Relaxed);
    }

    pub fn update_pan_targets(&mut self) {
        let (l, r) = pan_gains(self.channels, self.pan);
        self.pan_l.set_target(l);
        self.pan_r.set_target(r);
    }

    pub fn attach(&mut self, input_slot: usize, channels: usize) {
        self.input_slot = Some(input_slot);
        self.channels = channels;
        // A new source starts with clean filter / detector memory.
        self.eq.reset();
        self.gate = GateState::new();
        self.comp = CompState::new();
        self.update_pan_targets();
        self.fresh = true;
        // ... and, on an external clock, a fresh resampler / controller for the new channel count.
        if let ClockDomain::External { target_ms } = self.clock {
            self.adapter.configure(channels, target_ms);
        }
    }

    pub fn detach(&mut self) {
        self.input_slot = None;
    }

    pub fn apply_dsp(&mut self, dsp: StripDsp) {
        self.dsp = dsp;
        self.eq.set_bands(&dsp.eq);
        // ? A bypassed effect forgets its state, so enabling it later starts neutral.
        if !dsp.gate.enabled {
            self.gate = GateState::new();
        }
        if !dsp.comp.enabled {
            self.comp = CompState::new();
        }
    }

    fn snap_ramps(&mut self) {
        self.gain.snap();
        self.audible.snap();
        self.pan_l.snap();
        self.pan_r.snap();
        for s in self.sends.iter_mut() {
            s.gain.snap();
        }
    }

    // ? A strip block is processed in three stages so the engine can run the EQ of FOUR strips
    // ? at once (see `EqChain::process_quad`): 1) `read_and_gate`, 2) EQ, 3) `comp_mix_meter`.
    // ? The order of effects inside one strip is unchanged: gate -> EQ -> compressor.
    // ? The block length of the current chunk is `k.frames` (set by `BlockConsts::ensure`).

    /// Stage 1: ring -> planar block, then the gate. Returns `false` when nothing is attached;
    /// the strip is then silent and stages 2 and 3 must be skipped.
    pub fn read_and_gate(
        &mut self,
        input: Option<&mut Consumer<f32>>,
        buf: &mut Planar,
        interleave: &mut InterleaveBuf,
        k: &BlockConsts,
        tel: StripTelemetry<'_>,
        stats: &EngineStats,
    ) -> bool {
        let Some(cons) = input else {
            self.meter.measure_silence(tel.meter, &k.meter);
            return false;
        };
        if self.fresh {
            self.snap_ramps();
            self.fresh = false;
        }
        let frames = k.frames;
        let n = self.channels;
        match self.clock {
            ClockDomain::Master => {
                let got = read_interleaved(cons, n, frames, buf, interleave);
                if got < frames {
                    stats.input_underruns.fetch_add(1, Ordering::Relaxed);
                }
            }
            ClockDomain::External { .. } => {
                // FIFO -> drift resampler -> block. Writes all `frames` frames (audio or silence).
                let report = self.adapter.pull(cons, buf, frames);
                if report.underrun {
                    stats.input_underruns.fetch_add(1, Ordering::Relaxed);
                }
                if report.overrun {
                    stats.input_overruns.fetch_add(1, Ordering::Relaxed);
                }
                self.publish_clock(tel.lan);
            }
        }
        if self.dsp.gate.enabled {
            self.gate.process(&self.dsp.gate, buf, n, frames);
        }
        true
    }

    /// Stage 2 is run by the engine on this chain (alone, or together with three others).
    pub fn eq_mut(&mut self) -> &mut EqChain {
        &mut self.eq
    }

    /// Stage 3: compressor, mute/solo, sends, fader, sends, meter.
    /// Returns this strip's post-fader peak (linear) for the activity flag.
    pub fn comp_mix_meter(
        &mut self,
        buf: &mut Planar,
        bus_acc: &mut [Planar; MAX_BUSES],
        info: &BusInfo,
        k: &BlockConsts,
        meter_out: &MeterAtomics,
    ) -> f32 {
        let frames = k.frames;
        let n = self.channels;
        if self.dsp.comp.enabled {
            self.comp.process(&self.dsp.comp, buf, n, frames);
        }

        // 3. mute / solo (this is the pre-fader tap)
        let (a0, a1) = self.audible.advance(k.smooth);
        for row in buf.iter_mut().take(n) {
            apply_gain_ramp(&mut row[..frames], a0, a1);
        }
        let pan_l = self.pan_l.advance(k.smooth);
        let pan_r = self.pan_r.advance(k.smooth);
        // ? A muted strip is all zeros, so mixing it would only add zeros. Still step the send
        // ? ramps (inside `run_sends`) so they stay in sync with their targets.
        let muted = a0 == 0.0 && a1 == 0.0;

        // 4. pre-fader sends
        run_sends(
            &mut self.sends,
            true,
            muted,
            buf,
            n,
            bus_acc,
            info,
            pan_l,
            pan_r,
            frames,
            k.smooth,
        );

        // 5. fader
        let (g0, g1) = self.gain.advance(k.smooth);
        for row in buf.iter_mut().take(n) {
            apply_gain_ramp(&mut row[..frames], g0, g1);
        }

        // 6. post-fader sends
        run_sends(
            &mut self.sends,
            false,
            muted,
            buf,
            n,
            bus_acc,
            info,
            pan_l,
            pan_r,
            frames,
            k.smooth,
        );

        // 7. meter (post-fader, post-mute)
        self.meter.measure(buf, n, frames, meter_out, &k.meter)
    }
}

#[allow(clippy::too_many_arguments)]
// ? A free function (not a method) so it can borrow `sends` mutably while the caller still
// ? holds other fields of the same `Strip` — a `&mut self` method would lock them all.
fn run_sends(
    sends: &mut [SendState; MAX_BUSES],
    pre_fader: bool,
    muted: bool,
    buf: &Planar,
    strip_ch: usize,
    bus_acc: &mut [Planar; MAX_BUSES],
    info: &BusInfo,
    pan_l: (f32, f32),
    pan_r: (f32, f32),
    frames: usize,
    smooth_k: f32,
) {
    for (b, send) in sends.iter_mut().enumerate() {
        if send.pre_fader != pre_fader {
            continue;
        }
        // Advance exactly once per block, even when nothing is mixed.
        let (s0, s1) = send.gain.advance(smooth_k);
        if muted || !info.active[b] || (s0 == 0.0 && s1 == 0.0) {
            continue;
        }
        let gains = SendGains {
            left: (s0 * pan_l.0, s1 * pan_l.1),
            right: (s0 * pan_r.0, s1 * pan_r.1),
            flat: (s0, s1),
        };
        mix_into_bus(
            &mut bus_acc[b],
            info.channels[b],
            buf,
            strip_ch,
            &gains,
            frames,
        );
    }
}
