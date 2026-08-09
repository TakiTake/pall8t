#!/bin/sh
# pall8t lint gate: rustfmt + clippy (-D warnings) across every supported
# target. Shared by the pre-commit hook (.githooks/pre-commit) and run by
# hand. CI (.github/workflows/ci.yml) enforces the same lints but via its
# own OS matrix — it lints each OS natively (stronger than this script's
# cross-lint-from-one-host), so it deliberately does not call this.
#
# Clippy is run once per target on purpose: a lint behind a
# `#[cfg(target_os = "...")]` gate is invisible on the host triple, so a
# macOS-only warning would otherwise only surface in CI's macos runner (or on
# a user's Mac). The tree has no such gates at the moment (ADR-0008 removed
# the last ones) — this is the standing guard for the next one, not dead
# ceremony. To keep that guarantee we never *silently* skip the darwin leg.
# The flake toolchain (`nix develop` on the host, preinstalled in the pall8t
# dev container) ships the target std already; a rustup toolchain gets it
# installed on demand (idempotent). Only a toolchain that lacks the target
# std with no rustup to add it skips the leg, and loudly.
set -eu

CROSS_TARGET=aarch64-apple-darwin

echo "lint: cargo fmt --check"
cargo fmt --check

echo "lint: cargo clippy --all-targets (host)"
cargo clippy --all-targets -- -D warnings

if command -v rustup >/dev/null 2>&1 && \
   ! rustup target list --installed | grep -qx "$CROSS_TARGET"; then
    echo "lint: installing missing cross-lint target $CROSS_TARGET"
    rustup target add "$CROSS_TARGET"
fi

if [ -d "$(rustc --print target-libdir --target "$CROSS_TARGET")" ]; then
    echo "lint: cargo clippy --all-targets ($CROSS_TARGET)"
    cargo clippy --all-targets --target "$CROSS_TARGET" -- -D warnings
else
    echo "lint: WARNING: no std for $CROSS_TARGET in this toolchain (and no rustup" \
         "to add it) — skipping the cross-lint; cfg(target_os = \"macos\") code is" \
         "NOT linted locally (CI still covers it)." >&2
fi
