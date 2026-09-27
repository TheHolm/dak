//! Centralized, filterable output for the program.
//!
//! Every line the program writes goes through [`Log`], which filters it by [`Level`]
//! (and debug lines additionally per [`Subsystem`]) before handing it to the
//! process-wide set of outputs ([`Sinks`]): the console, the systemd journal (stderr
//! with `<N>` priority prefixes), syslog and/or a log file. Until [`install`] is called
//! (and in tests) lines go to the console exactly as before: informational and debug
//! lines to stdout, warnings and errors to stderr.
//!
//! The outputs come from the config's `logging` section ([`check_logging`] validates it
//! into a [`LoggingConfig`]) merged with the command-line overrides ([`CliLogging`]) by
//! [`LogSettings::resolve`]. `auto` picks the conventional destination for how the
//! program was started: the journal under systemd, syslog once detached, the console
//! otherwise.

use std::fmt;
use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};

use serde_json::Value;

/// The debug subsystems a `-d` flag can enable. Each enables its own debug output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Subsystem {
    /// Device use and behaviour: connection, capabilities, key events, image updates.
    Device,
    /// Scene lifecycle: entering/leaving scenes and applying their setup operations.
    Scene,
    /// What actions ran and what triggered them (key presses, timers, ...).
    Actions,
    /// The fonts in use: the lookup order and, for each configured `defaults.fonts`
    /// file, which characters it cannot draw and why. Silent without configured fonts.
    Fonts,
}

impl Subsystem {
    /// Parses one `-d` value; both `action` and `actions` select the actions subsystem.
    pub fn parse(name: &str) -> Option<Subsystem> {
        match name {
            "device" => Some(Subsystem::Device),
            "scene" => Some(Subsystem::Scene),
            "action" | "actions" => Some(Subsystem::Actions),
            "fonts" | "font" => Some(Subsystem::Fonts),
            _ => None,
        }
    }

    /// The short label used in debug output lines.
    pub fn label(self) -> &'static str {
        match self {
            Subsystem::Device => "device",
            Subsystem::Scene => "scene",
            Subsystem::Actions => "actions",
            Subsystem::Fonts => "fonts",
        }
    }
}

/// How important a line is. Ordered from most to least important, so a configured
/// level lets through every line whose level is `<=` it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub enum Level {
    /// Something failed. Always written, whatever the configured level.
    Error,
    /// Something is wrong but the program carries on.
    Warning,
    /// Normal operation: the banner, connects and disconnects.
    #[default]
    Info,
    /// Per-subsystem detail, only for the subsystems enabled with `-d`/`logging.debug`.
    Debug,
}

/// The `logging.level` (and `--log-level`) values, most to least important.
pub const LOG_LEVELS: &[&str] = &["error", "warning", "info", "debug"];

impl Level {
    /// Parses a `logging.level`/`--log-level` value.
    pub fn parse(name: &str) -> Option<Level> {
        match name {
            "error" => Some(Level::Error),
            "warning" => Some(Level::Warning),
            "info" => Some(Level::Info),
            "debug" => Some(Level::Debug),
            _ => None,
        }
    }

    /// The name used in config and on the command line.
    pub fn name(self) -> &'static str {
        match self {
            Level::Error => "error",
            Level::Warning => "warning",
            Level::Info => "info",
            Level::Debug => "debug",
        }
    }

    /// The matching syslog(3) priority (also the number in a journal `<N>` prefix).
    pub fn syslog_priority(self) -> i32 {
        match self {
            Level::Error => libc::LOG_ERR,
            Level::Warning => libc::LOG_WARNING,
            Level::Info => libc::LOG_INFO,
            Level::Debug => libc::LOG_DEBUG,
        }
    }
}

/// The program's output filter: the most detailed [`Level`] written, and which debug
/// subsystems write lines.
///
/// Defaults to [`Level::Info`] with every debug subsystem disabled, so by default only
/// the program banner, the config location, and warnings/errors are printed. Errors are
/// never filtered; everything else is filtered here, just before it would be written.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Log {
    level: Level,
    device: bool,
    scene: bool,
    actions: bool,
    fonts: bool,
}

impl Log {
    /// Builds the log from the raw `-d` values; unknown values are ignored. Enabling any
    /// subsystem also lowers the level to [`Level::Debug`], so its lines are written.
    pub fn from_debug_values(values: &[String]) -> Log {
        let subsystems: Vec<Subsystem> =
            values.iter().filter_map(|v| Subsystem::parse(v)).collect();
        let level = if subsystems.is_empty() {
            Level::Info
        } else {
            Level::Debug
        };
        Log::new(level, &subsystems)
    }

    /// A log writing lines up to `level`, with debug lines from `subsystems` only.
    pub fn new(level: Level, subsystems: &[Subsystem]) -> Log {
        let mut log = Log {
            level,
            ..Log::default()
        };
        for subsystem in subsystems {
            match subsystem {
                Subsystem::Device => log.device = true,
                Subsystem::Scene => log.scene = true,
                Subsystem::Actions => log.actions = true,
                Subsystem::Fonts => log.fonts = true,
            }
        }
        log
    }

    /// The most detailed level this log writes.
    pub fn level(&self) -> Level {
        self.level
    }

    /// Whether lines of `level` are written.
    pub fn writes(&self, level: Level) -> bool {
        level == Level::Error || level <= self.level
    }

    /// Whether the debug output of `subsystem` is enabled (and the level lets debug
    /// lines through at all).
    pub fn enabled(&self, subsystem: Subsystem) -> bool {
        self.writes(Level::Debug)
            && match subsystem {
                Subsystem::Device => self.device,
                Subsystem::Scene => self.scene,
                Subsystem::Actions => self.actions,
                Subsystem::Fonts => self.fonts,
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

    /// Writes a debug line when `subsystem`'s debug output is enabled.
    pub fn debug(&self, subsystem: Subsystem, message: impl fmt::Display) {
        if let Some(line) = self.debug_line(subsystem, message) {
            emit(Level::Debug, &line);
        }
    }

    /// Writes an informational line (e.g. the program banner and config location),
    /// unless the level is below [`Level::Info`].
    pub fn info(&self, message: impl fmt::Display) {
        if self.writes(Level::Info) {
            emit(Level::Info, &message.to_string());
        }
    }

    /// The formatted warning line, carrying the `warning:` prefix.
    pub fn warn_line(&self, message: impl fmt::Display) -> String {
        format!("warning: {message}")
    }

    /// Writes a warning, unless the level is [`Level::Error`].
    pub fn warn(&self, message: impl fmt::Display) {
        if self.writes(Level::Warning) {
            emit(Level::Warning, &self.warn_line(message));
        }
    }

    /// The formatted error line, carrying the `error:` prefix.
    pub fn error_line(&self, message: impl fmt::Display) -> String {
        format!("error: {message}")
    }

    /// Writes an error; errors are always written.
    pub fn error(&self, message: impl fmt::Display) {
        emit(Level::Error, &self.error_line(message));
    }
}

/// The keys the optional top-level `logging` object may contain. Part of the vocabulary
/// `tests/man_pages.rs` requires `dak-config.5` to document.
pub const LOGGING_KEYS: &[&str] = &[
    "output",
    "file",
    "syslog_facility",
    "level",
    "debug",
    "timestamps",
];

/// The `logging.output` values.
pub const LOG_OUTPUTS: &[&str] = &["auto", "console", "journal", "syslog", "file"];

/// The `logging.syslog_facility` values.
pub const SYSLOG_FACILITIES: &[&str] = &[
    "user", "daemon", "local0", "local1", "local2", "local3", "local4", "local5", "local6",
    "local7",
];

/// The `logging.timestamps` values (besides the JSON booleans `true`/`false`).
pub const TIMESTAMP_VALUES: &[&str] = &["auto", "true", "false"];

/// One configured output destination.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Output {
    /// Pick one of the others from how the program was started (see [`Output::resolve`]).
    Auto,
    /// Info/debug to stdout, warnings/errors to stderr.
    Console,
    /// Everything to stderr with a `<N>` syslog priority prefix, which journald turns into
    /// the entry's priority.
    Journal,
    /// syslog(3).
    Syslog,
    /// Appended to `logging.file`.
    File,
}

impl Output {
    /// Parses one `logging.output` value.
    pub fn parse(name: &str) -> Option<Output> {
        match name {
            "auto" => Some(Output::Auto),
            "console" => Some(Output::Console),
            "journal" => Some(Output::Journal),
            "syslog" => Some(Output::Syslog),
            "file" => Some(Output::File),
            _ => None,
        }
    }

    /// What `auto` stands for in `env`: the journal when stderr is connected to it
    /// (systemd sets `JOURNAL_STREAM`), syslog once detached from the terminal, and the
    /// console otherwise. Every other output stands for itself.
    pub fn resolve(self, env: &Environment) -> Output {
        match self {
            Output::Auto if env.stderr_is_journal => Output::Journal,
            Output::Auto if env.detached => Output::Syslog,
            Output::Auto => Output::Console,
            other => other,
        }
    }
}

/// A syslog facility, as configured by `logging.syslog_facility`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Facility(i32);

impl Facility {
    /// `LOG_USER`, the default: dak normally runs as an ordinary user program.
    pub const USER: Facility = Facility(libc::LOG_USER);

    /// Parses one of [`SYSLOG_FACILITIES`].
    pub fn parse(name: &str) -> Option<Facility> {
        let code = match name {
            "user" => libc::LOG_USER,
            "daemon" => libc::LOG_DAEMON,
            "local0" => libc::LOG_LOCAL0,
            "local1" => libc::LOG_LOCAL1,
            "local2" => libc::LOG_LOCAL2,
            "local3" => libc::LOG_LOCAL3,
            "local4" => libc::LOG_LOCAL4,
            "local5" => libc::LOG_LOCAL5,
            "local6" => libc::LOG_LOCAL6,
            "local7" => libc::LOG_LOCAL7,
            _ => return None,
        };
        Some(Facility(code))
    }

    /// The syslog(3) facility code.
    pub fn code(self) -> i32 {
        self.0
    }
}

/// Whether lines get a timestamp.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Timestamps {
    /// Only in the log file: the journal and syslog stamp every line themselves, and a
    /// terminal user watches the lines as they arrive.
    #[default]
    Auto,
    /// On every console, journal and file line (syslog always stamps its own).
    On,
    /// Never.
    Off,
}

impl Timestamps {
    /// Whether lines written to `output` get a timestamp.
    pub fn applies_to(self, output: Output) -> bool {
        match self {
            Timestamps::Auto => output == Output::File,
            Timestamps::On => output != Output::Syslog,
            Timestamps::Off => false,
        }
    }
}

/// The validated `logging` section. Absent keys hold their defaults.
#[derive(Debug, Clone, PartialEq)]
pub struct LoggingConfig {
    /// Where lines go; never empty.
    pub outputs: Vec<Output>,
    /// The log file for the `file` output (`~` already expanded); `None` uses
    /// [`default_log_file`].
    pub file: Option<PathBuf>,
    /// The facility syslog lines are sent with.
    pub syslog_facility: Facility,
    /// The most detailed level written.
    pub level: Level,
    /// The debug subsystems enabled.
    pub debug: Vec<Subsystem>,
    /// Whether lines get timestamps.
    pub timestamps: Timestamps,
}

impl Default for LoggingConfig {
    /// `auto` output at `info`, no debug subsystems, automatic timestamps, `user`.
    fn default() -> Self {
        Self {
            outputs: vec![Output::Auto],
            file: None,
            syslog_facility: Facility::USER,
            level: Level::Info,
            debug: Vec::new(),
            timestamps: Timestamps::Auto,
        }
    }
}

/// The JSON type name of `value`, for error messages.
fn json_type(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "an array",
        Value::Object(_) => "an object",
    }
}

/// Renders names as a `"a", "b"` list for error messages.
fn quoted(names: &[&str]) -> String {
    names
        .iter()
        .map(|name| format!("\"{name}\""))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Validates the optional top-level `logging` object, pushing every problem found to
/// `errors` (and non-fatal ones to `warnings`), and returns the settings with the
/// defaults filled in. `expand` expands a leading `~` in the `file` path.
pub fn check_logging(
    value: &Value,
    expand: impl Fn(&str) -> String,
    errors: &mut Vec<String>,
    warnings: &mut Vec<String>,
) -> LoggingConfig {
    let mut result = LoggingConfig::default();
    let Some(map) = value.as_object() else {
        errors.push(format!(
            "config \"logging\" must be an object, got {}",
            json_type(value)
        ));
        return result;
    };
    for (key, value) in map {
        match key.as_str() {
            "output" => {
                let names: Vec<&Value> = match value {
                    Value::String(_) => vec![value],
                    Value::Array(items) => items.iter().collect(),
                    other => {
                        errors.push(format!(
                            "logging.output must be a string or an array of strings, got {}",
                            json_type(other)
                        ));
                        continue;
                    }
                };
                if names.is_empty() {
                    errors.push("logging.output must name at least one output".to_string());
                    continue;
                }
                let mut outputs = Vec::new();
                for name in names {
                    let Some(text) = name.as_str() else {
                        errors.push(format!(
                            "logging.output entries must be strings, got {}",
                            json_type(name)
                        ));
                        continue;
                    };
                    match Output::parse(text) {
                        Some(output) if outputs.contains(&output) => {
                            errors.push(format!("logging.output lists \"{text}\" more than once"))
                        }
                        Some(output) => outputs.push(output),
                        None => errors.push(format!(
                            "logging.output: unknown output \"{text}\", expected one of {}",
                            quoted(LOG_OUTPUTS)
                        )),
                    }
                }
                if outputs.contains(&Output::Auto) && outputs.len() > 1 {
                    errors.push(
                        "logging.output: \"auto\" cannot be combined with other outputs"
                            .to_string(),
                    );
                }
                if !outputs.is_empty() {
                    result.outputs = outputs;
                }
            }
            "file" => match value.as_str() {
                Some("") => errors.push("logging.file must not be empty".to_string()),
                Some(path) => result.file = Some(PathBuf::from(expand(path))),
                None => errors.push(format!(
                    "logging.file must be a path string, got {}",
                    json_type(value)
                )),
            },
            "syslog_facility" => match value.as_str().map(|name| (name, Facility::parse(name))) {
                Some((_, Some(facility))) => result.syslog_facility = facility,
                Some((name, None)) => errors.push(format!(
                    "logging.syslog_facility: unknown facility \"{name}\", expected one of {}",
                    quoted(SYSLOG_FACILITIES)
                )),
                None => errors.push(format!(
                    "logging.syslog_facility must be a string, got {}",
                    json_type(value)
                )),
            },
            "level" => match value.as_str().map(|name| (name, Level::parse(name))) {
                Some((_, Some(level))) => result.level = level,
                Some((name, None)) => errors.push(format!(
                    "logging.level: unknown level \"{name}\", expected one of {}",
                    quoted(LOG_LEVELS)
                )),
                None => errors.push(format!(
                    "logging.level must be a string, got {}",
                    json_type(value)
                )),
            },
            "debug" => {
                let Some(items) = value.as_array() else {
                    errors.push(format!(
                        "logging.debug must be an array of subsystem names, got {}",
                        json_type(value)
                    ));
                    continue;
                };
                for item in items {
                    match item.as_str().map(|name| (name, Subsystem::parse(name))) {
                        Some((_, Some(subsystem))) => {
                            if !result.debug.contains(&subsystem) {
                                result.debug.push(subsystem);
                            }
                        }
                        Some((name, None)) => errors.push(format!(
                            "logging.debug: unknown subsystem \"{name}\", expected one of \
                             \"device\", \"scene\", \"actions\", \"fonts\""
                        )),
                        None => errors.push(format!(
                            "logging.debug entries must be strings, got {}",
                            json_type(item)
                        )),
                    }
                }
            }
            "timestamps" => match value {
                Value::Bool(true) => result.timestamps = Timestamps::On,
                Value::Bool(false) => result.timestamps = Timestamps::Off,
                Value::String(text) => match text.as_str() {
                    "auto" => result.timestamps = Timestamps::Auto,
                    "true" => result.timestamps = Timestamps::On,
                    "false" => result.timestamps = Timestamps::Off,
                    other => errors.push(format!(
                        "logging.timestamps: unknown value \"{other}\", expected one of {}",
                        quoted(TIMESTAMP_VALUES)
                    )),
                },
                other => errors.push(format!(
                    "logging.timestamps must be \"auto\", true or false, got {}",
                    json_type(other)
                )),
            },
            _ => errors.push(format!(
                "logging: unknown key \"{key}\", expected one of {}",
                quoted(LOGGING_KEYS)
            )),
        }
    }
    if result.file.is_some() && !result.outputs.contains(&Output::File) {
        warnings.push(
            "logging.file is set but logging.output does not include \"file\"; it is not used"
                .to_string(),
        );
    }
    result
}

/// The default log file: `$XDG_STATE_HOME/dak/dak.log`, else
/// `~/.local/state/dak/dak.log`; `None` when neither variable is set.
pub fn default_log_file() -> Option<PathBuf> {
    if let Some(state) = std::env::var_os("XDG_STATE_HOME").filter(|v| !v.is_empty()) {
        return Some(PathBuf::from(state).join("dak").join("dak.log"));
    }
    std::env::var_os("HOME")
        .filter(|v| !v.is_empty())
        .map(|home| PathBuf::from(home).join(".local/state/dak/dak.log"))
}

/// The logging-related command-line overrides.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CliLogging {
    /// `--log-level`: replaces `logging.level`.
    pub level: Option<Level>,
    /// `--log-file`: adds the `file` output and replaces `logging.file`.
    pub file: Option<PathBuf>,
    /// `--syslog`: adds the `syslog` output.
    pub syslog: bool,
    /// `-d`: adds these subsystems (and lowers the level to `debug`).
    pub debug: Vec<Subsystem>,
}

impl CliLogging {
    /// Collects the overrides from the parsed command line; unknown `-d` values are
    /// ignored (as they always were), an unknown `--log-level` is refused by clap.
    pub fn from_cli(cli: &crate::cli::Cli) -> Self {
        Self {
            level: cli.log_level.as_deref().and_then(Level::parse),
            file: cli.log_file.clone(),
            syslog: cli.syslog,
            debug: cli
                .debug
                .iter()
                .filter_map(|v| Subsystem::parse(v))
                .collect(),
        }
    }
}

/// How the program was started, which decides what `auto` means.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Environment {
    /// stderr is connected to the systemd journal (`JOURNAL_STREAM` names it).
    pub stderr_is_journal: bool,
    /// The program has detached from its terminal (`--detach`).
    pub detached: bool,
}

impl Environment {
    /// Probes the running process; `detached` is passed in by the caller.
    pub fn probe(detached: bool) -> Self {
        Self {
            stderr_is_journal: stderr_is_journal(),
            detached,
        }
    }
}

/// Whether stderr is the journal stream systemd set up: `JOURNAL_STREAM` holds
/// `<device>:<inode>` of that stream, and must match what fd 2 actually is (a child
/// process that redirected stderr inherits the variable but not the stream).
pub fn stderr_is_journal() -> bool {
    let Some(value) = std::env::var_os("JOURNAL_STREAM") else {
        return false;
    };
    // SAFETY: fstat only writes into the zeroed struct we pass it.
    let mut stat: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstat(2, &mut stat) } != 0 {
        return false;
    }
    journal_stream_matches(
        &value.to_string_lossy(),
        stat.st_dev as u64,
        stat.st_ino as u64,
    )
}

/// Whether a `JOURNAL_STREAM` value (`<device>:<inode>`, both decimal) names the stream
/// with this device and inode number.
pub fn journal_stream_matches(value: &str, dev: u64, ino: u64) -> bool {
    let Some((d, i)) = value.split_once(':') else {
        return false;
    };
    d.parse::<u64>() == Ok(dev) && i.parse::<u64>() == Ok(ino)
}

/// The fully resolved logging setup: config merged with the command line, `auto`
/// resolved, and the log file path decided.
#[derive(Debug, Clone, PartialEq)]
pub struct LogSettings {
    /// The concrete outputs, without `auto`, without duplicates, never empty.
    pub outputs: Vec<Output>,
    /// The log file, when `outputs` has [`Output::File`].
    pub file: Option<PathBuf>,
    /// The facility syslog lines are sent with.
    pub syslog_facility: Facility,
    /// The filter every line passes through.
    pub log: Log,
    /// Whether lines get timestamps.
    pub timestamps: Timestamps,
}

impl LogSettings {
    /// Merges `config` with the `cli` overrides in `env`.
    ///
    /// The command line wins: `--log-level` replaces the level, `-d` adds subsystems
    /// and lowers the level to `debug`, `--log-file`/`--syslog` add their outputs next
    /// to the configured ones (with `auto` resolved first, so `--syslog` from a
    /// terminal still also prints). Fails when the file output has no path to use.
    pub fn resolve(
        config: &LoggingConfig,
        cli: &CliLogging,
        env: &Environment,
    ) -> Result<Self, String> {
        let mut outputs: Vec<Output> = Vec::new();
        let mut add = |output: Output| {
            if !outputs.contains(&output) {
                outputs.push(output);
            }
        };
        for output in &config.outputs {
            add(output.resolve(env));
        }
        if cli.file.is_some() {
            add(Output::File);
        }
        if cli.syslog {
            add(Output::Syslog);
        }

        let file = if outputs.contains(&Output::File) {
            match cli
                .file
                .clone()
                .or_else(|| config.file.clone())
                .or_else(default_log_file)
            {
                Some(file) => Some(file),
                None => {
                    return Err(
                        "logging: the file output needs logging.file (HOME and XDG_STATE_HOME \
                         are unset, so there is no default location)"
                            .to_string(),
                    )
                }
            }
        } else {
            None
        };

        let mut subsystems = config.debug.clone();
        for subsystem in &cli.debug {
            if !subsystems.contains(subsystem) {
                subsystems.push(*subsystem);
            }
        }
        let mut level = cli.level.unwrap_or(config.level);
        if !cli.debug.is_empty() && cli.level.is_none() {
            level = Level::Debug;
        }

        Ok(Self {
            outputs,
            file,
            syslog_facility: config.syslog_facility,
            log: Log::new(level, &subsystems),
            timestamps: config.timestamps,
        })
    }

    /// Opens every output (creating the log file and its directory as needed) into a
    /// [`Sinks`] ready for [`install`].
    pub fn open(&self) -> Result<Sinks, String> {
        let file = match &self.file {
            Some(path) => Some(FileSink::open(path)?),
            None => None,
        };
        Ok(Sinks {
            outputs: self.outputs.clone(),
            file,
            syslog_facility: self.syslog_facility,
            timestamps: self.timestamps,
        })
    }
}

/// An open, appended-to log file that can be reopened (after logrotate moved it).
#[derive(Debug)]
pub struct FileSink {
    /// Where the file lives.
    path: PathBuf,
    /// The open file.
    file: Mutex<File>,
}

impl FileSink {
    /// Opens `path` for appending, creating it (mode 0600) and its parent directory
    /// (mode 0700) when missing.
    pub fn open(path: &Path) -> Result<Self, String> {
        Ok(Self {
            path: path.to_path_buf(),
            file: Mutex::new(open_log_file(path)?),
        })
    }

    /// Reopens the file by name, so lines go to a fresh file after the old one was
    /// renamed away. On failure the old file is kept and the error returned.
    pub fn reopen(&self) -> Result<(), String> {
        let fresh = open_log_file(&self.path)?;
        *self.file.lock().expect("log file lock poisoned") = fresh;
        Ok(())
    }

    /// The path this sink writes to.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Appends one line (a newline is added).
    fn write_line(&self, line: &str) {
        let mut file = self.file.lock().expect("log file lock poisoned");
        let _ = writeln!(file, "{line}");
    }
}

/// Opens (creating as needed) the log file at `path` for appending.
fn open_log_file(path: &Path) -> Result<File, String> {
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(parent)
            .map_err(|error| {
                format!("cannot create log directory {}: {error}", parent.display())
            })?;
    }
    std::fs::OpenOptions::new()
        .append(true)
        .create(true)
        .mode(0o600)
        .open(path)
        .map_err(|error| format!("cannot open log file {}: {error}", path.display()))
}

/// The set of open outputs every line is written to.
#[derive(Debug)]
pub struct Sinks {
    /// The concrete outputs.
    outputs: Vec<Output>,
    /// The log file, when [`Output::File`] is used.
    file: Option<FileSink>,
    /// The facility syslog lines are sent with.
    syslog_facility: Facility,
    /// Whether lines get timestamps.
    timestamps: Timestamps,
}

impl Sinks {
    /// Plain console output, what the program uses before [`install`] is called.
    pub fn console() -> Self {
        Self {
            outputs: vec![Output::Console],
            file: None,
            syslog_facility: Facility::USER,
            timestamps: Timestamps::Auto,
        }
    }

    /// The outputs in use.
    pub fn outputs(&self) -> &[Output] {
        &self.outputs
    }

    /// Reopens the log file, if there is one (see [`FileSink::reopen`]).
    pub fn reopen(&self) -> Result<(), String> {
        match &self.file {
            Some(file) => file.reopen(),
            None => Ok(()),
        }
    }

    /// Writes one already-formatted line of `level` to every output.
    pub fn write(&self, level: Level, line: &str) {
        for output in &self.outputs {
            let stamped;
            let text = if self.timestamps.applies_to(*output) {
                stamped = format!("{} {line}", timestamp());
                stamped.as_str()
            } else {
                line
            };
            match output {
                Output::Console => {
                    if level <= Level::Warning {
                        eprintln!("{text}");
                    } else {
                        println!("{text}");
                    }
                }
                Output::Journal => eprintln!("{}", journal_line(level, text)),
                Output::Syslog => send_syslog(self.syslog_facility, level, text),
                Output::File => {
                    if let Some(file) = &self.file {
                        file.write_line(text);
                    }
                }
                Output::Auto => unreachable!("auto is resolved before sinks are opened"),
            }
        }
    }
}

/// A line prefixed for journald: every line of a multi-line message gets the `<N>`
/// priority, since journald splits on newlines and reads the prefix per line.
pub fn journal_line(level: Level, text: &str) -> String {
    let priority = level.syslog_priority();
    text.split('\n')
        .map(|line| format!("<{priority}>{line}"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// The local time as an RFC 3339 timestamp with milliseconds.
pub fn timestamp() -> String {
    chrono::Local::now()
        .format("%Y-%m-%dT%H:%M:%S%.3f%:z")
        .to_string()
}

/// Sends `text` to syslog(3) with `facility` and `level`'s priority. The first call
/// opens the connection under the name `dak` with the pid in every entry.
fn send_syslog(facility: Facility, level: Level, text: &str) {
    static OPENED: std::sync::Once = std::sync::Once::new();
    OPENED.call_once(|| {
        // SAFETY: the ident is a 'static C string, as openlog(3) requires.
        unsafe {
            libc::openlog(
                c"dak".as_ptr(),
                libc::LOG_PID | libc::LOG_NDELAY,
                libc::LOG_USER,
            )
        };
    });
    let message =
        std::ffi::CString::new(text.replace('\0', "\u{FFFD}")).expect("NUL bytes were replaced");
    // SAFETY: a "%s" format with one valid C string argument.
    unsafe {
        libc::syslog(
            facility.code() | level.syslog_priority(),
            c"%s".as_ptr(),
            message.as_ptr(),
        )
    };
}

/// The installed outputs; `None` means plain console output.
static SINKS: RwLock<Option<Arc<Sinks>>> = RwLock::new(None);

/// Makes `sinks` the outputs every later line is written to, returning the previous
/// ones (e.g. to keep a reload's failed log file from replacing a working setup).
pub fn install(sinks: Sinks) -> Option<Arc<Sinks>> {
    SINKS
        .write()
        .expect("log sinks lock poisoned")
        .replace(Arc::new(sinks))
}

/// The currently installed outputs, if any.
pub fn installed() -> Option<Arc<Sinks>> {
    SINKS.read().expect("log sinks lock poisoned").clone()
}

/// Writes one line of `level` to the installed outputs (the console when none are).
fn emit(level: Level, line: &str) {
    match installed() {
        Some(sinks) => sinks.write(level, line),
        None => Sinks::console().write(level, line),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Every `-d` value maps to its subsystem, including the `actions` alias.
    #[test]
    fn subsystem_parse_accepts_known_values() {
        assert_eq!(Subsystem::parse("device"), Some(Subsystem::Device));
        assert_eq!(Subsystem::parse("scene"), Some(Subsystem::Scene));
        assert_eq!(Subsystem::parse("action"), Some(Subsystem::Actions));
        assert_eq!(Subsystem::parse("actions"), Some(Subsystem::Actions));
        assert_eq!(Subsystem::parse("fonts"), Some(Subsystem::Fonts));
        assert_eq!(Subsystem::parse("font"), Some(Subsystem::Fonts));
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
        assert_eq!(Subsystem::Fonts.label(), "fonts");
    }

    /// A default log disables every debug subsystem: only info/warnings/errors print.
    #[test]
    fn default_log_disables_all_debug_subsystems() {
        let log = Log::default();
        assert_eq!(log.level(), Level::Info);
        assert!(!log.enabled(Subsystem::Device));
        assert!(!log.enabled(Subsystem::Scene));
        assert!(!log.enabled(Subsystem::Actions));
        assert!(!log.enabled(Subsystem::Fonts));
    }

    /// `-d fonts` enables only the fonts subsystem.
    #[test]
    fn from_debug_values_enables_fonts() {
        let log = Log::from_debug_values(&["fonts".to_string()]);
        assert!(log.enabled(Subsystem::Fonts));
        assert!(!log.enabled(Subsystem::Scene));
        assert_eq!(
            log.debug_line(Subsystem::Fonts, "x"),
            Some("debug[fonts]: x".to_string())
        );
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

    /// No `-d` values leave every subsystem off (and the level at info).
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

    /// An enabled subsystem stays silent unless the level lets debug lines through.
    #[test]
    fn debug_needs_both_level_and_subsystem() {
        let log = Log::new(Level::Info, &[Subsystem::Device]);
        assert!(!log.enabled(Subsystem::Device));
        let log = Log::new(Level::Debug, &[]);
        assert!(!log.enabled(Subsystem::Device));
        let log = Log::new(Level::Debug, &[Subsystem::Device]);
        assert!(log.enabled(Subsystem::Device));
    }

    /// Each level writes itself and everything more important; errors always pass.
    #[test]
    fn level_filter_is_ordered_and_errors_always_pass() {
        let warning = Log::new(Level::Warning, &[]);
        assert!(warning.writes(Level::Error));
        assert!(warning.writes(Level::Warning));
        assert!(!warning.writes(Level::Info));
        let error = Log::new(Level::Error, &[]);
        assert!(error.writes(Level::Error));
        assert!(!error.writes(Level::Warning));
        assert!(Log::new(Level::Debug, &[]).writes(Level::Info));
    }

    /// Level names round-trip and map to the syslog priorities.
    #[test]
    fn level_names_and_priorities() {
        for name in LOG_LEVELS {
            assert_eq!(Level::parse(name).unwrap().name(), *name);
        }
        assert_eq!(Level::parse("verbose"), None);
        assert_eq!(Level::Error.syslog_priority(), 3);
        assert_eq!(Level::Warning.syslog_priority(), 4);
        assert_eq!(Level::Info.syslog_priority(), 6);
        assert_eq!(Level::Debug.syslog_priority(), 7);
    }

    /// Warning and error lines always carry their prefix, without any filtering.
    #[test]
    fn warn_and_error_lines_are_prefixed_and_unfiltered() {
        let log = Log::default();
        assert_eq!(log.warn_line("slow disk"), "warning: slow disk");
        assert_eq!(log.error_line("failed"), "error: failed");
    }

    /// `info` prints its message and `debug` prints only when the subsystem is enabled;
    /// both go through instead of panicking on the logging side.
    #[test]
    fn info_prints_and_debug_respects_filter() {
        let log = Log::from_debug_values(&["device".to_string()]);
        log.info("warm greeting");
        log.debug(Subsystem::Device, "visible detail");
        log.debug(Subsystem::Actions, "hidden detail");
    }

    /// The journal prefix is the syslog priority, on every line of a message.
    #[test]
    fn journal_lines_carry_the_priority_on_every_line() {
        assert_eq!(journal_line(Level::Warning, "warning: x"), "<4>warning: x");
        assert_eq!(journal_line(Level::Error, "a\nb"), "<3>a\n<3>b");
        assert_eq!(journal_line(Level::Debug, "d"), "<7>d");
        assert_eq!(journal_line(Level::Info, "i"), "<6>i");
    }

    /// `JOURNAL_STREAM` must name exactly this device and inode.
    #[test]
    fn journal_stream_matching() {
        assert!(journal_stream_matches("8:12345", 8, 12345));
        assert!(!journal_stream_matches("8:12345", 8, 1));
        assert!(!journal_stream_matches("9:12345", 8, 12345));
        assert!(!journal_stream_matches("garbage", 8, 12345));
        assert!(!journal_stream_matches("", 0, 0));
    }

    /// `auto` means the journal under systemd, syslog once detached, else the console;
    /// the journal wins when both apply (a `Type=notify` unit never detaches anyway).
    #[test]
    fn auto_output_resolution() {
        let terminal = Environment::default();
        let journal = Environment {
            stderr_is_journal: true,
            detached: false,
        };
        let detached = Environment {
            stderr_is_journal: false,
            detached: true,
        };
        let both = Environment {
            stderr_is_journal: true,
            detached: true,
        };
        assert_eq!(Output::Auto.resolve(&terminal), Output::Console);
        assert_eq!(Output::Auto.resolve(&journal), Output::Journal);
        assert_eq!(Output::Auto.resolve(&detached), Output::Syslog);
        assert_eq!(Output::Auto.resolve(&both), Output::Journal);
        assert_eq!(Output::File.resolve(&journal), Output::File);
    }

    /// Automatic timestamps only stamp file lines; `true` stamps everything but syslog.
    #[test]
    fn timestamps_apply_per_output() {
        assert!(Timestamps::Auto.applies_to(Output::File));
        assert!(!Timestamps::Auto.applies_to(Output::Console));
        assert!(!Timestamps::Auto.applies_to(Output::Journal));
        assert!(Timestamps::On.applies_to(Output::Console));
        assert!(!Timestamps::On.applies_to(Output::Syslog));
        assert!(!Timestamps::Off.applies_to(Output::File));
    }

    /// Timestamps look like RFC 3339 local time with milliseconds.
    #[test]
    fn timestamp_format() {
        let stamp = timestamp();
        // 2026-09-27T10:00:00.123+10:00
        assert_eq!(stamp.len(), 29, "{stamp}");
        assert_eq!(&stamp[4..5], "-");
        assert_eq!(&stamp[10..11], "T");
        assert_eq!(&stamp[19..20], ".");
    }

    /// Facility names map to their syslog(3) codes.
    #[test]
    fn facilities_parse() {
        for name in SYSLOG_FACILITIES {
            assert!(Facility::parse(name).is_some(), "{name}");
        }
        assert_eq!(Facility::parse("daemon").unwrap().code(), libc::LOG_DAEMON);
        assert_eq!(Facility::parse("local7").unwrap().code(), libc::LOG_LOCAL7);
        assert_eq!(Facility::parse("kern"), None);
        assert_eq!(Facility::default(), Facility(0));
        assert_eq!(Facility::USER.code(), libc::LOG_USER);
    }

    /// Validates `value` as a `logging` section, returning (config, errors, warnings).
    fn check(value: Value) -> (LoggingConfig, Vec<String>, Vec<String>) {
        let mut errors = Vec::new();
        let mut warnings = Vec::new();
        let config = check_logging(
            &value,
            |path| path.replace('~', "/home/u"),
            &mut errors,
            &mut warnings,
        );
        (config, errors, warnings)
    }

    /// An empty section is the defaults.
    #[test]
    fn empty_logging_section_is_the_default() {
        let (config, errors, warnings) = check(json!({}));
        assert!(errors.is_empty() && warnings.is_empty());
        assert_eq!(config, LoggingConfig::default());
    }

    /// Every key is read, `~` in the file path is expanded, and `output` may be a
    /// single string.
    #[test]
    fn full_logging_section_is_read() {
        let (config, errors, warnings) = check(json!({
            "output": ["file", "syslog"],
            "file": "~/dak.log",
            "syslog_facility": "local3",
            "level": "warning",
            "debug": ["device", "action"],
            "timestamps": true
        }));
        assert!(errors.is_empty(), "{errors:?}");
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(config.outputs, vec![Output::File, Output::Syslog]);
        assert_eq!(config.file, Some(PathBuf::from("/home/u/dak.log")));
        assert_eq!(config.syslog_facility, Facility::parse("local3").unwrap());
        assert_eq!(config.level, Level::Warning);
        assert_eq!(config.debug, vec![Subsystem::Device, Subsystem::Actions]);
        assert_eq!(config.timestamps, Timestamps::On);

        let (config, errors, _) = check(json!({"output": "journal", "timestamps": "false"}));
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(config.outputs, vec![Output::Journal]);
        assert_eq!(config.timestamps, Timestamps::Off);
    }

    /// Bad values of every key are reported, all at once.
    #[test]
    fn bad_logging_values_are_errors() {
        let (_, errors, _) = check(json!({
            "output": ["console", "console", "printer"],
            "file": 3,
            "syslog_facility": "kern",
            "level": "loud",
            "debug": ["device", "gpu"],
            "timestamps": "sometimes",
            "colour": true
        }));
        let text = errors.join("\n");
        for expected in [
            "lists \"console\" more than once",
            "unknown output \"printer\"",
            "logging.file must be a path string",
            "unknown facility \"kern\"",
            "unknown level \"loud\"",
            "unknown subsystem \"gpu\"",
            "logging.timestamps: unknown value \"sometimes\"",
            "logging: unknown key \"colour\"",
        ] {
            assert!(text.contains(expected), "missing {expected:?} in:\n{text}");
        }
    }

    /// `auto` stands alone; an empty output list and a non-object section are refused.
    #[test]
    fn auto_combined_or_empty_outputs_are_errors() {
        let (_, errors, _) = check(json!({"output": ["auto", "file"]}));
        assert!(errors.join("\n").contains("\"auto\" cannot be combined"));
        let (_, errors, _) = check(json!({"output": []}));
        assert!(errors.join("\n").contains("at least one output"));
        let (_, errors, _) = check(json!("console"));
        assert!(errors.join("\n").contains("must be an object"));
    }

    /// A file nobody writes to is worth a warning.
    #[test]
    fn unused_log_file_is_a_warning() {
        let (_, errors, warnings) = check(json!({"file": "/tmp/x.log"}));
        assert!(errors.is_empty());
        assert!(warnings.join("\n").contains("not used"));
    }

    /// The command line wins: `--log-level` replaces the level, `-d` adds subsystems
    /// and lowers the level, `--syslog`/`--log-file` add outputs after `auto` resolved.
    #[test]
    fn cli_overrides_merge_over_the_config() {
        let config = LoggingConfig {
            level: Level::Warning,
            debug: vec![Subsystem::Scene],
            ..LoggingConfig::default()
        };
        let cli = CliLogging {
            debug: vec![Subsystem::Device],
            syslog: true,
            file: Some(PathBuf::from("/tmp/cli.log")),
            ..CliLogging::default()
        };
        let settings = LogSettings::resolve(&config, &cli, &Environment::default()).unwrap();
        assert_eq!(
            settings.outputs,
            vec![Output::Console, Output::File, Output::Syslog]
        );
        assert_eq!(settings.file, Some(PathBuf::from("/tmp/cli.log")));
        assert_eq!(settings.log.level(), Level::Debug);
        assert!(settings.log.enabled(Subsystem::Scene));
        assert!(settings.log.enabled(Subsystem::Device));

        let cli = CliLogging {
            level: Some(Level::Error),
            debug: vec![Subsystem::Device],
            ..CliLogging::default()
        };
        let settings = LogSettings::resolve(&config, &cli, &Environment::default()).unwrap();
        assert_eq!(settings.log.level(), Level::Error, "--log-level beats -d");
    }

    /// Without `-d` or `--log-level` the configured level is used as it is.
    #[test]
    fn config_level_applies_without_overrides() {
        let config = LoggingConfig {
            level: Level::Warning,
            ..LoggingConfig::default()
        };
        let settings =
            LogSettings::resolve(&config, &CliLogging::default(), &Environment::default()).unwrap();
        assert_eq!(settings.log.level(), Level::Warning);
        assert_eq!(settings.outputs, vec![Output::Console]);
        assert_eq!(settings.file, None);
    }

    /// The configured file is used for the file output; the command line's beats it.
    #[test]
    fn configured_log_file_is_used() {
        let config = LoggingConfig {
            outputs: vec![Output::File],
            file: Some(PathBuf::from("/tmp/conf.log")),
            ..LoggingConfig::default()
        };
        let settings =
            LogSettings::resolve(&config, &CliLogging::default(), &Environment::default()).unwrap();
        assert_eq!(settings.file, Some(PathBuf::from("/tmp/conf.log")));
    }

    /// A unique scratch directory for the file-output tests.
    fn scratch_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("dak_log_{name}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    /// The file output creates its directory and file, appends stamped lines, and
    /// after the file is renamed away (logrotate) a reopen writes to a fresh one.
    #[test]
    fn file_output_writes_and_reopens() {
        let dir = scratch_dir("reopen");
        let path = dir.join("sub").join("dak.log");
        let settings = LogSettings {
            outputs: vec![Output::File],
            file: Some(path.clone()),
            syslog_facility: Facility::USER,
            log: Log::default(),
            timestamps: Timestamps::Auto,
        };
        let sinks = settings.open().unwrap();
        sinks.write(Level::Warning, "warning: first");
        let rotated = dir.join("dak.log.1");
        std::fs::rename(&path, &rotated).unwrap();
        sinks.write(Level::Info, "still old file");
        sinks.reopen().unwrap();
        sinks.write(Level::Error, "error: second");

        let old = std::fs::read_to_string(&rotated).unwrap();
        let new = std::fs::read_to_string(&path).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        assert!(old.contains(" warning: first\n"), "{old}");
        assert!(old.contains(" still old file\n"), "{old}");
        assert!(new.ends_with(" error: second\n"), "{new}");
        assert!(!new.contains("first"));
        assert_eq!(
            new.as_bytes()[4],
            b'-',
            "file lines start with a timestamp: {new}"
        );
    }

    /// A log file that cannot be created is reported with its path.
    #[test]
    fn unopenable_log_file_is_an_error() {
        let error = FileSink::open(Path::new("/proc/definitely/not/here.log")).unwrap_err();
        assert!(error.contains("/proc/definitely"), "{error}");
    }

    /// The file output falls back to the XDG state directory.
    #[test]
    fn default_log_file_prefers_xdg_state_home() {
        // Only read here: the environment is not modified by this test.
        if let Some(state) = std::env::var_os("XDG_STATE_HOME").filter(|v| !v.is_empty()) {
            assert_eq!(
                default_log_file(),
                Some(PathBuf::from(state).join("dak/dak.log"))
            );
        } else if let Some(home) = std::env::var_os("HOME").filter(|v| !v.is_empty()) {
            assert_eq!(
                default_log_file(),
                Some(PathBuf::from(home).join(".local/state/dak/dak.log"))
            );
        }
    }

    /// Writing to syslog (and to the console) does not fail or panic, NUL bytes included.
    #[test]
    fn syslog_and_console_writes_do_not_panic() {
        let sinks = Sinks {
            outputs: vec![Output::Console, Output::Journal, Output::Syslog],
            file: None,
            syslog_facility: Facility::USER,
            timestamps: Timestamps::On,
        };
        sinks.write(
            Level::Debug,
            "debug[device]: dak unit test line with \0 NUL",
        );
        assert!(sinks.reopen().is_ok(), "no file: reopen is a no-op");
        assert_eq!(sinks.outputs().len(), 3);
    }
}
