#!/bin/bash -ex

DATA_DIR="/tmp/s3s-boto3-test"
mkdir -p "$DATA_DIR"

if [ -z "$RUST_LOG" ]; then
    export RUST_LOG="s3s_fs=debug,s3s=debug"
fi

killall s3s-fs 2>/dev/null || true

s3s-fs \
    --access-key    AKEXAMPLES3S    \
    --secret-key    SKEXAMPLES3S    \
    --host          localhost       \
    --port          8014            \
    --domain        localhost:8014  \
    --domain        localhost       \
    "$DATA_DIR" | tee target/s3s-fs-boto3.log &

sleep 2

export AWS_ENDPOINT_URL=http://localhost:8014
export AWS_ACCESS_KEY_ID=AKEXAMPLES3S
export AWS_SECRET_ACCESS_KEY=SKEXAMPLES3S
export AWS_DEFAULT_REGION=us-east-1

uv run python3 tests/boto3/rustfs_issue_984.py
