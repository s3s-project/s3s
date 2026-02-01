#!/bin/bash -ex

DATA_DIR="/tmp/s3s-s3tests"
CONF_PATH="/tmp/s3tests.conf"
REPORT_DIR="/tmp/s3s-s3tests-report"
JUNIT_PATH="/reports/junit.xml"
S3TEST_IMAGE="${S3TEST_IMAGE:-quay.io/ceph/s3-tests:latest}"

mkdir -p "$DATA_DIR"
mkdir -p "$REPORT_DIR"
mkdir -p target

if [ -z "$RUST_LOG" ]; then
    export RUST_LOG="s3s_fs=debug,s3s=debug"
fi

kill_s3s_fs() {
    local s3s_fs_pids
    s3s_fs_pids=$(pgrep -x s3s-fs || true)
    if [ -n "$s3s_fs_pids" ]; then
        for pid in $s3s_fs_pids; do
            kill "$pid" || true
        done
    fi
}

kill_s3s_fs

if ! command -v s3s-fs >/dev/null 2>&1; then
    echo "s3s-fs is required; run: just install s3s-fs"
    exit 1
fi

s3s-fs \
    --access-key    AKEXAMPLES3S    \
    --secret-key    SKEXAMPLES3S    \
    --host          localhost       \
    --port          8014            \
    --domain        localhost:8014  \
    --domain        localhost       \
    "$DATA_DIR" | tee target/s3s-fs.log &

sleep 1s

cat > "$CONF_PATH" <<'EOF'
[DEFAULT]
host = localhost
port = 8014
is_secure = False
ssl_verify = False

[fixtures]
bucket prefix = s3s-{random}-

[s3 main]
display_name = s3s main
user_id = s3s-main
access_key = AKEXAMPLES3S
secret_key = SKEXAMPLES3S

[s3 alt]
display_name = s3s alt
user_id = s3s-alt
access_key = AKEXAMPLES3S
secret_key = SKEXAMPLES3S
EOF

S3TEST_ARGS=("$@")
if [ ${#S3TEST_ARGS[@]} -eq 0 ]; then
    S3TEST_ARGS=(-- s3tests/functional/test_s3.py::test_bucket_list_empty)
fi
S3TEST_ARGS+=("--junitxml=$JUNIT_PATH")

docker run --rm --network host \
    -e S3TEST_CONF=/etc/s3-tests.conf \
    -v "$CONF_PATH":/etc/s3-tests.conf:ro \
    -v "$REPORT_DIR":/reports \
    "$S3TEST_IMAGE" \
    tox "${S3TEST_ARGS[@]}" | tee target/s3-tests.log

if [ -f "$REPORT_DIR/junit.xml" ]; then
    cp "$REPORT_DIR/junit.xml" target/s3-tests.junit.xml
    ./scripts/report-s3tests.py target/s3-tests.junit.xml
else
    echo "missing junit report at $REPORT_DIR/junit.xml"
    exit 1
fi

kill_s3s_fs
