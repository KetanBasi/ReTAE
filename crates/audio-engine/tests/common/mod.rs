//! Shared test rig: an engine plus helpers to feed strips and drain buses.
//!
//! Each integration-test file is its own binary, so each one pulls this module in with
//! `mod common;` and gets its own copy (including the checking allocator below).
#![allow(dead_code)] // ? Not every test file uses every helper.

use audio_engine::{
    EngineHandles, InputPort, MixerEngine, OutputPort, RecorderPort, create_engine,
};
use audio_types::{
    BusId, ENGINE_BLOCK_FRAMES, ENGINE_SAMPLE_RATE, EngineCommand, RouteSend, StripId, Telemetry,
};

// * With the checking allocator installed, any alloc/free inside `process_block` aborts the
// * test binary. Compiled out in release (the crate's `disable_release` default).
#[cfg(debug_assertions)]
#[global_allocator]
static ALLOC: assert_no_alloc::AllocDisabler = assert_no_alloc::AllocDisabler;

pub const BLOCK: usize = ENGINE_BLOCK_FRAMES;
pub const FS: f32 = ENGINE_SAMPLE_RATE as f32;

/// A signal generator: `(absolute_frame, channel) -> sample`.
pub type Sig<'a> = &'a dyn Fn(usize, usize) -> f32;

pub struct Rig {
    pub engine: MixerEngine,
    pub h: EngineHandles,
    inputs: Vec<Option<InputPort>>,
    outputs: Vec<Option<OutputPort>>,
    recorders: Vec<Option<RecorderPort>>,
    in_ch: [usize; 16],
    out_ch: [usize; 16],
    /// Absolute frame counter used by the signal generators.
    t: usize,
}

impl Rig {
    pub fn new() -> Self {
        let (engine, h) = create_engine();
        Self {
            engine,
            h,
            inputs: (0..16).map(|_| None).collect(),
            outputs: (0..16).map(|_| None).collect(),
            recorders: (0..16).map(|_| None).collect(),
            in_ch: [2; 16],
            out_ch: [2; 16],
            t: 0,
        }
    }

    /// The common case: strip 0 and bus 0, both stereo, routed at unity, and settled.
    pub fn unity() -> Self {
        let mut r = Self::new();
        r.attach_input(0, 2);
        r.attach_output(0, 2);
        r.route(0, 0, 0.0, false);
        r
    }

    pub fn tel(&self) -> &Telemetry {
        &self.h.telemetry
    }

    /// Queue a command (applied at the start of the next block).
    pub fn send(&mut self, cmd: EngineCommand) {
        self.h.commands.push(cmd).expect("command queue full");
    }

    /// Attach a fresh input ring to `strip`. Runs one block so the attach has taken effect
    /// (attach discards ring contents, so feed only *after* this returns).
    pub fn attach_input(&mut self, strip: u8, channels: u8) {
        let port = self.h.ports.acquire_input().expect("free input slot");
        self.send(EngineCommand::AttachStripInput {
            strip: StripId(strip),
            slot: port.slot(),
            channels,
        });
        self.inputs[strip as usize] = Some(port);
        self.in_ch[strip as usize] = channels as usize;
        self.settle();
    }

    pub fn attach_output(&mut self, bus: u8, channels: u8) {
        let port = self.h.ports.acquire_output().expect("free output slot");
        self.send(EngineCommand::AttachBusOutput {
            bus: BusId(bus),
            slot: port.slot(),
            channels,
        });
        self.outputs[bus as usize] = Some(port);
        self.out_ch[bus as usize] = channels as usize;
        self.settle();
    }

    pub fn attach_recorder(&mut self, bus: u8) {
        let port = self
            .h
            .ports
            .acquire_recorder(BusId(bus))
            .expect("free recorder");
        self.recorders[bus as usize] = Some(port);
    }

    /// Enable a send strip -> bus.
    pub fn route(&mut self, strip: u8, bus: u8, gain_db: f32, pre_fader: bool) {
        self.send(EngineCommand::SetRouteSend {
            strip: StripId(strip),
            bus: BusId(bus),
            send: RouteSend {
                enabled: true,
                gain_db,
                pre_fader,
            },
        });
    }

    /// Process one block, then throw away whatever the buses produced.
    pub fn settle(&mut self) {
        self.process(1);
        for b in 0..16u8 {
            let _ = self.drain(b);
        }
    }

    pub fn process(&mut self, blocks: usize) {
        for _ in 0..blocks {
            self.engine.process_block(BLOCK);
        }
    }

    /// Push interleaved samples into the strip's input ring.
    pub fn feed(&mut self, strip: u8, interleaved: &[f32]) {
        let port = self.inputs[strip as usize]
            .as_mut()
            .expect("strip has no input");
        assert_eq!(port.push(interleaved), interleaved.len(), "input ring full");
    }

    /// Everything currently in the bus's output ring, interleaved.
    pub fn drain(&mut self, bus: u8) -> Vec<f32> {
        let Some(port) = self.outputs[bus as usize].as_mut() else {
            return Vec::new();
        };
        let mut all = Vec::new();
        let mut tmp = vec![0.0; 4096];
        loop {
            let n = port.pop(&mut tmp);
            if n == 0 {
                break;
            }
            all.extend_from_slice(&tmp[..n]);
        }
        all
    }

    pub fn drain_recorder(&mut self, bus: u8) -> Vec<f32> {
        let port = self.recorders[bus as usize].as_mut().expect("no recorder");
        let mut all = Vec::new();
        let mut tmp = vec![0.0; 4096];
        loop {
            let n = port.pop(&mut tmp);
            if n == 0 {
                break;
            }
            all.extend_from_slice(&tmp[..n]);
        }
        all
    }

    /// Feed one generated block to every listed strip, process it, and return what bus
    /// `bus` produced. `sig(absolute_frame, channel) -> sample`.
    pub fn step(&mut self, bus: u8, feeds: &[(u8, Sig)]) -> Vec<f32> {
        for (strip, sig) in feeds {
            let n = self.in_ch[*strip as usize];
            let mut blk = Vec::with_capacity(BLOCK * n);
            for f in 0..BLOCK {
                for c in 0..n {
                    blk.push(sig(self.t + f, c));
                }
            }
            self.feed(*strip, &blk);
        }
        self.t += BLOCK;
        self.process(1);
        self.drain(bus)
    }

    /// `blocks` calls of `step`, concatenated.
    pub fn run_many(&mut self, blocks: usize, bus: u8, feeds: &[(u8, Sig)]) -> Vec<f32> {
        let mut out = Vec::new();
        for _ in 0..blocks {
            out.extend(self.step(bus, feeds));
        }
        out
    }

    /// Single-strip convenience for `run_many`.
    pub fn run(
        &mut self,
        strip: u8,
        bus: u8,
        blocks: usize,
        sig: impl Fn(usize, usize) -> f32,
    ) -> Vec<f32> {
        self.run_many(blocks, bus, &[(strip, &sig)])
    }
}

/// Number of blocks after which every 5 ms smoother has settled exactly (> 20 time constants).
pub const SETTLE: usize = 40;

/// Constant signal.
pub fn dc(v: f32) -> impl Fn(usize, usize) -> f32 {
    move |_, _| v
}

/// Sine at `amp`, `freq` Hz, identical on every channel.
pub fn tone(amp: f32, freq: f32) -> impl Fn(usize, usize) -> f32 {
    move |t, _| amp * (core::f32::consts::TAU * freq * t as f32 / FS).sin()
}

/// ±`amp` square wave: |x| == amp always, so detector levels are exact.
pub fn square(amp: f32) -> impl Fn(usize, usize) -> f32 {
    move |t, _| if (t / 24) % 2 == 0 { amp } else { -amp }
}

/// Last `n` frames' worth of one channel.
pub fn tail(x: &[f32], channels: usize, c: usize, n: usize) -> Vec<f32> {
    let ch = chan(x, channels, c);
    ch[ch.len().saturating_sub(n)..].to_vec()
}

// SECTION: Signal helpers
/// Interleaved sine, the same on every channel.
pub fn sine(channels: usize, amp: f32, freq: f32, start_frame: usize, frames: usize) -> Vec<f32> {
    let mut v = Vec::with_capacity(channels * frames);
    for f in 0..frames {
        let t = (start_frame + f) as f32 / FS;
        let s = amp * (core::f32::consts::TAU * freq * t).sin();
        for _ in 0..channels {
            v.push(s);
        }
    }
    v
}

/// One channel out of an interleaved buffer.
pub fn chan(x: &[f32], channels: usize, c: usize) -> Vec<f32> {
    x.iter().skip(c).step_by(channels).copied().collect()
}

pub fn rms(x: &[f32]) -> f32 {
    (x.iter().map(|v| v * v).sum::<f32>() / x.len().max(1) as f32).sqrt()
}

pub fn peak(x: &[f32]) -> f32 {
    x.iter().fold(0.0, |m, v| v.abs().max(m))
}

pub fn db(lin: f32) -> f32 {
    20.0 * lin.max(1.0e-9).log10()
}
// !SECTION: Signal helpers
