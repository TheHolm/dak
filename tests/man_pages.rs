//! Tests for the man pages and the packaging wiring that installs them.
//!
//! `man/dak.1` is generated from the clap definition in `src/cli.rs` (plus the roff
//! appendix below); `man/dak-config.5` is hand-written. Nothing else would catch a page
//! whose `.TH` header names the wrong file/section, whose version drifts from
//! `Cargo.toml`, whose CLI text drifts from the real flags, or which stops documenting a
//! config key the code accepts - nor a packaging path that silently stops shipping one of
//! them. These tests read and render files directly rather than invoking `man`, `groff` or
//! `cargo-deb`, none of which are assumed to be installed.
//!
//! The generated and committed `dak.1` are compared *semantically* (a normalized token
//! multiset), not byte-for-byte: that keeps the guard working across `clap_mangen`/`roff`
//! releases that only change escaping or wrapping, so neither dependency needs pinning.
//! A real content change - a renamed flag, reworded help, edited appendix - still fails.

use clap::CommandFactory;
use dak::actions::{
    CONTROL_EVENTS, DEFAULTS_KEYS, ENCODER_EVENTS, SETUP_ENTRY_FIELDS, SETUP_KINDS,
    SUPPORTED_CONFIG_VERSION, TOP_LEVEL_KEYS,
};
use dak::cli::Cli;
use dak::imaging::FORMATS as IMAGE_FORMATS;
use dak::log::{LOGGING_KEYS, LOG_LEVELS, LOG_OUTPUTS, SYSLOG_FACILITIES, TIMESTAMP_VALUES};
use dak::markup::MARKUP_VALUES;
use dak::text::FONT_KEYS;
use dak::variables::{RESERVED_NAMES, VARIABLE_KEYS};

/// `man/dak.1`: the file that must exist, the name/section its `.TH` header declares,
/// and the packaging files that must reference it.
const DAK_1: ManPage = ManPage {
    path: "man/dak.1",
    th_prefix: ".TH DAK 1 ",
};

/// `man/dak-config.5`: see [`DAK_1`].
const DAK_CONFIG_5: ManPage = ManPage {
    path: "man/dak-config.5",
    th_prefix: ".TH DAK-CONFIG 5 ",
};

/// The hand-written roff tail of `man/dak.1`, appended verbatim after the sections clap
/// generates (`.TH`, NAME, SYNOPSIS, DESCRIPTION, OPTIONS). It holds the sections
/// clap_mangen has no field for, so they keep their own headings rather than being folded
/// into an "EXTRA" section.
const DAK_1_APPENDIX: &str = r##".SH "CONFIG FILE SEARCH"
When
.B \-c
is not given, a file named
.I config.json
is loaded from the first location that exists, in this order:
.IP "1." 4
.IB ~/.config/dak/config.json
.IP "2." 4
.I <current\-directory>/config.json
.IP "3." 4
.I <the\-directory\-of\-the\-binary>/config.json
.PP
An explicit
.B \-c
path is always used as it is and disables the search; a missing file is
reported and the program exits.
.SH ENVIRONMENT
.TP
.B HOME
Used to expand a leading
.B ~
in command lines and paths, and to find the
.IB ~/.config/dak
search location.
.TP
.B PATH
Used to locate a command when an action or setup entry names one without a
path. A command that contains an unquoted shell operator is run through the
.BR sh (1)
found on
.BR PATH .
.TP
.B JOURNAL_STREAM
Set by systemd when stderr goes to the journal. When it names the actual
stderr, the
.B auto
log output writes journal lines (see
.B LOGGING
in
.BR dak\-config (5)).
.TP
.B NOTIFY_SOCKET
Set by systemd for a
.B Type=notify
service. When present,
.B dak
reports
.B READY=1
once its devices are connected (or waiting for another instance),
.B STOPPING=1
when it is told to stop, and a
.B STATUS=
line. It is removed from the environment of every program
.B dak
starts, so none of them can report to systemd in its name;
.B image_exec ", " text_exec
and
.B $(command)
programs also lose
.BR JOURNAL_STREAM ,
since their stderr is read by
.BR dak .
.TP
.B DAK_LOCK_DIR
The directory device lock files are kept in, instead of
.I /run/lock
(or
.I /tmp
where that does not exist). Every
.B dak
that may compete for a keypad must use the same directory.
.TP
.BR XDG_STATE_HOME
Where the default log file lives:
.IR $XDG_STATE_HOME/dak/dak.log ,
else
.IR ~/.local/state/dak/dak.log .
.SH FILES
.TP
.I ~/.config/dak/config.json
The preferred per\-user configuration location.
.TP
.I ./config.json
Configuration in the current directory.
.TP
.I ~/.local/state/dak/dak.log
The default log file, when the
.B file
log output is used.
.TP
.I PIDFILE
The file named by
.BR \-\-pid\-file ;
holds the process id while
.B dak
runs and is removed on exit.
.TP
.I /usr/lib/systemd/user/dak.service
The systemd user unit shipped by the Debian/Ubuntu package
.RB ( "systemctl \-\-user enable \-\-now dak" ).
.TP
.I /usr/share/doc/dak/examples/dak.desktop
.TQ
.I /usr/local/share/examples/dak/dak.desktop
An inactive XDG autostart entry
.RB ( "dak \-\-detach \-\-wait" )
for desktops without systemd, such as FreeBSD; copy it to
.IR ~/.config/autostart/ .
A detached
.B dak
does not end at logout; stop it from the logout path with
.BR "pkill \-u $USER \-x dak" .
.TP
.I /usr/share/doc/dak/examples/99\-dak\-rescan.rules
.TQ
.I /usr/local/share/examples/dak/dak\-rescan.conf
Inactive udev and devd rules that send
.B SIGUSR1
to every
.B dak
when a keypad is plugged in.
.TP
.I /run/lock/dak\-<vid>\-<pid>\-<serial>.lock
One lock file per keypad (in
.I /tmp
where
.I /run/lock
does not exist), holding the process id, user and start time of the
.B dak
using it.
.SH "RUNNING IN THE BACKGROUND"
With
.B \-\-detach
.B dak
forks into the background, starts a new session, changes to
.I /
and points its standard input, output and error at
.IR /dev/null .
The configuration and the log outputs are checked before forking, so their
errors appear in the terminal; the command then waits until the daemon has
connected its devices (or is waiting for a device held by another instance)
and exits with status 0, or exits with the daemon's own status and last error
when it failed to start. The
.B auto
log output goes to syslog once detached. Programs run by actions get
.I /dev/null
as standard input in every mode.
.PP
Under systemd do not use
.BR \-\-detach :
run
.B dak
in the foreground as a
.B Type=notify
service instead; it reports readiness through
.BR NOTIFY_SOCKET .
.SH SIGNALS
.TP
.BR SIGINT ", " SIGTERM
Stop cleanly: clear the button images this session changed, shut every
device down and exit with status 0. A second one while that cleanup is
still running exits at once with status 1.
.TP
.B SIGHUP
Reload: reopen the log file (for log rotation) and re\-read the
configuration. When it is invalid the errors are logged and the running
configuration is kept. When it is valid, logging is set up from it again and
every device is restarted with it, from its
.B on_start
scene with the variables at their initial values; devices still in use keep
their lock, and newly defined or plugged\-in ones are picked up.
.TP
.B SIGUSR1
Rescan: devices that gave up reconnecting (see
.B device_reconnect_max_attempts
in
.BR dak\-config (5))
start a fresh round of attempts, and configured devices that were not found
or were in use by another instance are looked for again. Running devices are
left alone. A device that has given up releases its lock, so another
instance may take it in the meantime.
.PP
When no configured device is attached (at startup, or once every device has
given up),
.B dak
exits with status 4 when run from a terminal, but keeps running and waits for
.B SIGUSR1
when it runs as a service: with
.BR \-\-detach ,
or under systemd
.RB ( NOTIFY_SOCKET
set).
.SH "EXIT STATUS"
.TP
.B 0
Normal termination, including termination by Ctrl\-C or
.BR SIGTERM .
.TP
.B 1
Unspecified failure, for example a device that could not be opened, or a
repeated stop signal that ended the program before its cleanup finished.
.TP
.B 2
Invalid command line.
.TP
.B 3
Configuration error: the configuration file could not be read or failed
validation. Restarting will not help until it is fixed.
.TP
.B 4
No configured device found: no device defined in the configuration is
attached, or every one was lost and given up on (in the foreground only; see
.BR SIGNALS ).
.TP
.B 5
Every configured device that was found is held by another running
.B dak
(or, with
.BR \-\-map ,
the chosen device is).
.SH EXAMPLES
.TP
.B dak
Run with the default
.IR config.json .
.TP
.B dak \-c /path/to/config.json
Run with an explicit configuration path.
.TP
.B dak \-d device,scene
Print device and scene debug output.
.TP
.B dak \-\-log\-level warning \-\-log\-file /tmp/dak.log
Write only warnings and errors, to the console and to a log file.
.TP
.B dak \-\-map
Capture the connected device's mapping as JSON and exit.
.TP
.B dak \-\-detach \-\-pid\-file /tmp/dak.pid
Run in the background, logging to syslog; returns once the devices are
connected.
.TP
.B dak \-\-replace
Take the keypad over from a
.B dak
already running as the same user.
.SH NOTES
Only one
.B dak
drives a keypad at a time, whichever user runs it. Before opening a device,
.B dak
takes an exclusive lock on its lock file (see
.BR FILES );
the kernel drops the lock when the process ends, even if it crashes. When
another instance holds it, the device is skipped with a message naming that
instance's user and process id; with
.B \-\-wait
.B dak
waits for it to be released, and with
.B \-\-replace
it asks the holder to stop and takes the device over, after checking that the
recorded process really is a
.B dak
of the recorded user (lock files can be written by every user, so a record
alone is never trusted). A lock file that is a symlink, a hard link or not a
regular file is refused. Since the lock directory is shared, any local user can
hold a keypad's lock and so keep it from being used; set
.B DAK_LOCK_DIR
to a directory only the keypad's users can write to where that matters. When every configured
device is held elsewhere the program exits with status 5. The lock is kept
while a lost device is being waited for, so no other instance takes it over
meanwhile. The
.B \-\-map
wizard also takes the lock and refuses a device in use.
.PP
The device definition printed by
.B \-\-map
is the same JSON that the
.B devices
section of the configuration expects; paste it in and adjust it.
.SH "SEE ALSO"
.BR dak\-config (5),
.BR sh (1)
.PP
The project README and INSTALL documents ship alongside the binary, and the
example configurations are under
.IR examples/ .
"##;

/// One man page under test: where it lives and how its `.TH` line must begin.
struct ManPage {
    /// Path to the page, relative to the crate root.
    path: &'static str,
    /// The exact prefix of the page's first `.TH` line (name and section).
    th_prefix: &'static str,
}

/// Reads a page and returns its first `.TH` line, panicking with a useful message when
/// the file is missing, empty, or has no `.TH` header at all.
fn read_th(page: &ManPage) -> String {
    let contents = std::fs::read_to_string(page.path)
        .unwrap_or_else(|error| panic!("{} is not readable: {error}", page.path));
    assert!(!contents.trim().is_empty(), "{} is empty", page.path);
    contents
        .lines()
        .find(|line| line.starts_with(".TH "))
        .unwrap_or_else(|| panic!("{} has no .TH header", page.path))
        .to_string()
}

/// Renders `man/dak.1` from the clap definition in `src/cli.rs` followed by
/// [`DAK_1_APPENDIX`]. `regenerate_dak_1` writes exactly these bytes to disk, so the
/// committed page is a build product of this function.
fn render_dak_1() -> Vec<u8> {
    let man = clap_mangen::Man::new(Cli::command())
        .title("DAK")
        .section("1")
        .date("2026-09-28")
        .source(format!("dak {}", env!("CARGO_PKG_VERSION")))
        .manual("User Commands");
    let mut page = Vec::new();
    man.render(&mut page)
        .expect("rendering the dak(1) man page to memory");
    page.extend_from_slice(DAK_1_APPENDIX.as_bytes());

    // roff leaves a trailing space on some lines (e.g. the SYNOPSIS); strip them so the
    // committed page is clean and diffs do not carry invisible whitespace.
    let rendered = String::from_utf8(page).expect("clap_mangen output is UTF-8");
    let mut trimmed = rendered
        .lines()
        .map(str::trim_end)
        .collect::<Vec<_>>()
        .join("\n");
    trimmed.push('\n');
    trimmed.into_bytes()
}

/// Resolves the ROFF escape sequences `roff` and hand-written pages use down to the
/// characters they print (`\-` to `-`, `\e` to backslash, `\fB`/`\fR` fonts dropped, the
/// apostrophe and quote strings, ...). Unknown escapes degrade to their trailing
/// character rather than being dropped, so canonicalization stays stable.
fn unescape_roff(input: &str) -> String {
    let mut out = String::new();
    let mut chars = input.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('-') => out.push('-'),
            Some('e') => out.push('\\'),
            Some('&') => {}
            Some(' ') => out.push(' '),
            Some('c') => {}
            Some('f') => {
                chars.next();
            }
            Some('*') | Some('(') => {
                let first = chars.next();
                let second = chars.next();
                let name: String = [first, second].into_iter().flatten().collect();
                match name.as_str() {
                    "Aq" | "aq" => out.push('\''),
                    "dq" | "lq" | "rq" => out.push('"'),
                    "bu" => out.push('*'),
                    _ => {}
                }
            }
            Some('[') => {
                let mut name = String::new();
                for ch in chars.by_ref() {
                    if ch == ']' {
                        break;
                    }
                    name.push(ch);
                }
                match name.as_str() {
                    "char34" => out.push('"'),
                    _ => {}
                }
            }
            Some(other) => out.push(other),
            None => {}
        }
    }
    out
}

/// Flattens ROFF source to its visible text: drops the `.TH`/preamble and the
/// argument-less structure requests, keeps the text of every other request, unescapes,
/// and collapses all whitespace to single spaces.
fn canonical_text(roff: &str) -> String {
    let mut out = String::new();
    for line in roff.lines() {
        let trimmed = line.trim_end();
        if trimmed.is_empty() {
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix('.') {
            let mut parts = rest.splitn(2, char::is_whitespace);
            let request = parts.next().unwrap_or("");
            let args = parts.next().unwrap_or("").trim();
            match request {
                "TH" | "ie" | "el" | "ds" | "PP" | "TP" | "RS" | "RE" | "nf" | "fi" | "br"
                | "ad" | "na" => continue,
                _ => {
                    out.push(' ');
                    out.push_str(args);
                }
            }
        } else {
            out.push(' ');
            out.push_str(trimmed);
        }
    }
    unescape_roff(&out)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// The sorted token multiset of a page's visible text, used to compare the generated and
/// committed `dak.1` without depending on how either was wrapped or escaped.
fn canonical_tokens(roff: &str) -> Vec<String> {
    let mut tokens: Vec<String> = canonical_text(roff)
        .split_whitespace()
        .map(str::to_string)
        .collect();
    tokens.sort();
    tokens
}

/// Whether `word` appears in `haystack` as a whole word (bounded by non-identifier
/// characters), so `clear` does not match inside `clearly`.
fn contains_word(haystack: &str, word: &str) -> bool {
    let is_identifier = |c: char| c.is_ascii_alphanumeric() || c == '_';
    let mut start = 0;
    while let Some(offset) = haystack[start..].find(word) {
        let at = start + offset;
        let before_ok = haystack[..at]
            .chars()
            .next_back()
            .is_none_or(|c| !is_identifier(c));
        let after = at + word.len();
        let after_ok = haystack[after..]
            .chars()
            .next()
            .is_none_or(|c| !is_identifier(c));
        if before_ok && after_ok {
            return true;
        }
        start = after;
    }
    false
}

/// Whether a `-x`/`--long` flag appears as a token (possibly followed by punctuation like
/// the comma in `-c,`), so `-c` is not satisfied by the `--config` token.
fn contains_flag(text: &str, flag: &str) -> bool {
    text.split_whitespace().any(|token| {
        token.strip_prefix(flag).is_some_and(|rest| {
            rest.chars()
                .next()
                .is_none_or(|c| !c.is_ascii_alphanumeric() && c != '-')
        })
    })
}

/// Every page declares its own filename and section in its `.TH` header, so a rename
/// or a move between sections cannot leave the page mislabelled in `man`.
#[test]
fn man_pages_declare_their_name_and_section() {
    for page in [DAK_1, DAK_CONFIG_5] {
        let th = read_th(&page);
        assert!(
            th.starts_with(page.th_prefix),
            "{}: .TH line {th:?} should start with {:?}",
            page.path,
            page.th_prefix
        );
    }
}

/// Both pages carry the current crate version in their `.TH` header, so a release
/// cannot ship a page still advertising the previous version.
#[test]
fn man_pages_are_stamped_with_the_crate_version() {
    let version = env!("CARGO_PKG_VERSION");
    for page in [DAK_1, DAK_CONFIG_5] {
        let th = read_th(&page);
        assert!(
            th.contains(version),
            "{}: .TH line {th:?} does not contain the crate version {version}",
            page.path
        );
    }
}

/// The committed `man/dak.1` says the same thing as a fresh render from the clap
/// definition: same flags, same help text, same appendix. Cosmetic rendering differences
/// are ignored (see the module docs); any real change fails until the page is regenerated.
#[test]
fn dak_1_matches_generated_semantically() {
    let generated = String::from_utf8(render_dak_1()).expect("generated page is UTF-8");
    let committed = std::fs::read_to_string(DAK_1.path).expect("man/dak.1 is readable");
    assert_eq!(
        canonical_tokens(&generated),
        canonical_tokens(&committed),
        "man/dak.1 is out of date; regenerate it with \
         `cargo test --test man_pages -- --ignored regenerate_dak_1`"
    );
}

/// Every flag clap knows about is documented in the committed page, so a page that is
/// missing an option fails with a message naming that option. (The semantic comparison
/// above already covers this; this test exists to point straight at the culprit.)
#[test]
fn dak_1_documents_every_cli_argument() {
    let text = canonical_text(&std::fs::read_to_string(DAK_1.path).expect("man/dak.1 readable"));
    let mut checked = 0;
    for arg in Cli::command().get_arguments() {
        if let Some(long) = arg.get_long() {
            let flag = format!("--{long}");
            assert!(
                contains_flag(&text, &flag),
                "man/dak.1 does not document {flag}"
            );
            checked += 1;
        }
        if let Some(short) = arg.get_short() {
            let flag = format!("-{short}");
            assert!(
                contains_flag(&text, &flag),
                "man/dak.1 does not document {flag}"
            );
            checked += 1;
        }
    }
    assert!(
        checked >= 4,
        "expected to inspect several CLI flags, only found {checked}"
    );
}

/// `man/dak-config.5` names every key, type and event the config loader accepts, with the
/// vocabulary lists taken straight from the code, so a rename or addition cannot leave the
/// page describing a format the program no longer understands.
#[test]
fn dak_config_5_documents_the_config_vocabulary() {
    let text = canonical_text(
        &std::fs::read_to_string(DAK_CONFIG_5.path).expect("man/dak-config.5 readable"),
    );
    let groups: [(&str, &[&str]); 16] = [
        ("top-level key", TOP_LEVEL_KEYS),
        ("defaults key", DEFAULTS_KEYS),
        ("setup type", SETUP_KINDS),
        ("setup entry field", SETUP_ENTRY_FIELDS),
        ("markup value", MARKUP_VALUES),
        ("defaults.fonts key", FONT_KEYS),
        ("image format", IMAGE_FORMATS),
        ("button event", CONTROL_EVENTS),
        ("encoder event", ENCODER_EVENTS),
        ("variable key", VARIABLE_KEYS),
        ("reserved variable name", RESERVED_NAMES),
        ("logging key", LOGGING_KEYS),
        ("logging.output value", LOG_OUTPUTS),
        ("logging.level value", LOG_LEVELS),
        ("logging.syslog_facility value", SYSLOG_FACILITIES),
        ("logging.timestamps value", TIMESTAMP_VALUES),
    ];
    for (label, names) in groups {
        for name in names {
            assert!(
                contains_word(&text, name),
                "man/dak-config.5 does not document the {label} {name:?}"
            );
        }
    }
}

/// `man/dak-config.5` states the config schema version this build supports, taken
/// straight from `SUPPORTED_CONFIG_VERSION`, so raising it cannot leave the page behind.
#[test]
fn dak_config_5_names_the_supported_config_version() {
    let text = canonical_text(
        &std::fs::read_to_string(DAK_CONFIG_5.path).expect("man/dak-config.5 readable"),
    );
    let sentence = format!("supports config version {SUPPORTED_CONFIG_VERSION}.");
    assert!(
        text.contains(&sentence),
        "man/dak-config.5 does not say {sentence:?}"
    );
}

/// The EXIT STATUS section of `dak(1)` lists every status the program can exit with,
/// taken straight from `dak::exit::EXIT_CODES`, so a new status cannot go undocumented.
#[test]
fn dak_1_documents_every_exit_status() {
    let page = std::fs::read_to_string(DAK_1.path).expect("man/dak.1 readable");
    let section = page
        .split(".SH \"EXIT STATUS\"")
        .nth(1)
        .expect("man/dak.1 has an EXIT STATUS section")
        .split("\n.SH ")
        .next()
        .unwrap();
    for (code, meaning) in dak::exit::EXIT_CODES {
        assert!(
            section.contains(&format!(".B {code}\n")),
            "man/dak.1 EXIT STATUS does not document status {code} ({meaning})"
        );
    }
}

/// Both packaging paths reference both pages: cargo-deb installs them via the
/// `[package.metadata.deb]` assets in `Cargo.toml`, and the FreeBSD `.pkg` step
/// copies them into the staged `/usr/local/share/man` tree in the release workflow.
#[test]
fn both_packaging_paths_reference_the_man_pages() {
    let cargo_toml = std::fs::read_to_string("Cargo.toml").expect("Cargo.toml is readable");
    let release_yaml = std::fs::read_to_string(".woodpecker/release.yaml")
        .expect(".woodpecker/release.yaml is readable");

    for page in [DAK_1, DAK_CONFIG_5] {
        assert!(
            cargo_toml.contains(page.path),
            "Cargo.toml does not reference {} in its cargo-deb assets",
            page.path
        );
        assert!(
            release_yaml.contains(page.path),
            ".woodpecker/release.yaml does not stage {} into the FreeBSD .pkg",
            page.path
        );
    }
}

/// Rewrites `man/dak.1` from the current clap definition plus the appendix. Ignored by
/// default because it writes a tracked file; run it after changing `src/cli.rs` or the
/// appendix, then review the diff:
/// `cargo test --test man_pages -- --ignored regenerate_dak_1`.
#[test]
#[ignore = "writes man/dak.1; run explicitly to regenerate the page"]
fn regenerate_dak_1() {
    std::fs::write(DAK_1.path, render_dak_1()).expect("writing man/dak.1");
}
