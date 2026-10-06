#!/bin/bash
# SPDX-License-Identifier: Apache-2.0
# SPDX-FileCopyrightText: 2023-2026 The s3s Authors

. "$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)/mint.env"

mkdir -p /tmp/mint
# RUN_ON_FAIL=1 keeps a suite going after its first failure. Some suites abort the
# whole suite otherwise - minio-java stops at its first failing case - which hides
# every later case and makes the expected-failure list and the pass floors in
# xtask/src/report/mint.rs unreachable. The recorded baseline is a full run.
# ENABLE_HTTP_TESTS=1 runs the `aws-sdk-java-v2` cases over plaintext: the image's
# patch 0006 makes them return early unless `ENABLE_HTTPS=1` or this switch is
# set, and this run is plaintext. Without the switch the suite exits zero without
# logging a line and the group has no row in the report at all.
docker run \
    -e "SERVER_ENDPOINT=localhost:8014"   \
    -e "ACCESS_KEY=minioadmin" \
    -e "SECRET_KEY=minioadmin" \
    -e "RUN_ON_FAIL=1" \
    -e "ENABLE_HTTP_TESTS=1" \
    --network host \
    -v /tmp/mint:/mint/log \
    "$MINT_IMAGE_REF"

cargo run -q -p xtask -- report mint /tmp/mint/log.json
