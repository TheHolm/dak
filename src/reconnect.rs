//! Surviving the keypad disappearing and coming back (host suspend/resume, unplug,
//! USB reset).
//!
//! When the host sleeps the keypad drops off the USB bus; on resume the kernel
//! enumerates it as a brand-new device and every call on the old handle fails
//! (`ENODEV` on Linux, `ENXIO`/`EIO` on FreeBSD). [`SwappableDevice`] is the handle the
//! scene runner draws through: it forwards to the current connection, fails fast with
//! [`SwapError::Disconnected`] once the connection is known to be gone, notices a
//! disconnect from any failing call (not only from the input reader), and lets the
//! input loop swap a fresh connection in underneath the runner once the device is
//! back. [`wait_until`] is the polling loop that waits for it, paced and bounded by a
//! [`ReconnectPolicy`].

use std::future::Future;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use image::DynamicImage;
use mirajazz::error::MirajazzError;
use mirajazz::types::ImageFormat;
use tokio::sync::watch;

use crate::actions::ButtonDevice;

/// How often a lost device is looked for again, and how many times, after the
/// `defaults`/per-device `device_reconnect_*` settings are combined (see
/// [`ReconnectPolicy::resolve`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReconnectPolicy {
    /// Time between attempts (the first attempt is immediate).
    pub interval: Duration,
    /// Attempts before giving up; 0 means unlimited.
    pub max_attempts: u64,
}

impl ReconnectPolicy {
    /// The policy for one device: its own `device_reconnect_interval` (seconds) and
    /// `device_reconnect_max_attempts` when set, each falling back independently to the
    /// `defaults` value (itself already defaulted to 15 s / unlimited).
    pub fn resolve(
        defaults: &crate::press::Defaults,
        device_interval_secs: Option<u64>,
        device_max_attempts: Option<u64>,
    ) -> Self {
        Self {
            interval: device_interval_secs
                .map(Duration::from_secs)
                .unwrap_or(defaults.device_reconnect_interval),
            max_attempts: device_max_attempts.unwrap_or(defaults.device_reconnect_max_attempts),
        }
    }
}

/// How [`wait_until`] ended.
#[derive(Debug, PartialEq, Eq)]
pub enum WaitOutcome<T> {
    /// An attempt succeeded with this value.
    Found(T),
    /// `cancel` resolved first.
    Cancelled,
    /// Every one of the allowed attempts failed.
    GaveUp,
}

/// Error from a [`SwappableDevice`] call.
#[derive(Debug)]
pub enum SwapError<E> {
    /// No connection is currently attached: the device went away and has not been
    /// reconnected yet. Returned without touching any hardware.
    Disconnected,
    /// The underlying connection reported this error.
    Device(E),
}

impl<E: std::fmt::Display> std::fmt::Display for SwapError<E> {
    /// `Disconnected` reads as "device disconnected"; device errors pass through as-is.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SwapError::Disconnected => f.write_str("device disconnected"),
            SwapError::Device(error) => error.fmt(f),
        }
    }
}

impl<E: std::error::Error + 'static> std::error::Error for SwapError<E> {
    /// The underlying device error, when there is one.
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            SwapError::Disconnected => None,
            SwapError::Device(error) => Some(error),
        }
    }
}

/// A device handle whose underlying connection can be dropped and replaced while the
/// scene runner keeps borrowing the same handle.
///
/// Every [`ButtonDevice`] call clones the current connection's `Arc` (so no lock is
/// held across an `await`) and forwards to it. A call failing with an error that
/// `is_disconnect` classifies as "device gone" detaches that connection on the spot,
/// so later calls fail fast and [`SwappableDevice::disconnected`] wakes the input loop
/// even when the input reader itself has not noticed yet (or never will).
pub struct SwappableDevice<D: ButtonDevice> {
    /// The live connection, or `None` while disconnected.
    current: RwLock<Option<Arc<D>>>,
    /// Button count of the device, fixed at construction so it is still known while
    /// disconnected (the runner consults it when validating operations).
    key_count: u8,
    /// Connection state (`true` = connected), watched by
    /// [`SwappableDevice::disconnected`].
    state: watch::Sender<bool>,
    /// Classifies a device error as "the device is gone".
    is_disconnect: fn(&D::Error) -> bool,
}

impl<D: ButtonDevice> SwappableDevice<D> {
    /// Wraps an already-connected `device`; `is_disconnect` decides which of its errors
    /// mean the device has gone away (see [`is_disconnect_error`] for the real device).
    pub fn new(device: D, is_disconnect: fn(&D::Error) -> bool) -> Self {
        let key_count = device.key_count();
        Self {
            current: RwLock::new(Some(Arc::new(device))),
            key_count,
            state: watch::Sender::new(true),
            is_disconnect,
        }
    }

    /// The current connection, if any, e.g. to open an input reader on it or shut it
    /// down. Holding the returned `Arc` keeps that connection (and its OS handle) open,
    /// so callers must drop it before reconnecting.
    pub fn current(&self) -> Option<Arc<D>> {
        self.current.read().expect("device lock poisoned").clone()
    }

    /// Detaches the current connection (dropping this handle's reference to it) and
    /// wakes [`SwappableDevice::disconnected`] waiters. Does nothing when already
    /// disconnected.
    pub fn mark_disconnected(&self) {
        let old = self.current.write().expect("device lock poisoned").take();
        if old.is_some() {
            self.state.send_replace(false);
        }
    }

    /// Attaches `device` as the new live connection, replacing any previous one.
    pub fn replace(&self, device: D) {
        *self.current.write().expect("device lock poisoned") = Some(Arc::new(device));
        self.state.send_replace(true);
    }

    /// Resolves once the device is (or already was) disconnected.
    pub async fn disconnected(&self) {
        let mut state = self.state.subscribe();
        // The sender lives in `self`, which outlives this borrow, so the watch can
        // never close under us; a closed watch is treated like a disconnect anyway.
        let _ = state.wait_for(|connected| !*connected).await;
    }

    /// Runs `call` against the current connection, mapping "no connection" to
    /// [`SwapError::Disconnected`] and detaching the connection when the call fails with
    /// a disconnect error - but only if that connection is still the current one, so a
    /// straggling call on an old connection can never detach its replacement.
    async fn forward<T, F, Fut>(&self, call: F) -> Result<T, SwapError<D::Error>>
    where
        F: FnOnce(Arc<D>) -> Fut,
        Fut: Future<Output = Result<T, D::Error>>,
    {
        let Some(device) = self.current() else {
            return Err(SwapError::Disconnected);
        };
        match call(device.clone()).await {
            Ok(value) => Ok(value),
            Err(error) => {
                if (self.is_disconnect)(&error) {
                    let mut current = self.current.write().expect("device lock poisoned");
                    if current
                        .as_ref()
                        .is_some_and(|live| Arc::ptr_eq(live, &device))
                    {
                        *current = None;
                        drop(current);
                        self.state.send_replace(false);
                    }
                }
                Err(SwapError::Device(error))
            }
        }
    }
}

impl<D: ButtonDevice> ButtonDevice for SwappableDevice<D> {
    type Error = SwapError<D::Error>;

    async fn set_button_image(
        &self,
        key: u8,
        image_format: ImageFormat,
        image: DynamicImage,
    ) -> Result<(), Self::Error> {
        self.forward(
            |device| async move { device.set_button_image(key, image_format, image).await },
        )
        .await
    }

    async fn clear_button_image(&self, key: u8) -> Result<(), Self::Error> {
        self.forward(|device| async move { device.clear_button_image(key).await })
            .await
    }

    async fn flush(&self) -> Result<(), Self::Error> {
        self.forward(|device| async move { device.flush().await })
            .await
    }

    fn key_count(&self) -> u8 {
        self.key_count
    }

    async fn set_brightness(&self, percent: u8) -> Result<(), Self::Error> {
        self.forward(|device| async move { device.set_brightness(percent).await })
            .await
    }

    async fn set_led_brightness(&self, percent: u8) -> Result<(), Self::Error> {
        self.forward(|device| async move { device.set_led_brightness(percent).await })
            .await
    }

    fn is_connected(&self) -> bool {
        *self.state.borrow()
    }
}

/// Whether a mirajazz error means the device itself has gone away (unplugged, or
/// dropped off the bus by a host suspend), as opposed to some other failure.
///
/// Recognizes async-hid's own `Disconnected`/`NotConnected`, and a backend OS error of
/// `ENODEV` (Linux), `ENXIO` or `EIO` (FreeBSD's `hidraw(4)` after detach), or a timed
/// out write (`ETIMEDOUT`: seen on FreeBSD for a write in flight while the device is
/// being unplugged). Treating a timeout as a disconnect is deliberately generous: if a
/// device that is still present ever times out, the cost is one quick reopen and a
/// full repaint, which is also the best recovery for a device that stopped answering.
/// The FreeBSD backend wraps its errors as `nix` errno values rather than
/// `std::io::Error`, so those are matched on their errno display name instead of a
/// downcast - which keeps this crate free of a direct `nix` dependency.
pub fn is_disconnect_error(error: &MirajazzError) -> bool {
    use async_hid::HidError;
    const GONE: [i32; 3] = [libc_errno::ENODEV, libc_errno::ENXIO, libc_errno::EIO];
    let MirajazzError::HidError(error) = error else {
        return false;
    };
    match error {
        HidError::Disconnected | HidError::NotConnected => true,
        HidError::Other(inner) => {
            if let Some(io) = inner.downcast_ref::<std::io::Error>() {
                return io.kind() == std::io::ErrorKind::TimedOut
                    || io.raw_os_error().is_some_and(|code| GONE.contains(&code));
            }
            let text = inner.to_string();
            ["ENODEV", "ENXIO", "EIO", "ETIMEDOUT"].iter().any(|name| {
                text.split(|c: char| !c.is_ascii_alphanumeric())
                    .any(|word| word == *name)
            })
        }
        _ => false,
    }
}

/// errno values for the "device is gone" errors, identical on Linux and FreeBSD
/// except `ENODEV`/`ENXIO`, which also match there (19 and 6 on both).
mod libc_errno {
    /// `EIO`: I/O error.
    pub const EIO: i32 = 5;
    /// `ENXIO`: device not configured / no such device or address.
    pub const ENXIO: i32 = 6;
    /// `ENODEV`: no such device (operation not supported by device on FreeBSD).
    pub const ENODEV: i32 = 19;
}

/// The always-shown warning printed once when device `device_number` goes away;
/// `reason` says how it was noticed (typically the failing call's error).
pub fn disconnected_message(device_number: u8, reason: &str) -> String {
    format!("device #{device_number} disconnected ({reason}); waiting for it to come back")
}

/// The always-shown line printed when device `device_number` is connected again.
pub fn reconnected_message(device_number: u8, name: &str, serial: &str) -> String {
    format!("device #{device_number} reconnected ({name} s/n {serial})")
}

/// Calls `attempt` right away and then every `policy.interval` until it yields a value
/// ([`WaitOutcome::Found`]), `cancel` resolves ([`WaitOutcome::Cancelled`]), or
/// `policy.max_attempts` attempts (when nonzero) have all failed
/// ([`WaitOutcome::GaveUp`], returned right after the last one, without a final wait).
///
/// This is the "wait for the device to come back" loop: `attempt` receives the 1-based
/// attempt number, rediscovers and reconnects, returning `None` while the device is
/// still absent (or not yet ready to be opened); `cancel` is Ctrl-C, so the program can
/// still be stopped while it waits. `cancel` is also checked during an attempt, not
/// only between attempts.
pub async fn wait_until<T, A, Fut, C>(
    policy: ReconnectPolicy,
    mut attempt: A,
    cancel: C,
) -> WaitOutcome<T>
where
    A: FnMut(u64) -> Fut,
    Fut: Future<Output = Option<T>>,
    C: Future,
{
    tokio::pin!(cancel);
    let mut number = 0u64;
    loop {
        number += 1;
        tokio::select! {
            found = attempt(number) => {
                if let Some(value) = found {
                    return WaitOutcome::Found(value);
                }
            }
            _ = &mut cancel => return WaitOutcome::Cancelled,
        }
        if policy.max_attempts != 0 && number >= policy.max_attempts {
            return WaitOutcome::GaveUp;
        }
        let interval = policy.interval;
        tokio::select! {
            _ = tokio::time::sleep(interval) => {}
            _ = &mut cancel => return WaitOutcome::Cancelled,
        }
    }
}

/// The always-shown error printed when device `device_number` did not come back within
/// its `attempts` allowed reconnect attempts.
pub fn gave_up_message(device_number: u8, attempts: u64) -> String {
    format!("device #{device_number} did not come back after {attempts} attempts; giving up on it")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    /// A test error: `gone` marks it as a disconnect for [`is_gone`].
    #[derive(Debug)]
    struct FakeError {
        gone: bool,
    }

    impl std::fmt::Display for FakeError {
        /// Fixed text, distinguishing the two kinds.
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str(if self.gone { "gone" } else { "other" })
        }
    }

    impl std::error::Error for FakeError {}

    /// Classifier for [`FakeError`].
    fn is_gone(error: &FakeError) -> bool {
        error.gone
    }

    /// A device recording every call under its own `name`, optionally failing all of
    /// them with the configured error kind.
    struct FakeDevice {
        name: &'static str,
        calls: Arc<Mutex<Vec<String>>>,
        fail: Option<bool>,
    }

    impl FakeDevice {
        /// A device that succeeds and records into `calls`.
        fn ok(name: &'static str, calls: &Arc<Mutex<Vec<String>>>) -> Self {
            Self {
                name,
                calls: calls.clone(),
                fail: None,
            }
        }

        /// Records `what` and returns the configured result.
        fn record(&self, what: &str) -> Result<(), FakeError> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("{}:{what}", self.name));
            match self.fail {
                None => Ok(()),
                Some(gone) => Err(FakeError { gone }),
            }
        }
    }

    impl ButtonDevice for FakeDevice {
        type Error = FakeError;

        async fn set_button_image(
            &self,
            key: u8,
            _image_format: ImageFormat,
            _image: DynamicImage,
        ) -> Result<(), Self::Error> {
            self.record(&format!("image{key}"))
        }

        async fn clear_button_image(&self, key: u8) -> Result<(), Self::Error> {
            self.record(&format!("clear{key}"))
        }

        async fn flush(&self) -> Result<(), Self::Error> {
            self.record("flush")
        }

        fn key_count(&self) -> u8 {
            6
        }

        async fn set_brightness(&self, percent: u8) -> Result<(), Self::Error> {
            self.record(&format!("brightness{percent}"))
        }

        async fn set_led_brightness(&self, percent: u8) -> Result<(), Self::Error> {
            self.record(&format!("led{percent}"))
        }
    }

    /// Calls go to the wrapped device while connected.
    #[tokio::test]
    async fn forwards_while_connected() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let device = SwappableDevice::new(FakeDevice::ok("a", &calls), is_gone);
        device.clear_button_image(2).await.unwrap();
        device.flush().await.unwrap();
        device.set_brightness(40).await.unwrap();
        device.set_led_brightness(30).await.unwrap();
        assert!(device.is_connected());
        assert_eq!(device.key_count(), 6);
        assert_eq!(
            *calls.lock().unwrap(),
            vec!["a:clear2", "a:flush", "a:brightness40", "a:led30"]
        );
    }

    /// Once marked disconnected, calls fail with `Disconnected` without reaching any
    /// device, the key count is still known, and `disconnected()` resolves.
    #[tokio::test]
    async fn fails_fast_while_disconnected() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let device = SwappableDevice::new(FakeDevice::ok("a", &calls), is_gone);
        device.mark_disconnected();
        assert!(matches!(device.flush().await, Err(SwapError::Disconnected)));
        assert!(!device.is_connected());
        assert!(device.current().is_none());
        assert_eq!(device.key_count(), 6);
        tokio::time::timeout(Duration::from_secs(1), device.disconnected())
            .await
            .expect("disconnected() must resolve");
        assert!(calls.lock().unwrap().is_empty());
    }

    /// After `replace`, calls reach the new device and the state is connected again.
    #[tokio::test]
    async fn replace_forwards_to_the_new_device() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let device = SwappableDevice::new(FakeDevice::ok("a", &calls), is_gone);
        device.mark_disconnected();
        device.replace(FakeDevice::ok("b", &calls));
        assert!(device.is_connected());
        device.flush().await.unwrap();
        assert_eq!(*calls.lock().unwrap(), vec!["b:flush"]);
    }

    /// A call failing with a disconnect error detaches the connection by itself, so
    /// `disconnected()` wakes even though nobody called `mark_disconnected`.
    #[tokio::test]
    async fn disconnect_error_detaches_the_connection() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let failing = FakeDevice {
            fail: Some(true),
            ..FakeDevice::ok("a", &calls)
        };
        let device = SwappableDevice::new(failing, is_gone);
        let error = device.flush().await.unwrap_err();
        assert!(matches!(error, SwapError::Device(FakeError { gone: true })));
        assert!(!device.is_connected());
        tokio::time::timeout(Duration::from_secs(1), device.disconnected())
            .await
            .expect("disconnected() must resolve");
    }

    /// Any other error is passed through and leaves the connection attached.
    #[tokio::test]
    async fn other_errors_keep_the_connection() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let failing = FakeDevice {
            fail: Some(false),
            ..FakeDevice::ok("a", &calls)
        };
        let device = SwappableDevice::new(failing, is_gone);
        assert!(device.flush().await.is_err());
        assert!(device.is_connected());
        assert!(device.current().is_some());
    }

    /// A disconnect error from a call still running on an *old* connection must not
    /// detach the replacement that was swapped in meanwhile.
    #[tokio::test]
    async fn stale_failure_does_not_detach_the_replacement() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let device = SwappableDevice::new(FakeDevice::ok("a", &calls), is_gone);
        let result = device
            .forward(|_old| async {
                device.replace(FakeDevice::ok("b", &calls));
                Err::<(), _>(FakeError { gone: true })
            })
            .await;
        assert!(result.is_err());
        assert!(device.is_connected());
        device.flush().await.unwrap();
        assert_eq!(*calls.lock().unwrap(), vec!["b:flush"]);
    }

    /// `Disconnected` has its own message; device errors display as themselves.
    #[test]
    fn swap_error_display() {
        assert_eq!(
            SwapError::<FakeError>::Disconnected.to_string(),
            "device disconnected"
        );
        assert_eq!(
            SwapError::Device(FakeError { gone: false }).to_string(),
            "other"
        );
    }

    /// The exact error from the bug report (Linux `ENODEV`), async-hid's own
    /// disconnect variants, FreeBSD-style errno text and `EIO`/`ENXIO` OS errors are all
    /// disconnects; unrelated errors are not.
    #[test]
    fn classifies_disconnect_errors() {
        use async_hid::HidError;
        let os = |code| {
            MirajazzError::HidError(HidError::Other(Box::new(
                std::io::Error::from_raw_os_error(code),
            )))
        };
        let text = |message: &'static str| MirajazzError::HidError(HidError::Other(message.into()));
        assert!(is_disconnect_error(&os(19)));
        assert!(is_disconnect_error(&os(6)));
        assert!(is_disconnect_error(&os(5)));
        assert!(is_disconnect_error(&MirajazzError::HidError(
            HidError::Disconnected
        )));
        assert!(is_disconnect_error(&MirajazzError::HidError(
            HidError::NotConnected
        )));
        assert!(is_disconnect_error(&text("ENXIO: Device not configured")));
        // The FreeBSD unplug case: `HidError(Other(ETIMEDOUT))` from a flush.
        assert!(is_disconnect_error(&text("ETIMEDOUT")));
        assert!(is_disconnect_error(&MirajazzError::HidError(
            HidError::Other(Box::new(std::io::Error::from(std::io::ErrorKind::TimedOut)))
        )));

        assert!(!is_disconnect_error(&os(13)));
        assert!(!is_disconnect_error(&text("EIOX is not a real errno")));
        assert!(!is_disconnect_error(&MirajazzError::BadData));
        assert!(!is_disconnect_error(&MirajazzError::HidError(
            HidError::message("something else")
        )));
    }

    /// The connect/disconnect lines name the device and say what happens next.
    #[test]
    fn connection_messages() {
        assert_eq!(
            disconnected_message(1, "No such device"),
            "device #1 disconnected (No such device); waiting for it to come back"
        );
        assert_eq!(
            reconnected_message(2, "AKP03E", "ABC123"),
            "device #2 reconnected (AKP03E s/n ABC123)"
        );
        assert_eq!(
            gave_up_message(1, 5),
            "device #1 did not come back after 5 attempts; giving up on it"
        );
    }

    /// `wait_until` keeps polling at the interval until an attempt succeeds.
    #[tokio::test]
    async fn wait_until_retries_until_found() {
        let attempts = AtomicUsize::new(0);
        let found = wait_until(
            policy(0),
            |number| {
                attempts.fetch_add(1, Ordering::SeqCst);
                async move { (number == 4).then_some("device") }
            },
            std::future::pending::<()>(),
        )
        .await;
        assert_eq!(found, WaitOutcome::Found("device"));
        assert_eq!(attempts.load(Ordering::SeqCst), 4);
    }

    /// A short-interval policy with the given attempt limit, for fast tests.
    fn policy(max_attempts: u64) -> ReconnectPolicy {
        ReconnectPolicy {
            interval: Duration::from_millis(5),
            max_attempts,
        }
    }

    /// With a limit, `wait_until` makes exactly that many attempts (numbered from 1)
    /// and then gives up, without waiting another interval.
    #[tokio::test]
    async fn wait_until_gives_up_after_max_attempts() {
        let seen = Mutex::new(Vec::new());
        let outcome = wait_until(
            policy(3),
            |number| {
                seen.lock().unwrap().push(number);
                async { None::<()> }
            },
            std::future::pending::<()>(),
        )
        .await;
        assert_eq!(outcome, WaitOutcome::GaveUp);
        assert_eq!(*seen.lock().unwrap(), vec![1, 2, 3]);
    }

    /// Success on the very last allowed attempt still counts as found.
    #[tokio::test]
    async fn wait_until_found_on_last_allowed_attempt() {
        let outcome = wait_until(
            policy(2),
            |number| async move { (number == 2).then_some(()) },
            std::future::pending::<()>(),
        )
        .await;
        assert_eq!(outcome, WaitOutcome::Found(()));
    }

    /// Per-device settings override `defaults` independently; unset ones fall back.
    #[test]
    fn policy_resolves_device_then_defaults() {
        let defaults = crate::press::Defaults {
            device_reconnect_interval: Duration::from_secs(7),
            device_reconnect_max_attempts: 4,
            ..crate::press::Defaults::default()
        };
        assert_eq!(
            ReconnectPolicy::resolve(&defaults, None, None),
            ReconnectPolicy {
                interval: Duration::from_secs(7),
                max_attempts: 4
            }
        );
        assert_eq!(
            ReconnectPolicy::resolve(&defaults, Some(1), None),
            ReconnectPolicy {
                interval: Duration::from_secs(1),
                max_attempts: 4
            }
        );
        assert_eq!(
            ReconnectPolicy::resolve(&defaults, None, Some(0)),
            ReconnectPolicy {
                interval: Duration::from_secs(7),
                max_attempts: 0
            }
        );
        let builtin = ReconnectPolicy::resolve(&crate::press::Defaults::default(), None, None);
        assert_eq!(builtin.interval, Duration::from_secs(15));
        assert_eq!(builtin.max_attempts, 0);
    }

    /// `wait_until` gives up with `None` as soon as `cancel` resolves.
    #[tokio::test]
    async fn wait_until_stops_when_cancelled() {
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        let attempts = AtomicUsize::new(0);
        let waiter = wait_until(
            policy(0),
            |_| {
                attempts.fetch_add(1, Ordering::SeqCst);
                async { None::<()> }
            },
            rx,
        );
        let cancel = async {
            tokio::time::sleep(Duration::from_millis(30)).await;
            let _ = tx.send(());
        };
        let (found, ()) = tokio::join!(waiter, cancel);
        assert_eq!(found, WaitOutcome::Cancelled);
        // Timing-dependent count: it polled more than once, then stopped for good.
        let polled = attempts.load(Ordering::SeqCst);
        assert!(polled >= 2, "{polled}");
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert_eq!(attempts.load(Ordering::SeqCst), polled);
    }
}
