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
use dak::actions::{CONTROL_EVENTS, DEFAULTS_KEYS, ENCODER_EVENTS, SETUP_KINDS, TOP_LEVEL_KEYS};
use dak::cli::Cli;
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
.SH FILES
.TP
.I ~/.config/dak/config.json
The preferred per\-user configuration location.
.TP
.I ./config.json
Configuration in the current directory.
.SH "EXIT STATUS"
.TP
.B 0
Normal termination, including termination by Ctrl\-C.
.TP
.B non\-zero
No configured device was found, a device could not be connected, or the
configuration could not be loaded.
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
.B dak \-\-map
Capture the connected device's mapping as JSON and exit.
.SH NOTES
Only one process may hold the device open at a time; stop any other instance
of
.B dak
(or other program using the keypad) before starting a new one.
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
        .date("2026-09-20")
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
    let groups: [(&str, &[&str]); 7] = [
        ("top-level key", TOP_LEVEL_KEYS),
        ("defaults key", DEFAULTS_KEYS),
        ("setup type", SETUP_KINDS),
        ("button event", CONTROL_EVENTS),
        ("encoder event", ENCODER_EVENTS),
        ("variable key", VARIABLE_KEYS),
        ("reserved variable name", RESERVED_NAMES),
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
