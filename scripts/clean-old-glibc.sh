#!/usr/bin/env bash
#
# Remove everything the old-glibc container builds created (all arches):
#   - the custom builder images (frog-builder:old-glibc-<arch>, plus the
#     legacy un-suffixed frog-builder:old-glibc)
#   - the named volumes (frog-oldglibc-output-<arch> and
#     frog-oldglibc-cargo-cache-<arch>, plus their legacy un-suffixed names)
#   - the dist/ output directory
#
# The base images this builder is based on (ubuntu:20.04, per platform) are
# left untouched in case you use them elsewhere.
#
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_DIR="$(dirname "$SCRIPT_DIR")"

ARCHES="x86_64 aarch64"
CACHE_VOLUMES="frog-oldglibc-cargo-cache-x86_64 frog-oldglibc-cargo-cache-aarch64 frog-oldglibc-cargo-cache"
OUTPUT_DIR="${REPO_DIR}/dist"

for ARCH in $ARCHES; do
    echo "==> Removing builder image frog-builder:old-glibc-${ARCH}..."
    docker rmi --force "frog-builder:old-glibc-${ARCH}" >/dev/null 2>&1 || true
done
echo "==> Removing legacy builder image frog-builder:old-glibc (if any)..."
docker rmi --force "frog-builder:old-glibc" >/dev/null 2>&1 || true

for ARCH in $ARCHES; do
    echo "==> Removing volume frog-oldglibc-output-${ARCH}..."
    docker volume rm "frog-oldglibc-output-${ARCH}" >/dev/null 2>&1 || true
done
echo "==> Removing legacy volume frog-oldglibc-output (if any)..."
docker volume rm "frog-oldglibc-output" >/dev/null 2>&1 || true

for VOL in $CACHE_VOLUMES; do
    echo "==> Removing cache volume ${VOL} (if any)..."
    docker volume rm "$VOL" >/dev/null 2>&1 || true
done

if [ -d "$OUTPUT_DIR" ]; then
    echo "==> Removing output directory ${OUTPUT_DIR}"
    rm -rf "$OUTPUT_DIR"
else
    echo "==> No output directory to remove (${OUTPUT_DIR})."
fi

echo "Cleanup complete."
