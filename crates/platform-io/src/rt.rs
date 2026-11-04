//! Real-time priority for the calling thread (PLAN.md §4.3, §6.2).
//!
//! * With the `rt` feature, Linux asks rtkit over D-Bus (`audio_thread_priority`), the same way
//!   PipeWire and PulseAudio do, so it works for an ordinary user.
//! * Without it, or when rtkit is missing (a container, a minimal system), the call reports
//!   why and the thread keeps its normal priority. That is never an error: audio still runs, it
//!   just has less protection from a busy machine.

/// What happened when a thread asked for real-time priority.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RtOutcome {
    Promoted,
    /// Not promoted; the text says why (shown by `routerd` at start-up).
    Unavailable(String),
}

impl RtOutcome {
    pub fn is_promoted(&self) -> bool {
        matches!(self, Self::Promoted)
    }
}

/// Ask for real-time scheduling for the *calling* thread. `period_frames` is how much audio the
/// thread produces per wake-up (the scheduler uses it to size the real-time budget).
#[cfg(feature = "rt")]
pub fn promote_current_thread(period_frames: u32, sample_rate: u32) -> RtOutcome {
    match audio_thread_priority::promote_current_thread_to_real_time(period_frames, sample_rate) {
        // ? The handle is only needed to *undo* the promotion. The thread keeps the priority
        // ? until it ends, so it is dropped here on purpose.
        Ok(_handle) => RtOutcome::Promoted,
        Err(e) => RtOutcome::Unavailable(e.to_string()),
    }
}

#[cfg(not(feature = "rt"))]
pub fn promote_current_thread(_period_frames: u32, _sample_rate: u32) -> RtOutcome {
    RtOutcome::Unavailable("built without the `rt` feature".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn asking_never_panics_and_always_explains_a_refusal() {
        // In a container there is no rtkit, so the usual answer is a refusal. Either answer is
        // fine; what matters is that the thread survives and the reason is not empty.
        let handle = std::thread::spawn(|| promote_current_thread(128, 48_000));
        match handle.join().expect("the thread must survive the request") {
            RtOutcome::Promoted => {}
            RtOutcome::Unavailable(why) => assert!(!why.is_empty()),
        }
    }
}
