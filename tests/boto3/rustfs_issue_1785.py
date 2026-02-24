"""
Regression tests for https://github.com/rustfs/rustfs/issues/1785

POST Object with presigned post: boto3 adds a {"bucket": "..."} condition
to the policy but does NOT include "bucket" in the returned form fields.
The server must validate the bucket condition against the URL path bucket,
not form fields.

Test points:
1. Presigned POST without explicit bucket condition should work without
   adding bucket to form fields.
2. The decoded policy contains a bucket condition even though fields do not.
"""

import base64
import json
import os
import sys
import uuid

import boto3
import requests
from botocore.config import Config

ENDPOINT_URL = os.environ.get("AWS_ENDPOINT_URL", "http://localhost:8014")
ACCESS_KEY = os.environ.get("AWS_ACCESS_KEY_ID", "AKEXAMPLES3S")
SECRET_KEY = os.environ.get("AWS_SECRET_ACCESS_KEY", "SKEXAMPLES3S")
REGION = os.environ.get("AWS_DEFAULT_REGION", "us-east-1")


def make_client():
    return boto3.client(
        "s3",
        endpoint_url=ENDPOINT_URL,
        aws_access_key_id=ACCESS_KEY,
        aws_secret_access_key=SECRET_KEY,
        region_name=REGION,
        config=Config(signature_version="s3v4"),
    )


def create_bucket(client, bucket: str):
    client.create_bucket(Bucket=bucket)


def delete_bucket(client, bucket: str):
    try:
        resp = client.list_objects_v2(Bucket=bucket)
        for obj in resp.get("Contents", []):
            client.delete_object(Bucket=bucket, Key=obj["Key"])
    except Exception:
        pass
    try:
        client.delete_bucket(Bucket=bucket)
    except Exception:
        pass


def test_presigned_post_without_bucket_in_fields():
    """
    Presigned POST should succeed without manually adding bucket to fields.
    boto3 adds {"bucket": "..."} to the policy but not to the fields.
    The server must match the bucket condition against the URL path.
    """
    client = make_client()
    bucket = f"test-issue1785-{uuid.uuid4().hex[:8]}"
    create_bucket(client, bucket)

    try:
        key = "test-file.txt"

        presigned = client.generate_presigned_post(
            Bucket=bucket,
            Key=key,
            ExpiresIn=3600,
        )

        # Verify that boto3 does NOT include bucket in the fields
        assert "bucket" not in presigned["fields"], (
            f"Expected 'bucket' not in fields, but got: {presigned['fields']}"
        )

        # Verify that the policy DOES contain a bucket condition
        policy_b64 = presigned["fields"]["policy"]
        policy_json = json.loads(base64.b64decode(policy_b64))
        bucket_conditions = [
            c
            for c in policy_json["conditions"]
            if isinstance(c, dict) and "bucket" in c
        ]
        assert len(bucket_conditions) > 0, (
            f"Expected bucket condition in policy, got: {policy_json['conditions']}"
        )
        assert bucket_conditions[0]["bucket"] == bucket

        # Do NOT add bucket to fields — this is the bug scenario
        files = {"file": ("test.txt", b"hello world")}
        resp = requests.post(presigned["url"], data=presigned["fields"], files=files)

        assert resp.status_code in (200, 204), (
            f"Expected 200 or 204, got {resp.status_code}: {resp.text}"
        )
        print("PASS: test_presigned_post_without_bucket_in_fields")
    finally:
        delete_bucket(client, bucket)


def test_presigned_post_with_conditions_without_bucket_in_fields():
    """
    Presigned POST with extra conditions should work without bucket in fields.
    """
    client = make_client()
    bucket = f"test-issue1785-{uuid.uuid4().hex[:8]}"
    create_bucket(client, bucket)

    try:
        key = "uploads/${filename}"

        presigned = client.generate_presigned_post(
            Bucket=bucket,
            Key=key,
            Conditions=[
                ["content-length-range", 0, 10485760],
                ["starts-with", "$Content-Type", ""],
            ],
            ExpiresIn=3600,
        )

        assert "bucket" not in presigned["fields"], (
            f"Expected 'bucket' not in fields, but got: {presigned['fields']}"
        )

        files = {"file": ("test.txt", b"hello world")}
        resp = requests.post(presigned["url"], data=presigned["fields"], files=files)

        assert resp.status_code in (200, 204), (
            f"Expected 200 or 204, got {resp.status_code}: {resp.text}"
        )
        print("PASS: test_presigned_post_with_conditions_without_bucket_in_fields")
    finally:
        delete_bucket(client, bucket)


def main():
    tests = [
        test_presigned_post_without_bucket_in_fields,
        test_presigned_post_with_conditions_without_bucket_in_fields,
    ]
    passed = 0
    failed = 0
    for test in tests:
        try:
            test()
            passed += 1
        except Exception as e:
            print(f"FAIL: {test.__name__}: {e}")
            failed += 1

    print(f"\n{passed} passed, {failed} failed, {len(tests)} total")
    if failed > 0:
        sys.exit(1)


if __name__ == "__main__":
    main()
