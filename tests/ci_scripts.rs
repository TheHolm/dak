//! Tests for the release pipeline's helper scripts in `scripts/`: the tag check and the
//! download verification. The network is replaced by a fake `curl` on `PATH` that
//! serves files from a directory, so these run offline and can simulate tampering.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

mod common;

/// Runs `scripts/<script>` with `args`, with `PATH` starting at `bin` when given.
fn run(script: &str, args: &[&str], bin: Option<&Path>, env: &[(&str, &str)]) -> Output {
    let mut command = Command::new("sh");
    command
        .arg(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("scripts")
                .join(script),
        )
        .args(args);
    if let Some(bin) = bin {
        let path = std::env::var("PATH").unwrap_or_default();
        command.env("PATH", format!("{}:{path}", bin.display()));
    }
    for (key, value) in env {
        command.env(key, value);
    }
    command.output().unwrap()
}

/// A directory with a fake `curl` that answers `https://<host>/<path>` from
/// `<root>/<host>/<path>` (`-o FILE` or stdout), failing like `curl -f` when missing.
fn fake_network() -> (PathBuf, PathBuf) {
    let dir = common::temp_dir();
    let bin = dir.join("bin");
    let root = dir.join("net");
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::create_dir_all(&root).unwrap();
    let curl = bin.join("curl");
    std::fs::write(
        &curl,
        format!(
            r#"#!/bin/sh
out=""
url=""
while [ "$#" -gt 0 ]; do
    case "$1" in
        -o) out=$2; shift ;;
        --proto|--tlsv1.2) [ "$1" = --proto ] && shift ;;
        -*) ;;
        *) url=$1 ;;
    esac
    shift
done
file="{root}/${{url#https://}}"
[ -f "$file" ] || exit 22
if [ -n "$out" ]; then cp "$file" "$out"; else cat "$file"; fi
"#,
            root = root.display()
        ),
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&curl, std::fs::Permissions::from_mode(0o755)).unwrap();
    (bin, root)
}

/// Writes `contents` at `<root>/<url without https://>`.
fn serve(root: &Path, url: &str, contents: &[u8]) {
    let path = root.join(url.trim_start_matches("https://"));
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, contents).unwrap();
}

/// The SHA-256 of `bytes` in hex, via the system's `sha256sum`.
fn sha256(bytes: &[u8]) -> String {
    let dir = common::temp_dir();
    let file = dir.join("f");
    std::fs::write(&file, bytes).unwrap();
    let output = Command::new("sha256sum").arg(&file).output().unwrap();
    let _ = std::fs::remove_dir_all(&dir);
    String::from_utf8(output.stdout).unwrap()[..64].to_string()
}

/// A MANIFEST line for `name` with `hash`, as FreeBSD writes them.
fn manifest(name: &str, hash: &str) -> String {
    format!(
        "base-dbg.txz\t{}\t1\tbase_dbg\t\"x\"\toff\n{name}\t{hash}\t2\tbase\t\"Base\"\ton\n",
        "0".repeat(64)
    )
}

const RELEASE: &str = "releases/amd64/amd64/15.1-RELEASE";
const PRIMARY: &str = "https://primary.example";
const MIRRORS: &str = "https://m1.example/pub https://m2.example/pub";

/// Sets up the primary and both mirrors with `manifest_text`, and the primary's set.
fn freebsd_site(root: &Path, manifest_text: &str, set: &[u8]) {
    serve(
        root,
        &format!("{PRIMARY}/{RELEASE}/MANIFEST"),
        manifest_text.as_bytes(),
    );
    for mirror in MIRRORS.split(' ') {
        serve(
            root,
            &format!("{mirror}/{RELEASE}/MANIFEST"),
            manifest_text.as_bytes(),
        );
    }
    serve(root, &format!("{PRIMARY}/{RELEASE}/base.txz"), set);
}

/// Runs the FreeBSD verification against the fake site, into `output`.
fn verify(bin: &Path, output: &Path) -> Output {
    run(
        "verify-freebsd-dist.sh",
        &[RELEASE, "base.txz", output.to_str().unwrap()],
        Some(bin),
        &[("FREEBSD_PRIMARY", PRIMARY), ("FREEBSD_MIRRORS", MIRRORS)],
    )
}

/// A set matching a MANIFEST that every mirror agrees on is accepted.
#[test]
fn freebsd_set_matching_agreed_manifest_is_accepted() {
    let (bin, root) = fake_network();
    let set = b"the base system";
    freebsd_site(&root, &manifest("base.txz", &sha256(set)), set);
    let output_file = root.join("out.txz");
    let output = verify(&bin, &output_file);
    assert!(output.status.success(), "{output:?}");
    assert_eq!(std::fs::read(&output_file).unwrap(), set);
    assert!(String::from_utf8_lossy(&output.stdout).contains("2 mirror(s)"));
}

/// A tampered set (hash differs from MANIFEST) is refused and not left behind.
#[test]
fn freebsd_tampered_set_is_refused() {
    let (bin, root) = fake_network();
    freebsd_site(
        &root,
        &manifest("base.txz", &sha256(b"genuine")),
        b"tampered",
    );
    let output_file = root.join("out.txz");
    let output = verify(&bin, &output_file);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("does not match"));
    assert!(!output_file.exists());
}

/// A primary serving a MANIFEST that matches its tampered set is caught by a mirror
/// with the genuine MANIFEST.
#[test]
fn freebsd_manifest_disagreeing_with_a_mirror_is_refused() {
    let (bin, root) = fake_network();
    freebsd_site(
        &root,
        &manifest("base.txz", &sha256(b"genuine")),
        b"tampered",
    );
    serve(
        &root,
        &format!("{PRIMARY}/{RELEASE}/MANIFEST"),
        manifest("base.txz", &sha256(b"tampered")).as_bytes(),
    );
    let output = verify(&bin, &root.join("out.txz"));
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("differs"));
}

/// Without any mirror to confirm the MANIFEST, nothing is trusted; one unreachable
/// mirror is fine as long as another confirms.
#[test]
fn freebsd_manifest_needs_a_confirming_mirror() {
    let (bin, root) = fake_network();
    let set = b"the base system";
    serve(
        &root,
        &format!("{PRIMARY}/{RELEASE}/MANIFEST"),
        manifest("base.txz", &sha256(set)).as_bytes(),
    );
    serve(&root, &format!("{PRIMARY}/{RELEASE}/base.txz"), set);
    let output = verify(&bin, &root.join("out.txz"));
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("no mirror could confirm"));

    serve(
        &root,
        &format!("https://m2.example/pub/{RELEASE}/MANIFEST"),
        manifest("base.txz", &sha256(set)).as_bytes(),
    );
    let output = verify(&bin, &root.join("out.txz"));
    assert!(output.status.success(), "{output:?}");
}

/// A set missing from MANIFEST is refused.
#[test]
fn freebsd_set_missing_from_manifest_is_refused() {
    let (bin, root) = fake_network();
    freebsd_site(&root, &manifest("other.txz", &sha256(b"x")), b"x");
    let output = verify(&bin, &root.join("out.txz"));
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("not listed"));
}

/// `fetch-verified.sh` accepts a file matching its published `.sha256` and refuses a
/// mismatch or a missing checksum.
#[test]
fn fetch_verified_checks_the_published_sha256() {
    let (bin, root) = fake_network();
    let url = "https://static.example/rustup-init";
    serve(&root, url, b"installer");
    serve(
        &root,
        &format!("{url}.sha256"),
        format!("{} *./rustup-init\n", sha256(b"installer")).as_bytes(),
    );
    let out = root.join("rustup-init");
    let output = run(
        "fetch-verified.sh",
        &[url, out.to_str().unwrap()],
        Some(&bin),
        &[],
    );
    assert!(output.status.success(), "{output:?}");

    serve(&root, url, b"evil installer");
    let output = run(
        "fetch-verified.sh",
        &[url, out.to_str().unwrap()],
        Some(&bin),
        &[],
    );
    assert!(!output.status.success());
    assert!(!out.exists());

    let output = run(
        "fetch-verified.sh",
        &["https://static.example/unknown", out.to_str().unwrap()],
        Some(&bin),
        &[],
    );
    assert!(!output.status.success());
}

/// Only plain `vX.Y.Z` tags are released; anything that could smuggle shell syntax,
/// a newline or an odd version into later steps is refused.
#[test]
fn release_tags_must_be_plain_versions() {
    for tag in ["v0.15.0", "v1.2.3", "v10.0.12"] {
        assert!(
            run("check-release-tag.sh", &[tag], None, &[])
                .status
                .success(),
            "{tag}"
        );
    }
    for tag in [
        "",
        "v1.2",
        "1.2.3",
        "v1.2.3.4",
        "v1.2.3;id",
        "v1.2.3\nx",
        "v1.2.3 ",
        "v$(id).1.1",
        "v12345.1.1",
    ] {
        assert!(
            !run("check-release-tag.sh", &[tag], None, &[])
                .status
                .success(),
            "{tag:?}"
        );
    }
}
