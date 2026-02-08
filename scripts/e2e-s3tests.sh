#!/bin/bash -ex

ROOT_DIR="$(pwd)"
TARGET_DIR="$ROOT_DIR/target"
S3TESTS_DIR="/tmp/s3-tests"
CONF_PATH="/tmp/s3tests.conf"
REPORT_DIR="/tmp/s3s-s3tests-report"
MINIO_DIR="/tmp/s3s-s3tests-minio"
S3S_PROXY_PID=""

mkdir -p "$TARGET_DIR"
mkdir -p "$REPORT_DIR"
mkdir -p "$MINIO_DIR"

if [ -z "$RUST_LOG" ]; then
    export RUST_LOG="s3s_proxy=debug,s3s_aws=debug,s3s=debug"
fi

cleanup() {
    if [ -n "$S3S_PROXY_PID" ]; then
        kill "$S3S_PROXY_PID" || true
    else
        local proxy_pids
        proxy_pids=$(pgrep -x s3s-proxy || true)
        if [ -n "$proxy_pids" ]; then
            for pid in $proxy_pids; do
                kill "$pid" || true
            done
        fi
    fi

    reset_minio_container
}

trap cleanup EXIT

check_docker() {
    if ! command -v docker >/dev/null 2>&1; then
        echo "docker is required to run minio"
        exit 1
    fi
    if ! docker info >/dev/null 2>&1; then
        echo "docker is not running"
        exit 1
    fi
}

wait_for_minio() {
    local attempt
    for attempt in {1..30}; do
        if curl -s -o /dev/null http://localhost:9000/minio/health/live; then
            return 0
        fi
        sleep 1
    done
    echo "minio did not become ready"
    return 1
}

wait_for_proxy() {
    local attempt
    for attempt in {1..30}; do
        if curl -s -o /dev/null http://localhost:8014/; then
            return 0
        fi
        sleep 1
    done
    echo "s3s-proxy did not become ready"
    return 1
}

ensure_minio_running() {
    local container_id="$1"
    if [ -z "$container_id" ]; then
        echo "minio container id missing"
        exit 1
    fi
    if ! docker container inspect -f '{{.State.Running}}' "$container_id" | grep -q true; then
        echo "minio container did not stay running"
        exit 1
    fi
}

ensure_proxy_running() {
    if ! kill -0 "$S3S_PROXY_PID" >/dev/null 2>&1; then
        echo "s3s-proxy failed to start"
        exit 1
    fi
}

reset_minio_container() {
    docker stop s3tests-minio || true
    docker container rm s3tests-minio || true
}

if ! command -v s3s-proxy >/dev/null 2>&1; then
    echo "s3s-proxy is required; run: just install s3s-proxy"
    exit 1
fi

check_docker
reset_minio_container
if ! MINIO_CONTAINER_ID=$(docker run -d \
    --name s3tests-minio \
    -p 9000:9000 -p 9001:9001 \
    -e "MINIO_DOMAIN=localhost:9000" \
    -e "MINIO_HTTP_TRACE=1" \
    -v "$MINIO_DIR":/data \
    minio/minio:latest server /data --console-address ":9001"); then
    echo "failed to start minio container"
    exit 1
fi

wait_for_minio
ensure_minio_running "$MINIO_CONTAINER_ID"

export AWS_ACCESS_KEY_ID=minioadmin
export AWS_SECRET_ACCESS_KEY=minioadmin
export AWS_REGION=us-east-1

s3s-proxy \
    --host          localhost       \
    --port          8014            \
    --domain        localhost:8014  \
    --endpoint-url  http://localhost:9000 > "$TARGET_DIR/s3s-proxy.log" 2>&1 &
S3S_PROXY_PID=$!

wait_for_proxy
ensure_proxy_running
ensure_minio_running "$MINIO_CONTAINER_ID"

if [ -d "$S3TESTS_DIR/.git" ]; then
    git -C "$S3TESTS_DIR" fetch --depth 1 origin HEAD
    git -C "$S3TESTS_DIR" reset --hard FETCH_HEAD
else
    rm -rf "$S3TESTS_DIR"
    git clone --depth 1 https://github.com/ceph/s3-tests.git "$S3TESTS_DIR"
fi
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
if [ "${S3TEST_ARGS[0]:-}" = "--" ]; then
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
PYTEST_STATUS=${PIPESTATUS[0]}; set -e
popd

REPORT_STATUS=0
if [ -f "$REPORT_DIR/junit.xml" ]; then
    cp "$REPORT_DIR/junit.xml" "$TARGET_DIR/s3-tests.junit.xml"
    python3 "$ROOT_DIR/scripts/report-s3tests.py" "$TARGET_DIR/s3-tests.junit.xml" || REPORT_STATUS=$?
else
    echo "missing junit report at $REPORT_DIR/junit.xml"
    exit 1
fi

if [ $PYTEST_STATUS -ne 0 ]; then
    exit $PYTEST_STATUS
fi

exit $REPORT_STATUS
