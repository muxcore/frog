#!/usr/bin/env bash
#
# Build frog inside a Docker container with an old glibc, so the binary runs on
# servers whose glibc is older than the local build host's (e.g. glibc 2.34).
#
# Usage:  ./scripts/build-old-glibc.sh [ARCH]
#   ARCH: x86_64 (default) or aarch64  (amd64/arm64 accepted as aliases);
#         may also come from $FROG_ARCH (explicit argument wins).
#         Without an explicit ARCH the host architecture is built.
#
# Produces:  dist/frog-<arch>  (glibc <= TARGET_GLIBC compatible, dynamically linked)
#
# The container runs the *target* architecture (native when it matches the host,
# QEMU-emulated otherwise), so the regular native toolchain just works — including
# the C sources compiled into the binary for ODPI-C. No Oracle client is needed at
# build time; install the matching-arch Oracle Instant Client on the target server.
#
# Why this is needed and how it works: see README.md ->
# "Building for older glibc (containers)".
#
# Cleanup (containers, images, volumes, dist) is done by scripts/clean-old-glibc.sh.
#
set -euo pipefail

# --- architecture handling ----------------------------------------------------
normalize_arch() {
    case "$1" in
        x86_64|amd64)   echo "x86_64" ;;
        aarch64|arm64)  echo "aarch64" ;;
        *)
            echo "error: unknown architecture '$1' (want x86_64|amd64|aarch64|arm64)" >&2
            exit 1
            ;;
    esac
}

host_arch() {
    case "$(uname -m)" in
        x86_64|amd64)   echo "x86_64" ;;
        aarch64|arm64)  echo "aarch64" ;;
        *)
            echo "error: unsupported host architecture '$(uname -m)'" >&2
            exit 1
            ;;
    esac
}

REQUESTED="${1:-${FROG_ARCH:-}}"
if [ -z "$REQUESTED" ]; then
    ARCH="$(host_arch)"
    echo "==> No ARCH given, building for host architecture (${ARCH})."
else
    ARCH="$(normalize_arch "$REQUESTED")"
fi

case "$ARCH" in
    x86_64)  PLATFORM="linux/amd64" ;;
    aarch64) PLATFORM="linux/arm64" ;;
esac

HOST_ARCH="$(host_arch)"

# Number of CPU threads to give the containerised build (default: all).
NPROC_DEFAULT="$(getconf _NPROCESSORS_ONLN 2>/dev/null || echo 4)"
JOBS="${FROG_BUILD_JOBS:-${NPROC_DEFAULT}}"

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_DIR="$(dirname "$SCRIPT_DIR")"

BUILD_IMAGE="frog-builder:old-glibc-${ARCH}"
OUTPUT_VOLUME="frog-oldglibc-output-${ARCH}"
# The toolchain shims live in $CARGO_HOME, so the cargo home cannot be shared
# between architectures (an x86_64 cargo binary cannot run in the arm64
# container and vice versa) — one cache volume per arch. The registry/git
# content inside is re-downloaded once per arch.
CACHE_VOLUME="frog-oldglibc-cargo-cache-${ARCH}"
TARGET_GLIBC="2.34"
OUTPUT_DIR="${REPO_DIR}/dist"
OUTPUT_BINARY="frog-${ARCH}"

cd "$REPO_DIR"

if ! command -v docker >/dev/null 2>&1; then
    echo "error: docker is required to run this script" >&2
    exit 1
fi

# Cross-arch builds run the container under QEMU emulation — fail fast with a
# helpful message when the host cannot do that instead of dying mid-build.
if [ "$ARCH" != "$HOST_ARCH" ]; then
    echo "==> Cross-arch build: ${PLATFORM} container on ${HOST_ARCH} host (QEMU emulation, slower)..."
    if ! docker run --rm --platform "$PLATFORM" ubuntu:20.04 true >/dev/null 2>&1; then
        echo "error: this host cannot run ${PLATFORM} containers." >&2
        echo "  Install QEMU binfmt support, e.g.:" >&2
        echo "    docker run --privileged --rm tonistiigi/binfmt --install all" >&2
        echo "  (Docker Desktop ships this out of the box.)" >&2
        exit 1
    fi
fi

mkdir -p "$OUTPUT_DIR"

echo "==> Building builder image ${BUILD_IMAGE} (${PLATFORM}, glibc 2.31 / Ubuntu 20.04)..."
docker build \
    --platform "$PLATFORM" \
    --network=host \
    -t "$BUILD_IMAGE" \
    -f scripts/Dockerfile.oldglibc .

echo "==> Creating build container..."
CID="$(
    docker create \
        --platform "$PLATFORM" \
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

echo "==> Building frog ${ARCH} in container (${JOBS} jobs)..."
docker start -a "$CID"
[ "$?" -eq 0 ] || {
    echo "error: container build failed" >&2
    exit 1
}

echo "==> Copying binary to ${OUTPUT_DIR}/${OUTPUT_BINARY} ..."
docker cp "${CID}:/out/target/release/frog" "${OUTPUT_DIR}/${OUTPUT_BINARY}"

echo
echo "Build complete: ${OUTPUT_DIR}/${OUTPUT_BINARY}"
if [ "$ARCH" = "$HOST_ARCH" ] && command -v objdump >/dev/null 2>&1; then
    objdump -T "${OUTPUT_DIR}/${OUTPUT_BINARY}" 2>/dev/null \
        | grep -o 'GLIBC_[0-9.]*' | sort -uV | tail -1 \
        | sed 's/^/Host-verified max GLIBC symbol: /'
else
    echo "(host objdump cannot inspect a foreign-arch binary; the container already verified max GLIBC <= ${TARGET_GLIBC})"
fi
echo "Deploy ${OUTPUT_DIR}/${OUTPUT_BINARY} to the old-glibc ${ARCH} server (it still needs the matching-arch Oracle Instant Client + libgcc)."
