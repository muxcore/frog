#!/usr/bin/env bash
#
# Remove everything the old-glibc container build created:
#   - the custom builder image (frog-builder:old-glibc)
#   - the named volumes (frog-oldglibc-output, frog-oldglibc-cargo-cache)
#   - the dist/ output directory
#
# The base image this builder is based on (ubuntu:20.04) is left untouched in
# case you use it elsewhere. To remove it too, run:
#   docker rmi ubuntu:20.04
#
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_DIR="$(dirname "$SCRIPT_DIR")"
BUILD_IMAGE="frog-builder:old-glibc"
CACHE_VOLUME="frog-oldglibc-cargo-cache"
OUTPUT_VOLUME="frog-oldglibc-output"
OUTPUT_DIR="${REPO_DIR}/dist"

echo "==> Removing builder image ${BUILD_IMAGE}..."
docker rmi --force "$BUILD_IMAGE" >/dev/null 2>&1 || true

echo "==> Removing volumes ${CACHE_VOLUME}, ${OUTPUT_VOLUME}..."
docker volume rm "$CACHE_VOLUME" "$OUTPUT_VOLUME" >/dev/null 2>&1 || true

if [ -d "$OUTPUT_DIR" ]; then
    echo "==> Removing output directory ${OUTPUT_DIR}"
    rm -rf "$OUTPUT_DIR"
else
    echo "==> No output directory to remove (${OUTPUT_DIR})."
fi

echo "Cleanup complete."