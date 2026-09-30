#!/usr/bin/env bash
# Run the S3-compatible backup target tests against a throwaway MinIO.
#
#   scripts/minio-test.sh          start MinIO, run the tests, stop it
#   scripts/minio-test.sh start    leave MinIO running on 127.0.0.1:9000
#   scripts/minio-test.sh test     run the tests against a running MinIO
#   scripts/minio-test.sh stop     stop and remove it
#
# MinIO runs as a rootless podman container when DISTR_HNSW_MINIO_IMAGE names
# an image that exists locally or can be pulled (MinIO's own registries stopped
# serving anonymous pulls in 2025). Otherwise the official GitHub release
# binary is downloaded once, checksum-verified against the published
# .sha256sum, cached under target/minio/, and run as a plain process. Either
# way the data directory is disposable and removed on stop.
set -euo pipefail

ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
PORT=${DISTR_HNSW_MINIO_PORT:-9000}
ENDPOINT="http://127.0.0.1:${PORT}"
CONTAINER=distr-hnsw-minio
IMAGE=${DISTR_HNSW_MINIO_IMAGE:-}
RELEASE=${DISTR_HNSW_MINIO_RELEASE:-RELEASE.2025-09-07T16-13-09Z}
CACHE="$ROOT/target/minio"
PIDFILE="$CACHE/minio.pid"
DATA="$CACHE/data"
export MINIO_ROOT_USER=${MINIO_ROOT_USER:-minioadmin}
export MINIO_ROOT_PASSWORD=${MINIO_ROOT_PASSWORD:-minioadmin}
export PATH="$HOME/.cargo/bin:$PATH"

ready() { curl -fsS "$ENDPOINT/minio/health/live" >/dev/null 2>&1; }

wait_ready() {
    for _ in $(seq 1 120); do
        ready && return 0
        sleep 0.5
    done
    echo "minio did not become ready at $ENDPOINT" >&2
    return 1
}

start_container() {
    podman run -d --name "$CONTAINER" -p "127.0.0.1:${PORT}:9000" \
        -e MINIO_ROOT_USER -e MINIO_ROOT_PASSWORD "$IMAGE" server /data >/dev/null
    echo "minio: container $CONTAINER ($IMAGE)"
}

start_binary() {
    mkdir -p "$CACHE"
    local binary="$CACHE/minio-$RELEASE"
    if [ ! -x "$binary" ]; then
        local base="https://github.com/minio/minio/releases/download/$RELEASE/minio.linux-amd64.$RELEASE"
        echo "minio: downloading $RELEASE from GitHub releases"
        curl -fsSL -o "$binary.tmp" "$base"
        curl -fsSL -o "$binary.sha256sum" "$base.sha256sum"
        local expected actual
        expected=$(cut -d' ' -f1 "$binary.sha256sum")
        actual=$(sha256sum "$binary.tmp" | cut -d' ' -f1)
        if [ "$expected" != "$actual" ]; then
            echo "minio: checksum mismatch for $RELEASE ($actual != $expected)" >&2
            rm -f "$binary.tmp"
            exit 1
        fi
        chmod +x "$binary.tmp"
        mv "$binary.tmp" "$binary"
    fi
    rm -rf "$DATA"
    mkdir -p "$DATA"
    nohup "$binary" server "$DATA" --address "127.0.0.1:${PORT}" \
        --console-address "127.0.0.1:$((PORT + 1))" >"$CACHE/minio.log" 2>&1 &
    echo $! >"$PIDFILE"
    echo "minio: binary $RELEASE (pid $(cat "$PIDFILE"), log $CACHE/minio.log)"
}

start() {
    if ready; then
        echo "minio: already serving at $ENDPOINT"
        return 0
    fi
    if [ -n "$IMAGE" ] && command -v podman >/dev/null 2>&1 \
        && { podman image exists "$IMAGE" || podman pull "$IMAGE"; }; then
        start_container
    else
        start_binary
    fi
    wait_ready
    echo "minio: ready at $ENDPOINT"
}

stop() {
    if command -v podman >/dev/null 2>&1; then
        podman rm -f "$CONTAINER" >/dev/null 2>&1 || true
    fi
    if [ -f "$PIDFILE" ]; then
        kill "$(cat "$PIDFILE")" 2>/dev/null || true
        rm -f "$PIDFILE"
    fi
    rm -rf "$DATA"
    echo "minio: stopped"
}

run_tests() {
    cd "$ROOT"
    DISTR_HNSW_S3_TEST_ENDPOINT="$ENDPOINT" \
        AWS_ACCESS_KEY_ID="$MINIO_ROOT_USER" \
        AWS_SECRET_ACCESS_KEY="$MINIO_ROOT_PASSWORD" \
        AWS_REGION=us-east-1 \
        cargo test -p distr-hnsw --test backup_s3 -- --nocapture "$@"
}

case "${1:-run}" in
    start) start ;;
    stop) stop ;;
    test)
        shift
        run_tests "$@"
        ;;
    run)
        shift || true
        start
        trap stop EXIT
        run_tests "$@"
        ;;
    *)
        echo "usage: $0 [run|start|test|stop]" >&2
        exit 2
        ;;
esac
