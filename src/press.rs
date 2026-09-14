//! Complex press-event detection: turning a button's press/release timeline into
//! `short_press`, `long_press` and `double_click` events.
//!
//! A click is decided the moment its release is registered. A press held past
//! `short_press_duration` is a long press and fires immediately; anything shorter is a
//! short press, but it is only confirmed after `double_click_gap` passes without a
//! second press — so a double click never spuriously fires a short press for its first
//! click. A second press landing within `double_click_gap` of the previous release is a
//! double click and fires on that second release, no matter how long it is held.

use std::time::{Duration, Instant};

/// The timing knobs for complex press events, read from the config `defaults` section.
///
/// Durations are expressed in milliseconds. Values follow [`Default`] when a config
/// omits the whole `defaults` section or individual keys.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PressDefaults {
    /// A press held at most this long is a short press; longer presses are long.
    pub short_press_duration: Duration,
    /// A second press arriving within this long after the previous release is a
    /// double click.
    pub double_click_gap: Duration,
}

impl Default for PressDefaults {
    fn default() -> Self {
        Self {
            short_press_duration: Duration::from_millis(300),
            double_click_gap: Duration::from_millis(300),
        }
    }
}

/// An event a click produces, carried to the input loop by the caller's channel.
///
/// Only the short press outlives its release (it fires after `double_click_gap`, when
/// no second click has shown up); long presses and double clicks are decided and run
/// inline on release, so they never travel through the channel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClickEvent {
    /// A single short click was confirmed: run the `short_press` action.
    ShortPress,
}

/// What a button press resolves to when fed to [`ClickDetector`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PressDecision {
    /// The first (or only) press of a click.
    Fresh,
    /// A second press within the double-click gap: any pending short-press
    /// confirmation must be cancelled, and the upcoming release is a double click.
    Double,
}

/// What a button release resolves to when fed to [`ClickDetector`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReleaseDecision {
    /// The second click of a double click was released: the double click fires now.
    Double,
    /// The press ran past `short_press_duration`: the long press fires now.
    Long,
    /// A short press: firing is delayed until `double_click_gap` passes without a
    /// second press, and only then the caller runs the `short_press` action.
    Short,
}

/// The complex click detector for one physical button.
///
/// It is fed press and release edges with their timestamps and answers which complex
/// event each edge produces; the async work of actually delaying a short press is
/// owned by the caller, which consults the decisions [`press`](Self::press) and
/// [`release`](Self::release) return.
#[derive(Debug, Default)]
pub struct ClickDetector {
    short_press_duration: Duration,
    double_click_gap: Duration,
    /// When the current press started; `None` while the button is up.
    press_start: Option<Instant>,
    /// When the previous release happened; the reference point for double-click
    /// detection until the click settles as a single short press.
    last_release: Option<Instant>,
    /// True while the current press is the second click of a double click.
    double_pending: bool,
}

impl ClickDetector {
    /// Builds a detector for the timing knobs in `defaults`.
    pub fn new(defaults: PressDefaults) -> Self {
        Self {
            short_press_duration: defaults.short_press_duration,
            double_click_gap: defaults.double_click_gap,
            ..Self::default()
        }
    }

    /// Feeds a press edge occurring at `now`.
    ///
    /// Returns [`PressDecision::Double`] when the press lands within
    /// `double_click_gap` of the previous release; the caller must then cancel any
    /// pending short-press confirmation so the double click's first click does not
    /// fire a short press as well.
    pub fn press(&mut self, now: Instant) -> PressDecision {
        let is_double = self
            .last_release
            .is_some_and(|release| now.duration_since(release) <= self.double_click_gap);
        self.press_start = Some(now);
        self.double_pending = is_double;
        if is_double {
            PressDecision::Double
        } else {
            PressDecision::Fresh
        }
    }

    /// Feeds a release edge occurring at `now` for a press fed earlier.
    ///
    /// A held-too-long press resolves to [`ReleaseDecision::Long`] immediately. A
    /// double click, once pending, always resolves to [`ReleaseDecision::Double`] on
    /// this second release, regardless of how long it was held. Anything else is a
    /// [`ReleaseDecision::Short`] awaiting confirmation after `double_click_gap`.
    pub fn release(&mut self, now: Instant) -> ReleaseDecision {
        let press_start = self.press_start.take();
        if self.double_pending {
            self.double_pending = false;
            // A settled double click never starts another one.
            self.last_release = None;
            ReleaseDecision::Double
        } else if press_start
            .is_some_and(|start| now.duration_since(start) > self.short_press_duration)
        {
            self.last_release = Some(now);
            ReleaseDecision::Long
        } else {
            self.last_release = Some(now);
            ReleaseDecision::Short
        }
    }

    /// Marks a short press as settled: no second press arrived within the gap.
    ///
    /// The input loop calls this when the delayed short-press confirmation actually
    /// fires, clearing the release reference so a much later press cannot be misread
    /// as the second click of a double click.
    pub fn confirm_single(&mut self) {
        self.last_release = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The default knobs: 300 ms short/long boundary and 300 ms double-click gap.
    #[test]
    fn defaults_are_300ms_boundaries() {
        assert_eq!(defaults().short_press_duration, Duration::from_millis(300));
        assert_eq!(defaults().double_click_gap, Duration::from_millis(300));
    }

    /// Two quick clicks (each under the short threshold, second within the gap of the
    /// first release) resolve to a double click; the short press never fires for the
    /// first click.
    #[test]
    fn quick_double_click_resolves_to_double() {
        let t0 = Instant::now();
        let mut detector = ClickDetector::new(defaults());

        assert_eq!(detector.press(t0), PressDecision::Fresh);
        assert_eq!(
            detector.release(t0 + Duration::from_millis(100)),
            ReleaseDecision::Short
        );
        // Second press lands 80 ms after the first release: within the gap.
        assert_eq!(
            detector.press(t0 + Duration::from_millis(180)),
            PressDecision::Double
        );
        assert_eq!(
            detector.release(t0 + Duration::from_millis(230)),
            ReleaseDecision::Double
        );
    }

    /// A second click arriving after the gap resolves to a fresh short press instead
    /// of a double click.
    #[test]
    fn late_second_click_is_not_double() {
        let t0 = Instant::now();
        let mut detector = ClickDetector::new(defaults());

        assert_eq!(detector.press(t0), PressDecision::Fresh);
        assert_eq!(
            detector.release(t0 + Duration::from_millis(100)),
            ReleaseDecision::Short
        );
        // 350 ms between release and next press: past the 300 ms gap.
        assert_eq!(
            detector.press(t0 + Duration::from_millis(450)),
            PressDecision::Fresh
        );
        assert_eq!(
            detector.release(t0 + Duration::from_millis(500)),
            ReleaseDecision::Short
        );
    }

    /// A press held past the short threshold is a long press on release. Any release
    /// opens the double-click window: a quick second press is still a double click.
    #[test]
    fn long_press_fires_on_release() {
        let t0 = Instant::now();
        let mut detector = ClickDetector::new(defaults());

        assert_eq!(detector.press(t0), PressDecision::Fresh);
        assert_eq!(
            detector.release(t0 + Duration::from_millis(301)),
            ReleaseDecision::Long
        );
        // 49 ms after the long release: inside the 300 ms gap, so a double click.
        assert_eq!(
            detector.press(t0 + Duration::from_millis(350)),
            PressDecision::Double
        );
        assert_eq!(
            detector.release(t0 + Duration::from_millis(400)),
            ReleaseDecision::Double
        );
    }

    /// Even a very long second click is a double click once the first click's release
    /// was followed quickly by this press.
    #[test]
    fn long_second_click_still_double() {
        let t0 = Instant::now();
        let mut detector = ClickDetector::new(defaults());

        assert_eq!(detector.press(t0), PressDecision::Fresh);
        assert_eq!(
            detector.release(t0 + Duration::from_millis(80)),
            ReleaseDecision::Short
        );
        assert_eq!(
            detector.press(t0 + Duration::from_millis(130)),
            PressDecision::Double
        );
        assert_eq!(
            detector.release(t0 + Duration::from_millis(800)),
            ReleaseDecision::Double
        );
    }

    /// Confirming a single short press clears the release reference, so a later press
    /// cannot be read as the second click of a double.
    #[test]
    fn confirm_single_resets_the_double_reference() {
        let t0 = Instant::now();
        let mut detector = ClickDetector::new(defaults());

        assert_eq!(detector.press(t0), PressDecision::Fresh);
        assert_eq!(
            detector.release(t0 + Duration::from_millis(80)),
            ReleaseDecision::Short
        );
        detector.confirm_single();

        // A press well within the old gap but after confirmation is a fresh click.
        assert_eq!(
            detector.press(t0 + Duration::from_millis(150)),
            PressDecision::Fresh
        );
    }

    /// A press released at exactly the short threshold is still a short press.
    #[test]
    fn exactly_short_threshold_is_short() {
        let t0 = Instant::now();
        let mut detector = ClickDetector::new(defaults());
        assert_eq!(detector.press(t0), PressDecision::Fresh);
        assert_eq!(
            detector.release(t0 + Duration::from_millis(300)),
            ReleaseDecision::Short
        );
    }

    /// Custom knobs change the boundaries: with a 100 ms short threshold a 150 ms
    /// press is long, and a 90 ms gap rejects a 100 ms gap double click.
    #[test]
    fn custom_knobs_change_boundaries() {
        let defaults = PressDefaults {
            short_press_duration: Duration::from_millis(100),
            double_click_gap: Duration::from_millis(90),
        };
        let t0 = Instant::now();
        let mut detector = ClickDetector::new(defaults);

        assert_eq!(detector.press(t0), PressDecision::Fresh);
        assert_eq!(
            detector.release(t0 + Duration::from_millis(150)),
            ReleaseDecision::Long
        );
        assert_eq!(
            detector.press(t0 + Duration::from_millis(1000)),
            PressDecision::Fresh
        );
        assert_eq!(
            detector.release(t0 + Duration::from_millis(1100)),
            ReleaseDecision::Short
        );
        // 100 ms after that release: outside the 90 ms gap, so fresh again.
        assert_eq!(
            detector.press(t0 + Duration::from_millis(1200)),
            PressDecision::Fresh
        );
    }

    fn defaults() -> PressDefaults {
        PressDefaults::default()
    }
}
