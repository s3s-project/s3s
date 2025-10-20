#!/bin/bash -ex

cargo build -p s3s-e2e --release

export AWS_ACCESS_KEY_ID=minioadmin
export AWS_SECRET_ACCESS_KEY=minioadmin
export AWS_REGION=us-east-1
export AWS_ENDPOINT_URL=http://localhost:9000

if [ -z "$RUST_LOG" ]; then
    export RUST_LOG="s3s_e2e=debug,s3s_test=info,s3s=debug"
fi
export RUST_BACKTRACE=full

./target/release/s3s-e2e "$@"