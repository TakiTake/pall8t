#!/bin/sh
# pall8t lint gate: rustfmt + clippy (-D warnings) across every supported
# target. Shared by the pre-commit hook (.githooks/pre-commit) and
# `mise run lint`. CI (.github/workflows/ci.yml) enforces the same lints but
# via its own OS matrix — it lints each OS natively (stronger than this
# script's cross-lint-from-one-host), so it deliberately does not call this.
#
# Clippy is run once per target on purpose: a lint behind a
# `#[cfg(target_os = "...")]` gate is invisible on the host triple, so a
# macOS-only warning would otherwise only surface in CI's macos runner (or on
# a user's Mac). To keep that guarantee we never *silently* skip the darwin
# leg: when rustup is present we install the target on demand (idempotent, a
# no-op in the pall8t container where the image already added it) and then
# lint it; only a toolchain without rustup at all skips it, and loudly.
set -eu

CROSS_TARGET=aarch64-apple-darwin

echo "lint: cargo fmt --check"
cargo fmt --check

echo "lint: cargo clippy --all-targets (host)"
cargo clippy --all-targets -- -D warnings

if command -v rustup >/dev/null 2>&1; then
    if ! rustup target list --installed | grep -qx "$CROSS_TARGET"; then
        echo "lint: installing missing cross-lint target $CROSS_TARGET"
        rustup target add "$CROSS_TARGET"
    fi
    echo "lint: cargo clippy --all-targets ($CROSS_TARGET)"
    cargo clippy --all-targets --target "$CROSS_TARGET" -- -D warnings
else
    echo "lint: WARNING: rustup not found — skipping the $CROSS_TARGET cross-lint;" \
         "cfg(target_os = \"macos\") code is NOT linted locally (CI still covers it)." >&2
fi
