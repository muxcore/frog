#!/usr/bin/env bash
#
# Build frog inside a Docker container with an old glibc, so the binary runs on
# servers whose glibc is older than the local build host's (e.g. glibc 2.34).
#
# Produces:  dist/frog   (glibc <= TARGET_GLIBC compatible, dynamically linked)
#
# Why this is needed and how it works: see README.md ->
# "Building for older glibc (containers)".
#
# Cleanup (container, images, volumes, dist) is done by scripts/clean-old-glibc.sh.
#
set -euo pipefail

# Number of CPU threads to give the containerised build (default: all).
NPROC_DEFAULT="$(getconf _NPROCESSORS_ONLN 2>/dev/null || echo 4)"
JOBS="${FROG_BUILD_JOBS:-${NPROC_DEFAULT}}"

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_DIR="$(dirname "$SCRIPT_DIR")"

BUILD_IMAGE="frog-builder:old-glibc"
OUTPUT_VOLUME="frog-oldglibc-output"
CACHE_VOLUME="frog-oldglibc-cargo-cache"
TARGET_GLIBC="2.34"
OUTPUT_DIR="${REPO_DIR}/dist"

cd "$REPO_DIR"

if ! command -v docker >/dev/null 2>&1; then
    echo "error: docker is required to run this script" >&2
    exit 1
fi

mkdir -p "$OUTPUT_DIR"

echo "==> Building builder image ${BUILD_IMAGE} (glibc 2.31 / Ubuntu 20.04)..."
docker build \
    --network=host \
    -t "$BUILD_IMAGE" \
    -f scripts/Dockerfile.oldglibc .

echo "==> Creating build container..."
CID="$(
    docker create \
        --mount type=bind,source="${REPO_DIR}",target=/src,readonly \
        --mount "source=${OUTPUT_VOLUME},target=/out" \
        --mount "source=${CACHE_VOLUME},target=/root/.cargo" \
        --env TARGET_GLIBC="${TARGET_GLIBC}" \
        --env JOBS="${JOBS}" \
        --env CARGO_TARGET_DIR=/out/target \
        --workdir /src \
        "${BUILD_IMAGE}" \
        bash -lc '
            set -euo pipefail
            cargo build --release -j"$JOBS"
            max_glibc=$(objdump -T /out/target/release/frog \
                | grep -o "GLIBC_[0-9.]*" | sort -uV | tail -1)
            echo "Max GLIBC symbol version required: ${max_glibc}"
            if [ -n "$max_glibc" ]; then
                required="${max_glibc#GLIBC_}"
                if [ "$required" != "$TARGET_GLIBC" ] \
                   && [ "$(printf "%s\n" "$required" "$TARGET_GLIBC" | sort -V | tail -1)" = "$required" ]; then
                    echo "error: binary requires ${max_glibc} (target is <= ${TARGET_GLIBC})" >&2
                    exit 1
                fi
            fi
        '
)"

trap 'docker rm -f "$CID" >/dev/null 2>&1 || true' EXIT

echo "==> Building frog in container (${JOBS} jobs)..."
docker start -a "$CID"
[ "$?" -eq 0 ] || {
    echo "error: container build failed" >&2
    exit 1
}

echo "==> Copying binary to ${OUTPUT_DIR}/frog ..."
docker cp "${CID}:/out/target/release/frog" "${OUTPUT_DIR}/frog"

echo
echo "Build complete: ${OUTPUT_DIR}/frog"
objdump -T "${OUTPUT_DIR}/frog" 2>/dev/null \
    | grep -o 'GLIBC_[0-9.]*' | sort -uV | tail -1 \
    | sed 's/^/Host-verified max GLIBC symbol: /'
echo "Deploy ${OUTPUT_DIR}/frog to the old-glibc server (it still needs Oracle Instant Client + libgcc)."