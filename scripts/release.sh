#!/usr/bin/env bash
# Bump the patch version of cpaq and refresh Cargo.lock.
#
# Release policy (owner requirement): patch-only releases on the 0.1.x line.
# The major version is pinned at 0 and the minor at 1, so released versions are
# always 0.1.N. A manifest outside that line is a manual decision: the script
# fails and never rewrites the version it was given.
#
# Usage: scripts/release.sh [--dry-run]

set -euo pipefail

usage() {
    cat <<'EOF'
usage: scripts/release.sh [--dry-run]

Bumps the patch component of the [package] version in Cargo.toml, keeps the
major/minor pinned at 0.1, and refreshes Cargo.lock.

  --dry-run   print the version change and exit without writing anything
  -h, --help  show this help

stdout:
  dry-run  one line: "<current> -> <next>"
  real     the same line, then the new version alone on the last line
EOF
}

dry_run=0
while [ "$#" -gt 0 ]; do
    case "$1" in
        --dry-run) dry_run=1 ;;
        -h | --help)
            usage
            exit 0
            ;;
        *)
            echo "release: unknown argument: $1" >&2
            usage >&2
            exit 2
            ;;
    esac
    shift
done

cd -- "$(dirname -- "$0")/.."
manifest=Cargo.toml
if [ ! -f "$manifest" ]; then
    echo "release: $manifest not found in $(pwd)" >&2
    exit 1
fi

# Read the version from the [package] table only, so dependency version lines
# are never considered.
current=$(
    awk '
        /^\[/ { section = $0 }
        section == "[package]" && /^version[[:space:]]*=/ {
            line = $0
            sub(/^version[[:space:]]*=[[:space:]]*"/, "", line)
            sub(/".*$/, "", line)
            print line
            exit
        }
    ' "$manifest"
)

if ! [[ "$current" =~ ^([0-9]+)\.([0-9]+)\.([0-9]+)$ ]]; then
    echo "release: cannot parse the [package] version in $manifest: '$current'" >&2
    exit 1
fi
major=${BASH_REMATCH[1]}
minor=${BASH_REMATCH[2]}
patch=${BASH_REMATCH[3]}

if [ "$major" != "0" ]; then
    echo "release: refusing to release $current: the major version is pinned at 0 and versions stay 0.1.N" >&2
    exit 1
fi
if [ "$minor" != "1" ]; then
    echo "release: refusing to release $current: the minor version is pinned at 1 and versions stay 0.1.N" >&2
    echo "release: fix Cargo.toml by hand before releasing; this script never changes the minor" >&2
    exit 1
fi

next="0.1.$((10#$patch + 1))"

if [ "$dry_run" = "1" ]; then
    printf '%s -> %s\n' "$current" "$next"
    exit 0
fi

tmp=$(mktemp)
trap 'rm -f "$tmp"' EXIT

awk -v newver="$next" '
    /^\[/ { section = $0 }
    section == "[package]" && !done && /^version[[:space:]]*=/ {
        print "version = \"" newver "\""
        done = 1
        next
    }
    { print }
    END { if (!done) exit 3 }
' "$manifest" >"$tmp" || {
    echo "release: failed to rewrite the [package] version in $manifest" >&2
    exit 1
}
cat -- "$tmp" >"$manifest"

if ! command -v cargo >/dev/null 2>&1; then
    echo "release: cargo not found; cannot refresh Cargo.lock" >&2
    exit 1
fi
cargo metadata --format-version 1 >/dev/null

if ! grep -A1 '^name = "cpaq"$' Cargo.lock | grep -qF "version = \"$next\""; then
    echo "release: Cargo.lock still does not record cpaq $next" >&2
    exit 1
fi

echo "release: bumped cpaq from $current to $next" >&2
printf '%s -> %s\n' "$current" "$next"
printf '%s\n' "$next"
