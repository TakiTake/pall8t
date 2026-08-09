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
# dev container) ships the target std already; a rustup-managed toolchain
# gets it installed on demand (idempotent). Only a toolchain that lacks the
# target std with no rustup managing it skips the leg, and loudly.
set -eu

CROSS_TARGET=aarch64-apple-darwin

echo "lint: cargo fmt --check"
cargo fmt --check

echo "lint: cargo clippy --all-targets (host)"
cargo clippy --all-targets -- -D warnings

# Only consult rustup when the active toolchain actually lacks the target
# std AND rustup manages that toolchain (the sysroot lives under rustup's
# home). Rustup merely being on PATH is not enough: next to a nix toolchain
# it would download a std the lint below never uses, and with no active
# toolchain at all `rustup target add` fails outright — which under `set -e`
# would abort the gate instead of reaching the loud skip below.
if [ ! -d "$(rustc --print target-libdir --target "$CROSS_TARGET")" ] && \
   command -v rustup >/dev/null 2>&1; then
    rustup_home="$(rustup show home 2>/dev/null || true)"
    sysroot="$(rustc --print sysroot)"
    if [ -n "$rustup_home" ] && [ "${sysroot#"$rustup_home"/}" != "$sysroot" ]; then
        echo "lint: installing missing cross-lint target $CROSS_TARGET"
        rustup target add "$CROSS_TARGET"
    fi
fi

if [ -d "$(rustc --print target-libdir --target "$CROSS_TARGET")" ]; then
    echo "lint: cargo clippy --all-targets ($CROSS_TARGET)"
    cargo clippy --all-targets --target "$CROSS_TARGET" -- -D warnings
else
    echo "lint: WARNING: no std for $CROSS_TARGET in this toolchain (and no rustup" \
         "managing it to add one) — skipping the cross-lint; cfg(target_os =" \
         "\"macos\") code is NOT linted locally (CI still covers it)." >&2
fi
