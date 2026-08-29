#!/bin/bash
# SPDX-License-Identifier: Apache-2.0
# SPDX-FileCopyrightText: 2023-2026 The s3s Authors

set -euo pipefail

ROOT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
TARGET_DIR="$ROOT_DIR/target"
S3S_FS_BIN="${S3S_FS_BIN:-s3s-fs}"
S3S_RCLONE_PORT="${S3S_RCLONE_PORT:-8014}"
DATA_DIR="$(mktemp -d -t s3s-rclone-fs.XXXXXX)"
SERVER_LOG="$TARGET_DIR/s3s-fs-rclone.log"
RCLONE_LOG="$TARGET_DIR/rclone.log"
S3S_FS_PID=""

mkdir -p "$TARGET_DIR"

cleanup() {
    if [[ -n "$S3S_FS_PID" ]] && kill -0 "$S3S_FS_PID" >/dev/null 2>&1; then
        kill -INT "$S3S_FS_PID" >/dev/null 2>&1 || true
        wait "$S3S_FS_PID" || true
    fi
    rm -rf "$DATA_DIR"
}
trap cleanup EXIT

if ! command -v "$S3S_FS_BIN" >/dev/null 2>&1; then
    echo "s3s-fs is required; run: just install s3s-fs" >&2
    exit 1
fi
if [[ -z "${RUST_LOG:-}" ]]; then
    export RUST_LOG="s3s_fs=debug,s3s=debug"
fi

"$S3S_FS_BIN" \
    --access-key AKEXAMPLES3S \
    --secret-key SKEXAMPLES3S \
    --host 127.0.0.1 \
    --port "$S3S_RCLONE_PORT" \
    --domain localhost \
    "$DATA_DIR" > "$SERVER_LOG" 2>&1 &
S3S_FS_PID=$!

endpoint="http://127.0.0.1:$S3S_RCLONE_PORT"
ready=0
for _ in {1..60}; do
    if ! kill -0 "$S3S_FS_PID" >/dev/null 2>&1; then
        echo "s3s-fs exited before becoming ready" >&2
        tail -100 "$SERVER_LOG" >&2 || true
        exit 1
    fi
    if (exec 3<>"/dev/tcp/127.0.0.1/$S3S_RCLONE_PORT") 2>/dev/null; then
        ready=1
        break
    fi
    sleep 0.5
done
if [[ "$ready" -ne 1 ]]; then
    echo "timed out waiting for s3s-fs at $endpoint" >&2
    tail -100 "$SERVER_LOG" >&2 || true
    exit 1
fi

export AWS_ENDPOINT_URL="$endpoint"
export AWS_ACCESS_KEY_ID="AKEXAMPLES3S"
export AWS_SECRET_ACCESS_KEY="SKEXAMPLES3S"
export AWS_REGION="us-east-1"

"$ROOT_DIR/scripts/e2e-rclone.sh" 2>&1 | tee "$RCLONE_LOG"
