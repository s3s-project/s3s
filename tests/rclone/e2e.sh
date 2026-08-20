#!/bin/sh

set -eu

: "${AWS_ENDPOINT_URL:?AWS_ENDPOINT_URL is required}"
: "${AWS_ACCESS_KEY_ID:?AWS_ACCESS_KEY_ID is required}"
: "${AWS_SECRET_ACCESS_KEY:?AWS_SECRET_ACCESS_KEY is required}"
: "${RCLONE_EXPECTED_VERSION:?RCLONE_EXPECTED_VERSION is required}"

RCLONE_BIN="${RCLONE_BIN:-rclone}"
AWS_REGION="${AWS_REGION:-${AWS_DEFAULT_REGION:-us-east-1}}"
RCLONE_S3_PROVIDER="${RCLONE_S3_PROVIDER:-Other}"
RCLONE_S3_FORCE_PATH_STYLE="${RCLONE_S3_FORCE_PATH_STYLE:-true}"
REMOTE_NAME="s3s"
REMOVE_WORK_ROOT=0
BUCKET_CREATED=0

if [ "$#" -gt 1 ]; then
    echo "usage: $0 [work-directory]" >&2
    exit 2
fi

if [ "$#" -eq 1 ]; then
    WORK_ROOT="$1"
    mkdir -p "$WORK_ROOT"
else
    WORK_ROOT="$(mktemp -d -t s3s-rclone-e2e.XXXXXX)"
    REMOVE_WORK_ROOT=1
fi

CONFIG_PATH="$WORK_ROOT/rclone.conf"
SOURCE_ROOT="$WORK_ROOT/source"
DOWNLOAD_ROOT="$WORK_ROOT/download"
EXPECTED_LIST="$WORK_ROOT/expected-list.txt"
ACTUAL_LIST="$WORK_ROOT/actual-list.txt"
EXPECTED_RANGE="$WORK_ROOT/expected-range.bin"
ACTUAL_RANGE="$WORK_ROOT/actual-range.bin"
ACTUAL_COPY="$WORK_ROOT/actual-copy.txt"

if [ -n "${RCLONE_TEST_BUCKET:-}" ]; then
    BUCKET="$RCLONE_TEST_BUCKET"
else
    random_suffix="$(od -An -N4 -tu4 /dev/urandom | tr -d ' ')"
    BUCKET="s3s-rclone-$(date +%s)-$$-$random_suffix"
fi
REMOTE_ROOT="$REMOTE_NAME:$BUCKET"

rclone_cmd() {
    "$RCLONE_BIN" \
        --config "$CONFIG_PATH" \
        --low-level-retries "${RCLONE_LOW_LEVEL_RETRIES:-3}" \
        "$@"
}

cleanup() {
    status=$?
    trap - EXIT

    if [ "$BUCKET_CREATED" -eq 1 ]; then
        echo "==> Cleaning up $REMOTE_ROOT" >&2
        rclone_cmd delete "$REMOTE_ROOT" --rmdirs >/dev/null 2>&1 || true
        rclone_cmd rmdir "$REMOTE_ROOT" >/dev/null 2>&1 || true
    fi

    if [ "$REMOVE_WORK_ROOT" -eq 1 ]; then
        rm -rf "$WORK_ROOT"
    fi

    exit "$status"
}

trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

if ! command -v "$RCLONE_BIN" >/dev/null 2>&1; then
    echo "rclone executable not found: $RCLONE_BIN" >&2
    exit 1
fi

actual_version="$($RCLONE_BIN version | sed -n '1p')"
if [ "$actual_version" != "$RCLONE_EXPECTED_VERSION" ]; then
    echo "unexpected rclone version: expected '$RCLONE_EXPECTED_VERSION', got '$actual_version'" >&2
    exit 1
fi

umask 077
cat > "$CONFIG_PATH" <<EOF
[$REMOTE_NAME]
type = s3
provider = $RCLONE_S3_PROVIDER
env_auth = false
access_key_id = $AWS_ACCESS_KEY_ID
secret_access_key = $AWS_SECRET_ACCESS_KEY
session_token = ${AWS_SESSION_TOKEN:-}
region = $AWS_REGION
endpoint = $AWS_ENDPOINT_URL
force_path_style = $RCLONE_S3_FORCE_PATH_STYLE
EOF

echo "==> Preparing small-object and multipart fixtures"
mkdir -p \
    "$SOURCE_ROOT/nested/deeper" \
    "$SOURCE_ROOT/special" \
    "$SOURCE_ROOT/fanout/group-0" \
    "$SOURCE_ROOT/fanout/group-1" \
    "$SOURCE_ROOT/fanout/group-2" \
    "$SOURCE_ROOT/fanout/group-3" \
    "$SOURCE_ROOT/blobs"
printf 'root object\n' > "$SOURCE_ROOT/root.txt"
printf 'nested child object\n' > "$SOURCE_ROOT/nested/child.txt"
printf 'deep object\n' > "$SOURCE_ROOT/nested/deeper/grandchild.txt"
printf 'space in object key\n' > "$SOURCE_ROOT/special/space name.txt"
printf 'unicode object key\n' > "$SOURCE_ROOT/special/你好.txt"
: > "$SOURCE_ROOT/empty.bin"

group=0
while [ "$group" -lt 4 ]; do
    file=0
    while [ "$file" -lt 8 ]; do
        printf 'group=%s file=%s\n' "$group" "$file" \
            > "$SOURCE_ROOT/fanout/group-$group/file-$file.txt"
        file=$((file + 1))
    done
    group=$((group + 1))
done

# The 5 MiB settings below force this 12 MiB object through three multipart
# upload parts independently of rclone's release-specific default cutoff.
dd if=/dev/urandom of="$SOURCE_ROOT/blobs/multipart-12m.bin" bs=1M count=12 2>/dev/null

echo "==> Creating the temporary bucket $BUCKET"
rclone_cmd mkdir "$REMOTE_ROOT"
BUCKET_CREATED=1

echo "==> Uploading recursively (including a forced three-part upload)"
rclone_cmd copy \
    "$SOURCE_ROOT" \
    "$REMOTE_ROOT/fixture" \
    --s3-upload-cutoff 5Mi \
    --s3-chunk-size 5Mi \
    --transfers 4 \
    --checkers 4

echo "==> Verifying recursive listing and object keys"
(
    cd "$SOURCE_ROOT"
    find . -type f | sed 's|^\./||' | LC_ALL=C sort
) > "$EXPECTED_LIST"
rclone_cmd lsf -R --files-only "$REMOTE_ROOT/fixture" | LC_ALL=C sort > "$ACTUAL_LIST"
diff -u "$EXPECTED_LIST" "$ACTUAL_LIST"

echo "==> Checking remote content by downloading every object"
rclone_cmd check "$SOURCE_ROOT" "$REMOTE_ROOT/fixture" --download

echo "==> Downloading recursively and comparing the local trees"
mkdir -p "$DOWNLOAD_ROOT"
rclone_cmd copy "$REMOTE_ROOT/fixture" "$DOWNLOAD_ROOT" --transfers 4 --checkers 4
diff -r "$SOURCE_ROOT" "$DOWNLOAD_ROOT"

echo "==> Verifying a byte range from the multipart object"
dd \
    if="$SOURCE_ROOT/blobs/multipart-12m.bin" \
    of="$EXPECTED_RANGE" \
    bs=1 \
    skip=5242877 \
    count=4099 \
    2>/dev/null
rclone_cmd cat \
    "$REMOTE_ROOT/fixture/blobs/multipart-12m.bin" \
    --offset 5242877 \
    --count 4099 \
    > "$ACTUAL_RANGE"
cmp "$EXPECTED_RANGE" "$ACTUAL_RANGE"

echo "==> Verifying remote-to-remote object copy"
rclone_cmd copyto \
    "$REMOTE_ROOT/fixture/nested/child.txt" \
    "$REMOTE_ROOT/copied/child.txt"
rclone_cmd cat "$REMOTE_ROOT/copied/child.txt" > "$ACTUAL_COPY"
cmp "$SOURCE_ROOT/nested/child.txt" "$ACTUAL_COPY"

echo "==> Verifying single-object deletion"
rclone_cmd deletefile "$REMOTE_ROOT/copied/child.txt"
rclone_cmd lsf -R --files-only "$REMOTE_ROOT" > "$ACTUAL_LIST"
if grep -Fqx 'copied/child.txt' "$ACTUAL_LIST"; then
    echo "deleted object is still visible under $REMOTE_ROOT/copied" >&2
    exit 1
fi

echo "==> Removing the temporary bucket"
rclone_cmd delete "$REMOTE_ROOT" --rmdirs
rclone_cmd rmdir "$REMOTE_ROOT"
BUCKET_CREATED=0

echo "rclone S3 integration test passed"
