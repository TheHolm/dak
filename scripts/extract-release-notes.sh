#!/usr/bin/env bash
# Extracts just one release's section from RELEASE_NOTES.md, for use as a
# GitHub Release body (`gh release create --notes-file`) - RELEASE_NOTES.md
# itself holds the full history of every release (see AGENTS.md), so it
# can't be used verbatim as a single release's notes.
#
# Each entry starts with a line like "## v0.8.1 — <summary>" and runs until
# the next "## v" heading (or EOF for the newest entry, since it's at the
# top of the file - see AGENTS.md's release convention).
#
# Usage: extract-release-notes.sh <tag> <release-notes-file>
# Prints the section to stdout; exits 1 if the tag has no matching heading.
set -euo pipefail

if [[ $# -ne 2 ]]; then
    echo "usage: $(basename "$0") <tag> <release-notes-file>" >&2
    exit 1
fi

tag="$1"
file="$2"

section="$(awk -v tag="## ${tag} " '
    $0 ~ "^" tag { found=1; print; next }
    found && /^## v/ { exit }
    found { print }
' "$file")"

if [[ -z "$section" ]]; then
    echo "error: no \"## ${tag} \" heading found in ${file}" >&2
    exit 1
fi

printf '%s\n' "$section"
