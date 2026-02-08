#!/bin/bash -ex

ROOT_DIR="$(pwd)"
TARGET_DIR="$ROOT_DIR/target"
S3TESTS_DIR="/tmp/s3-tests"
CONF_PATH="/tmp/s3tests.conf"
REPORT_DIR="/tmp/s3s-s3tests-report"
MINIO_DIR="/tmp/s3s-s3tests-minio"

mkdir -p "$TARGET_DIR"
mkdir -p "$REPORT_DIR"
mkdir -p "$MINIO_DIR"

if [ -z "$RUST_LOG" ]; then
    export RUST_LOG="s3s_proxy=debug,s3s_aws=debug,s3s=debug"
fi

cleanup() {
    local proxy_pids
    proxy_pids=$(pgrep -x s3s-proxy || true)
    if [ -n "$proxy_pids" ]; then
        for pid in $proxy_pids; do
            kill "$pid" || true
        done
    fi

    docker stop s3tests-minio || true
    docker container rm s3tests-minio || true
}

trap cleanup EXIT

if ! command -v s3s-proxy >/dev/null 2>&1; then
    echo "s3s-proxy is required; run: just install s3s-proxy"
    exit 1
fi

docker stop s3tests-minio || true
docker container rm s3tests-minio || true
docker run \
    --name s3tests-minio \
    -p 9000:9000 -p 9001:9001 \
    -e "MINIO_DOMAIN=localhost:9000" \
    -e "MINIO_HTTP_TRACE=1" \
    -v "$MINIO_DIR":/data \
    minio/minio:latest server /data --console-address ":9001" &

sleep 3s

export AWS_ACCESS_KEY_ID=minioadmin
export AWS_SECRET_ACCESS_KEY=minioadmin
export AWS_REGION=us-east-1

s3s-proxy \
    --host          localhost       \
    --port          8014            \
    --domain        localhost:8014  \
    --endpoint-url  http://localhost:9000 | tee "$TARGET_DIR/s3s-proxy.log" &

sleep 3s

rm -rf "$S3TESTS_DIR"
git clone --depth 1 https://github.com/ceph/s3-tests.git "$S3TESTS_DIR"
python3 -m venv "$S3TESTS_DIR/.venv"
"$S3TESTS_DIR/.venv/bin/pip" install -r "$S3TESTS_DIR/requirements.txt"

cat > "$CONF_PATH" <<'EOF'
[DEFAULT]
host = localhost
port = 8014
is_secure = False
ssl_verify = False

[fixtures]
bucket prefix = s3s-proxy-{random}-

[s3 main]
display_name = s3s proxy
user_id = s3s-proxy
email = s3s-proxy@example.com
access_key = minioadmin
secret_key = minioadmin

[s3 alt]
display_name = s3s proxy alt
user_id = s3s-proxy-alt
email = s3s-proxy-alt@example.com
access_key = minioadmin
secret_key = minioadmin

[s3 tenant]
display_name = s3s proxy tenant
user_id = s3s-proxy-tenant
email = s3s-proxy-tenant@example.com
access_key = minioadmin
secret_key = minioadmin
tenant = s3s-proxy

[iam]
display_name = s3s proxy iam
user_id = s3s-proxy-iam
email = s3s-proxy-iam@example.com
access_key = minioadmin
secret_key = minioadmin

[iam root]
user_id = s3s-proxy-iam-root
email = s3s-proxy-iam-root@example.com
access_key = minioadmin
secret_key = minioadmin

[iam alt root]
user_id = s3s-proxy-iam-alt-root
email = s3s-proxy-iam-alt-root@example.com
access_key = minioadmin
secret_key = minioadmin
EOF

S3TEST_ARGS=("$@")
if [ ${#S3TEST_ARGS[@]} -gt 0 ] && [ "${S3TEST_ARGS[0]}" = "--" ]; then
    S3TEST_ARGS=("${S3TEST_ARGS[@]:1}")
fi
if [ ${#S3TEST_ARGS[@]} -eq 0 ]; then
    S3TEST_ARGS=(s3tests/functional/test_s3.py::test_bucket_list_empty)
fi

pushd "$S3TESTS_DIR"
set +e
S3TEST_CONF="$CONF_PATH" \
    "$S3TESTS_DIR/.venv/bin/pytest" \
    "${S3TEST_ARGS[@]}" \
    --junitxml="$REPORT_DIR/junit.xml" | tee "$TARGET_DIR/s3-tests.log"
PYTEST_STATUS=${PIPESTATUS[0]}
set -e
popd

if [ -f "$REPORT_DIR/junit.xml" ]; then
    cp "$REPORT_DIR/junit.xml" "$TARGET_DIR/s3-tests.junit.xml"
    REPORT_STATUS=0
    python3 "$ROOT_DIR/scripts/report-s3tests.py" "$TARGET_DIR/s3-tests.junit.xml" || REPORT_STATUS=$?
else
    echo "missing junit report at $REPORT_DIR/junit.xml"
    exit 1
fi

if [ $PYTEST_STATUS -ne 0 ]; then
    exit $PYTEST_STATUS
fi

exit $REPORT_STATUS
