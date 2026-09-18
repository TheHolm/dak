#!/usr/bin/env python3
"""Build a FreeBSD `.pkg` file from a staged install tree.

This is the packaging recipe from NOTES.md section 2 ("Building a FreeBSD
`.pkg` on Linux"), promoted from a throwaway script into a real, tested,
parametrized tool per that section's own TODO. It needs no FreeBSD host or
`pkg` binary - see NOTES.md for how the on-disk `.pkg` format (a zstd tar
archive with a JSON manifest) was reverse-engineered and validated against
the real `pkg` tool.

Usage:
    build-freebsd-pkg.py --version 0.8.1 --stage-root ./stage-root \
        --output dist/dak-0.8.1-freebsd-amd64.pkg

`--stage-root` must contain the package's files laid out at their final
absolute install paths (e.g. `<stage-root>/usr/local/bin/dak`). Every regular
file found by walking `--stage-root` is included in the package, using its
on-disk permission bits - there is no separate file list to keep in sync.
"""

import argparse
import hashlib
import json
import os
import subprocess
import sys


def sha256_of(path: str) -> str:
    """Return the "1$<hex sha256>" digest string `pkg` uses for a file's `sum`."""
    digest = hashlib.sha256()
    with open(path, "rb") as f:
        for chunk in iter(lambda: f.read(1 << 20), b""):
            digest.update(chunk)
    return "1$" + digest.hexdigest()


def collect_files(stage_root: str) -> dict:
    """Walk `stage_root` and build the manifest's `files` dict.

    Keys are absolute install paths (leading `/`, matching `stage_root`-relative
    paths); values carry the checksum/ownership/permission/mtime fields `pkg`
    expects. Ownership is always root:wheel, matching every FreeBSD base-system
    package - dak installs nothing that needs a different owner.
    """
    files = {}
    for dirpath, _dirnames, filenames in os.walk(stage_root):
        for name in filenames:
            full = os.path.join(dirpath, name)
            rel = os.path.relpath(full, stage_root)
            abs_install_path = "/" + rel
            st = os.stat(full)
            files[abs_install_path] = {
                "sum": sha256_of(full),
                "uname": "root",
                "gname": "wheel",
                "perm": oct(st.st_mode & 0o7777)[2:].zfill(4),
                "mtime": int(st.st_mtime),
            }
    return files


def build_manifest(args: argparse.Namespace, files: dict) -> dict:
    """Assemble the `+MANIFEST` JSON document (schema per NOTES.md section 2.2)."""
    flatsize = sum(
        os.path.getsize(os.path.join(args.stage_root, rel.lstrip("/")))
        for rel in files
    )
    return {
        "name": args.name,
        "origin": args.origin,
        "version": args.version,
        "comment": args.comment,
        "maintainer": args.maintainer,
        "www": args.www,
        "abi": f"FreeBSD:{args.freebsd_major}:{args.arch}",
        "arch": f"freebsd:{args.freebsd_major}:{args.arch_alias}",
        "prefix": "/usr/local",
        "flatsize": flatsize,
        "licenselogic": "single",
        "licenses": [args.license],
        "desc": args.desc,
        "categories": [args.category],
        "files": files,
    }


def write_pkg(manifest: dict, stage_root: str, output: str) -> None:
    """Write the manifests + staged files into a zstd-compressed tar at `output`.

    Reproduces the exact tar invocation from NOTES.md section 2.1/2.3,
    including the leading-slash gotcha documented there: staged files need a
    real leading `/` in the tar member name (`-P --transform 's,^usr/,/usr/,'`)
    while `+MANIFEST`/`+COMPACT_MANIFEST` must stay slash-less at the tar root.
    """
    compact = {k: v for k, v in manifest.items() if k not in ("files", "scripts")}

    work_dir = os.path.dirname(os.path.abspath(output)) or "."
    manifest_dir = os.path.join(work_dir, ".pkg-manifest-stage")
    os.makedirs(manifest_dir, exist_ok=True)
    with open(os.path.join(manifest_dir, "+MANIFEST"), "w") as f:
        json.dump(manifest, f, separators=(",", ":"))
    with open(os.path.join(manifest_dir, "+COMPACT_MANIFEST"), "w") as f:
        json.dump(compact, f, separators=(",", ":"))

    # GNU tar's `-C` is positional/cumulative, not reset per use: a later
    # `-C <relative-dir>` resolves relative to wherever the *previous* `-C`
    # left tar's directory context, not the original invocation cwd. Since
    # the first `-C` above already points at `manifest_dir` (an absolute
    # path), every later `-C stage_root` must also be absolute, or tar goes
    # looking for `stage_root` *inside* `manifest_dir` and fails with
    # "Cannot open: No such file or directory".
    stage_root_abs = os.path.abspath(stage_root)

    tar_cmd = [
        "tar", "--numeric-owner", "--owner=0", "--group=0",
        "-P", "--transform=s,^usr/,/usr/,",
        "-C", manifest_dir, "-cf", "-", "+MANIFEST", "+COMPACT_MANIFEST",
    ]
    for rel in manifest["files"]:
        tar_cmd += ["-C", stage_root_abs, rel.lstrip("/")]

    os.makedirs(os.path.dirname(os.path.abspath(output)) or ".", exist_ok=True)
    with open(output, "wb") as out:
        tar_proc = subprocess.Popen(tar_cmd, stdout=subprocess.PIPE)
        zstd_proc = subprocess.run(
            ["zstd", "-19", "-q", "-c"], stdin=tar_proc.stdout, stdout=out
        )
        tar_proc.wait()
        if tar_proc.returncode != 0:
            raise subprocess.CalledProcessError(tar_proc.returncode, tar_cmd)
        if zstd_proc.returncode != 0:
            raise subprocess.CalledProcessError(zstd_proc.returncode, ["zstd"])


def parse_args(argv: list) -> argparse.Namespace:
    """Parse command-line arguments, applying dak-specific defaults."""
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--name", default="dak")
    parser.add_argument("--origin", default="sysutils/dak")
    parser.add_argument("--version", required=True, help="e.g. 0.8.1 (no leading v)")
    parser.add_argument("--stage-root", required=True)
    parser.add_argument("--output", required=True)
    parser.add_argument(
        "--comment", default="Controls an Ajazz AKP03E/AKP03R USB macro keypad"
    )
    parser.add_argument("--maintainer", default="theholm@github.com")
    parser.add_argument("--www", default="https://github.com/theholm/dak")
    parser.add_argument("--license", default="AGPL3")
    parser.add_argument("--category", default="sysutils")
    parser.add_argument(
        "--desc",
        default=(
            "DAK controls an Ajazz AKP03E/AKP03R USB macro keypad: paints button "
            "images, sets brightness, and reacts to key/encoder input."
        ),
    )
    parser.add_argument("--freebsd-major", default="15")
    parser.add_argument("--arch", default="amd64")
    parser.add_argument("--arch-alias", default="x86:64")
    return parser.parse_args(argv)


def main(argv: list) -> int:
    """Entry point: build the manifest from --stage-root and write the .pkg."""
    args = parse_args(argv)
    files = collect_files(args.stage_root)
    if not files:
        print(f"error: no files found under --stage-root {args.stage_root}", file=sys.stderr)
        return 1
    manifest = build_manifest(args, files)
    write_pkg(manifest, args.stage_root, args.output)
    print(f"wrote {args.output} ({len(files)} files)")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
