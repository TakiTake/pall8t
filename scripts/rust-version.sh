#!/usr/bin/env sh
# Prints the Rust version the dev toolchain is pinned to, read from
# flake.nix — the one place it is declared.
#
# Extracted so there is exactly one implementation, the way
# release-notes.sh is: every workflow that installs a toolchain asks this
# script, so CI and release cannot drift from what `nix develop` gives a
# developer. A CI that lints and tests on a different compiler than the
# one people build with is testing a configuration nobody ships (issue
# #91, finding 10 of #83).
#
# Usage: scripts/rust-version.sh [flake-path]
# Exits non-zero, with a message on stderr, when no pin is found — which
# means the flake changed shape and the workflows are about to install
# whatever `stable` happens to be that day, silently.
set -eu

FLAKE="${1:-flake.nix}"

# `rust-bin.stable."1.96.0"` — the quoted version is the pin. Matched from
# the attribute path rather than by looking for any version-shaped string,
# so an unrelated version elsewhere in the file cannot answer this.
VERSION="$(
  sed -n 's/.*rust-bin\.stable\."\([0-9][0-9.]*\)".*/\1/p' "$FLAKE" | head -n 1
)"

if [ -z "$VERSION" ]; then
    echo "no \`rust-bin.stable.\"<version>\"\` pin found in $FLAKE" >&2
    exit 1
fi

echo "$VERSION"
