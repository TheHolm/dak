#!/bin/sh
# Downloads URL to FILE and checks it against the SHA-256 published at URL.sha256 (the
# convention of static.rust-lang.org, among others: "<hex>  <name>"), so nothing has to
# be piped straight into a shell and no checksum is written into this repository.
#
# Usage: fetch-verified.sh <url> <output file>
set -eu

if [ "$#" -ne 2 ]; then
    echo "usage: $0 <url> <output file>" >&2
    exit 2
fi
url=$1
output=$2

expected=$(curl -fsSL --proto '=https' --tlsv1.2 "$url.sha256" | cut -d' ' -f1)
if [ "${#expected}" -ne 64 ]; then
    echo "no SHA-256 published at $url.sha256" >&2
    exit 1
fi
curl -fsSL --proto '=https' --tlsv1.2 -o "$output" "$url"
actual=$(sha256sum "$output" | cut -d' ' -f1)
if [ "$actual" != "$expected" ]; then
    echo "$url: SHA-256 $actual does not match the published $expected" >&2
    rm -f "$output"
    exit 1
fi
echo "$url verified ($expected)"
