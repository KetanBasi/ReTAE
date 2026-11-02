//! One mixer strip: input ring -> gate -> EQ -> compressor -> mute/solo -> sends -> fader -> sends -> meter.
//!
//! * "Pre-fader" sends tap the signal after DSP and mute/solo but before the fader;
//!   "post-fader" sends tap it after the fader. Pan applies to both.

use crate::block::{BlockConsts, BusInfo};
use crate::dsp::Planar;
use crate::dsp::biquad::EqChain;
use crate::dsp::dynamics::{CompState, GateState};
use crate::dsp::smooth::{Ramp, apply_gain_ramp};
use crate::meter::MeterState;
use crate::mixmap::{SendGains, mix_into_bus, pan_gains};
use crate::ports::{InterleaveBuf, read_interleaved};
use crate::stats::EngineStats;
use audio_types::{MAX_BUSES, MeterAtomics, StripDsp};
use core::sync::atomic::Ordering;
use rtrb::Consumer;

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
        };
        s.update_pan_targets();
        s
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
        meter_out: &MeterAtomics,
        stats: &EngineStats,
    ) -> bool {
        let Some(cons) = input else {
            self.meter.measure_silence(meter_out, &k.meter);
            return false;
        };
        if self.fresh {
            self.snap_ramps();
            self.fresh = false;
        }
        let frames = k.frames;
        let n = self.channels;
        let got = read_interleaved(cons, n, frames, buf, interleave);
        if got < frames {
            stats.input_underruns.fetch_add(1, Ordering::Relaxed);
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
