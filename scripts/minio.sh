#!/bin/bash -ex
# SPDX-License-Identifier: Apache-2.0
# SPDX-FileCopyrightText: 2023-2026 The s3s Authors

. "$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)/minio.env"

mkdir -p /tmp/minio
docker run \
    -p 9000:9000 -p 9001:9001 \
    -e "MINIO_DOMAIN=localhost:9000" \
    -e "MINIO_HTTP_TRACE=1" \
    -v /tmp/minio:/data \
    "$MINIO_IMAGE_REF" server /data --console-address ":9001" &
