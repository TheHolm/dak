#!/usr/bin/env bash
# Checks whether the OS versions .woodpecker/release.yaml targets have gone
# stale, so that doesn't have to be discovered by an actual release build
# failing months from now. Meant to run periodically (see
# .woodpecker/check-target-freshness.yaml's cron trigger), not on every
# commit - these versions change on the order of months/years, not days.
#
# Checks three independent things, each against the pin currently hardcoded
# in .woodpecker/release.yaml:
#   1. Has Debian trixie been demoted from "stable" to "oldstable"?
#   2. Is Ubuntu 26.04 still the latest LTS release?
#   3. Is FreeBSD 15.0-RELEASE still the latest FreeBSD release?
#
# Exits non-zero (so the CI job shows red) if any pin is stale - that's the
# whole point, to be loud rather than silently drift. Exits non-zero on a
# network/parse failure too (fail loud rather than silently report "fresh"
# when a check couldn't actually run).
set -euo pipefail

# Keep these in sync with .woodpecker/release.yaml by hand - deliberately
# not derived automatically from that file, so a change to one doesn't
# silently change what this script considers "current" without a human
# noticing the two have diverged.
DEBIAN_CODENAME_PINNED="trixie"
UBUNTU_LTS_PINNED="26.04"
FREEBSD_RELEASE_PINNED="15.1-RELEASE"

stale=0

echo "== Debian: pinned to '${DEBIAN_CODENAME_PINNED}' =="
debian_release_info="$(curl -fsSL https://deb.debian.org/debian/dists/stable/Release)"
debian_stable_codename="$(echo "$debian_release_info" | grep '^Codename:' | awk '{print $2}')"
if [[ -z "$debian_stable_codename" ]]; then
    echo "  ERROR: could not determine current Debian stable codename" >&2
    exit 1
fi
if [[ "$debian_stable_codename" != "$DEBIAN_CODENAME_PINNED" ]]; then
    echo "  STALE: Debian stable is now '${debian_stable_codename}' - '${DEBIAN_CODENAME_PINNED}' has been demoted to oldstable (or older)."
    echo "         Update docker image tag in .woodpecker/release.yaml's deb-trixie step (rust:1.92-trixie -> rust:1.92-${debian_stable_codename} or similar) and this script's DEBIAN_CODENAME_PINNED."
    stale=1
else
    echo "  OK: '${DEBIAN_CODENAME_PINNED}' is still Debian stable."
fi

echo "== Ubuntu: pinned to '${UBUNTU_LTS_PINNED}' LTS =="
# meta-release-lts lists every LTS release as its own "Version: X.Y.Z LTS"
# block, oldest first - the last one is the newest. Strip the point-release
# suffix and " LTS" text to get a bare "X.Y" to compare against the pin.
latest_lts_raw="$(curl -fsSL https://changelogs.ubuntu.com/meta-release-lts | grep '^Version:' | tail -1 | awk '{print $2}')"
latest_lts="$(echo "$latest_lts_raw" | grep -oE '^[0-9]+\.[0-9]+')"
if [[ -z "$latest_lts" ]]; then
    echo "  ERROR: could not determine latest Ubuntu LTS version" >&2
    exit 1
fi
if [[ "$latest_lts" != "$UBUNTU_LTS_PINNED" ]]; then
    echo "  STALE: latest Ubuntu LTS is now '${latest_lts}' - '${UBUNTU_LTS_PINNED}' is no longer the newest LTS."
    echo "         Update the image tag in .woodpecker/release.yaml's deb-ubuntu2604 step (ubuntu:${UBUNTU_LTS_PINNED} -> ubuntu:${latest_lts}), the step's name, and this script's UBUNTU_LTS_PINNED."
    stale=1
else
    echo "  OK: '${UBUNTU_LTS_PINNED}' is still the latest Ubuntu LTS release."
fi

echo "== FreeBSD: pinned to '${FREEBSD_RELEASE_PINNED}' =="
latest_freebsd="$(curl -fsSL https://download.freebsd.org/releases/amd64/amd64/ | grep -oE '[0-9]+\.[0-9]+-RELEASE' | sort -V -u | tail -1)"
if [[ -z "$latest_freebsd" ]]; then
    echo "  ERROR: could not determine latest FreeBSD release" >&2
    exit 1
fi
if [[ "$latest_freebsd" != "$FREEBSD_RELEASE_PINNED" ]]; then
    echo "  STALE: latest FreeBSD release is now '${latest_freebsd}' - '${FREEBSD_RELEASE_PINNED}' is no longer the newest."
    echo "         Update FREEBSD_RELEASE-equivalent URL in .woodpecker/release.yaml's freebsd-pkg step and this script's FREEBSD_RELEASE_PINNED."
    echo "         Re-verify the sysroot file list against the new base.txz per NOTES.md section 1.4a - sonames do change between releases (e.g. libutil.so.9 -> .so.10 between 14.5 and 15.0)."
    stale=1
else
    echo "  OK: '${FREEBSD_RELEASE_PINNED}' is still the latest FreeBSD release."
fi

if [[ "$stale" -ne 0 ]]; then
    echo
    echo "One or more CI target versions are stale - see STALE lines above."
    exit 1
fi

echo
echo "All CI target versions are current."
