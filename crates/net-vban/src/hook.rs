//! A start-up hook for the network threads, so the host can raise their priority.
//!
//! `net-vban` does not know how to ask for real-time priority (rtkit on Linux, MMCSS on Windows,
//! a Java call on Android): that needs platform libraries this crate avoids on purpose. The
//! program that links both crates registers one function here, and every RX/TX thread calls it
//! first thing, on the thread being promoted (priorities are per thread).

use std::sync::OnceLock;

/// Which kind of worker is starting.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ThreadRole {
    Rx,
    Tx,
}

// ? A plain `fn` pointer (not a closure) is `Copy + Send + Sync` for free, so it can sit in a
// ? `static` and be called from any thread without a lock.
static HOOK: OnceLock<fn(ThreadRole)> = OnceLock::new();

/// Register the function every network thread calls when it starts. Only the first call counts;
/// returns `false` if a hook was already set. Threads started before this call are not promoted.
pub fn set_thread_start_hook(hook: fn(ThreadRole)) -> bool {
    HOOK.set(hook).is_ok()
}

/// Called by the RX/TX workers on their own thread, before they touch a socket.
pub(crate) fn thread_started(role: ThreadRole) {
    if let Some(hook) = HOOK.get() {
        hook(role);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering::Relaxed};

    static CALLS: AtomicU32 = AtomicU32::new(0);

    #[test]
    fn the_first_hook_wins_and_is_called_on_the_new_thread() {
        // ? One test owns the process-wide hook; a second registration must be refused.
        assert!(set_thread_start_hook(|_| {
            CALLS.fetch_add(1, Relaxed);
        }));
        assert!(!set_thread_start_hook(|_| {}));
        std::thread::spawn(|| thread_started(ThreadRole::Tx))
            .join()
            .unwrap();
        assert_eq!(CALLS.load(Relaxed), 1);
    }
}
