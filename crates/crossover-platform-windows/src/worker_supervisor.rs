//! The service watchdog as a pure state machine (ADR 0011).
//!
//! The Windows service daemon (`crossover-svc`) must keep exactly one worker
//! (`crossover.exe run`) alive in the active user's console session: launch it
//! on logon, relaunch it with bounded backoff if it crashes, stop it on logoff,
//! and drain cleanly on service stop. All of that *decision-making* lives here,
//! free of Win32, so it is exhaustively unit-testable and runs on every CI OS.
//! The privileged glue in [`crate::service`] only translates real OS events
//! into [`WorkerSupervisor`] calls and executes the [`WorkerAction`]s it emits.
//!
//! Design notes that shape the state machine:
//! - **Session-gated, not exit-code-gated.** A worker is wanted only while a
//!   user is logged on to the console; logon/logoff drive launch/stop, per
//!   ADR 0011 ("session state, not just exit code, gates a relaunch").
//! - **A clean exit is intentional.** If the worker exits with success while
//!   the user stays logged on, the user quit it deliberately (e.g. ran
//!   `crossover` interactively); the supervisor goes quiescent rather than
//!   fighting them, and waits for the next logon.
//! - **A crash relaunches, but cannot spin.** Non-success exits relaunch after
//!   an exponential backoff capped at a maximum; a worker that ran healthily
//!   before crashing resets the backoff (mirrors the reconnect policy's intent).

/// A Windows session identifier (as from `WTSGetActiveConsoleSessionId`).
pub type SessionId = u32;

/// Tuning for the watchdog's backoff and health thresholds.
#[derive(Debug, Clone, Copy)]
pub struct WorkerSupervisorConfig {
    /// Delay before the first relaunch after a crash.
    pub base_backoff_ms: u64,
    /// Ceiling the exponential backoff never exceeds.
    pub max_backoff_ms: u64,
    /// A worker that ran at least this long before crashing is treated as
    /// having been healthy, so the backoff streak resets.
    pub healthy_run_ms: u64,
}

impl Default for WorkerSupervisorConfig {
    fn default() -> Self {
        // Mirrors the reconnect policy's spirit: quick first retry, capped
        // spin rate, a stable run clears the penalty.
        Self {
            base_backoff_ms: 1_000,
            max_backoff_ms: 30_000,
            healthy_run_ms: 10_000,
        }
    }
}

/// Why the driver is being told to stop the running worker. Carried on
/// [`WorkerAction::StopWorker`] purely for observability — it does not change
/// the state machine's behavior — so a supervision log can tell a
/// service-initiated stop apart from a session change without guessing from
/// timing (the 2026-08-19 silent-death incident: no exit code, no reason).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopReason {
    /// The service itself is stopping or shutting down (`SERVICE_CONTROL_STOP`
    /// / `SERVICE_CONTROL_SHUTDOWN`).
    ServiceStopping,
    /// No user is logged on to the console (logoff, or no console session).
    Logoff,
    /// The active console session changed while the worker was running in the
    /// previous one (fast user switch).
    SessionChanged,
}

/// What the driver should do next. Exactly one action per [`WorkerSupervisor::poll`];
/// the driver executes it, which produces the next event (or waits until
/// [`WorkerSupervisor::wake_deadline`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkerAction {
    /// Launch the worker in this session now. The supervisor already considers
    /// it running as of the `now_ms` passed to `poll`; if the launch fails, the
    /// driver must call [`WorkerSupervisor::note_launch_failed`].
    Launch(SessionId),
    /// Terminate the running worker, for the carried [`StopReason`]. The
    /// driver reports the resulting exit via [`WorkerSupervisor::note_worker_exited`].
    StopWorker(StopReason),
    /// Nothing to do now. If [`WorkerSupervisor::wake_deadline`] is `Some`, call
    /// `poll` again at that time (a pending backoff relaunch).
    Idle,
    /// Terminal: a stop was requested and no worker is running. The daemon may
    /// report `SERVICE_STOPPED` and exit.
    Stopped,
}

/// What the single supervised worker is doing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Worker {
    /// No worker; launch as soon as a session is present.
    Idle,
    /// Running in `session` since `since_ms`.
    Running { session: SessionId, since_ms: u64 },
    /// Crashed; do not relaunch before `resume_at_ms`.
    Backoff { resume_at_ms: u64 },
    /// Exited cleanly while logged on — treated as an intentional quit. Do not
    /// relaunch until the next logon.
    Quiescent,
}

/// Decides when to launch, stop, and relaunch the worker (ADR 0011). Pure: it
/// touches no OS API and takes the current time as a parameter, so every
/// transition is deterministic and testable.
#[derive(Debug, Clone)]
pub struct WorkerSupervisor {
    config: WorkerSupervisorConfig,
    active_session: Option<SessionId>,
    worker: Worker,
    consecutive_crashes: u32,
    stopping: bool,
    /// Set when `poll` emitted [`WorkerAction::StopWorker`] for a reason other
    /// than a service stop (logoff, or a session switch). The exit that follows
    /// is then neither a crash nor an intentional quit — it is a stop *we*
    /// asked for — so it returns the worker to `Idle`, ready to relaunch.
    stop_worker_pending: bool,
}

impl WorkerSupervisor {
    /// A supervisor with no active session and no worker.
    #[must_use]
    pub fn new(config: WorkerSupervisorConfig) -> Self {
        Self {
            config,
            active_session: None,
            worker: Worker::Idle,
            consecutive_crashes: 0,
            stopping: false,
            stop_worker_pending: false,
        }
    }

    /// Record the active console session: `Some(id)` on logon or a console
    /// connect, `None` on logoff or disconnect. A change to a *new* session
    /// clears any backoff/quiescence so the next `poll` launches afresh.
    pub fn note_active_session(&mut self, session: Option<SessionId>) {
        if session == self.active_session {
            return;
        }
        self.active_session = session;
        // A new session is a fresh context: forget the previous run's penalty
        // and any "user quit it" quiescence so a worker is launched for it.
        self.consecutive_crashes = 0;
        if matches!(self.worker, Worker::Backoff { .. } | Worker::Quiescent) {
            self.worker = Worker::Idle;
        }
        // A `Running` worker in the old session is left for `poll` to stop
        // (it detects the session mismatch), keeping the stop path in one place.
    }

    /// Record that the worker process exited. `crashed` is true for any
    /// non-success exit. Ignored unless a worker was considered running.
    pub fn note_worker_exited(&mut self, crashed: bool, now_ms: u64) {
        // During a stop, an exit just clears the worker; `poll` drives the
        // terminal transition, and backoff is irrelevant.
        if self.stopping {
            self.worker = Worker::Idle;
            return;
        }
        // A stop we initiated (logoff / session switch) is neither crash nor
        // quit: return to Idle so `poll` can relaunch per the current session.
        if self.stop_worker_pending {
            self.stop_worker_pending = false;
            self.worker = Worker::Idle;
            return;
        }
        let Worker::Running { since_ms, .. } = self.worker else {
            return;
        };
        if !crashed {
            self.consecutive_crashes = 0;
            self.worker = Worker::Quiescent;
            return;
        }
        let ran_ms = now_ms.saturating_sub(since_ms);
        self.consecutive_crashes = if ran_ms >= self.config.healthy_run_ms {
            1
        } else {
            self.consecutive_crashes.saturating_add(1)
        };
        self.worker = Worker::Backoff {
            resume_at_ms: now_ms.saturating_add(self.backoff_ms()),
        };
    }

    /// Record that launching the worker failed. Treated as a crash so the same
    /// bounded backoff applies rather than a tight relaunch loop.
    pub fn note_launch_failed(&mut self, now_ms: u64) {
        if self.stopping {
            self.worker = Worker::Idle;
            return;
        }
        self.consecutive_crashes = self.consecutive_crashes.saturating_add(1);
        self.worker = Worker::Backoff {
            resume_at_ms: now_ms.saturating_add(self.backoff_ms()),
        };
    }

    /// Ask the service to shut down: the next `poll` stops any worker and then
    /// reports [`WorkerAction::Stopped`].
    pub fn request_stop(&mut self) {
        self.stopping = true;
    }

    /// The single action to perform now, advancing internal state for launches.
    pub fn poll(&mut self, now_ms: u64) -> WorkerAction {
        if self.stopping {
            return if matches!(self.worker, Worker::Running { .. }) {
                WorkerAction::StopWorker(StopReason::ServiceStopping)
            } else {
                WorkerAction::Stopped
            };
        }

        let Some(session) = self.active_session else {
            // No user logged on: ensure nothing is running.
            return if matches!(self.worker, Worker::Running { .. }) {
                self.stop_worker_pending = true;
                WorkerAction::StopWorker(StopReason::Logoff)
            } else {
                WorkerAction::Idle
            };
        };

        match self.worker {
            Worker::Running {
                session: running_session,
                ..
            } if running_session == session => WorkerAction::Idle,
            // A worker running in a stale session (the active session changed
            // under it) must be stopped before relaunching in the new one.
            Worker::Running { .. } => {
                self.stop_worker_pending = true;
                WorkerAction::StopWorker(StopReason::SessionChanged)
            }
            Worker::Idle => {
                self.worker = Worker::Running {
                    session,
                    since_ms: now_ms,
                };
                WorkerAction::Launch(session)
            }
            Worker::Backoff { resume_at_ms } => {
                if now_ms >= resume_at_ms {
                    self.worker = Worker::Running {
                        session,
                        since_ms: now_ms,
                    };
                    WorkerAction::Launch(session)
                } else {
                    WorkerAction::Idle
                }
            }
            // Intentional quit: wait for the next logon (a session change
            // clears this), not for a timer.
            Worker::Quiescent => WorkerAction::Idle,
        }
    }

    /// When the driver should next call [`Self::poll`] absent any OS event: the
    /// pending backoff relaunch time, if one is due. `None` means "wait for the
    /// next event".
    #[must_use]
    pub fn wake_deadline(&self) -> Option<u64> {
        if self.stopping || self.active_session.is_none() {
            return None;
        }
        match self.worker {
            Worker::Backoff { resume_at_ms } => Some(resume_at_ms),
            _ => None,
        }
    }

    /// Current exponential backoff for the crash streak, capped.
    fn backoff_ms(&self) -> u64 {
        let steps = self.consecutive_crashes.saturating_sub(1).min(20);
        let scaled = self.config.base_backoff_ms.saturating_mul(1_u64 << steps);
        scaled.min(self.config.max_backoff_ms)
    }
}

#[cfg(test)]
mod tests {
    use super::{SessionId, StopReason, WorkerAction, WorkerSupervisor, WorkerSupervisorConfig};

    const SESSION: SessionId = 1;
    const OTHER_SESSION: SessionId = 2;

    fn config() -> WorkerSupervisorConfig {
        WorkerSupervisorConfig {
            base_backoff_ms: 1_000,
            max_backoff_ms: 8_000,
            healthy_run_ms: 10_000,
        }
    }

    #[test]
    fn no_session_means_no_worker() {
        let mut sup = WorkerSupervisor::new(config());
        assert_eq!(sup.poll(0), WorkerAction::Idle);
        assert_eq!(sup.wake_deadline(), None);
    }

    #[test]
    fn logon_launches_then_stays_idle_while_running() {
        let mut sup = WorkerSupervisor::new(config());
        sup.note_active_session(Some(SESSION));
        assert_eq!(sup.poll(0), WorkerAction::Launch(SESSION));
        // Already running in the same session: nothing more to do.
        assert_eq!(sup.poll(5), WorkerAction::Idle);
        assert_eq!(sup.poll(10), WorkerAction::Idle);
    }

    #[test]
    fn crash_relaunches_after_backoff() {
        let mut sup = WorkerSupervisor::new(config());
        sup.note_active_session(Some(SESSION));
        assert_eq!(sup.poll(0), WorkerAction::Launch(SESSION));

        // Crash after a short run -> first backoff = base (1_000).
        sup.note_worker_exited(true, 500);
        assert_eq!(sup.wake_deadline(), Some(1_500));
        assert_eq!(sup.poll(1_000), WorkerAction::Idle); // too early
        assert_eq!(sup.poll(1_500), WorkerAction::Launch(SESSION)); // due
    }

    #[test]
    fn repeated_crashes_back_off_exponentially_and_cap() {
        let mut sup = WorkerSupervisor::new(config());
        sup.note_active_session(Some(SESSION));

        let mut now = 0;
        let expected = [1_000_u64, 2_000, 4_000, 8_000, 8_000]; // capped at max 8_000
        for delay in expected {
            assert_eq!(sup.poll(now), WorkerAction::Launch(SESSION));
            // Crash immediately (unhealthy run) so the streak escalates.
            sup.note_worker_exited(true, now);
            assert_eq!(sup.wake_deadline(), Some(now + delay));
            now += delay;
        }
    }

    #[test]
    fn a_healthy_run_resets_the_backoff() {
        let mut sup = WorkerSupervisor::new(config());
        sup.note_active_session(Some(SESSION));
        assert_eq!(sup.poll(0), WorkerAction::Launch(SESSION));

        // Two quick crashes escalate to a 2_000 backoff...
        sup.note_worker_exited(true, 100);
        assert_eq!(sup.wake_deadline(), Some(1_100));
        assert_eq!(sup.poll(1_100), WorkerAction::Launch(SESSION));
        sup.note_worker_exited(true, 1_200);
        assert_eq!(sup.wake_deadline(), Some(3_200)); // base * 2

        // ...then a launch that runs past the healthy threshold before crashing
        // resets the streak, so the next backoff is base again.
        assert_eq!(sup.poll(3_200), WorkerAction::Launch(SESSION));
        sup.note_worker_exited(true, 3_200 + 10_000);
        assert_eq!(sup.wake_deadline(), Some(3_200 + 10_000 + 1_000));
    }

    #[test]
    fn clean_exit_goes_quiescent_until_next_logon() {
        let mut sup = WorkerSupervisor::new(config());
        sup.note_active_session(Some(SESSION));
        assert_eq!(sup.poll(0), WorkerAction::Launch(SESSION));

        // User quit it deliberately: do not relaunch, and no timer is pending.
        sup.note_worker_exited(false, 5_000);
        assert_eq!(sup.poll(6_000), WorkerAction::Idle);
        assert_eq!(sup.wake_deadline(), None);

        // A fresh logon (new session) relaunches.
        sup.note_active_session(Some(OTHER_SESSION));
        assert_eq!(sup.poll(7_000), WorkerAction::Launch(OTHER_SESSION));
    }

    #[test]
    fn logoff_stops_the_worker_and_next_logon_relaunches() {
        let mut sup = WorkerSupervisor::new(config());
        sup.note_active_session(Some(SESSION));
        assert_eq!(sup.poll(0), WorkerAction::Launch(SESSION));

        // Logoff: stop the running worker.
        sup.note_active_session(None);
        assert_eq!(
            sup.poll(1_000),
            WorkerAction::StopWorker(StopReason::Logoff)
        );
        sup.note_worker_exited(false, 1_000);
        assert_eq!(sup.poll(1_100), WorkerAction::Idle);

        // Next logon relaunches.
        sup.note_active_session(Some(SESSION));
        assert_eq!(sup.poll(2_000), WorkerAction::Launch(SESSION));
    }

    #[test]
    fn active_session_change_restarts_in_the_new_session() {
        let mut sup = WorkerSupervisor::new(config());
        sup.note_active_session(Some(SESSION));
        assert_eq!(sup.poll(0), WorkerAction::Launch(SESSION));

        // Fast user switch: the console is now a different session.
        sup.note_active_session(Some(OTHER_SESSION));
        assert_eq!(
            sup.poll(1_000),
            WorkerAction::StopWorker(StopReason::SessionChanged)
        );
        sup.note_worker_exited(false, 1_000);
        assert_eq!(sup.poll(1_100), WorkerAction::Launch(OTHER_SESSION));
    }

    #[test]
    fn launch_failure_backs_off_like_a_crash() {
        let mut sup = WorkerSupervisor::new(config());
        sup.note_active_session(Some(SESSION));
        assert_eq!(sup.poll(0), WorkerAction::Launch(SESSION));

        // CreateProcessAsUser failed: back off rather than spin.
        sup.note_launch_failed(200);
        assert_eq!(sup.wake_deadline(), Some(1_200));
        assert_eq!(sup.poll(500), WorkerAction::Idle);
        assert_eq!(sup.poll(1_200), WorkerAction::Launch(SESSION));
    }

    #[test]
    fn stop_while_running_stops_then_reports_stopped() {
        let mut sup = WorkerSupervisor::new(config());
        sup.note_active_session(Some(SESSION));
        assert_eq!(sup.poll(0), WorkerAction::Launch(SESSION));

        sup.request_stop();
        assert_eq!(
            sup.poll(1_000),
            WorkerAction::StopWorker(StopReason::ServiceStopping)
        );
        sup.note_worker_exited(false, 1_000);
        assert_eq!(sup.poll(1_100), WorkerAction::Stopped);
    }

    #[test]
    fn stop_while_idle_reports_stopped_immediately() {
        let mut sup = WorkerSupervisor::new(config());
        sup.note_active_session(Some(SESSION));
        // Never polled a launch; stop should be immediate.
        sup.request_stop();
        assert_eq!(sup.poll(0), WorkerAction::Stopped);
    }

    #[test]
    fn stop_during_backoff_reports_stopped_without_waiting() {
        let mut sup = WorkerSupervisor::new(config());
        sup.note_active_session(Some(SESSION));
        assert_eq!(sup.poll(0), WorkerAction::Launch(SESSION));
        sup.note_worker_exited(true, 100); // now backing off
        sup.request_stop();
        assert_eq!(sup.poll(200), WorkerAction::Stopped);
        assert_eq!(sup.wake_deadline(), None);
    }
}
