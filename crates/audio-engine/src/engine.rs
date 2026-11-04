//! `MixerEngine`: the RT mixer. `process_block` is the only function the audio callback calls.
//!
//! # RT contract (CLAUDE.md law 1)
//! `process_block` and everything it calls: no heap alloc/free, no locks, no syscalls, no
//! logging. Input is the `rtrb` command queue and the attached port rings, nothing else.
//! In debug/test builds the call is wrapped in `assert_no_alloc`.
//
// ? The one non-trivial call is `Instant::now()` for the load meter. On Linux, macOS and
// ? Windows it reads a user-space clock (vDSO / QPC) and never enters the kernel.

use crate::block::{BlockConsts, Scratch};
use crate::bus::Bus;
use crate::dsp::biquad::EqChain;
use crate::dsp::util::{FADER_MAX_DB, SAMPLE_RATE, fader_to_lin, finite_clamp};
use crate::params::{sanitize_bands, sanitize_dsp};
use crate::ports::{
    ControlPorts, EnginePorts, create_port_pool, discard_all, input_index, output_index,
};
use crate::stats::EngineStats;
use crate::strip::{Strip, StripTelemetry};
use assert_no_alloc::assert_no_alloc;
use audio_types::{
    ClockDomain, ENGINE_BLOCK_FRAMES, EngineCommand, MAX_BUSES, MAX_CHANNELS, MAX_STRIPS,
    Telemetry, store_f32,
};
use core::sync::atomic::Ordering;
use rtrb::{Consumer, Producer, RingBuffer};
use std::sync::Arc;
use std::time::Instant;

/// Upper bound on commands applied per chunk, so a command flood can't starve the audio.
pub const MAX_COMMANDS_PER_BLOCK: usize = 256;
/// Default command-queue capacity (control thread -> RT thread).
pub const DEFAULT_COMMAND_CAPACITY: usize = 1024;
/// -90 dBFS. `Telemetry::any_audio_active` is true above this (PLAN.md §11 S1).
const ACTIVITY_THRESHOLD_LIN: f32 = 3.162_277_7e-5;
/// Smoothing for the published load figure (EMA, per `process_block` call).
const LOAD_EMA: f32 = 0.1;

pub struct MixerEngine {
    strips: [Strip; MAX_STRIPS],
    buses: [Bus; MAX_BUSES],
    cmd_rx: Consumer<EngineCommand>,
    ports: EnginePorts,
    telemetry: Arc<Telemetry>,
    stats: Arc<EngineStats>,
    scratch: Box<Scratch>,
    consts: BlockConsts,
    /// Mute / solo targets need recomputing (a flag changed, or a strip was attached).
    routing_dirty: bool,
    load_ema: f32,
}

/// Everything the control plane keeps after handing the engine to the audio thread.
pub struct EngineHandles {
    /// Send `EngineCommand`s here. Single producer: wrap in a `Mutex` if several control
    /// threads need to send (that's fine, the control thread may lock).
    pub commands: Producer<EngineCommand>,
    /// The non-RT halves of the 48 port rings.
    pub ports: ControlPorts,
    pub telemetry: Arc<Telemetry>,
    pub stats: Arc<EngineStats>,
}

/// Build an engine plus its control-plane handles. Control thread only; this allocates.
pub fn create_engine() -> (MixerEngine, EngineHandles) {
    let (tx, rx) = RingBuffer::new(DEFAULT_COMMAND_CAPACITY);
    let (engine_ports, control_ports) = create_port_pool();
    let telemetry = Arc::new(Telemetry::default());
    let stats = Arc::new(EngineStats::default());
    let engine = MixerEngine::new(rx, engine_ports, Arc::clone(&telemetry), Arc::clone(&stats));
    (
        engine,
        EngineHandles {
            commands: tx,
            ports: control_ports,
            telemetry,
            stats,
        },
    )
}

impl MixerEngine {
    pub fn new(
        cmd_rx: Consumer<EngineCommand>,
        ports: EnginePorts,
        telemetry: Arc<Telemetry>,
        stats: Arc<EngineStats>,
    ) -> Self {
        Self {
            strips: core::array::from_fn(|_| Strip::new()),
            buses: core::array::from_fn(|_| Bus::new()),
            cmd_rx,
            ports,
            telemetry,
            stats,
            scratch: Scratch::new(),
            consts: BlockConsts::default(),
            routing_dirty: true,
            load_ema: 0.0,
        }
    }

    /// Mix `frames` frames. Called from the master device callback (or a timer thread).
    /// Any `frames` is accepted; it is processed in chunks of at most 128.
    pub fn process_block(&mut self, frames: usize) {
        assert_no_alloc(|| self.process_block_inner(frames));
    }

    fn process_block_inner(&mut self, frames: usize) {
        if frames == 0 {
            return;
        }
        let started = Instant::now();
        let mut left = frames;
        while left > 0 {
            let n = left.min(ENGINE_BLOCK_FRAMES);
            self.process_chunk(n);
            left -= n;
        }

        // Load = time spent / time available.
        let load = started.elapsed().as_secs_f32() / (frames as f32 / SAMPLE_RATE);
        if load > 1.0 {
            self.telemetry.rt_overruns.fetch_add(1, Ordering::Relaxed);
        }
        self.load_ema += LOAD_EMA * (load - self.load_ema);
        // ? `as u32` saturates on overflow and maps NaN to 0, so this cast can't misbehave.
        self.telemetry
            .rt_load_permille
            .store((self.load_ema * 1000.0) as u32, Ordering::Relaxed);
    }

    fn process_chunk(&mut self, frames: usize) {
        self.consts.ensure(frames);
        self.drain_commands();
        self.refresh_routing();

        // ? Destructure `self` into its fields so each can be borrowed separately below
        // ? (e.g. a strip mutably AND the port ring it reads from). Calling `self.method()`
        // ? inside these loops would borrow all of `self` and the compiler would refuse.
        let Self {
            strips,
            buses,
            ports,
            telemetry,
            stats,
            scratch,
            consts,
            ..
        } = self;
        let tel: &Telemetry = telemetry;
        let stats: &EngineStats = stats;

        // Bus accumulators start from silence (only the ones in use).
        for (b, bus) in buses.iter().enumerate() {
            if scratch.bus.active[b] {
                for row in scratch.bus_acc[b].iter_mut().take(bus.channels) {
                    row[..frames].fill(0.0);
                }
            }
        }

        // ? Strips are handled in QUADS so the EQ of four stereo strips runs in one 8-lane SIMD
        // ? loop (`EqChain::process_quad`). Per strip the order is unchanged:
        // ? read -> gate -> EQ -> comp -> mix, and strips still mix into the buses in index
        // ? order, so sums are bit-identical to processing them one by one.
        // ? `chunks_exact_mut(4)` hands out four `&mut Strip` at once, which indexing
        // ? `strips[i]`, `strips[i + 1]`, ... could not (the borrow checker allows one `&mut`).
        let mut loudest = 0.0_f32;
        for (q, quad) in strips.chunks_exact_mut(4).enumerate() {
            let base = 4 * q;
            // Stage 1: ring -> block, gate.
            let mut on = [false; 4];
            for (m, (strip, buf)) in quad
                .iter_mut()
                .zip(scratch.strip_buf.iter_mut())
                .enumerate()
            {
                let input = strip.input_slot.map(|s| &mut ports.inputs[s]);
                let strip_tel = StripTelemetry {
                    meter: &tel.strips[base + m],
                    lan: &tel.lan_rx[base + m],
                };
                on[m] = strip.read_and_gate(
                    input,
                    buf,
                    &mut scratch.interleave,
                    consts,
                    strip_tel,
                    stats,
                );
            }

            // Stage 2: EQ for all four at once.
            {
                let [s0, s1, s2, s3] = quad else { continue };
                let channels = [s0.channels, s1.channels, s2.channels, s3.channels];
                let [b0, b1, b2, b3] = &mut scratch.strip_buf;
                EqChain::process_quad(
                    [s0.eq_mut(), s1.eq_mut(), s2.eq_mut(), s3.eq_mut()],
                    [b0, b1, b2, b3],
                    channels,
                    on,
                    &mut scratch.eq_lanes,
                    frames,
                );
            }

            // Stage 3: compressor, mute/solo, sends, fader, meter.
            for (m, (strip, buf)) in quad
                .iter_mut()
                .zip(scratch.strip_buf.iter_mut())
                .enumerate()
            {
                if on[m] {
                    let peak = strip.comp_mix_meter(
                        buf,
                        &mut scratch.bus_acc,
                        &scratch.bus,
                        consts,
                        &tel.strips[base + m],
                    );
                    loudest = loudest.max(peak);
                }
            }
        }

        // Bus EQ, four buses at a time (same kernel as the strips).
        for (q, (quad, accs)) in buses
            .chunks_exact_mut(4)
            .zip(scratch.bus_acc.chunks_exact_mut(4))
            .enumerate()
        {
            let [b0, b1, b2, b3] = quad else { continue };
            let [a0, a1, a2, a3] = accs else { continue };
            let active = core::array::from_fn(|m| scratch.bus.active[4 * q + m]);
            let channels = [b0.channels, b1.channels, b2.channels, b3.channels];
            EqChain::process_quad(
                [b0.eq_mut(), b1.eq_mut(), b2.eq_mut(), b3.eq_mut()],
                [a0, a1, a2, a3],
                channels,
                active,
                &mut scratch.eq_lanes,
                frames,
            );
        }

        // Bus gain, meter, output and recorder rings.
        for (b, bus) in buses.iter_mut().enumerate() {
            if scratch.bus.active[b] {
                let output = bus.output_slot.map(|s| &mut ports.outputs[s]);
                let peak = bus.finish(
                    frames,
                    &mut scratch.bus_acc[b],
                    &mut scratch.interleave,
                    output,
                    &mut ports.recorders[b],
                    consts,
                    &tel.buses[b],
                    stats,
                );
                loudest = loudest.max(peak);
            } else {
                bus.idle(consts, &tel.buses[b]);
            }
        }

        tel.any_audio_active
            .store(loudest > ACTIVITY_THRESHOLD_LIN, Ordering::Relaxed);
    }

    // SECTION: Routing bookkeeping
    fn refresh_routing(&mut self) {
        if self.routing_dirty {
            let any_solo = self.strips.iter().any(|s| s.solo);
            for s in self.strips.iter_mut() {
                // ? Solo = "everyone who isn't soloed is muted", resolved here once.
                let audible = !s.mute && (!any_solo || s.solo);
                s.audible.set_target(if audible { 1.0 } else { 0.0 });
            }
            self.routing_dirty = false;
        }

        let mut used = [false; MAX_BUSES];
        for s in self.strips.iter().filter(|s| s.input_slot.is_some()) {
            for (b, send) in s.sends.iter().enumerate() {
                used[b] |= send.enabled;
            }
        }
        for (b, bus) in self.buses.iter().enumerate() {
            self.scratch.bus.channels[b] = bus.channels;
            self.scratch.bus.active[b] = bus.output_slot.is_some() || bus.record || used[b];
        }
    }
    // !SECTION: Routing bookkeeping

    // SECTION: Command handling
    fn drain_commands(&mut self) {
        for _ in 0..MAX_COMMANDS_PER_BLOCK {
            match self.cmd_rx.pop() {
                Ok(cmd) => self.apply(cmd),
                Err(_) => break,
            }
        }
    }

    /// Apply one command. Public so tests can drive the engine without a queue.
    /// An invalid command (bad id, bad slot, NaN, wrong channel count) changes nothing and
    /// bumps `EngineStats::rejected_commands`.
    pub fn apply(&mut self, cmd: EngineCommand) {
        use EngineCommand as C;
        let rejected = &self.stats.rejected_commands;
        // ? Early-out used by every `let ... else { bail!() }` below.
        macro_rules! bail {
            () => {{
                rejected.fetch_add(1, Ordering::Relaxed);
                return;
            }};
        }

        match cmd {
            C::SetStripGain { strip, gain_db } => {
                let Some(db) = finite_clamp(gain_db, -200.0, FADER_MAX_DB) else {
                    bail!()
                };
                let Some(s) = self.strips.get_mut(strip.0 as usize) else {
                    bail!()
                };
                s.gain.set_target(fader_to_lin(db));
            }
            C::SetStripPan { strip, pan } => {
                let Some(pan) = finite_clamp(pan, -1.0, 1.0) else {
                    bail!()
                };
                let Some(s) = self.strips.get_mut(strip.0 as usize) else {
                    bail!()
                };
                s.pan = pan;
                s.update_pan_targets();
            }
            C::SetStripMute { strip, mute } => {
                let Some(s) = self.strips.get_mut(strip.0 as usize) else {
                    bail!()
                };
                s.mute = mute;
                self.routing_dirty = true;
            }
            C::SetStripSolo { strip, solo } => {
                let Some(s) = self.strips.get_mut(strip.0 as usize) else {
                    bail!()
                };
                s.solo = solo;
                self.routing_dirty = true;
            }
            C::SetStripDsp { strip, dsp } => {
                let Some(dsp) = sanitize_dsp(&dsp) else {
                    bail!()
                };
                let Some(s) = self.strips.get_mut(strip.0 as usize) else {
                    bail!()
                };
                s.apply_dsp(dsp);
            }
            C::SetRouteSend { strip, bus, send } => {
                let Some(db) = finite_clamp(send.gain_db, -200.0, FADER_MAX_DB) else {
                    bail!()
                };
                if bus.0 as usize >= MAX_BUSES {
                    bail!()
                }
                let Some(s) = self.strips.get_mut(strip.0 as usize) else {
                    bail!()
                };
                let st = &mut s.sends[bus.0 as usize];
                st.enabled = send.enabled;
                st.pre_fader = send.pre_fader;
                st.gain
                    .set_target(if send.enabled { fader_to_lin(db) } else { 0.0 });
            }
            C::SetBusGain { bus, gain_db } => {
                let Some(db) = finite_clamp(gain_db, -200.0, FADER_MAX_DB) else {
                    bail!()
                };
                let Some(b) = self.buses.get_mut(bus.0 as usize) else {
                    bail!()
                };
                b.gain_db = db;
                b.update_level_target();
            }
            C::SetBusMute { bus, mute } => {
                let Some(b) = self.buses.get_mut(bus.0 as usize) else {
                    bail!()
                };
                b.mute = mute;
                b.update_level_target();
            }
            C::SetBusEq { bus, eq } => {
                let Some(eq) = sanitize_bands(&eq) else {
                    bail!()
                };
                let Some(b) = self.buses.get_mut(bus.0 as usize) else {
                    bail!()
                };
                b.eq.set_bands(&eq);
            }
            C::SetBusRecord { bus, enabled } => {
                let Some(b) = self.buses.get_mut(bus.0 as usize) else {
                    bail!()
                };
                b.record = enabled;
            }
            C::AttachStripInput {
                strip,
                slot,
                channels,
            } => {
                let ch = channels as usize;
                let si = strip.0 as usize;
                let Some(idx) = input_index(slot) else {
                    bail!()
                };
                if si >= MAX_STRIPS || !(1..=MAX_CHANNELS).contains(&ch) {
                    bail!()
                }
                // A ring has one consumer, so two strips can't share a slot.
                let taken = self
                    .strips
                    .iter()
                    .enumerate()
                    .any(|(i, s)| i != si && s.input_slot == Some(idx));
                if taken {
                    bail!()
                }
                // Anything left over from a previous user of this ring is stale.
                discard_all(&mut self.ports.inputs[idx]);
                self.strips[si].attach(idx, ch);
                self.routing_dirty = true;
            }
            C::DetachStripInput { strip } => {
                let Some(s) = self.strips.get_mut(strip.0 as usize) else {
                    bail!()
                };
                s.detach();
            }
            C::SetStripClock { strip, clock } => {
                let si = strip.0 as usize;
                // A non-finite target is garbage: reject the whole command, apply nothing.
                if let ClockDomain::External { target_ms } = clock {
                    if !target_ms.is_finite() {
                        bail!()
                    }
                }
                let Some(s) = self.strips.get_mut(si) else {
                    bail!()
                };
                s.set_clock(clock);
                // Stale numbers from the previous clock must not linger in the UI.
                let lan = &self.telemetry.lan_rx[si];
                store_f32(&lan.buffer_ms, 0.0);
                store_f32(&lan.target_ms, 0.0);
                store_f32(&lan.drift_ppm, 0.0);
                lan.underruns.store(0, Ordering::Relaxed);
            }
            C::SetStripTarget { strip, target_ms } => {
                if !target_ms.is_finite() {
                    bail!()
                }
                let Some(s) = self.strips.get_mut(strip.0 as usize) else {
                    bail!()
                };
                s.set_target_ms(target_ms);
            }
            C::AttachBusOutput {
                bus,
                slot,
                channels,
            } => {
                let ch = channels as usize;
                let bi = bus.0 as usize;
                let Some(idx) = output_index(slot) else {
                    bail!()
                };
                if bi >= MAX_BUSES || !(1..=MAX_CHANNELS).contains(&ch) {
                    bail!()
                }
                let taken = self
                    .buses
                    .iter()
                    .enumerate()
                    .any(|(i, b)| i != bi && b.output_slot == Some(idx));
                if taken {
                    bail!()
                }
                self.buses[bi].attach(idx, ch);
            }
            C::DetachBusOutput { bus } => {
                let Some(b) = self.buses.get_mut(bus.0 as usize) else {
                    bail!()
                };
                b.detach();
            }
        }
    }
    // !SECTION: Command handling
}
