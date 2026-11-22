#!/usr/bin/env bash
#
# One-command release: build old-glibc compatible frog binaries (x86_64 +
# aarch64) and (re)publish them to GitHub.
#
#   ./scripts/release.sh [tag]
#
# Flow (for every arch in $FROG_ARCHES, default "x86_64 aarch64"):
#   1. scripts/build-old-glibc.sh <arch> -> dist/frog-<arch>
#   2. gzip -kf dist/frog-<arch>          -> dist/frog-<arch>.gz
#   3. gh release create <tag>             -> new GitHub release with all .gz assets,
#      or gh release upload --clobber when the release already exists
#      ("refresh" with the newly built binaries).
#
# Only build one arch:  FROG_ARCHES="x86_64" ./scripts/release.sh [tag]
#
# The tag defaults to v<VERSION> from Cargo.toml (e.g. v0.8.0).
# Release notes default to the matching CHANGELOG.md section.
#
# Requires: docker (via build-old-glibc.sh) and an authenticated gh
# (run `gh auth login` once, or export GH_TOKEN).
#
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_DIR="$(dirname "$SCRIPT_DIR")"

ARCHES="${FROG_ARCHES:-x86_64 aarch64}"

cd "$REPO_DIR"

if ! command -v gh >/dev/null 2>&1; then
    echo "error: gh (GitHub CLI) is required to publish the release" >&2
    exit 1
fi
if ! gh auth status >/dev/null 2>&1; then
    echo "error: gh is not authenticated (run 'gh auth login' or export GH_TOKEN)" >&2
    exit 1
fi

if [ -z "$ARCHES" ]; then
    echo "error: FROG_ARCHES is empty (want e.g. \"x86_64 aarch64\")" >&2
    exit 1
fi

VERSION="$(grep -m1 '^version = ' Cargo.toml | cut -d'"' -f2)"
if [ -z "$VERSION" ]; then
    echo "error: could not parse version from Cargo.toml" >&2
    exit 1
fi
TAG="${1:-v${VERSION}}"

if ! git diff --quiet || ! git diff --cached --quiet; then
    echo "warning: working tree has uncommitted changes — the binaries are built"
    echo "         from the working tree, but the release tag will point at HEAD"
    echo "         ($(git rev-parse --short HEAD)). Commit first for a reproducible release."
fi

ASSETS=()
for ARCH in $ARCHES; do
    case "$ARCH" in
        x86_64|aarch64) ;;
        *)
            echo "error: unknown architecture '$ARCH' in FROG_ARCHES (want x86_64 and/or aarch64)" >&2
            exit 1
            ;;
    esac

    BINARY="${REPO_DIR}/dist/frog-${ARCH}"
    GZBINARY="${BINARY}.gz"

    echo "==> [${ARCH}] Step 1/3: building old-glibc binary..."
    "${SCRIPT_DIR}/build-old-glibc.sh" "$ARCH"

    if [ ! -x "$BINARY" ]; then
        echo "error: expected binary missing after build: $BINARY" >&2
        exit 1
    fi

    echo "==> [${ARCH}] Step 2/3: gzipping binary..."
    gzip -kf "$BINARY"
    ls -lh "$BINARY" "$GZBINARY"
    MAXGLIBC="$(objdump -T "$BINARY" 2>/dev/null | grep -o 'GLIBC_[0-9.]*' | sort -uV | tail -1 || true)"
    if [ -n "$MAXGLIBC" ]; then
        echo "Max GLIBC symbol version required (${ARCH}): ${MAXGLIBC}"
    else
        echo "Note: host objdump cannot inspect the ${ARCH} binary (foreign arch); the container already verified it."
    fi

    ASSETS+=("$GZBINARY")
done

if [ "${#ASSETS[@]}" -eq 0 ]; then
    echo "error: nothing was built (FROG_ARCHES='${ARCHES}')" >&2
    exit 1
fi

echo "==> Step 3/3: publishing GitHub release ${TAG} (${#ASSETS[@]} assets)..."
NOTES_FILE="$(mktemp)"
trap 'rm -f "$NOTES_FILE"' EXIT
awk -v h="## [$VERSION]" 'index($0, h) == 1 {flag=1; next} /^## / {flag=0} flag' \
    CHANGELOG.md | sed '/./,$!d' > "$NOTES_FILE"
if [ ! -s "$NOTES_FILE" ]; then
    echo "frog $TAG — old-glibc (Ubuntu 20.04) builds." > "$NOTES_FILE"
fi
{
    echo ""
    echo "Binaries: old-glibc (Ubuntu 20.04, glibc <= 2.34) builds for: ${ARCHES}."
    echo "Each target server still needs its matching-arch Oracle Instant Client + libgcc."
} >> "$NOTES_FILE"

if gh release view "$TAG" >/dev/null 2>&1; then
    echo "Release $TAG exists — refreshing its assets..."
    for ASSET in "${ASSETS[@]}"; do
        gh release upload "$TAG" "$ASSET" --clobber
    done
else
    gh release create "$TAG" "${ASSETS[@]}" \
        --title "frog $TAG" \
        --notes-file "$NOTES_FILE" \
        --target "$(git rev-parse HEAD)"
fi

echo
echo "Done: $(gh release view "$TAG" --json url -q .url)"
