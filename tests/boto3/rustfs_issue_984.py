"""
Regression tests for https://github.com/rustfs/rustfs/issues/984

POST Object with presigned post must enforce content-length-range conditions.

Test points:
1. File exceeding content-length-range max should be rejected with EntityTooLarge.
2. File within content-length-range should be accepted (200/204).
3. File smaller than content-length-range min should be rejected with EntityTooSmall.
"""

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
    # Delete all objects first
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


def test_content_length_range_exceeds_max():
    """File larger than content-length-range max must be rejected with EntityTooLarge."""
    client = make_client()
    bucket = f"test-issue984-{uuid.uuid4().hex[:8]}"
    create_bucket(client, bucket)

    try:
        key = "test-file.txt"

        # Generate presigned POST with content-length-range [0, 10]
        presigned = client.generate_presigned_post(
            Bucket=bucket,
            Key=key,
            Conditions=[["content-length-range", 0, 10]],
            ExpiresIn=3600,
        )

        # boto3 adds a {"bucket": ...} policy condition but does not include it
        # in the form fields. s3s validates bucket against form fields, so add it.
        presigned["fields"]["bucket"] = bucket

        # Upload a file much larger than 10 bytes
        files = {"file": ("test.txt", b"very long contents, longer than 10 bytes")}
        resp = requests.post(presigned["url"], data=presigned["fields"], files=files)

        assert resp.status_code == 400, f"Expected 400, got {resp.status_code}"
        assert "EntityTooLarge" in resp.text, (
            f"Expected EntityTooLarge in response: {resp.text}"
        )
        print("PASS: test_content_length_range_exceeds_max")
    finally:
        delete_bucket(client, bucket)


def test_content_length_range_within_limit():
    """File within content-length-range should be accepted."""
    client = make_client()
    bucket = f"test-issue984-{uuid.uuid4().hex[:8]}"
    create_bucket(client, bucket)

    try:
        key = "test-file.txt"

        # Generate presigned POST with content-length-range [0, 1000]
        presigned = client.generate_presigned_post(
            Bucket=bucket,
            Key=key,
            Conditions=[["content-length-range", 0, 1000]],
            ExpiresIn=3600,
        )

        presigned["fields"]["bucket"] = bucket

        # Upload a file within limit
        files = {"file": ("test.txt", b"short")}
        resp = requests.post(presigned["url"], data=presigned["fields"], files=files)

        assert resp.status_code in (200, 204), (
            f"Expected 200 or 204, got {resp.status_code}: {resp.text}"
        )
        print("PASS: test_content_length_range_within_limit")
    finally:
        delete_bucket(client, bucket)


def test_content_length_range_below_min():
    """File smaller than content-length-range min must be rejected with EntityTooSmall."""
    client = make_client()
    bucket = f"test-issue984-{uuid.uuid4().hex[:8]}"
    create_bucket(client, bucket)

    try:
        key = "test-file.txt"

        # Generate presigned POST with content-length-range [100, 1000]
        presigned = client.generate_presigned_post(
            Bucket=bucket,
            Key=key,
            Conditions=[["content-length-range", 100, 1000]],
            ExpiresIn=3600,
        )

        presigned["fields"]["bucket"] = bucket

        # Upload a file smaller than 100 bytes
        files = {"file": ("test.txt", b"tiny")}
        resp = requests.post(presigned["url"], data=presigned["fields"], files=files)

        assert resp.status_code == 400, f"Expected 400, got {resp.status_code}"
        assert "EntityTooSmall" in resp.text, (
            f"Expected EntityTooSmall in response: {resp.text}"
        )
        print("PASS: test_content_length_range_below_min")
    finally:
        delete_bucket(client, bucket)


def main():
    tests = [
        test_content_length_range_exceeds_max,
        test_content_length_range_within_limit,
        test_content_length_range_below_min,
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
