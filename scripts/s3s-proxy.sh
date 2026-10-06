#!/bin/bash -ex
# SPDX-License-Identifier: Apache-2.0
# SPDX-FileCopyrightText: 2023-2026 The s3s Authors

. "$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)/minio.env"

mkdir -p /tmp/minio
docker run -p 9000:9000 -p 9001:9001 \
    -e "MINIO_DOMAIN=localhost:9000" \
    -e "MINIO_HTTP_TRACE=1" \
    -v /tmp/minio:/data \
    "$MINIO_IMAGE_REF" server /data --console-address ":9001" &

sleep 1s

export AWS_ACCESS_KEY_ID=minioadmin
export AWS_SECRET_ACCESS_KEY=minioadmin
export AWS_REGION=us-east-1

if [ -z "$RUST_LOG" ]; then
    export RUST_LOG="s3s_proxy=debug,s3s_aws=debug,s3s=debug"
fi
export RUST_BACKTRACE=full

# Bind IPv4 explicitly: `localhost` resolves to whichever address family the
# resolver prefers, and on a host that prefers `::1` binding the name would leave
# the proxy on IPv6 only. The suites run inside the mint container, where
# `localhost` resolves to `127.0.0.1`, so that is the address to listen on.
s3s-proxy \
    --host          127.0.0.1               \
    --port          8014                    \
    --domain        localhost:8014          \
    --endpoint-url  http://localhost:9000   \
    --enable-minio-route                    \
    --enable-auth-passthrough               \
    --enable-sig-v2
