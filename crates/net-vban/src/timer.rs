//! Timer resolution for the 1 ms TX tick (PLAN.md §5.2).
//!
//! * Linux / Android: `park_timeout` is a futex wait with a nanosecond timeout; nothing to do.
//! * Windows: the default system timer ticks every 15.6 ms, so a "1 ms" wait can last 15 ms
//!   and the outgoing stream would leave in bursts. `timeBeginPeriod(1)` asks for 1 ms ticks
//!   for as long as the guard lives.
//!
//! !! Unproven in the container: whether Windows really delivers 1 ms here (it has changed
//! !! between Windows 10 and 11) is one of the PLAN.md §12 hardware checks. `TxStats` records
//! !! the longest wake-up gap so a real run can show it.

/// While alive, the process asks the OS for 1 ms timer resolution (Windows) or does nothing.
#[derive(Debug)]
pub struct TimerResolution {
    #[cfg(windows)]
    active: bool,
}

impl TimerResolution {
    pub fn request_1ms() -> Self {
        Self {
            #[cfg(windows)]
            active: win::begin(),
        }
    }

    /// Whether the OS accepted the request (always false off Windows, where none is needed).
    pub fn is_active(&self) -> bool {
        #[cfg(windows)]
        {
            self.active
        }
        #[cfg(not(windows))]
        {
            false
        }
    }
}

#[cfg(windows)]
impl Drop for TimerResolution {
    fn drop(&mut self) {
        if self.active {
            win::end();
        }
    }
}

#[cfg(windows)]
#[allow(unsafe_code)]
mod win {
    use windows_sys::Win32::Media::{timeBeginPeriod, timeEndPeriod};

    const TIMERR_NOERROR: u32 = 0;

    pub fn begin() -> bool {
        // SAFETY: plain integer argument, no pointers. Every successful call is paired with
        // one `timeEndPeriod(1)` in `TimerResolution::drop`, as the API requires.
        unsafe { timeBeginPeriod(1) == TIMERR_NOERROR }
    }

    pub fn end() {
        // SAFETY: see `begin`.
        unsafe {
            timeEndPeriod(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_guard_can_be_taken_and_released_repeatedly() {
        for _ in 0..3 {
            let _t = TimerResolution::request_1ms();
        }
        // Off Windows there is nothing to request.
        #[cfg(not(windows))]
        assert!(!TimerResolution::request_1ms().is_active());
    }
}
