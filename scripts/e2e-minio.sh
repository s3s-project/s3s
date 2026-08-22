#!/bin/bash -ex
# SPDX-License-Identifier: Apache-2.0
# SPDX-FileCopyrightText: 2023-2026 The s3s Authors

mkdir -p /tmp/minio
docker stop e2e-minio || echo
docker container rm e2e-minio || echo
docker run \
    --name e2e-minio \
    -p 9000:9000 -p 9001:9001 \
    -e "MINIO_DOMAIN=localhost:9000" \
    -e "MINIO_HTTP_TRACE=1" \
    -v /tmp/minio:/data \
    minio/minio:latest server /data --console-address ":9001" &

sleep 3s

export AWS_ACCESS_KEY_ID=minioadmin
export AWS_SECRET_ACCESS_KEY=minioadmin
export AWS_REGION=us-east-1
export AWS_ENDPOINT_URL=http://localhost:9000

if [ -z "$RUST_LOG" ]; then
    export RUST_LOG="s3s_e2e=debug,s3s_test=info,s3s=debug"
fi
export RUST_BACKTRACE=full

s3s-e2e "$@"
