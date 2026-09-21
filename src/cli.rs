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
is interrupted (Ctrl-C) or the device disconnects, then restores the button images it changed \
and shuts the device down."
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
        help = "Debug subsystems to enable: device, scene, action",
        long_help = "Debug subsystems to enable: device, scene, action. The option may be \
given repeatedly, and several comma-separated subsystems may be given at once; the two \
forms are additive"
    )]
    pub debug: Vec<String>,

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
