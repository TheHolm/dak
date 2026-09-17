#!/usr/bin/env bash
# Builds the source-code tarball release asset ("code tar.gz") via `git
# archive` of the tag being released - no build/toolchain needed, just the
# tagged commit's tree, matching what a GitHub-native tag/release tarball
# would contain.
#
# Usage: make-src-tarball.sh <version> <output-dir>
#   <version> is WITHOUT a leading "v" (e.g. 0.8.1); the git ref archived is
#   "v<version>", matching this repo's vX.Y.Z tag convention (see `git tag -l`).
set -euo pipefail

if [[ $# -ne 2 ]]; then
    echo "usage: $(basename "$0") <version> <output-dir>" >&2
    exit 1
fi

version="$1"
outdir="$2"
ref="v${version}"

mkdir -p "$outdir"
git archive --format=tar.gz --prefix="dak-${version}/" \
    -o "${outdir}/dak-${version}-src.tar.gz" "$ref"
