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
