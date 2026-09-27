//! Guards the packages' single licence file, `debian/copyright`: it must quote every
//! licence in full (none of them is in Debian/Ubuntu's `/usr/share/common-licenses`),
//! stay in step with the licence files in the source tree, cover every embedded font,
//! and be installed by both packaging paths.

use std::fs;

/// Reads a repository file as text.
fn read(path: &str) -> String {
    fs::read_to_string(path).unwrap_or_else(|error| panic!("reading {path}: {error}"))
}

/// Undoes DEP-5 continuation-line formatting: strips the one leading space and turns a
/// lone `.` back into an empty line, then trims trailing whitespace from every line.
fn unfold(lines: &[&str]) -> String {
    lines
        .iter()
        .map(|line| {
            let line = line.strip_prefix(' ').unwrap_or(line);
            if line == "." {
                ""
            } else {
                line.trim_end()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The text of the standalone `License: <name>` paragraph of `copyright`, unfolded.
fn license_text(copyright: &str, name: &str) -> String {
    let lines: Vec<&str> = copyright.lines().collect();
    let heading = format!("License: {name}");
    // A standalone licence paragraph starts after a blank line and carries a body; the
    // `License:` lines inside `Files:` stanzas have no continuation lines of their own.
    let start = lines
        .iter()
        .enumerate()
        .position(|(index, line)| {
            *line == heading
                && index > 0
                && lines[index - 1].is_empty()
                && lines
                    .get(index + 1)
                    .is_some_and(|next| next.starts_with(' '))
        })
        .unwrap_or_else(|| panic!("debian/copyright has no standalone \"{heading}\" paragraph"));
    let body: Vec<&str> = lines[start + 1..]
        .iter()
        .take_while(|line| line.starts_with(' '))
        .copied()
        .collect();
    unfold(&body)
}

/// Normalizes a licence text for comparison: trailing whitespace off every line and
/// trailing blank lines dropped.
fn normalize(text: &str) -> String {
    text.lines()
        .map(str::trim_end)
        .collect::<Vec<_>>()
        .join("\n")
        .trim_end()
        .to_string()
}

/// The AGPL and OFL paragraphs are verbatim copies of `LICENSE` and `fonts/OFL.txt`, and
/// the Bitstream Vera and Arev paragraphs are the matching sections of
/// `fonts/LICENSE.txt`, so a licence update in the tree cannot leave the packages stale.
#[test]
fn copyright_quotes_every_licence_verbatim() {
    let copyright = read("debian/copyright");
    assert_eq!(
        license_text(&copyright, "AGPL-3.0-or-later"),
        normalize(&read("LICENSE"))
    );
    assert_eq!(
        license_text(&copyright, "OFL-1.1"),
        normalize(&read("fonts/OFL.txt"))
    );
    let dejavu = normalize(&read("fonts/LICENSE.txt"));
    for name in ["Bitstream-Vera", "Arev"] {
        let text = license_text(&copyright, name);
        assert!(
            dejavu.contains(&text),
            "the {name} paragraph is not a section of fonts/LICENSE.txt"
        );
    }
    assert!(license_text(&copyright, "Bitstream-Vera").starts_with("Bitstream Vera Fonts"));
    assert!(license_text(&copyright, "Arev").starts_with("Arev Fonts"));
}

/// Every font file in `fonts/` (all of them are embedded in the binary) is listed in a
/// `Files:` stanza, so adding a font cannot silently skip its licence.
#[test]
fn copyright_covers_every_font() {
    let copyright = read("debian/copyright");
    let mut fonts = 0;
    for entry in fs::read_dir("fonts").expect("fonts/ exists") {
        let path = entry.unwrap().path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("ttf") {
            continue;
        }
        let name = format!("fonts/{}", path.file_name().unwrap().to_str().unwrap());
        assert!(
            copyright
                .lines()
                .any(|line| line.trim_start_matches("Files:").trim() == name),
            "debian/copyright has no Files: entry for {name}"
        );
        fonts += 1;
    }
    assert_eq!(fonts, 5, "expected the five embedded fonts");
}

/// The file starts with the DEP-5 format header, which cargo-deb and lintian key on.
#[test]
fn copyright_is_machine_readable() {
    let copyright = read("debian/copyright");
    assert!(copyright
        .starts_with("Format: https://www.debian.org/doc/packaging-manuals/copyright-format/1.0/"));
    assert!(copyright.contains("\nFiles: *\n"));
}

/// Both packaging paths install the file: cargo-deb via an explicit asset (so it does not
/// generate its own name-only copyright), and the FreeBSD `.pkg` step by copying it into
/// the staged doc directory, with CI asserting it made it into each artifact.
#[test]
fn both_packaging_paths_install_the_copyright_file() {
    let cargo_toml = read("Cargo.toml");
    assert!(
        cargo_toml.contains(
            r#"{ source = "debian/copyright", dest = "usr/share/doc/dak/copyright", mode = "644" }"#
        ),
        "Cargo.toml does not install debian/copyright as the .deb's copyright file"
    );
    let release = read(".woodpecker/release.yaml");
    assert!(release.contains("cp debian/copyright stage-root/usr/local/share/doc/dak/copyright"));
    assert!(release.contains("grep -qF 'usr/local/share/doc/dak/copyright'"));
    assert_eq!(
        release
            .matches("cmp /tmp/copyright debian/copyright")
            .count(),
        2,
        "both .deb jobs should compare the packaged copyright file"
    );
}

/// The systemd user unit is a `Type=notify` service that reloads with SIGHUP, does not
/// restart on a config error (status 3), leaves `launch`ed programs alone on stop, runs
/// without `--detach` and follows the graphical session.
#[test]
fn systemd_unit_has_the_service_settings() {
    let unit = read("debian/dak.service");
    let setting = |line: &str| unit.lines().any(|l| l.trim() == line);
    for line in [
        "Type=notify",
        "ExecStart=/usr/bin/dak --wait",
        "ExecReload=/bin/kill -HUP $MAINPID",
        "Restart=on-failure",
        "RestartPreventExitStatus=3",
        "KillMode=process",
        "PartOf=graphical-session.target",
        "WantedBy=graphical-session.target",
    ] {
        assert!(setting(line), "debian/dak.service lacks {line:?}");
    }
    assert!(
        unit.lines()
            .filter(|line| line.starts_with("ExecStart="))
            .all(|line| !line.contains("--detach")),
        "a notify unit must not fork"
    );
    assert_eq!(
        format!("RestartPreventExitStatus={}", dak::exit::CONFIG),
        "RestartPreventExitStatus=3",
        "the unit's status 3 must be the config-error status"
    );
}

/// The XDG autostart example starts dak detached and waiting, stays out of menus,
/// is skipped by systemd sessions (which use the unit), and says it is not active.
#[test]
fn autostart_entry_has_the_settings() {
    let entry = read("examples/service/dak.desktop");
    let setting = |line: &str| entry.lines().any(|l| l.trim() == line);
    for line in [
        "[Desktop Entry]",
        "Type=Application",
        "Exec=dak --detach --wait",
        "Terminal=false",
        "NoDisplay=true",
        "X-systemd-skip=true",
    ] {
        assert!(setting(line), "dak.desktop lacks {line:?}");
    }
    assert!(entry.contains("Not active by default"));
    assert!(
        entry.contains("pkill -u \"$USER\" -x dak"),
        "it explains stopping at logout"
    );
}

/// The plug-in hooks send the rescan signal for the supported keypad, and say they are
/// not active by default.
#[test]
fn rescan_hooks_signal_dak() {
    let udev = read("examples/service/99-dak-rescan.rules");
    assert!(udev.contains(r#"ATTRS{idVendor}=="0300", ATTRS{idProduct}=="3002""#));
    assert!(udev.contains("pkill -USR1 -x dak"));
    let devd = read("examples/service/dak-rescan.conf");
    assert!(devd.contains(r#"match "vendor" "0x0300";"#));
    assert!(devd.contains(r#"match "product" "0x3002";"#));
    assert!(devd.contains("pkill -USR1 -x dak"));
    for hook in [&udev, &devd] {
        assert!(hook.contains("Not active by default"));
    }
}

/// Both packaging paths ship the service files, and CI checks each artifact has them.
#[test]
fn both_packaging_paths_ship_the_service_files() {
    let cargo_toml = read("Cargo.toml");
    for asset in [
        r#"{ source = "debian/dak.service", dest = "usr/lib/systemd/user/dak.service", mode = "644" }"#,
        r#"{ source = "examples/service/99-dak-rescan.rules", dest = "usr/share/doc/dak/examples/99-dak-rescan.rules", mode = "644" }"#,
        r#"{ source = "examples/service/dak.desktop", dest = "usr/share/doc/dak/examples/dak.desktop", mode = "644" }"#,
        r#"{ source = "examples/service.json", dest = "usr/share/doc/dak/examples/service.json", mode = "644" }"#,
    ] {
        assert!(
            cargo_toml.contains(asset),
            "Cargo.toml lacks the deb asset {asset}"
        );
    }
    let release = read(".woodpecker/release.yaml");
    for needle in [
        "cp examples/service/dak.desktop examples/service/dak-rescan.conf examples/service.json",
        "grep -qF 'usr/local/share/examples/dak/dak.desktop'",
        "grep -qF 'usr/local/share/examples/dak/dak-rescan.conf'",
    ] {
        assert!(release.contains(needle), "release.yaml lacks {needle:?}");
    }
    for needle in [
        "grep -qF 'usr/lib/systemd/user/dak.service'",
        "grep -qF 'usr/share/doc/dak/examples/99-dak-rescan.rules'",
        "grep -qF 'usr/share/doc/dak/examples/dak.desktop'",
    ] {
        assert_eq!(
            release.matches(needle).count(),
            2,
            "both .deb jobs should check {needle}"
        );
    }
}

/// The README states the current version (it once lagged several releases behind).
#[test]
fn readme_states_the_crate_version() {
    let readme = read("README.markdown");
    let expected = format!("The current version is **v{}**.", env!("CARGO_PKG_VERSION"));
    assert!(
        readme.contains(&expected),
        "README.markdown should say {expected:?}"
    );
}
