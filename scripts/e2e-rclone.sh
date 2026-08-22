#!/bin/bash
# SPDX-License-Identifier: Apache-2.0
# SPDX-FileCopyrightText: 2023-2026 The s3s Authors

set -euo pipefail

ROOT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
. "$ROOT_DIR/scripts/rclone.env"

: "${AWS_ENDPOINT_URL:?AWS_ENDPOINT_URL is required}"
: "${AWS_ACCESS_KEY_ID:?AWS_ACCESS_KEY_ID is required}"
: "${AWS_SECRET_ACCESS_KEY:?AWS_SECRET_ACCESS_KEY is required}"

AWS_REGION="${AWS_REGION:-${AWS_DEFAULT_REGION:-us-east-1}}"
RCLONE_S3_PROVIDER="${RCLONE_S3_PROVIDER:-Other}"
RCLONE_S3_FORCE_PATH_STYLE="${RCLONE_S3_FORCE_PATH_STYLE:-true}"
RCLONE_E2E_TIMEOUT="${RCLONE_E2E_TIMEOUT:-15m}"
WORK_ROOT="$(mktemp -d -t s3s-rclone-e2e.XXXXXX)"

cleanup() {
    if [[ "${RCLONE_KEEP_WORKDIR:-0}" == "1" ]]; then
        echo "rclone work directory retained at $WORK_ROOT" >&2
    else
        rm -rf "$WORK_ROOT"
    fi
}
trap cleanup EXIT

if ! command -v docker >/dev/null 2>&1; then
    echo "docker is required to run the pinned rclone client" >&2
    exit 1
fi
if ! docker info >/dev/null 2>&1; then
    echo "docker is not running" >&2
    exit 1
fi

docker_env=(
    --env AWS_ENDPOINT_URL
    --env AWS_ACCESS_KEY_ID
    --env AWS_SECRET_ACCESS_KEY
    --env AWS_REGION="$AWS_REGION"
    --env RCLONE_EXPECTED_VERSION="$RCLONE_VERSION_LINE"
    --env RCLONE_S3_PROVIDER="$RCLONE_S3_PROVIDER"
    --env RCLONE_S3_FORCE_PATH_STYLE="$RCLONE_S3_FORCE_PATH_STYLE"
    --env RCLONE_LOW_LEVEL_RETRIES="${RCLONE_LOW_LEVEL_RETRIES:-3}"
)
if [[ -n "${AWS_SESSION_TOKEN:-}" ]]; then
    docker_env+=(--env AWS_SESSION_TOKEN)
fi
if [[ -n "${RCLONE_TEST_BUCKET:-}" ]]; then
    docker_env+=(--env RCLONE_TEST_BUCKET)
fi

timeout "$RCLONE_E2E_TIMEOUT" docker run \
    --rm \
    --network host \
    --user "$(id -u):$(id -g)" \
    "${docker_env[@]}" \
    --volume "$WORK_ROOT:/work" \
    --volume "$ROOT_DIR/tests/rclone/e2e.sh:/test/e2e.sh:ro" \
    --entrypoint /bin/sh \
    "$RCLONE_IMAGE_REF" \
    /test/e2e.sh /work
