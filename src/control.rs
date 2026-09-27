//! Process-wide control: turning Unix signals into requests every device task sees.
//!
//! One task ([`spawn_signal_handler`]) owns the signal listeners for the whole life of
//! the program, so a signal arriving while a device loop is busy handling an event is
//! never missed (a listener re-created on every loop iteration would drop it). It
//! translates each signal into a [`SignalAction`] and records it in a [`Controller`];
//! interested code subscribes and waits for the request it cares about.
//!
//! Stopping is level-triggered through [`StopSource`]/[`StopSignal`]: once stopped,
//! every current and future waiter sees it immediately, so no task can miss it by not
//! being inside `select!` at the moment it happened.

use std::sync::Arc;

use tokio::sync::watch;

use crate::log::Log;

/// What a received signal asks the program to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignalAction {
    /// Stop cleanly: restore the changed buttons and close every device. A second
    /// quit request while the first is still being handled exits immediately.
    Quit,
}

/// The signals the program handles, by name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Signal {
    /// `SIGINT` (Ctrl-C in a terminal).
    Interrupt,
    /// `SIGTERM` (`kill`, `systemctl stop`, `service dak stop`).
    Terminate,
    /// `SIGHUP` (terminal hang-up, `systemctl reload`).
    Hangup,
}

impl Signal {
    /// The conventional signal name, for log lines.
    pub fn name(self) -> &'static str {
        match self {
            Signal::Interrupt => "SIGINT",
            Signal::Terminate => "SIGTERM",
            Signal::Hangup => "SIGHUP",
        }
    }

    /// What this signal asks for. Until configuration reload exists `SIGHUP` stops the
    /// program cleanly too, instead of killing it on the spot (its default action).
    pub fn action(self) -> SignalAction {
        match self {
            Signal::Interrupt | Signal::Terminate | Signal::Hangup => SignalAction::Quit,
        }
    }
}

/// The accumulated requests, as published to subscribers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ControlState {
    /// Whether a quit has been requested.
    pub quit: bool,
}

/// What [`Controller::handle`] decided to do about a signal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Handled {
    /// The request was recorded; subscribers act on it.
    Recorded,
    /// A quit arrived while a previous quit was still being carried out: the caller
    /// should exit the process right away.
    ForceExit,
}

/// Records the requests signals make and publishes them to subscribers.
#[derive(Debug)]
pub struct Controller {
    /// The current state; subscribers watch it.
    state: watch::Sender<ControlState>,
}

impl Default for Controller {
    /// A controller with nothing requested yet.
    fn default() -> Self {
        Self::new()
    }
}

impl Controller {
    /// A controller with nothing requested yet.
    pub fn new() -> Self {
        Self {
            state: watch::Sender::new(ControlState::default()),
        }
    }

    /// The requests recorded so far.
    pub fn state(&self) -> ControlState {
        *self.state.borrow()
    }

    /// A receiver that observes every later change (and the current state).
    pub fn subscribe(&self) -> watch::Receiver<ControlState> {
        self.state.subscribe()
    }

    /// Records `action`, returning [`Handled::ForceExit`] for a repeated quit.
    pub fn handle(&self, action: SignalAction) -> Handled {
        match action {
            SignalAction::Quit => {
                let mut repeated = false;
                self.state.send_modify(|state| {
                    repeated = state.quit;
                    state.quit = true;
                });
                if repeated {
                    Handled::ForceExit
                } else {
                    Handled::Recorded
                }
            }
        }
    }

    /// Requests a quit without a signal (e.g. when every device has ended).
    pub fn request_quit(&self) {
        self.state.send_modify(|state| state.quit = true);
    }

    /// Resolves once a quit has been requested (immediately if it already was).
    pub async fn quit_requested(&self) {
        let mut rx = self.state.subscribe();
        let _ = rx.wait_for(|state| state.quit).await;
    }
}

/// The owning side of a stop flag: once [`StopSource::stop`] is called every
/// [`StopSignal`] made from it reports stopped, now and forever.
#[derive(Debug)]
pub struct StopSource {
    /// `true` once stopped.
    flag: watch::Sender<bool>,
}

impl Default for StopSource {
    /// A source that has not been stopped.
    fn default() -> Self {
        Self::new()
    }
}

impl StopSource {
    /// A source that has not been stopped.
    pub fn new() -> Self {
        Self {
            flag: watch::Sender::new(false),
        }
    }

    /// Stops every signal made from this source. Idempotent.
    pub fn stop(&self) {
        self.flag.send_replace(true);
    }

    /// A signal observing this source.
    pub fn signal(&self) -> StopSignal {
        StopSignal {
            flag: self.flag.subscribe(),
        }
    }
}

/// The observing side of a [`StopSource`]; cheap to clone into every task.
#[derive(Debug, Clone)]
pub struct StopSignal {
    /// `true` once stopped.
    flag: watch::Receiver<bool>,
}

impl StopSignal {
    /// Whether the source was stopped. A dropped source counts as stopped, so a task
    /// can never be left running with nobody able to stop it.
    pub fn is_stopped(&self) -> bool {
        *self.flag.borrow() || self.flag.has_changed().is_err()
    }

    /// Resolves once the source is stopped (immediately if it already is). Safe to use
    /// as a `select!` branch that is re-created on every iteration: the flag is a level,
    /// not an edge, so nothing is lost between iterations.
    pub async fn stopped(&self) {
        let mut flag = self.flag.clone();
        let _ = flag.wait_for(|stopped| *stopped).await;
    }
}

/// Starts the task that owns the process's signal listeners and feeds `controller`.
///
/// Listeners are installed before this returns, so a signal that arrives right after
/// startup is already handled rather than killing the process. A quit signal that
/// arrives while a previous quit is still being carried out (e.g. a device that will
/// not close) exits the process immediately with [`crate::exit::FAILURE`].
pub fn spawn_signal_handler(
    controller: Arc<Controller>,
    log: Log,
) -> std::io::Result<tokio::task::JoinHandle<()>> {
    use tokio::signal::unix::{signal, SignalKind};
    let mut interrupt = signal(SignalKind::interrupt())?;
    let mut terminate = signal(SignalKind::terminate())?;
    let mut hangup = signal(SignalKind::hangup())?;
    Ok(tokio::spawn(async move {
        loop {
            let received = tokio::select! {
                Some(()) = interrupt.recv() => Signal::Interrupt,
                Some(()) = terminate.recv() => Signal::Terminate,
                Some(()) = hangup.recv() => Signal::Hangup,
                else => return,
            };
            dispatch(&controller, received, log);
        }
    }))
}

/// Records one received signal in `controller`, logging it, and exits the process on a
/// repeated quit.
fn dispatch(controller: &Controller, received: Signal, log: Log) {
    let action = received.action();
    match controller.handle(action) {
        Handled::Recorded => log.info(format!("received {}; shutting down", received.name())),
        Handled::ForceExit => {
            log.warn(format!(
                "received {} again while shutting down; exiting immediately",
                received.name()
            ));
            std::process::exit(crate::exit::FAILURE as i32);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// INT, TERM and (until reload exists) HUP all ask for a clean stop.
    #[test]
    fn stop_signals_map_to_quit() {
        for signal in [Signal::Interrupt, Signal::Terminate, Signal::Hangup] {
            assert_eq!(signal.action(), SignalAction::Quit, "{signal:?}");
        }
    }

    /// Signal names are the conventional upper-case ones.
    #[test]
    fn signal_names() {
        assert_eq!(Signal::Interrupt.name(), "SIGINT");
        assert_eq!(Signal::Terminate.name(), "SIGTERM");
        assert_eq!(Signal::Hangup.name(), "SIGHUP");
    }

    /// The first quit is recorded; a second one asks for an immediate exit.
    #[test]
    fn second_quit_forces_exit() {
        let controller = Controller::new();
        assert_eq!(controller.handle(SignalAction::Quit), Handled::Recorded);
        assert!(controller.state().quit);
        assert_eq!(controller.handle(SignalAction::Quit), Handled::ForceExit);
    }

    /// A quit requested before anyone waits is still seen by a later waiter.
    #[tokio::test]
    async fn quit_requested_sees_an_earlier_quit() {
        let controller = Controller::new();
        controller.request_quit();
        tokio::time::timeout(Duration::from_secs(1), controller.quit_requested())
            .await
            .expect("an already-requested quit resolves at once");
    }

    /// A stop is a level: signals made before and after it both see it, and waiting on
    /// it after the fact returns immediately.
    #[tokio::test]
    async fn stop_signal_is_level_triggered() {
        let source = StopSource::new();
        let early = source.signal();
        assert!(!early.is_stopped());
        source.stop();
        let late = source.signal();
        assert!(early.is_stopped() && late.is_stopped());
        tokio::time::timeout(Duration::from_secs(1), early.stopped())
            .await
            .expect("stopped() resolves for a stopped source");
    }

    /// A waiter blocked in `stopped()` wakes up when the source stops.
    #[tokio::test]
    async fn stop_wakes_a_waiter() {
        let source = StopSource::new();
        let signal = source.signal();
        let waiter = tokio::spawn(async move { signal.stopped().await });
        tokio::time::sleep(Duration::from_millis(20)).await;
        source.stop();
        tokio::time::timeout(Duration::from_secs(1), waiter)
            .await
            .expect("the waiter wakes up")
            .unwrap();
    }

    /// Dropping the source counts as stopping: no task may outlive its controller.
    #[tokio::test]
    async fn dropped_source_counts_as_stopped() {
        let source = StopSource::new();
        let signal = source.signal();
        drop(source);
        assert!(signal.is_stopped());
        tokio::time::timeout(Duration::from_secs(1), signal.stopped())
            .await
            .expect("stopped() resolves once the source is gone");
    }

    /// The real handler turns a signal delivered to the process into a recorded request.
    /// Once installed, tokio's handler stays for the life of the test process, so the
    /// signal never falls through to its default (process-killing) action.
    #[tokio::test]
    async fn signal_handler_records_a_delivered_signal() {
        let controller = Arc::new(Controller::new());
        let _task = spawn_signal_handler(controller.clone(), Log::default()).unwrap();
        // SAFETY: raising a signal at our own process; the handler is installed above.
        unsafe {
            libc::raise(libc::SIGHUP);
        }
        tokio::time::timeout(Duration::from_secs(2), controller.quit_requested())
            .await
            .expect("the delivered SIGHUP is recorded as a quit");
    }
}
