# NOTES.md

> **Disclaimer for agents:** this file is a working knowledge base, not
> user-facing documentation. It exists so agents (and humans) don't have to
> re-derive things by trial and error every time. It records *how things were
> figured out* - exact commands, gotchas, real error messages - not polished
> explanations. Treat it as a scratchpad of hard-won facts:
>
> - Prefer copy-pasteable commands over prose.
> - When you learn something new or find something here is stale/wrong, **fix
>   it in place** rather than leaving it to rot. Note the FreeBSD version /
>   host details things were verified against, since these facts can be
>   version-sensitive.
> - This is about **infrastructure** (cross-compiling, packaging, testing
>   without a full VM). Project-specific FreeBSD backend bugs/fixes (the
>   `uhid` vs `hidraw` story, the read-path thread-leak fix, etc.) live in
>   `vendor/README.md` instead - this file cross-references that one rather
>   than duplicating it.
> - Nothing here has been wired into actual CI yet (see the TODO section) -
>   it's the reconnaissance for building that.

All of sections 1-5 below was worked out and verified on: a Debian 13
(trixie) Linux sandbox (no root filesystem access beyond apt/rustup)
cross-compiling for, and testing against, a FreeBSD 14.5-RELEASE amd64 VM.
Re-verify version-specific details (library sonames, jail base.txz URL,
etc.) if the target FreeBSD release changes. Section 6 is unrelated
(animated button images against real Linux-attached hardware, not
FreeBSD/cross-compiling) and states its own environment inline.

## Table of contents

1. [Cross-compiling `dak` for FreeBSD from Linux](#1-cross-compiling-dak-for-freebsd-from-linux)
2. [Building a FreeBSD `.pkg` on Linux](#2-building-a-freebsd-pkg-on-linux)
3. [Testing a package without a full VM (jails)](#3-testing-a-package-without-a-full-vm-jails)
4. [`dak`'s actual runtime dependencies on FreeBSD](#4-daks-actual-runtime-dependencies-on-freebsd)
5. [CI TODO / open questions](#5-ci-todo--open-questions)
6. [Animated button images: real-hardware findings](#6-animated-button-images-real-hardware-findings)

---

## 1. Cross-compiling `dak` for FreeBSD from Linux

### 1.1 What works without any extra setup

```sh
rustup target add x86_64-unknown-freebsd
cargo check --target x86_64-unknown-freebsd
```

This alone is enough to catch **all** compile-time/type errors (missing
`cfg` gates, API mismatches, etc.) - it's how the `vendor/async-hid-freebsd`
FreeBSD backend was developed and iterated on entirely from Linux, before any
sysroot existed. `cargo check` never invokes the linker, so it does not need
anything FreeBSD-specific installed.

**This is the cheap, fast CI check to run on every commit.** It would have
caught, for instance, the original "no FreeBSD backend in upstream
`async-hid`" problem instantly.

### 1.2 What needs extra setup: actually linking a binary

`cargo build --target x86_64-unknown-freebsd` fails out of the box:

```
error: linking with `cc` failed: exit status: 1
/usr/bin/ld: cannot find -lexecinfo: No such file or directory
/usr/bin/ld: cannot find -lkvm: No such file or directory
/usr/bin/ld: cannot find -lmemstat: No such file or directory
/usr/bin/ld: cannot find -lprocstat: No such file or directory
/usr/bin/ld: cannot find -ldevstat: No such file or directory
```

These are **not `dak` dependencies** - they're baked into `rustc`'s own
default link args for the `x86_64-unknown-freebsd` target (for `libstd`'s
backtrace/stack-overflow-guard support), and none of them exist on a Linux
host. Cross-*linking* (unlike cross-*checking*) genuinely needs a FreeBSD
sysroot: real `.so`/`.a`/`.o` files from an actual FreeBSD system.

### 1.3 Toolchain: use `clang` + `lld`, not GCC

GCC on Linux has no idea how to target FreeBSD. `clang` does, via `--target=`.
Install both:

```sh
apt-get install -y clang lld
```

### 1.4 Building a sysroot

Pull the exact files needed from a real FreeBSD box (matching release/arch).
This list was derived by iterating on the linker's "cannot find" errors until
it succeeded - it is a minimal **link-time** set, not a minimal runtime set
(see [section 4](#4-daks-actual-runtime-dependencies-on-freebsd) for what's
actually loaded at runtime, which is much smaller).

```
/lib/libc.so.7
/usr/lib/libc.so          # linker script: GROUP ( /lib/libc.so.7 /usr/lib/libc_nonshared.a )
/usr/lib/libc_nonshared.a
/lib/libm.so.5
/usr/lib/libm.so
/lib/libutil.so.9
/usr/lib/libutil.so
/lib/librt.so.1
/usr/lib/librt.so
/usr/lib/libexecinfo.so.1
/usr/lib/libexecinfo.so
/lib/libkvm.so.7
/usr/lib/libkvm.so
/usr/lib/libmemstat.so.3
/usr/lib/libmemstat.so
/usr/lib/libprocstat.so.1
/usr/lib/libprocstat.so
/lib/libdevstat.so.7
/usr/lib/libdevstat.so
/lib/libthr.so.3
/usr/lib/libthr.so        # <- easy to miss! libpthread.so -> libthr.so -> ../../lib/libthr.so.3
/usr/lib/libpthread.so    # the WHOLE symlink chain must exist, not just the final target
/lib/libgcc_s.so.1
/usr/lib/libgcc_s.so
/usr/lib/crt1.o
/usr/lib/Scrt1.o
/usr/lib/crti.o
/usr/lib/crtn.o
/usr/lib/crtbegin.o
/usr/lib/crtbeginS.o
/usr/lib/crtbeginT.o
/usr/lib/crtend.o
/usr/lib/crtendS.o
```

**Gotcha:** `usr/lib/libpthread.so` is a *relative* symlink to `libthr.so`
(not `libthr.so.3` directly). If you only copy the final target
(`/lib/libthr.so.3`) and forget the intermediate `usr/lib/libthr.so` symlink,
linking fails with `unable to find library -lpthread` even though
`libthr.so.3` is right there - the chain is broken. Copy the whole chain of
symlinks, not just where they eventually point.

Pull them (adjust the SSH target), preserving the `lib/` vs `usr/lib/`
layout, e.g. with `rsync --files-from`:

```sh
cat > /tmp/sysroot_files.txt <<'EOF'
/lib/libc.so.7
/usr/lib/libc.so
/usr/lib/libc_nonshared.a
/lib/libm.so.5
/usr/lib/libm.so
/lib/libutil.so.9
/usr/lib/libutil.so
/lib/librt.so.1
/usr/lib/librt.so
/usr/lib/libexecinfo.so.1
/usr/lib/libexecinfo.so
/lib/libkvm.so.7
/usr/lib/libkvm.so
/usr/lib/libmemstat.so.3
/usr/lib/libmemstat.so
/usr/lib/libprocstat.so.1
/usr/lib/libprocstat.so
/lib/libdevstat.so.7
/usr/lib/libdevstat.so
/lib/libthr.so.3
/usr/lib/libthr.so
/usr/lib/libpthread.so
/lib/libgcc_s.so.1
/usr/lib/libgcc_s.so
/usr/lib/crt1.o
/usr/lib/Scrt1.o
/usr/lib/crti.o
/usr/lib/crtn.o
/usr/lib/crtbegin.o
/usr/lib/crtbeginS.o
/usr/lib/crtbeginT.o
/usr/lib/crtend.o
/usr/lib/crtendS.o
EOF
rsync -avz --files-from=/tmp/sysroot_files.txt --relative <freebsd-host>:/ ./freebsd-sysroot/
```

### 1.4a Building the same sysroot without a live FreeBSD host (CI)

`.woodpecker/release.yaml`'s `freebsd-pkg` step needs the same file list but
has no live FreeBSD box to `rsync` from - only outbound internet access. It
instead downloads a release's `base.txz` distribution set directly from
`download.freebsd.org` and extracts just the needed members with GNU tar's
selective extraction (`tar -xJf base.txz -C sysroot <member> <member> ...`).

This needs two adjustments relative to the `rsync` recipe above, both found
by actually downloading and inspecting a real FreeBSD **15.0-RELEASE**
`base.txz` (`tar -tvJf base.txz`, ~30k entries) after the CI step first
failed with every single member reporting "Not found in archive":

- **Every member needs a literal leading `./`** (e.g. `./lib/libc.so.7`, not
  `lib/libc.so.7` or `/lib/libc.so.7`) - `base.txz`'s tar members are all
  stored as `./lib/...`/`./usr/lib/...`, and GNU tar's selective extraction
  does **not** normalize away a missing/extra leading `./` when matching
  member names given on the command line - it needs an exact string match.
  (This is unrelated to the section 2.1 `.pkg`-building gotcha, which is
  about `-P`/`--transform` and tar member names on *write*, not `extract`
  argument matching on *read* - don't conflate the two.)
- **`libutil.so.9` is `libutil.so.10` in FreeBSD 15** (soname bump between
  14.5 and 15.0) - confirmed via `grep libutil /tmp/full_list.txt` against
  the real 15.0-RELEASE `base.txz`. Every other file in the list is
  unchanged between 14.5 and 15.0.

If a future FreeBSD release moves another soname, the same
download-and-`tar -tvJf`-grep process will find the new name - don't
re-guess from the 14.5 list.

### 1.5 Cargo/linker configuration

`.cargo/config.toml` (this exact file is **gitignored** - see below - because
the sysroot path is machine-specific; regenerate it per-environment):

```toml
[target.x86_64-unknown-freebsd]
linker = "clang"
rustflags = [
    "-C", "link-arg=--target=x86_64-unknown-freebsd14",
    "-C", "link-arg=--sysroot=/absolute/path/to/freebsd-sysroot",
    "-C", "link-arg=-fuse-ld=lld",
    "-C", "link-arg=-B/absolute/path/to/freebsd-sysroot/usr/lib",
]
```

Notes:
- `--target=x86_64-unknown-freebsd14` is passed to **clang** (not rustc) so
  clang knows FreeBSD ABI/PIE defaults; the major version (`14`) is enough,
  no need for the full `14.5`.
- `-fuse-ld=lld` avoids relying on Linux's GNU `ld` needing to understand
  anything FreeBSD-specific; `lld` is target-agnostic at the ELF level as
  long as the input objects (which `rustc` already produced correctly for
  the FreeBSD target) are correct.
- `-B<sysroot>/usr/lib` helps clang find the CRT object files
  (`crt1.o`/`Scrt1.o`/`crtbegin*.o`/`crtend*.o`) alongside `--sysroot`.

For CI, generate this file dynamically (e.g. `envsubst` a template, or a
`build.rs`/shell step) rather than committing one with a baked-in path - or
use environment variables instead of a config file:
`CARGO_TARGET_X86_64_UNKNOWN_FREEBSD_LINKER=clang` and put the rest in
`RUSTFLAGS`.

### 1.6 Build

```sh
cargo build --release --target x86_64-unknown-freebsd
```

Verify the output is a real FreeBSD binary:

```sh
readelf -h target/x86_64-unknown-freebsd/release/dak | grep 'OS/ABI'
# OS/ABI:  UNIX - FreeBSD
```

This binary is byte-for-byte a normal FreeBSD executable - it was copied to
a real FreeBSD 14.5 VM and ran correctly against real hardware, with
identical behavior (including the same clean-shutdown/no-thread-leak
properties) to one built natively on FreeBSD with `cargo build` there. There
is no meaningful functional difference between the cross-compiled and
natively-built binary.

### 1.7 Alternatives not used (and why)

- **`cargo-zigbuild`**: considered but not tried. Zig bundles portable
  sysroots for several targets but FreeBSD support is not as mature/well-known
  as its Linux/macOS/Windows support. The manual-sysroot approach above is
  more predictable and was already proven to work in one pass once the right
  file list was found.
- **`pkg`/`libpkg` running on Linux directly**: not possible - `pkg` is a
  FreeBSD-native binary (syscalls, capsicum sandboxing, etc.) and Linux has no
  FreeBSD binary-compatibility layer (unlike FreeBSD's own Linux emulation
  layer, there's no equivalent the other way). This is why package building
  (section 2) hand-constructs the `.pkg` format instead of trying to run real
  `pkg create`.

---

## 2. Building a FreeBSD `.pkg` on Linux

No FreeBSD needed at all for this - verified by round-tripping through the
*real* `pkg` tool on a FreeBSD VM (`pkg add`, `pkg info -l`, `pkg check -s`,
`pkg delete` all worked correctly against a package built by the recipe
below).

### 2.1 Format (reverse-engineered from a real package)

A `.pkg` file is a **zstd-compressed tar archive** (modern `pkg`, FreeBSD
14.x; older releases used `.txz` = xz instead of zstd - check
`xxd yourfile.pkg | head -1`: zstd magic is `28 b5 2f fd`, xz magic is
`fd 37 7a 58 5a`). Confirmed by pulling a real cached package from
`/var/cache/pkg/*.pkg` and inspecting it:

```sh
zstd -dc somepackage.pkg | tar tvf -
```

Contents, in order:
1. `+MANIFEST` - full JSON manifest (see schema below), stored **without** a
   leading slash, at the tar root.
2. `+COMPACT_MANIFEST` - the same JSON minus the `files` and `scripts` keys
   (used for repo catalog browsing without downloading the full package).
   Also no leading slash.
3. The actual files, at their **absolute install paths, WITH a leading
   slash** in the tar member name (e.g. `/usr/local/bin/dak`), owned by
   `root`/`wheel` (uid/gid 0/0), with real permission bits.

**Gotcha (cost real debugging time):** if the real files are stored in the
tar *without* a leading slash (e.g. `usr/local/bin/dak`), `pkg add` fails
with:

```
pkg: File //usr/local/bin/dak not specified in the manifest
```

(note the double slash - `pkg` prepends `/` when checking, so a relative tar
entry name mismatches the manifest's absolute key.) The fix is to give the
files a real leading slash in the tar. With GNU `tar`, this needs
`-P` (`--absolute-names`, so tar doesn't strip a leading `/` for safety) plus
`--transform 's,^usr/,/usr/,'` (anchored so it does *not* also mangle
`+MANIFEST`/`+COMPACT_MANIFEST`, which must stay slash-less). Anchoring to
`^usr/` also makes the transform safely idempotent if it ends up applied more
than once (GNU tar's `--transform` accumulates across the whole command line
rather than applying positionally per-file the way `-C` does - if you build
the tar command by looping and appending `--transform` once per file, it
gets applied multiple times to *everything*, including files added earlier
in the argument list; an anchored, idempotent transform sidesteps needing to
reason about that).

### 2.2 Manifest schema (fields seen in a real manifest)

```json
{
  "name": "dak",
  "origin": "sysutils/dak",
  "version": "0.6.0",
  "comment": "short one-liner",
  "maintainer": "you@example.com",
  "www": "https://...",
  "abi": "FreeBSD:14:amd64",
  "arch": "freebsd:14:x86:64",
  "prefix": "/usr/local",
  "flatsize": 1234567,
  "licenselogic": "single",
  "licenses": ["AGPL3"],
  "desc": "longer, possibly multi-line description",
  "categories": ["sysutils"],
  "deps": {"other-pkg": {"origin": "category/other-pkg", "version": "1.0"}},
  "shlibs_required": ["libfoo.so.1"],
  "files": {
    "/usr/local/bin/dak": {
      "sum": "1$<sha256 hex of file contents>",
      "uname": "root",
      "gname": "wheel",
      "perm": "0555",
      "mtime": 1234567890
    },
    "/usr/local/bin/symlink-example": {
      "sum": "...",
      "uname": "root",
      "gname": "wheel",
      "perm": "0755",
      "symlink_target": "dak",
      "mtime": 1234567890
    }
  },
  "scripts": {"post-install": "shell code", "post-deinstall": "shell code"}
}
```

Notes:
- `sum` is `"1$" + sha256_hexdigest(file_bytes)` - the `1$` is a hash-format
  version prefix.
- `perm` is a **4-digit octal string**, e.g. `"0555"`, not a bare number.
- `abi` and `arch` use different casing/separator conventions from each
  other (`FreeBSD:14:amd64` vs `freebsd:14:x86:64`) - copy both exactly as
  shown, don't try to derive one from the other.
- `deps`/`shlibs_required` were empty/omittable for `dak` (it only links
  base-system libraries, which `pkg` doesn't model as installable
  dependencies) - only add them if actually needed.
- Field order in the JSON does not matter (verified: `pkg` does not
  re-validate manifest field ordering, and the per-file `sum` hashes file
  *content*, not the manifest itself).

### 2.3 Minimal build recipe (Python, using only `tar`/`zstd`/stdlib)

```python
#!/usr/bin/env python3
import hashlib, json, os, subprocess

ROOT = "./stage-root"        # files laid out at their final install paths
OUT = "./dak-0.6.0.pkg"
FILES = {                     # path -> octal perm
    "/usr/local/bin/dak": 0o555,
    "/usr/local/share/doc/dak/README.markdown": 0o644,
}

def sha256_of(p):
    h = hashlib.sha256()
    with open(p, "rb") as f:
        for chunk in iter(lambda: f.read(1 << 20), b""):
            h.update(chunk)
    return h.hexdigest()

files_manifest, flatsize = {}, 0
for rel, perm in FILES.items():
    st = os.stat(ROOT + rel)
    flatsize += st.st_size
    files_manifest[rel] = {
        "sum": "1$" + sha256_of(ROOT + rel),
        "uname": "root", "gname": "wheel",
        "perm": oct(perm)[2:].zfill(4), "mtime": int(st.st_mtime),
    }

manifest = {
    "name": "dak", "origin": "sysutils/dak", "version": "0.6.0",
    "comment": "...", "maintainer": "...", "www": "...",
    "abi": "FreeBSD:14:amd64", "arch": "freebsd:14:x86:64",
    "prefix": "/usr/local", "flatsize": flatsize,
    "licenselogic": "single", "licenses": ["AGPL3"],
    "desc": "...", "categories": ["sysutils"],
    "files": files_manifest,
}
compact = {k: v for k, v in manifest.items() if k not in ("files", "scripts")}

os.makedirs("stage", exist_ok=True)
json.dump(manifest, open("stage/+MANIFEST", "w"), separators=(",", ":"))
json.dump(compact, open("stage/+COMPACT_MANIFEST", "w"), separators=(",", ":"))

tar_cmd = ["tar", "--numeric-owner", "--owner=0", "--group=0",
           "-P", "--transform=s,^usr/,/usr/,",
           "-C", "stage", "-cf", "-", "+MANIFEST", "+COMPACT_MANIFEST"]
for rel in FILES:
    tar_cmd += ["-C", os.path.abspath(ROOT), rel.lstrip("/")]

with open(OUT, "wb") as out:
    p1 = subprocess.Popen(tar_cmd, stdout=subprocess.PIPE)
    subprocess.run(["zstd", "-19", "-q", "-c"], stdin=p1.stdout, stdout=out)
    p1.wait()
```

**Gotcha (found later, when this recipe was promoted into
`scripts/build-freebsd-pkg.py` and actually exercised in real CI - the
throwaway version above was hand-run once and happened not to hit this,
which is why it isn't flagged in section 2.4 below):** GNU tar's `-C` is
positional/cumulative, not reset per use - a later `-C <relative-dir>`
resolves **relative to wherever the previous `-C` left tar's directory
context**, not the original invocation cwd. This command has `-C stage`
first (for the manifest files), then `-C <ROOT>` for every staged file - if
`ROOT` is relative, tar looks for it *inside* `stage/`, not next to it, and
fails with `tar: <ROOT>: Cannot open: No such file or directory`. Fix (also
already applied above): pass `-C` an **absolute** path
(`os.path.abspath(ROOT)`) once more than one `-C` appears in the same tar
invocation.

(This is the exact shape of the script used and validated against the real
`pkg` tool - see section 2.4. It was a throwaway script, not committed
anywhere; extend the `FILES` dict with whatever docs/licenses/examples a
release actually needs.)

### 2.4 Validating the result

Round-trip through the real `pkg` tool (needs a FreeBSD host/VM/jail - see
section 3 for testing without a *full* VM):

```sh
pkg add ./dak-0.6.0.pkg          # install
pkg info -l dak                  # lists installed files - cross-check
pkg check -s dak                 # verifies checksums match
pkg delete -y dak                # clean removal, verify nothing left behind
```

All four passed cleanly on the first correctly-formatted attempt (after
fixing the leading-slash gotcha above).

---

## 3. Testing a package without a full VM (jails)

A FreeBSD **jail** with a fresh base system is a good, cheap way to verify a
package's *runtime* dependencies are complete (as opposed to link-time
dependencies, which can be a superset - see section 4) - much lighter than
spinning up a whole new VM, and it still needs to run on an actual FreeBSD
host/kernel (jails are not containers in the Linux sense - they share the
host FreeBSD kernel, so this step cannot be done from Linux).

### 3.1 Fetch a minimal base system

```sh
mkdir -p /jails/daktest
cd /jails/daktest
fetch https://download.freebsd.org/releases/amd64/amd64/14.5-RELEASE/base.txz
sudo tar -xf base.txz -C .
rm base.txz   # may need sudo depending on how it was fetched
```

(~679MB extracted for 14.5-RELEASE base; ~154MB compressed download.)

### 3.2 Install the package into the jail root - no bootstrap needed

`pkg`'s `-r` flag installs into an arbitrary root without needing `pkg`
itself present there:

```sh
sudo pkg -r /jails/daktest add ./dak-0.6.0.pkg
```

### 3.3 Run something inside the jail

```sh
sudo jail -c path=/jails/daktest mount.devfs host.hostname=daktest ip4=inherit \
    command=/usr/local/bin/dak --help

# Check actual runtime shared-library resolution:
sudo jail -c path=/jails/daktest mount.devfs host.hostname=daktest ip4=inherit \
    command=/usr/bin/ldd /usr/local/bin/dak
```

If a required shared library were missing from the package's dependency
chain, this would fail with a dynamic-linker error (`Shared object "libX.so"
not found`) instead of running the program - a clean run (even one that ends
in an expected *application-level* error, like "no compatible devices
found" when there's no USB access from inside the jail) confirms nothing is
missing.

### 3.4 Cleanup gotchas

```sh
sudo umount /jails/daktest/dev   # may need running twice - devfs got double-mounted
                                   # once in testing; harmless to run twice, umount
                                   # just no-ops/errors on the second call if already gone
sudo chflags -R noschg /jails/daktest   # base-system files ship with the
                                          # system-immutable flag set (init, login,
                                          # su, passwd, libc.so.7, ld-elf.so.1, ...) -
                                          # plain `rm -rf` fails on all of them with
                                          # "Operation not permitted" until cleared
sudo rm -rf /jails/daktest
```

---

## 4. `dak`'s actual runtime dependencies on FreeBSD

Verified via `ldd` inside a bare jail (section 3): only **4** shared objects
are actually loaded at runtime, despite the much longer link-time library
list in section 1.4:

```
libthr.so.3    (pthreads)
libgcc_s.so.1  (compiler runtime, unwinding)
libc.so.7
libm.so.5
```

The extra link-time-only libraries (`libexecinfo`, `libkvm`, `libmemstat`,
`libprocstat`, `libdevstat`) are part of `rustc`'s default FreeBSD link flags
(for `libstd`'s backtrace machinery) but get dropped from the final binary's
`DT_NEEDED` list by the linker's `--as-needed` flag, since `dak` never
actually calls into them. **They're still required at link time** (the
linker needs to find and open them to resolve/verify symbols even if none end
up used) **but not at install/runtime time** - don't be tempted to skip
fetching them into the sysroot just because `ldd` doesn't list them.

This means a `.pkg`'s `shlibs_required` field could, if populated at all,
correctly list just those 4 - though `pkg` seems to tolerate omitting it
entirely for a binary that only depends on base-system libraries (verified:
`dak`'s test package had no `shlibs_required` key at all and installed/ran
fine).

---

## 5. CI TODO / open questions

Nothing below is implemented yet - this is a punch list for whoever wires
this into an actual CI pipeline.

- [ ] Decide where the FreeBSD sysroot for CI comes from: re-derive it fresh
      from a FreeBSD container/VM image on every run (slow but always
      correct), or cache a pre-built tarball of it (fast but needs a process
      for detecting when it's stale against a new FreeBSD release).
- [ ] `cargo check --target x86_64-unknown-freebsd` needs no sysroot at all -
      this should be a fast, mandatory CI job on every PR (see 1.1).
- [ ] Full `cargo build --target x86_64-unknown-freebsd` (needing the
      sysroot) could be a slower, separate/optional job.
- [ ] No automated check exists yet for "did `vendor/mirajazz-freebsd` or
      `vendor/async-hid-freebsd` fall behind a newer upstream crates.io
      release" (this is also called out as a TODO in `README.markdown`) -
      would be good to fold into the same CI effort.
- [ ] The `.pkg`-building script currently only exists as a throwaway; if
      packages become a real release artifact, promote the recipe in
      section 2.3 into a real, tested script under version control (with
      real metadata, not placeholders).
- [ ] Jail-based dependency testing (section 3) isn't automated - could be
      turned into a script that fetches base.txz once, caches it, and
      re-runs the install+ldd+run dance per package build.
- [ ] Nothing here has been tried on FreeBSD architectures other than
      `amd64`, or FreeBSD releases other than 14.5 - version/arch-specific
      details (library sonames, `base.txz` URL shape, jail cleanup
      specifics) may need re-verification.

---

## 6. Animated button images: real-hardware findings

Verified on a real Ajazz AKP03E (protocol v2, VID:PID `0300:3002`) over SSH
against a Linux (Debian 13) test VM, using throwaway (never committed)
example binaries that pushed a drawn-not-static animation to button images
each frame instead of the usual "set once, leave it" usage. Buttons here
means the first 6 device-relative keys (0-5) - the screen-capable ones on
this device model; keys 6-8 have no display at all.

### Sustained throughput: 30fps across 6 buttons is free

A steady 30fps update of all 6 button screens at once ran for a full 3 hours
straight with **zero** HID write errors and **zero** fps dips - averaged
exactly 30.00fps for the entire run, every one of 360 logged 30-second
windows on target, no slowdown trend. Each frame (6 images staged +
1 shared `flush()`) cost ~12ms average / ~24ms worst case, comfortably
inside the 33ms/frame budget. Conclusion: 30fps on every screen at once is
solidly within this device's capability for as long as you'd want to run it,
not just a short demo.

### Max throughput and where the real bottleneck is

Removing all pacing (push frames as fast as the code + device allow) on the
same 6 buttons sustained **~85fps**, not much more - and the reason why is
useful:

- Per frame: encode (draw + JPEG-encode 6 images) ~1.9ms (~16%) vs. HID
  transfer (write + flush) ~9.9ms (~84%). Transfer dominates completely;
  encoding is nearly free.
- Process CPU usage: ~31% of *one* core, with 2 cores available - nowhere
  near CPU-bound.
- Actual USB-bus utilization (measured via `usbmon`, see below): only
  **~2.7%** of the 480Mbit/s High-Speed link.
- What *is* saturated: the rate of individual HID reports going out -
  **~1,600 reports/sec (~0.62ms/report)**, matching the observed frame rate
  almost exactly (a 6-button frame takes ~19 reports: a "BAT" header +
  ~2 image-data chunks per button, plus one shared "STP" flush report).

So the bottleneck is neither this host's CPU nor raw USB bandwidth - it's
the **number of individual HID reports per update**, each a fixed 1024-byte
interrupt-OUT transaction (protocol v2's `packet_size` is 1024) regardless of
how little of that report is meaningful payload. A short command (the image
header, the flush) costs exactly as much wire time as a full data chunk.
This means: fewer, larger writes would help throughput far more than a
faster host, less CPU work, or smaller images - there's currently no way to
batch multiple buttons' image data into fewer reports (mirajazz's
`send_image` always emits one "BAT" header per button), so this is a
protocol-level ceiling, not something dak's own code controls.

### Gotcha: `Device::shutdown()` blanks the display like `sleep()`

`Device::shutdown()`'s final wire command is byte-for-byte the same as
`Device::sleep()` (`"HAN"`). A test/demo that runs for a fixed duration and
then calls `shutdown()` will have already gone dark by the time a human
actually looks at the screen, since there's essentially always some delay
between "the program finished" and "someone checks" - this bit a first
attempt at an animation demo (ran, finished, blanked, all within the time it
took to report back "done"). Fix: for anything meant to be *watched live*,
loop until interrupted (SIGINT/`kill -INT <pid>`) instead of a fixed
duration, so there's a real window to look during which the screen is
still lit and updating.

### mirajazz tip: getting the encoded byte size

`Device::set_button_image` (and the `ButtonDevice` trait dak wraps it with)
hides the encoded JPEG size - it encodes and caches internally, returning
only `Result<(), _>`. Both `mirajazz::images::convert_image_with_format`
(the `pub fn` that does the encoding) and `Device::write_image` (the
`pub` method that stages already-encoded bytes, which
`set_button_image` calls internally after encoding) are public, so calling
them directly instead - encode yourself, then stage the bytes - gets you the
byte count for free, with an identical wire format either way.

### Gotcha: backgrounding a job over a non-interactive `ssh` doesn't reliably detach it

`ssh host 'nohup cmd >log 2>&1 </dev/null & disown; ...'` does start `cmd`
detached on the remote host correctly, **but** the invoking `ssh` command
itself can hang/block well past the point where the remote shell has
finished its own script - don't assume it returning promptly is required or
even expected; verify independently with a fresh `ssh host pgrep ...`
instead of trusting the return of the command that launched the background
job.

The converse bit harder: a local "poll until the remote job finishes" loop
(`ssh host 'while pgrep ...; do sleep 60; done; ...'`) that gets force-killed
from the *local* side (e.g. a tool's own timeout) does not necessarily kill
the *remote* loop - it can be left running as an orphan on the VM
indefinitely. Observed directly: such a loop kept polling for ~7 hours after
the process it was waiting for had already exited (and the loop's own exit
condition had been true the whole time) simply because nothing ever
re-connected to check on it or kill it. Always explicitly check for (and
clean up) stray remote processes after this pattern - killing/timing-out the
local side is not sufficient.

### Measuring real USB-bus traffic with `usbmon`

Independent of whatever the app itself reports, the kernel's `usbmon`
facility gives ground-truth bytes/URBs on the wire:

```sh
# one-time, needs root:
/sbin/modprobe usbmon        # full path: kmod's modprobe may not be on a non-root PATH via `su -c`

# find which USB bus the device is actually on:
lsusb -d 0300:3002           # e.g. "Bus 002 Device 003: ..." -> bus 2

# capture that bus only while the workload runs (bus number 0 = all buses combined):
cat /sys/kernel/debug/usb/usbmon/2u > capture.txt   # needs root; file is mode 0600 root:root
```

Text format, one line per event: `tag timestamp_us S|C|E address
status_or_pending length [hex data...]`, where `address` looks like
`Io:2:003:3` (`I`nterrupt/`C`ontrol, direction `i`n/`o`ut, bus, device
number, endpoint). `S` = submission, `C` = completion - sum the **`C`**
lines' length field for actual transferred bytes (summing both `S` and `C`
double-counts, since every URB gets one of each). One-liner for one
endpoint's total bytes/URB count:

```sh
awk '$3=="C" && $4 ~ /^Io:2:003/ {sum+=$6; n++} END {print n, sum}' capture.txt
```
