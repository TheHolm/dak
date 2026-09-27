//! Command-line interface definition for `dak`.
//!
//! The parser lives in the library rather than the binary so the man-page generator
//! (see `tests/man_pages.rs`) can render `man/dak.1` from exactly the definition the
//! binary parses, keeping the page from drifting away from the real flags. The binary
//! still calls [`Cli::parse`] as before; see AGENTS.md for the regeneration workflow.

use clap::Parser;
use std::path::PathBuf;

/// Command-line arguments for DAK (Dynamic Ajazz Keyboard), parsed by clap.
#[derive(Debug, Parser)]
#[command(
    name = "dak",
    about = "Control an Ajazz AKP03E / AKP03R USB macro keypad from a JSON config",
    long_about = "DAK (Dynamic Ajazz Keyboard) controls an Ajazz AKP03E or AKP03R USB macro \
keypad, part of the family of \"stream controller\" keypads that Ajazz and Mirabox sell \
under different names. It connects to the device, paints the configured images and text \
labels onto the button LCDs, sets the button and encoder brightness, and reacts to button \
presses, encoder turns and scene timers described by a JSON configuration file.\n\n\
All runtime behavior is configurable: scenes swap what the buttons show and do, per-button \
bindings distinguish short presses, long presses and double clicks, encoders bind one action \
per rotation notch, and each scene may run a timer. See dak-config(5) for the configuration \
file format.\n\n\
dak is a command-line-only tool: a config file plus a binary, with no GUI. It runs until it \
is told to stop (Ctrl-C or SIGTERM) or its devices are gone, then restores the button \
images it changed and shuts the devices down."
)]
pub struct Cli {
    /// Path to the config file.
    #[arg(
        short = 'c',
        long,
        help = "Path to the config file",
        long_help = "Path to the config file. When omitted, `config.json` is searched for in \
`~/.config/dak/`, then the current directory, then the binary directory"
    )]
    pub config: Option<PathBuf>,

    /// Debug subsystems to enable.
    #[arg(
        short = 'd',
        long,
        value_delimiter = ',',
        num_args = 1..,
        help = "Debug subsystems to enable: device, scene, action, fonts",
        long_help = "Debug subsystems to enable: device, scene, action, fonts (the fonts in use and the \
characters configured fonts cannot draw; silent without defaults.fonts). The option may be \
given repeatedly, and several comma-separated subsystems may be given at once; the two \
forms are additive"
    )]
    pub debug: Vec<String>,

    /// The most detailed log level written; replaces `logging.level`.
    #[arg(
        long,
        value_name = "LEVEL",
        value_parser = ["error", "warning", "info", "debug"],
        help = "Log level: error, warning, info or debug",
        long_help = "The most detailed level of log lines written: error, warning, info or \
debug. Replaces logging.level from the configuration. Errors are always written; debug lines \
also need their subsystem enabled with -d or logging.debug"
    )]
    pub log_level: Option<String>,

    /// Also append log lines to this file.
    #[arg(
        long,
        value_name = "PATH",
        help = "Also append log lines to PATH",
        long_help = "Also append log lines, each with a timestamp, to PATH (created with its \
directory when missing), in addition to the configured outputs; replaces logging.file. The file \
is reopened on SIGHUP, for log rotation"
    )]
    pub log_file: Option<PathBuf>,

    /// Also send log lines to syslog.
    #[arg(
        long,
        help = "Also send log lines to syslog",
        long_help = "Also send log lines to syslog(3), in addition to the configured outputs, \
with the facility from logging.syslog_facility (default user)"
    )]
    pub syslog: bool,

    /// Wait for devices held by another dak instead of skipping them.
    #[arg(
        long,
        conflicts_with = "replace",
        help = "Wait for devices held by another dak to be released",
        long_help = "When another dak (of any user) holds a configured device, wait until it releases it and then take it, instead of skipping it. Useful when switching between users: the next user's dak picks the keypad up as soon as the previous one stops"
    )]
    pub wait: bool,

    /// Stop the dak holding a device and take it over.
    #[arg(
        long,
        help = "Stop the dak holding a device and take it over",
        long_help = "When another dak holds a configured device, send it SIGTERM, wait up to 10 seconds for it to clean up and release the device, then take it. Only allowed for your own instances, or for any as root"
    )]
    pub replace: bool,

    /// Detach from the terminal and run in the background.
    #[arg(
        long,
        conflicts_with = "map",
        help = "Detach from the terminal and run in the background",
        long_help = "Detach from the terminal and run in the background as a daemon: fork, start \
a new session, change to /, and point stdin, stdout and stderr at /dev/null. The configuration \
is checked first, and the command only returns once the daemon has connected its devices (exit \
status 0) or failed to start (its exit status and last error). Log lines go to syslog unless \
the logging section or --log-file says otherwise. Not needed under systemd"
    )]
    pub detach: bool,

    /// Write the process id to this file.
    #[arg(
        long,
        value_name = "PATH",
        help = "Write the process id to PATH",
        long_help = "Write the process id (of the daemon, with --detach) to PATH, and remove the \
file again on exit"
    )]
    pub pid_file: Option<PathBuf>,

    /// Run the interactive device-mapping wizard instead of normal operation.
    #[arg(
        long,
        help = "Run the interactive device-mapping wizard and exit",
        long_help = "Run the interactive device-mapping wizard instead of normal operation: \
no config is read and no actions run; the wizard walks through capturing the connected \
device's buttons and encoders and prints the resulting device definition as JSON, then exits"
    )]
    pub map: bool,
}
