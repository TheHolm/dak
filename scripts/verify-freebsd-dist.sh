#!/bin/sh
# Downloads one FreeBSD distribution set (e.g. base.txz) and verifies it against the
# release's MANIFEST, without any checksum written into this repository.
#
# Usage: verify-freebsd-dist.sh <release path> <set> <output file>
#   e.g. verify-freebsd-dist.sh releases/amd64/amd64/15.1-RELEASE base.txz /tmp/base.txz
#
# FreeBSD publishes a MANIFEST next to each release's distribution sets, listing every
# set's SHA-256. A MANIFEST fetched from the same server as the set only proves the
# download was not corrupted, so it is fetched from the primary server
# (download.freebsd.org) AND from independent official mirrors: all the MANIFESTs that
# could be fetched must be identical, and at least one mirror must have answered. An
# attacker would have to control the primary and every reachable mirror at once.
#
# Mirrors can be overridden (space separated base URLs) with FREEBSD_MIRRORS; the
# primary with FREEBSD_PRIMARY. Needs curl and sha256sum (or sha256).
set -eu

if [ "$#" -ne 3 ]; then
    echo "usage: $0 <release path> <set> <output file>" >&2
    exit 2
fi
release=$1
set_name=$2
output=$3

primary=${FREEBSD_PRIMARY:-https://download.freebsd.org}
mirrors=${FREEBSD_MIRRORS:-https://ftp.de.freebsd.org/pub/FreeBSD https://mirror.aarnet.edu.au/pub/FreeBSD}

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

# sha256_of FILE: prints the hex SHA-256 of FILE.
sha256_of() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | cut -d' ' -f1
    else
        sha256 -q "$1"
    fi
}

# manifest_hash MANIFEST SET: prints SET's hash from MANIFEST (tab separated: name,
# sha256, ...), or nothing when SET is not listed.
manifest_hash() {
    awk -F '\t' -v set="$2" '$1 == set { print $2; exit }' "$1"
}

curl -fsSL --proto '=https' --tlsv1.2 -o "$work/MANIFEST.primary" "$primary/$release/MANIFEST"
expected=$(manifest_hash "$work/MANIFEST.primary" "$set_name")
case "$expected" in
    [0-9a-f][0-9a-f][0-9a-f][0-9a-f]*) ;;
    *)
        echo "$set_name is not listed in $primary/$release/MANIFEST" >&2
        exit 1
        ;;
esac
if [ "${#expected}" -ne 64 ]; then
    echo "MANIFEST hash for $set_name is not a SHA-256: $expected" >&2
    exit 1
fi

agreeing=0
index=0
for mirror in $mirrors; do
    index=$((index + 1))
    copy="$work/MANIFEST.mirror$index"
    if ! curl -fsSL --proto '=https' --tlsv1.2 -o "$copy" "$mirror/$release/MANIFEST"; then
        echo "note: could not fetch MANIFEST from $mirror; skipping it" >&2
        continue
    fi
    if ! cmp -s "$work/MANIFEST.primary" "$copy"; then
        echo "MANIFEST from $mirror differs from $primary's; refusing to trust either" >&2
        exit 1
    fi
    agreeing=$((agreeing + 1))
done
if [ "$agreeing" -eq 0 ]; then
    echo "no mirror could confirm $primary's MANIFEST" >&2
    exit 1
fi

curl -fsSL --proto '=https' --tlsv1.2 -o "$output" "$primary/$release/$set_name"
actual=$(sha256_of "$output")
if [ "$actual" != "$expected" ]; then
    echo "$set_name: SHA-256 $actual does not match MANIFEST's $expected" >&2
    rm -f "$output"
    exit 1
fi
echo "$set_name verified against MANIFEST ($expected; confirmed by $agreeing mirror(s))"
