//! The UI's connection to the daemon. A `ControlApi` call over IPC can block (a daemon that is
//! busy or hung), and a drawing thread must never wait on that, so the calls run on a worker
//! thread and the UI sends *jobs* and collects *replies*.
//!
//! * `Session` is the logic: it owns the connection, reconnects when it is lost, and turns a job
//!   into replies. It has no thread, no clock but `Instant`, and no egui, so tests drive it directly.
//! * `Link` puts a `Session` behind a channel pair (production) or calls it in place (tests).

use app_core::ControlApi;
use app_core::control::ControlResult;
use app_core::types::{Command, ControlError, MeterSnapshot, StateSnapshot};
use platform_io::{AppInfo, DeviceInfo};
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// Makes a connection. In production this is `RemoteControl::connect`; tests return a fake.
pub type Connector = Box<dyn FnMut() -> ControlResult<Arc<dyn ControlApi>> + Send>;

/// Called by the worker after a *user's* command has been answered, so the result is drawn
/// promptly. It must be cheap and must not block.
// ? Only commands wake the UI this way. Meters and snapshots are pulled by the UI from its own
// ? governed frames, so a minimized window is never woken by the worker (law 5).
pub type Wake = Arc<dyn Fn() + Send + Sync>;

#[derive(Clone, Debug, PartialEq)]
pub enum Job {
    Command(Command),
    /// `generation` is echoed in the reply: see `Model` for why.
    Snapshot {
        generation: u64,
    },
    Meters,
    Devices,
    Apps,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Reply {
    Connected,
    Disconnected(String),
    Snapshot {
        generation: u64,
        snapshot: Box<StateSnapshot>,
    },
    Meters(Box<MeterSnapshot>),
    Devices(Vec<DeviceInfo>),
    Apps(Vec<AppInfo>),
    /// The daemon refused a command (the connection is fine).
    Refused(ControlError),
}

pub struct Session {
    connector: Connector,
    api: Option<Arc<dyn ControlApi>>,
    /// Do not try to connect more often than this.
    retry_every: Duration,
    last_try: Option<Instant>,
    /// Why the last attempt failed, repeated while the next one is held back.
    last_error: String,
}

impl Session {
    pub fn new(connector: Connector, retry_every: Duration) -> Self {
        Self {
            connector,
            api: None,
            retry_every,
            last_try: None,
            last_error: String::new(),
        }
    }

    pub fn is_connected(&self) -> bool {
        self.api.is_some()
    }

    /// Run one job. Returns the replies in the order the UI should see them.
    pub fn run(&mut self, job: Job) -> Vec<Reply> {
        let mut out = Vec::new();
        if self.api.is_none() {
            match self.try_connect() {
                Ok(()) => {
                    out.push(Reply::Connected);
                    // ? A new connection starts with a full picture, whatever the job was.
                    out.extend(self.fetch_all());
                }
                Err(e) => {
                    // ? A command that cannot be delivered is not queued: the user would be
                    // ? surprised by a fader move that happens a minute later.
                    out.push(Reply::Disconnected(e.to_string()));
                    return out;
                }
            }
        }
        out.extend(self.execute(job));
        out
    }

    fn try_connect(&mut self) -> ControlResult<()> {
        if let Some(t) = self.last_try {
            if t.elapsed() < self.retry_every {
                return Err(ControlError::Disconnected(self.last_error.clone()));
            }
        }
        self.last_try = Some(Instant::now());
        match (self.connector)() {
            Ok(api) => {
                self.api = Some(api);
                Ok(())
            }
            Err(e) => {
                self.last_error = match &e {
                    ControlError::Disconnected(m) => m.clone(),
                    other => other.to_string(),
                };
                Err(e)
            }
        }
    }

    fn fetch_all(&mut self) -> Vec<Reply> {
        let mut out = self.execute(Job::Snapshot { generation: 0 });
        out.extend(self.execute(Job::Devices));
        out.extend(self.execute(Job::Apps));
        out
    }

    fn execute(&mut self, job: Job) -> Vec<Reply> {
        let Some(api) = self.api.clone() else {
            return Vec::new();
        };
        let result = match job {
            Job::Command(c) => api.command(c).map(|()| None),
            Job::Snapshot { generation } => api.snapshot().map(|s| {
                Some(Reply::Snapshot {
                    generation,
                    snapshot: Box::new(s),
                })
            }),
            Job::Meters => api.meters().map(|m| Some(Reply::Meters(Box::new(m)))),
            Job::Devices => api.devices().map(|d| Some(Reply::Devices(d))),
            Job::Apps => api.apps().map(|a| Some(Reply::Apps(a))),
        };
        match result {
            Ok(reply) => reply.into_iter().collect(),
            Err(ControlError::Disconnected(m) | ControlError::Protocol(m)) => {
                // ? Drop the connection so the next job reconnects from scratch.
                self.api = None;
                self.last_try = Some(Instant::now());
                self.last_error.clone_from(&m);
                vec![Reply::Disconnected(m)]
            }
            Err(e) => vec![Reply::Refused(e)],
        }
    }
}

enum Kind {
    Threaded {
        jobs: Sender<Job>,
        replies: Receiver<Vec<Reply>>,
        // ? Kept so the thread is owned by the link; it ends when `jobs` is dropped.
        _thread: JoinHandle<()>,
    },
    Inline {
        session: Box<Session>,
        queue: VecDeque<Reply>,
    },
}

pub struct Link {
    kind: Kind,
}

impl Link {
    pub fn spawn(mut session: Session, wake: Option<Wake>) -> Self {
        let (jobs, job_rx) = mpsc::channel::<Job>();
        let (reply_tx, replies) = mpsc::channel::<Vec<Reply>>();
        let thread = std::thread::Builder::new()
            .name("ui-link".into())
            .spawn(move || {
                // ? `recv_timeout`, not `recv`: while the UI is idle nothing is sent, and this
                // ? loop must still notice that the link was dropped.
                loop {
                    match job_rx.recv_timeout(Duration::from_millis(500)) {
                        Ok(job) => {
                            let from_user = matches!(job, Job::Command(_));
                            let out = session.run(job);
                            if reply_tx.send(out).is_err() {
                                return;
                            }
                            if from_user {
                                if let Some(w) = &wake {
                                    w();
                                }
                            }
                        }
                        Err(RecvTimeoutError::Timeout) => {}
                        Err(RecvTimeoutError::Disconnected) => return,
                    }
                }
            })
            .expect("the OS refused to start a thread");
        Self {
            kind: Kind::Threaded {
                jobs,
                replies,
                _thread: thread,
            },
        }
    }

    /// Runs every job on the calling thread. For tests.
    pub fn inline(session: Session) -> Self {
        Self {
            kind: Kind::Inline {
                session: Box::new(session),
                queue: VecDeque::new(),
            },
        }
    }

    pub fn send(&mut self, job: Job) {
        match &mut self.kind {
            // ? A send error means the worker is gone, which only happens at shutdown.
            Kind::Threaded { jobs, .. } => {
                let _ = jobs.send(job);
            }
            Kind::Inline { session, queue } => queue.extend(session.run(job)),
        }
    }

    /// Everything that arrived since the last call.
    pub fn drain(&mut self) -> Vec<Reply> {
        match &mut self.kind {
            Kind::Threaded { replies, .. } => replies.try_iter().flatten().collect(),
            Kind::Inline { queue, .. } => queue.drain(..).collect(),
        }
    }
}

/// The production connector: a named local socket to `routerd`.
pub fn ipc_connector(instance: Option<String>) -> Connector {
    Box::new(move || {
        let name = app_core::paths::ipc_name(instance.as_deref());
        let remote = app_core::ipc::RemoteControl::connect(&name)?;
        Ok(Arc::new(remote) as Arc<dyn ControlApi>)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::FakeDaemon;

    fn link(fake: &Arc<FakeDaemon>) -> Link {
        let f = Arc::clone(fake);
        Link::inline(Session::new(Box::new(move || f.connect()), Duration::ZERO))
    }

    #[test]
    fn the_first_job_connects_and_brings_a_full_picture() {
        let fake = FakeDaemon::new();
        let mut l = link(&fake);
        l.send(Job::Meters);
        let r = l.drain();
        assert_eq!(r[0], Reply::Connected);
        assert!(matches!(r[1], Reply::Snapshot { generation: 0, .. }));
        assert!(matches!(r[2], Reply::Devices(_)));
        assert!(matches!(r[3], Reply::Apps(_)));
        assert!(matches!(r[4], Reply::Meters(_)));
    }

    #[test]
    fn a_command_reaches_the_daemon() {
        let fake = FakeDaemon::new();
        let mut l = link(&fake);
        let cmd = Command::StripMute {
            strip: 1,
            mute: true,
        };
        l.send(Job::Command(cmd.clone()));
        assert_eq!(fake.commands(), vec![cmd]);
    }

    #[test]
    fn no_daemon_gives_a_reason_and_queues_nothing() {
        let fake = FakeDaemon::new();
        fake.set_down(true);
        let mut l = link(&fake);
        l.send(Job::Command(Command::Beacon { on: true }));
        let r = l.drain();
        assert!(matches!(r.as_slice(), [Reply::Disconnected(_)]));
        // Back up: the very next job connects, and the old command was NOT replayed.
        fake.set_down(false);
        l.send(Job::Meters);
        assert_eq!(l.drain()[0], Reply::Connected);
        assert!(fake.commands().is_empty());
    }

    #[test]
    fn losing_the_daemon_mid_session_is_reported_and_recovered_from() {
        let fake = FakeDaemon::new();
        let mut l = link(&fake);
        l.send(Job::Meters);
        l.drain();
        fake.set_down(true);
        l.send(Job::Meters);
        assert!(matches!(l.drain().as_slice(), [Reply::Disconnected(_)]));
        fake.set_down(false);
        l.send(Job::Meters);
        assert_eq!(l.drain()[0], Reply::Connected);
    }

    #[test]
    fn a_refused_command_does_not_drop_the_connection() {
        let fake = FakeDaemon::new();
        fake.refuse_commands("strip 9 does not exist");
        let mut l = link(&fake);
        l.send(Job::Command(Command::StripMute {
            strip: 9,
            mute: true,
        }));
        let r = l.drain();
        assert!(r.iter().any(|x| matches!(x, Reply::Refused(_))));
        l.send(Job::Meters);
        assert!(l.drain().iter().all(|x| *x != Reply::Connected));
    }

    #[test]
    fn retries_are_rate_limited() {
        let fake = FakeDaemon::new();
        fake.set_down(true);
        let f = Arc::clone(&fake);
        let mut l = Link::inline(Session::new(
            Box::new(move || f.connect()),
            Duration::from_secs(60),
        ));
        l.send(Job::Meters);
        l.send(Job::Meters);
        l.send(Job::Meters);
        assert_eq!(fake.connect_attempts(), 1, "three jobs, one attempt");
    }

    #[test]
    fn the_threaded_link_delivers_replies_and_wakes_on_commands() {
        use std::sync::atomic::{AtomicU32, Ordering};
        let fake = FakeDaemon::new();
        let woken = Arc::new(AtomicU32::new(0));
        let w = Arc::clone(&woken);
        let f = Arc::clone(&fake);
        let mut l = Link::spawn(
            Session::new(Box::new(move || f.connect()), Duration::ZERO),
            Some(Arc::new(move || {
                w.fetch_add(1, Ordering::SeqCst);
            })),
        );
        l.send(Job::Meters);
        l.send(Job::Command(Command::Beacon { on: true }));
        let mut got = Vec::new();
        for _ in 0..200 {
            got.extend(l.drain());
            if woken.load(Ordering::SeqCst) == 1 && !got.is_empty() {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(got.iter().any(|r| matches!(r, Reply::Meters(_))));
        assert_eq!(woken.load(Ordering::SeqCst), 1, "only the command wakes");
    }
}
