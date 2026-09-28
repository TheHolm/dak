//! Limits on how fast outside input can make dak do work.
//!
//! A keypad's firmware (or a device pretending to be one) decides when buttons are
//! pressed, and can report edges at the USB polling rate - hundreds or thousands per
//! second. Each accepted edge may start a program, and a scene timer may re-run itself.
//! Without limits a misbehaving device, or a program that sets a timer variable to 0,
//! could start processes as fast as the machine allows. This module holds the brakes:
//!
//! - [`EventLimiter`]: at most [`MAX_EVENTS_PER_SECOND`] events per control per second;
//!   a release whose press was dropped is dropped too, so held-down state stays right.
//! - [`CommandSlots`]: at most [`MAX_RUNNING_COMMANDS`] action commands and `$(...)`
//!   assignments running at once, per process ([`command_slots`]).
//! - [`MIN_TIMER_SECONDS`]: the shortest scene timer.
//! - [`Throttle`]: so the warnings about all of the above cannot flood the log either.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::baseplane::Reference;

/// Most input events (press edges or encoder notches) accepted per control per second.
/// Far above what a hand can do, far below a USB polling rate.
pub const MAX_EVENTS_PER_SECOND: u32 = 50;

/// Most action commands and `$(...)` assignments running at the same time.
pub const MAX_RUNNING_COMMANDS: usize = 32;

/// The shortest scene timer, in seconds. A timer of 0 that re-enters its own scene
/// (`@`) would otherwise be a busy loop.
pub const MIN_TIMER_SECONDS: u64 = 1;

/// The length of one [`EventLimiter`] counting window.
const WINDOW: Duration = Duration::from_secs(1);

/// One control's counting state in an [`EventLimiter`].
#[derive(Debug)]
struct ControlWindow {
    /// When the current window started.
    start: Instant,
    /// Events accepted in it.
    count: u32,
    /// Whether the control's last press was dropped, so its release is dropped too.
    press_dropped: bool,
}

/// Per-control rate limit for device input (see the module docs).
#[derive(Debug, Default)]
pub struct EventLimiter {
    /// Counting state per control.
    controls: HashMap<Reference, ControlWindow>,
    /// Events dropped since the last [`EventLimiter::take_dropped`].
    dropped: u64,
}

impl EventLimiter {
    /// A limiter with nothing counted yet.
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether to act on a press (`pressed`) or release edge of `control` at `now`.
    ///
    /// Presses count against the limit. A release is let through exactly when its press
    /// was, whatever the count, so a control is never left "held down" by a dropped
    /// release, nor released without having been pressed.
    pub fn allow_edge(&mut self, control: Reference, pressed: bool, now: Instant) -> bool {
        if pressed {
            let allowed = self.count(control, now);
            self.window(control, now).press_dropped = !allowed;
            allowed
        } else {
            let window = self.window(control, now);
            let allowed = !window.press_dropped;
            window.press_dropped = false;
            if !allowed {
                self.dropped += 1;
            }
            allowed
        }
    }

    /// Whether to act on an encoder notch of `control` at `now`.
    pub fn allow_turn(&mut self, control: Reference, now: Instant) -> bool {
        self.count(control, now)
    }

    /// How many events were dropped since the last call, resetting the count.
    pub fn take_dropped(&mut self) -> u64 {
        std::mem::take(&mut self.dropped)
    }

    /// The window of `control`, restarted when the current one is over.
    fn window(&mut self, control: Reference, now: Instant) -> &mut ControlWindow {
        let window = self.controls.entry(control).or_insert(ControlWindow {
            start: now,
            count: 0,
            press_dropped: false,
        });
        if now.saturating_duration_since(window.start) >= WINDOW {
            window.start = now;
            window.count = 0;
        }
        window
    }

    /// Counts one event of `control`, returning whether it is within the limit.
    fn count(&mut self, control: Reference, now: Instant) -> bool {
        let window = self.window(control, now);
        if window.count >= MAX_EVENTS_PER_SECOND {
            self.dropped += 1;
            false
        } else {
            window.count += 1;
            true
        }
    }
}

/// A bounded pool of "a command is running" slots (see the module docs).
#[derive(Debug, Clone)]
pub struct CommandSlots {
    /// One permit per slot.
    semaphore: Arc<Semaphore>,
}

impl CommandSlots {
    /// A pool of `slots` slots.
    pub fn new(slots: usize) -> Self {
        Self {
            semaphore: Arc::new(Semaphore::new(slots)),
        }
    }

    /// A free slot, held until the returned permit is dropped, or `None` when every slot
    /// is taken (the command should then not be started).
    pub fn try_take(&self) -> Option<OwnedSemaphorePermit> {
        self.semaphore.clone().try_acquire_owned().ok()
    }

    /// How many slots are free right now.
    pub fn available(&self) -> usize {
        self.semaphore.available_permits()
    }
}

/// The process-wide pool of [`MAX_RUNNING_COMMANDS`] slots every device shares.
pub fn command_slots() -> &'static CommandSlots {
    static SLOTS: OnceLock<CommandSlots> = OnceLock::new();
    SLOTS.get_or_init(|| CommandSlots::new(MAX_RUNNING_COMMANDS))
}

/// The warning for a command that was not started because every slot was taken.
pub fn busy_message(what: &str) -> String {
    format!(
        "not running {what}: {MAX_RUNNING_COMMANDS} commands are already running (a device \
         or timer may be triggering actions faster than they finish)"
    )
}

/// Lets something through at most once per interval, so a repeated warning is logged
/// once per interval instead of once per occurrence. Safe to share between threads.
#[derive(Debug)]
pub struct Throttle {
    /// The interval.
    interval: Duration,
    /// When something was last let through.
    last: Mutex<Option<Instant>>,
}

impl Throttle {
    /// A throttle letting one call through per `interval`.
    pub const fn new(interval: Duration) -> Self {
        Self {
            interval,
            last: Mutex::new(None),
        }
    }

    /// Whether a call at `now` may go through (and, if so, starts a new interval).
    pub fn ready(&self, now: Instant) -> bool {
        let mut last = self
            .last
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        match *last {
            Some(previous) if now.saturating_duration_since(previous) < self.interval => false,
            _ => {
                *last = Some(now);
                true
            }
        }
    }
}

/// Throttles the [`busy_message`] warnings of the whole process.
pub static BUSY_WARNING: Throttle = Throttle::new(Duration::from_secs(5));

#[cfg(test)]
mod tests {
    use super::*;

    /// The control every limiter test uses.
    fn button() -> Reference {
        Reference::button(1, 1)
    }

    /// Up to the limit of presses per second get through, the rest of that second's
    /// are dropped (with their releases), and a new second starts afresh.
    #[test]
    fn edges_are_limited_per_second() {
        let mut limiter = EventLimiter::new();
        let start = Instant::now();
        for _ in 0..MAX_EVENTS_PER_SECOND {
            assert!(limiter.allow_edge(button(), true, start));
            assert!(limiter.allow_edge(button(), false, start));
        }
        assert!(!limiter.allow_edge(button(), true, start));
        assert!(!limiter.allow_edge(button(), false, start));
        assert_eq!(limiter.take_dropped(), 2);
        assert_eq!(limiter.take_dropped(), 0);
        let later = start + WINDOW;
        assert!(limiter.allow_edge(button(), true, later));
        assert!(limiter.allow_edge(button(), false, later));
    }

    /// A release always follows the fate of its press: a press accepted just before the
    /// limit is hit still gets its release, even when that arrives over the limit.
    #[test]
    fn releases_follow_their_press() {
        let mut limiter = EventLimiter::new();
        let now = Instant::now();
        for _ in 0..MAX_EVENTS_PER_SECOND {
            assert!(limiter.allow_edge(button(), true, now));
        }
        // The last press was accepted, so its release is too.
        assert!(limiter.allow_edge(button(), false, now));
        // A release with no recorded press (the first ever event) is let through.
        assert!(EventLimiter::new().allow_edge(button(), false, now));
    }

    /// Controls are limited independently, and turns count like presses.
    #[test]
    fn controls_and_turns_are_counted_separately() {
        let mut limiter = EventLimiter::new();
        let now = Instant::now();
        let encoder = Reference::encoder(1, 1);
        for _ in 0..MAX_EVENTS_PER_SECOND {
            assert!(limiter.allow_turn(encoder, now));
        }
        assert!(!limiter.allow_turn(encoder, now));
        assert!(limiter.allow_edge(button(), true, now));
        assert!(limiter.allow_turn(Reference::encoder(1, 2), now));
    }

    /// Slots run out and come back as permits are dropped.
    #[test]
    fn command_slots_are_bounded() {
        let slots = CommandSlots::new(2);
        let first = slots.try_take().unwrap();
        let _second = slots.try_take().unwrap();
        assert!(slots.try_take().is_none());
        assert_eq!(slots.available(), 0);
        drop(first);
        assert!(slots.try_take().is_some());
        // Other tests may be running commands through the shared pool meanwhile.
        assert!(command_slots().available() <= MAX_RUNNING_COMMANDS);
        assert!(busy_message("x").contains("32 commands"));
    }

    /// A throttle lets one call through per interval.
    #[test]
    fn throttle_lets_one_through_per_interval() {
        let throttle = Throttle::new(Duration::from_secs(5));
        let now = Instant::now();
        assert!(throttle.ready(now));
        assert!(!throttle.ready(now));
        assert!(!throttle.ready(now + Duration::from_secs(4)));
        assert!(throttle.ready(now + Duration::from_secs(5)));
    }
}
