#!/usr/bin/env sh
# Prints the CHANGELOG.md section for one version — the release notes that
# end up on the GitHub Release.
#
# Extracted so there is exactly one implementation, run by all three of:
#   - `.github/workflows/release.yml`, to build the Release body
#   - `.github/workflows/ci.yml`, as a guard on every push
#   - `/bump`, as a preflight before the tag exists
# Them agreeing is the whole point: a heading this cannot find fails the
# release job *after* the tag is public, and a public tag cannot be
# pushed over.
#
# Usage: scripts/release-notes.sh <version> [changelog-path]
# Exits non-zero, with a message on stderr, when no section matches.
set -eu

VERSION="${1:?usage: release-notes.sh <version> [changelog-path]}"
CHANGELOG="${2:-CHANGELOG.md}"

# The heading format is a contract, not a style choice, and this enforces
# all of it: `## [<version>] - YYYY-MM-DD`, matched from the start of the
# line. A prefix match alone would accept `## [1.2.3] -` with no date and
# `## [1.2.3] -nonsense`, publishing a release from a heading the format
# says is malformed. `## [1.2.3]` with no date, or extra leading space,
# matches nothing.
NOTES="$(
  awk -v verline="## [$VERSION] -" '
    /^## \[/ {
      if (found) exit
      if (index($0, verline) == 1) {
        rest = substr($0, length(verline) + 1)
        # " YYYY-MM-DD", trailing blanks tolerated. Digits spelled out
        # rather than {4}: interval expressions are not portable across
        # the awks this runs on (GNU on CI ubuntu, BWK on macOS).
        if (rest ~ /^ [0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9] *$/) {
          found = 1
          next
        }
      }
    }
    found { print }
  ' "$CHANGELOG" |
    # Drop the link-reference footer, then trim leading blank lines.
    sed -e '/^\[.*\]: http/d' -e '/./,$!d'
)"

if [ -z "$NOTES" ]; then
  echo "no $CHANGELOG section found for version $VERSION" >&2
  echo "expected a heading starting exactly: ## [$VERSION] - YYYY-MM-DD" >&2
  exit 1
fi

printf '%s\n' "$NOTES"
