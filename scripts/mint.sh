#!/bin/bash
# SPDX-License-Identifier: Apache-2.0
# SPDX-FileCopyrightText: 2023-2026 The s3s Authors

. "$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)/mint.env"

mkdir -p /tmp/mint
docker run \
    -e "SERVER_ENDPOINT=localhost:8014"   \
    -e "ACCESS_KEY=minioadmin" \
    -e "SECRET_KEY=minioadmin" \
    --network host \
    -v /tmp/mint:/mint/log \
    "$MINT_IMAGE_REF"

cargo run -q -p xtask -- report mint /tmp/mint/log.json
