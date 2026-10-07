# SPDX-License-Identifier: Apache-2.0
# SPDX-FileCopyrightText: 2023-2026 The s3s Authors

S3TESTS_REF_SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
S3TESTS_REF_FILE="${S3TESTS_REF_FILE:-$S3TESTS_REF_SCRIPT_DIR/s3tests.env}"

# The env file carries both pins, so load it when either one is missing:
# presetting S3TESTS_REF alone must not leave the image ref undefined.
if [ -z "${S3TESTS_REF:-}" ] || [ -z "${S3TESTS_IMAGE_REF:-}" ]; then
    if [ ! -r "$S3TESTS_REF_FILE" ]; then
        echo "s3-tests ref file not readable: $S3TESTS_REF_FILE" >&2
        return 1 2>/dev/null || exit 1
    fi
    . "$S3TESTS_REF_FILE"
fi
if [ -z "${S3TESTS_REF:-}" ]; then
    echo "s3-tests ref is empty" >&2
    return 1 2>/dev/null || exit 1
fi
