//! The master clock when no audio device clocks the mixer (PLAN.md §5.1: "a timer thread").
//!
//! Real backends clock the mixer from the output device's callback. Until one is open, in
//! headless tests and on a box with no sound card, this thread calls `process_block`.

use crate::clock::RateClock;
use crate::rt::{RtOutcome, promote_current_thread};
use audio_engine::MixerEngine;
use audio_types::{ENGINE_BLOCK_FRAMES, ENGINE_SAMPLE_RATE};
use std::io;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, OnceLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// How often the thread looks at the clock. Blocks are mixed as they become due, so a late
/// wake only makes the next turn mix two blocks back to back.
const POLL: Duration = Duration::from_millis(1);

#[derive(Debug, Default)]
pub struct MasterStats {
    pub blocks: AtomicU64,
    /// Longest time between two looks at the clock. About 1 ms when the host is quiet; the dev
    /// container (a microVM) sometimes shows 3 to 30 ms. Tests read this to tell a noisy host
    /// from a real fault.
    pub max_gap_us: AtomicU32,
    /// Whether the mixer thread got real-time priority, and if not, why. Set once, right after
    /// the thread starts; empty for the first instants of its life.
    // ? `OnceLock` is a cell that can be filled exactly once from one thread and read from any
    // ? other, without a lock the reader has to wait on. That suits "the thread reports how its
    // ? start-up went".
    pub rt: OnceLock<RtOutcome>,
}

/// Runs the engine until `stop`. Dropping it stops the thread and drops the engine.
pub struct TimerMaster {
    stop: Arc<AtomicBool>,
    stats: Arc<MasterStats>,
    thread: Option<JoinHandle<MixerEngine>>,
}

impl TimerMaster {
    /// Start mixing. `ppm` makes the master clock run slightly fast or slow (tests).
    // ? If the OS cannot start the thread the closure, and with it the engine, is gone. The
    // ? caller has to build a new engine then; a machine that cannot start a thread has bigger
    // ? problems, and handing the engine back would need a second channel for a case that
    // ? practically never happens.
    pub fn spawn(engine: MixerEngine, ppm: f64) -> io::Result<Self> {
        let stop = Arc::new(AtomicBool::new(false));
        let stats = Arc::new(MasterStats::default());
        let thread = {
            let (stop, stats) = (stop.clone(), stats.clone());
            std::thread::Builder::new()
                .name("mixer-master".into())
                .spawn(move || run(engine, ppm, &stop, &stats))?
        };
        Ok(Self {
            stop,
            stats,
            thread: Some(thread),
        })
    }

    pub fn stats(&self) -> &Arc<MasterStats> {
        &self.stats
    }

    /// Stop the thread and take the engine back (to start it again, or to inspect it).
    pub fn stop(mut self) -> Option<MixerEngine> {
        self.shutdown()
    }

    fn shutdown(&mut self) -> Option<MixerEngine> {
        self.stop.store(true, Relaxed);
        self.thread.take()?.join().ok()
    }
}

impl Drop for TimerMaster {
    fn drop(&mut self) {
        let _ = self.shutdown();
    }
}

fn run(mut engine: MixerEngine, ppm: f64, stop: &AtomicBool, stats: &MasterStats) -> MixerEngine {
    // ? Ask for real-time scheduling on *this* thread, which is the one that mixes. A refusal
    // ? (no rtkit, a container) is recorded and the loop runs at normal priority.
    let _ = stats.rt.set(promote_current_thread(
        ENGINE_BLOCK_FRAMES as u32,
        ENGINE_SAMPLE_RATE,
    ));
    let mut clock = RateClock::new(f64::from(ENGINE_SAMPLE_RATE));
    clock.set_ppm(ppm);
    let mut pending = 0usize;
    let mut last_look = Instant::now();
    while !stop.load(Relaxed) {
        std::thread::sleep(POLL);
        let now = Instant::now();
        let gap = now
            .duration_since(last_look)
            .as_micros()
            .min(u128::from(u32::MAX)) as u32;
        last_look = now;
        stats.max_gap_us.fetch_max(gap, Relaxed);

        pending += clock.due();
        while pending >= ENGINE_BLOCK_FRAMES {
            engine.process_block(ENGINE_BLOCK_FRAMES);
            pending -= ENGINE_BLOCK_FRAMES;
            stats.blocks.fetch_add(1, Relaxed);
        }
    }
    engine
}

#[cfg(test)]
mod tests {
    use super::*;
    use audio_engine::create_engine;

    #[test]
    fn it_mixes_at_about_real_time_and_gives_the_engine_back() {
        let (engine, handles) = create_engine();
        let master = TimerMaster::spawn(engine, 0.0).unwrap();
        std::thread::sleep(Duration::from_millis(400));
        let blocks = master.stats().blocks.load(Relaxed);
        let engine = master.stop();
        assert!(engine.is_some(), "the engine must come back");
        // 400 ms = 150 blocks; allow a lot for a loaded machine, but it must be running.
        assert!((60..=190).contains(&blocks), "{blocks} blocks in 400 ms");
        drop(handles);
    }

    #[test]
    fn the_thread_reports_how_the_priority_request_went() {
        let (engine, _handles) = create_engine();
        let master = TimerMaster::spawn(engine, 0.0).unwrap();
        let stats = master.stats().clone();
        let end = Instant::now() + Duration::from_secs(2);
        while stats.rt.get().is_none() && Instant::now() < end {
            std::thread::sleep(Duration::from_millis(5));
        }
        match stats.rt.get().expect("the thread must report an outcome") {
            RtOutcome::Promoted => {}
            RtOutcome::Unavailable(why) => assert!(!why.is_empty()),
        }
    }
}
