//! One mixer bus: summed sends -> EQ -> gain/mute -> meter -> output ring (+ recorder ring).

use crate::block::BlockConsts;
use crate::dsp::Planar;
use crate::dsp::biquad::EqChain;
use crate::dsp::smooth::{Ramp, apply_gain_ramp};
use crate::dsp::util::fader_to_lin;
use crate::meter::MeterState;
use crate::ports::{InterleaveBuf, write_interleaved};
use crate::stats::EngineStats;
use audio_types::MeterAtomics;
use core::sync::atomic::Ordering;
use rtrb::Producer;

pub(crate) struct Bus {
    pub channels: usize,
    /// Index into `EnginePorts::outputs`; `None` = no device/stream attached.
    pub output_slot: Option<usize>,
    pub mute: bool,
    pub gain_db: f32,
    /// Fader and mute folded into one smoothed linear gain.
    level: Ramp,
    pub eq: EqChain,
    /// When true the block is also copied into this bus's recorder ring.
    pub record: bool,
    meter: MeterState,
    fresh: bool,
}

impl Bus {
    pub fn new() -> Self {
        Self {
            channels: 2,
            output_slot: None,
            mute: false,
            gain_db: 0.0,
            level: Ramp::new(1.0),
            eq: EqChain::new(),
            record: false,
            meter: MeterState::new(),
            fresh: false,
        }
    }

    /// Recompute the smoothed target after `gain_db` or `mute` changed.
    pub fn update_level_target(&mut self) {
        self.level.set_target(if self.mute {
            0.0
        } else {
            fader_to_lin(self.gain_db)
        });
    }

    pub fn attach(&mut self, output_slot: usize, channels: usize) {
        self.output_slot = Some(output_slot);
        self.channels = channels;
        self.eq.reset();
        self.fresh = true;
    }

    pub fn detach(&mut self) {
        self.output_slot = None;
    }

    /// Nothing is routed here: let the meters fall to the floor.
    pub fn idle(&mut self, k: &BlockConsts, meter_out: &MeterAtomics) {
        self.meter.measure_silence(meter_out, &k.meter);
    }

    /// The bus EQ is run by the engine on this chain (alone, or paired with another bus's),
    /// between the sends being summed and `finish`.
    pub fn eq_mut(&mut self) -> &mut EqChain {
        &mut self.eq
    }

    /// `acc` holds the summed sends *after* the EQ. Applies gain, meters, and writes the
    /// output / recorder rings. Returns the bus's post-gain peak (linear).
    #[allow(clippy::too_many_arguments)]
    pub fn finish(
        &mut self,
        frames: usize,
        acc: &mut Planar,
        interleave: &mut InterleaveBuf,
        output: Option<&mut Producer<f32>>,
        recorder: &mut Producer<f32>,
        k: &BlockConsts,
        meter_out: &MeterAtomics,
        stats: &EngineStats,
    ) -> f32 {
        if self.fresh {
            self.level.snap();
            self.fresh = false;
        }
        let n = self.channels;
        let (g0, g1) = self.level.advance(k.smooth);
        for row in acc.iter_mut().take(n) {
            apply_gain_ramp(&mut row[..frames], g0, g1);
        }
        let peak = self.meter.measure(acc, n, frames, meter_out, &k.meter);

        // ? A full ring means the consumer (device / network thread) stopped draining. We
        // ? drop this whole block rather than block or overwrite: the RT thread may never wait.
        if let Some(out) = output {
            if !write_interleaved(out, acc, n, frames, interleave) {
                stats.output_overruns.fetch_add(1, Ordering::Relaxed);
            }
        }
        if self.record && !write_interleaved(recorder, acc, n, frames, interleave) {
            stats.recorder_overruns.fetch_add(1, Ordering::Relaxed);
        }
        peak
    }
}
