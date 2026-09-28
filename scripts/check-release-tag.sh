#!/bin/sh
# Fails unless the tag being released is a plain vX.Y.Z, before any step puts it into
# a file name, a package version or a command line. A tag is chosen by whoever can
# push, and later steps expand it inside shell commands.
#
# Usage: check-release-tag.sh <tag>
set -eu

if [ "$#" -ne 1 ]; then
    echo "usage: $0 <tag>" >&2
    exit 2
fi
case "$1" in
    *[!v0-9.]*) ;; # anything else (a newline, a space, `$`, ...) is refused below
    *)
        if printf '%s\n' "$1" | grep -Eqx 'v[0-9]{1,4}\.[0-9]{1,4}\.[0-9]{1,4}'; then
            exit 0
        fi
        ;;
esac
echo "refusing to release tag \"$1\": expected vX.Y.Z" >&2
exit 1
