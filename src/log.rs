//! Centralized, filterable output for the program.
//!
//! Every line the program writes goes through [`Log`]: informational and debug lines to
//! stdout, warnings and errors to stderr. Debug output is filtered per [`Subsystem`]
//! and the filter is re-checked just before each line would be printed, so every kind
//! of output is handled through the same single choke point.

use std::fmt;

/// The debug subsystems a `-d` flag can enable. Each enables its own debug output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Subsystem {
    /// Device use and behaviour: connection, capabilities, key events, image updates.
    Device,
    /// Scene lifecycle: entering/leaving scenes and applying their setup operations.
    Scene,
    /// What actions ran and what triggered them (key presses, timers, ...).
    Actions,
}

impl Subsystem {
    /// Parses one `-d` value; both `action` and `actions` select the actions subsystem.
    pub fn parse(name: &str) -> Option<Subsystem> {
        match name {
            "device" => Some(Subsystem::Device),
            "scene" => Some(Subsystem::Scene),
            "action" | "actions" => Some(Subsystem::Actions),
            _ => None,
        }
    }

    /// The short label used in debug output lines.
    pub fn label(self) -> &'static str {
        match self {
            Subsystem::Device => "device",
            Subsystem::Scene => "scene",
            Subsystem::Actions => "actions",
        }
    }
}

/// The program's output filter: which debug subsystems currently write lines.
///
/// Defaults to all debug subsystems disabled, so by default only the program banner,
/// the config location, and warnings/errors are printed. Warnings and errors are never
/// filtered; debug lines are filtered here, just before printing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Log {
    device: bool,
    scene: bool,
    actions: bool,
}

impl Log {
    /// Builds the log from the raw `-d` values; unknown values are ignored.
    pub fn from_debug_values(values: &[String]) -> Log {
        let mut log = Log::default();
        for value in values {
            match Subsystem::parse(value) {
                Some(Subsystem::Device) => log.device = true,
                Some(Subsystem::Scene) => log.scene = true,
                Some(Subsystem::Actions) => log.actions = true,
                None => {}
            }
        }
        log
    }

    /// Whether the debug output of `subsystem` is enabled.
    pub fn enabled(&self, subsystem: Subsystem) -> bool {
        match subsystem {
            Subsystem::Device => self.device,
            Subsystem::Scene => self.scene,
            Subsystem::Actions => self.actions,
        }
    }

    /// The formatted debug line for `subsystem`, or `None` when that subsystem's debug
    /// output is disabled.
    ///
    /// Callers fetch the line and only print when one is produced, which is what makes
    /// the filter apply strictly before printing.
    pub fn debug_line(&self, subsystem: Subsystem, message: impl fmt::Display) -> Option<String> {
        if self.enabled(subsystem) {
            Some(format!("debug[{}]: {message}", subsystem.label()))
        } else {
            None
        }
    }

    /// Prints a debug line when `subsystem`'s debug output is enabled.
    pub fn debug(&self, subsystem: Subsystem, message: impl fmt::Display) {
        if let Some(line) = self.debug_line(subsystem, message) {
            println!("{line}");
        }
    }

    /// Prints a line that is always shown (e.g. the program banner and config location).
    pub fn info(&self, message: impl fmt::Display) {
        println!("{message}");
    }

    /// The formatted warning line, carrying the `warning:` prefix.
    pub fn warn_line(&self, message: impl fmt::Display) -> String {
        format!("warning: {message}")
    }

    /// Prints a warning to stderr; warnings are always shown.
    pub fn warn(&self, message: impl fmt::Display) {
        eprintln!("{}", self.warn_line(message));
    }

    /// The formatted error line, carrying the `error:` prefix.
    pub fn error_line(&self, message: impl fmt::Display) -> String {
        format!("error: {message}")
    }

    /// Prints an error to stderr; errors are always shown.
    pub fn error(&self, message: impl fmt::Display) {
        eprintln!("{}", self.error_line(message));
    }
}

#[cfg(test)]
mod tests {
    use super::{Log, Subsystem};

    /// Every `-d` value maps to its subsystem, including the `actions` alias.
    #[test]
    fn subsystem_parse_accepts_known_values() {
        assert_eq!(Subsystem::parse("device"), Some(Subsystem::Device));
        assert_eq!(Subsystem::parse("scene"), Some(Subsystem::Scene));
        assert_eq!(Subsystem::parse("action"), Some(Subsystem::Actions));
        assert_eq!(Subsystem::parse("actions"), Some(Subsystem::Actions));
    }

    /// Anything else is not a subsystem; filtering must not silently enable it.
    #[test]
    fn subsystem_parse_rejects_unknown_values() {
        assert_eq!(Subsystem::parse("actions!"), None);
        assert_eq!(Subsystem::parse(""), None);
    }

    /// Subsystem labels are the lowercase names used in `-d` and debug lines.
    #[test]
    fn subsystem_labels_are_short_and_lowercase() {
        assert_eq!(Subsystem::Device.label(), "device");
        assert_eq!(Subsystem::Scene.label(), "scene");
        assert_eq!(Subsystem::Actions.label(), "actions");
    }

    /// A default log disables every debug subsystem: only info/warnings/errors print.
    #[test]
    fn default_log_disables_all_debug_subsystems() {
        let log = Log::default();
        assert!(!log.enabled(Subsystem::Device));
        assert!(!log.enabled(Subsystem::Scene));
        assert!(!log.enabled(Subsystem::Actions));
    }

    /// `-d` values enable exactly their own subsystem, and unknown values are ignored.
    #[test]
    fn from_debug_values_enables_only_listed_subsystems() {
        let log = Log::from_debug_values(&["scene".to_string(), "bogus".to_string()]);
        assert!(!log.enabled(Subsystem::Device));
        assert!(log.enabled(Subsystem::Scene));
        assert!(!log.enabled(Subsystem::Actions));
    }

    /// All three values together enable every subsystem.
    #[test]
    fn from_debug_values_enables_all_subsystems() {
        let log = Log::from_debug_values(&[
            "device".to_string(),
            "scene".to_string(),
            "actions".to_string(),
        ]);
        assert!(log.enabled(Subsystem::Device));
        assert!(log.enabled(Subsystem::Scene));
        assert!(log.enabled(Subsystem::Actions));
    }

    /// No `-d` values leave every subsystem off.
    #[test]
    fn from_debug_values_without_values_disables_everything() {
        assert_eq!(Log::from_debug_values(&[]), Log::default());
    }

    /// A debug line is produced only when its subsystem is enabled, and carries the
    /// `debug[<subsystem>]:` prefix when it is.
    #[test]
    fn debug_line_respects_subsystem_filter() {
        let log = Log::from_debug_values(&["device".to_string()]);
        assert_eq!(
            log.debug_line(Subsystem::Device, "connecting"),
            Some("debug[device]: connecting".to_string())
        );
        assert_eq!(log.debug_line(Subsystem::Scene, "hidden"), None);
        assert_eq!(log.debug_line(Subsystem::Actions, "hidden"), None);
    }

    /// Warning and error lines always carry their prefix, without any filtering.
    #[test]
    fn warn_and_error_lines_are_prefixed_and_unfiltered() {
        let log = Log::default();
        assert_eq!(log.warn_line("slow disk"), "warning: slow disk");
        assert_eq!(log.error_line("failed"), "error: failed");
    }
}
