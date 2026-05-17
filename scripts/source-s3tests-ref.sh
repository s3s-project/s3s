S3TESTS_REF_SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
S3TESTS_REF_FILE="${S3TESTS_REF_FILE:-$S3TESTS_REF_SCRIPT_DIR/s3tests.env}"

if [ -z "${S3TESTS_REF:-}" ]; then
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
