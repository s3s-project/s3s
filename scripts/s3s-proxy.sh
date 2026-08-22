#!/bin/bash -ex
# SPDX-License-Identifier: Apache-2.0
# SPDX-FileCopyrightText: 2023-2026 The s3s Authors

mkdir -p /tmp/minio
docker run -p 9000:9000 -p 9001:9001 \
    -e "MINIO_DOMAIN=localhost:9000" \
    -e "MINIO_HTTP_TRACE=1" \
    -v /tmp/minio:/data \
    minio/minio:latest server /data --console-address ":9001" &

sleep 1s

export AWS_ACCESS_KEY_ID=minioadmin
export AWS_SECRET_ACCESS_KEY=minioadmin
export AWS_REGION=us-east-1

if [ -z "$RUST_LOG" ]; then
    export RUST_LOG="s3s_proxy=debug,s3s_aws=debug,s3s=debug"
fi
export RUST_BACKTRACE=full

s3s-proxy \
    --host          localhost               \
    --port          8014                    \
    --domain        localhost:8014          \
    --endpoint-url  http://localhost:9000
