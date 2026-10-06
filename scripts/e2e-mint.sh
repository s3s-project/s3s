#!/bin/bash
# SPDX-License-Identifier: Apache-2.0
# SPDX-FileCopyrightText: 2023-2026 The s3s Authors

# The proxy starts the backend container and binds its port afterwards, so a fixed
# sleep races with it: a suite that reaches the proxy before the listener exists is
# answered with `Connection refused`, and mint does not retry a suite that fails.
# Poll the health endpoint the proxy forwards to the backend instead - it answers
# only once the proxy listens and the backend is ready - and print the proxy log
# when it never comes up. Override MINT_PROXY_READY_TIMEOUT to wait longer than the
# default 120 seconds. The mint run itself is piped through `tee`, so this script's
# exit status does not carry it: a failing suite is reported by the gate step that
# reads the mint log, and this script only exits non-zero when the proxy never
# becomes ready.
PROXY_URL="http://localhost:8014"
PROXY_LOG="target/s3s-proxy.log"
PROXY_READY_TIMEOUT="${MINT_PROXY_READY_TIMEOUT:-120}"

mkdir -p target
./scripts/s3s-proxy.sh > "$PROXY_LOG" 2>&1 &
proxy_pid=$!

start=$SECONDS
proxy_ready=0
deadline=$((SECONDS + PROXY_READY_TIMEOUT))
while ((SECONDS < deadline)); do
    if curl -fsS --max-time 5 -o /dev/null "$PROXY_URL/minio/health/ready" 2>/dev/null; then
        proxy_ready=1
        break
    fi
    if ! kill -0 "$proxy_pid" 2>/dev/null; then
        echo "e2e-mint: s3s-proxy exited before it became ready" >&2
        break
    fi
    sleep 0.5
done

if ((proxy_ready == 0)); then
    echo "e2e-mint: $PROXY_URL did not become ready within ${PROXY_READY_TIMEOUT}s" >&2
    echo "e2e-mint: last lines of $PROXY_LOG:" >&2
    if [ -f "$PROXY_LOG" ]; then
        tail -n 50 "$PROXY_LOG" >&2
    fi
    exit 1
fi

echo "e2e-mint: the proxy is ready at $PROXY_URL after $((SECONDS - start))s"
./scripts/mint.sh | tee target/mint.log
