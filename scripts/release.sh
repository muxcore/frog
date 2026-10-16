#!/usr/bin/env bash
#
# One-command release: build an old-glibc compatible frog binary and
# (re)publish it to GitHub.
#
#   ./scripts/release.sh [tag]
#
# Flow:
#   1. scripts/build-old-glibc.sh   -> dist/frog
#   2. gzip -kf dist/frog            -> dist/frog.gz
#   3. gh release create <tag>       -> new GitHub release with frog.gz,
#      or gh release upload --clobber when the release already exists
#      ("refresh" with the newly built binary).
#
# The tag defaults to v<VERSION> from Cargo.toml (e.g. v0.5.6).
# Release notes default to the matching CHANGELOG.md section.
#
# Requires: docker (via build-old-glibc.sh) and an authenticated gh
# (run `gh auth login` once, or export GH_TOKEN).
#
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_DIR="$(dirname "$SCRIPT_DIR")"

BINARY="${REPO_DIR}/dist/frog"
GZBINARY="${REPO_DIR}/dist/frog.gz"

cd "$REPO_DIR"

if ! command -v gh >/dev/null 2>&1; then
    echo "error: gh (GitHub CLI) is required to publish the release" >&2
    exit 1
fi
if ! gh auth status >/dev/null 2>&1; then
    echo "error: gh is not authenticated (run 'gh auth login' or export GH_TOKEN)" >&2
    exit 1
fi

VERSION="$(grep -m1 '^version = ' Cargo.toml | cut -d'"' -f2)"
if [ -z "$VERSION" ]; then
    echo "error: could not parse version from Cargo.toml" >&2
    exit 1
fi
TAG="${1:-v${VERSION}}"

if ! git diff --quiet || ! git diff --cached --quiet; then
    echo "warning: working tree has uncommitted changes — the binary is built"
    echo "         from the working tree, but the release tag will point at HEAD"
    echo "         ($(git rev-parse --short HEAD)). Commit first for a reproducible release."
fi

echo "==> Step 1/3: building old-glibc binary..."
"${SCRIPT_DIR}/build-old-glibc.sh"

if [ ! -x "$BINARY" ]; then
    echo "error: expected binary missing after build: $BINARY" >&2
    exit 1
fi

echo "==> Step 2/3: gzipping binary..."
gzip -kf "$BINARY"
ls -lh "$BINARY" "$GZBINARY"
objdump -T "$BINARY" 2>/dev/null \
    | grep -o 'GLIBC_[0-9.]*' | sort -uV | tail -1 \
    | sed 's/^/Max GLIBC symbol version required: /'

echo "==> Step 3/3: publishing GitHub release ${TAG}..."
NOTES_FILE="$(mktemp)"
trap 'rm -f "$NOTES_FILE"' EXIT
awk -v h="## [$VERSION]" 'index($0, h) == 1 {flag=1; next} /^## / {flag=0} flag' \
    CHANGELOG.md | sed '/./,$!d' > "$NOTES_FILE"
if [ ! -s "$NOTES_FILE" ]; then
    echo "frog $TAG — old-glibc (Ubuntu 20.04) build." > "$NOTES_FILE"
fi

if gh release view "$TAG" >/dev/null 2>&1; then
    echo "Release $TAG exists — refreshing its frog.gz asset..."
    gh release upload "$TAG" "$GZBINARY" --clobber
else
    gh release create "$TAG" "$GZBINARY" \
        --title "frog $TAG" \
        --notes-file "$NOTES_FILE" \
        --target "$(git rev-parse HEAD)"
fi

echo
echo "Done: $(gh release view "$TAG" --json url -q .url)"
