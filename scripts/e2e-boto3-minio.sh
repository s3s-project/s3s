#!/bin/bash -ex

mkdir -p /tmp/minio-boto3
docker stop boto3-minio 2>/dev/null || true
docker container rm boto3-minio 2>/dev/null || true
docker run \
    --name boto3-minio \
    -p 9000:9000 -p 9001:9001 \
    -e "MINIO_DOMAIN=localhost:9000" \
    -v /tmp/minio-boto3:/data \
    minio/minio:latest server /data --console-address ":9001" &

sleep 3

export AWS_ENDPOINT_URL=http://localhost:9000
export AWS_ACCESS_KEY_ID=minioadmin
export AWS_SECRET_ACCESS_KEY=minioadmin
export AWS_DEFAULT_REGION=us-east-1

uv run python3 tests/boto3/rustfs_issue_984.py
