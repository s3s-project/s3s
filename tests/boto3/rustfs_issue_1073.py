"""
Regression tests for https://github.com/rustfs/rustfs/issues/1073

POST Object with success_action_status and success_action_redirect fields.

Test points:
1. success_action_status=200 returns 200 OK with empty body.
2. success_action_status=201 returns 201 Created with XML body containing
   bucket, key, etag, and location.
3. success_action_status=204 (or omitted) returns 204 No Content (default).
4. success_action_redirect returns 303 See Other with Location header
   including bucket, key, and etag as query parameters.
5. Invalid success_action_status (e.g. 302) falls back to 204 No Content.
"""

import os
import sys
import uuid
import xml.etree.ElementTree as ET
from urllib.parse import parse_qs, urlparse

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


def presigned_post_with_fields(client, bucket, key, fields=None, conditions=None):
    """Generate a presigned POST with fields and matching policy conditions.

    Each entry in ``fields`` is automatically added as an ``eq`` condition
    in the policy so the server accepts it.
    """
    fields = fields or {}
    conditions = list(conditions or [])
    for fname, fvalue in fields.items():
        conditions.append({fname: fvalue})
    return client.generate_presigned_post(
        Bucket=bucket,
        Key=key,
        Fields=fields,
        Conditions=conditions,
        ExpiresIn=3600,
    )


def test_success_action_status_200():
    """success_action_status=200 should return 200 OK."""
    client = make_client()
    bucket = f"test-issue1073-{uuid.uuid4().hex[:8]}"
    create_bucket(client, bucket)

    try:
        key = "test-file.txt"
        presigned = presigned_post_with_fields(
            client, bucket, key,
            fields={"success_action_status": "200"},
        )

        files = {"file": ("test.txt", b"hello world")}
        resp = requests.post(presigned["url"], data=presigned["fields"], files=files)

        assert resp.status_code == 200, (
            f"Expected 200, got {resp.status_code}: {resp.text}"
        )
        print("PASS: test_success_action_status_200")
    finally:
        delete_bucket(client, bucket)


def test_success_action_status_201():
    """success_action_status=201 should return 201 Created with XML body."""
    client = make_client()
    bucket = f"test-issue1073-{uuid.uuid4().hex[:8]}"
    create_bucket(client, bucket)

    try:
        key = "test-file.txt"
        presigned = presigned_post_with_fields(
            client, bucket, key,
            fields={"success_action_status": "201"},
        )

        files = {"file": ("test.txt", b"hello world")}
        resp = requests.post(presigned["url"], data=presigned["fields"], files=files)

        assert resp.status_code == 201, (
            f"Expected 201, got {resp.status_code}: {resp.text}"
        )

        # Parse XML body and verify fields
        root = ET.fromstring(resp.text)
        ns = {"s3": "http://s3.amazonaws.com/doc/2006-03-01/"}

        # Try with namespace first, then without
        def find_text(tag):
            el = root.find(f"s3:{tag}", ns)
            if el is None:
                el = root.find(tag)
            return el.text if el is not None else None

        assert find_text("Bucket") == bucket, (
            f"Expected Bucket={bucket} in XML: {resp.text}"
        )
        assert find_text("Key") == key, f"Expected Key={key} in XML: {resp.text}"
        assert find_text("ETag") is not None, f"Expected ETag in XML: {resp.text}"
        assert find_text("Location") is not None, (
            f"Expected Location in XML: {resp.text}"
        )
        print("PASS: test_success_action_status_201")
    finally:
        delete_bucket(client, bucket)


def test_success_action_status_204():
    """success_action_status=204 should return 204 No Content."""
    client = make_client()
    bucket = f"test-issue1073-{uuid.uuid4().hex[:8]}"
    create_bucket(client, bucket)

    try:
        key = "test-file.txt"
        presigned = presigned_post_with_fields(
            client, bucket, key,
            fields={"success_action_status": "204"},
        )

        files = {"file": ("test.txt", b"hello world")}
        resp = requests.post(presigned["url"], data=presigned["fields"], files=files)

        assert resp.status_code == 204, (
            f"Expected 204, got {resp.status_code}: {resp.text}"
        )
        print("PASS: test_success_action_status_204")
    finally:
        delete_bucket(client, bucket)


def test_success_action_status_default():
    """Omitting success_action_status should return 204 No Content by default."""
    client = make_client()
    bucket = f"test-issue1073-{uuid.uuid4().hex[:8]}"
    create_bucket(client, bucket)

    try:
        key = "test-file.txt"
        presigned = presigned_post_with_fields(client, bucket, key)

        files = {"file": ("test.txt", b"hello world")}
        resp = requests.post(presigned["url"], data=presigned["fields"], files=files)

        assert resp.status_code == 204, (
            f"Expected 204 (default), got {resp.status_code}: {resp.text}"
        )
        print("PASS: test_success_action_status_default")
    finally:
        delete_bucket(client, bucket)


def test_success_action_redirect():
    """success_action_redirect should return 303 with Location containing bucket, key, etag."""
    client = make_client()
    bucket = f"test-issue1073-{uuid.uuid4().hex[:8]}"
    create_bucket(client, bucket)

    try:
        key = "test-file.txt"
        redirect_url = "https://example.com/upload-done"
        presigned = presigned_post_with_fields(
            client, bucket, key,
            fields={"success_action_redirect": redirect_url},
        )

        files = {"file": ("test.txt", b"hello world")}
        resp = requests.post(
            presigned["url"],
            data=presigned["fields"],
            files=files,
            allow_redirects=False,
        )

        assert resp.status_code == 303, (
            f"Expected 303, got {resp.status_code}: {resp.text}"
        )

        location = resp.headers.get("Location", "")
        assert location, "Expected Location header, got none"

        parsed = urlparse(location)
        qs = parse_qs(parsed.query)

        assert qs.get("bucket") == [bucket], (
            f"Expected bucket={bucket} in Location query: {location}"
        )
        assert qs.get("key") == [key], (
            f"Expected key={key} in Location query: {location}"
        )
        assert "etag" in qs, f"Expected etag in Location query: {location}"
        print("PASS: test_success_action_redirect")
    finally:
        delete_bucket(client, bucket)


def test_success_action_invalid_status():
    """An invalid success_action_status (e.g. 302) should fall back to 204 No Content."""
    client = make_client()
    bucket = f"test-issue1073-{uuid.uuid4().hex[:8]}"
    create_bucket(client, bucket)

    try:
        key = "test-file.txt"
        presigned = presigned_post_with_fields(
            client, bucket, key,
            fields={"success_action_status": "302"},
        )

        files = {"file": ("test.txt", b"hello world")}
        resp = requests.post(presigned["url"], data=presigned["fields"], files=files)

        assert resp.status_code == 204, (
            f"Expected 204 (fallback for invalid status), got {resp.status_code}: {resp.text}"
        )
        print("PASS: test_success_action_invalid_status")
    finally:
        delete_bucket(client, bucket)


def test_form_field_not_in_policy_rejected():
    """Form fields not declared in POST policy conditions must be rejected with AccessDenied."""
    client = make_client()
    bucket = f"test-issue1073-{uuid.uuid4().hex[:8]}"
    create_bucket(client, bucket)

    try:
        key = "test-file.txt"
        # Generate a presigned POST WITHOUT success_action_status in the Fields param,
        # so it will NOT be included in the policy conditions.
        presigned = presigned_post_with_fields(client, bucket, key)

        # Manually inject the field into the form data, bypassing the policy.
        presigned["fields"]["success_action_status"] = "200"

        files = {"file": ("test.txt", b"hello world")}
        resp = requests.post(presigned["url"], data=presigned["fields"], files=files)

        assert resp.status_code == 403, (
            f"Expected 403, got {resp.status_code}: {resp.text}"
        )
        assert "AccessDenied" in resp.text, (
            f"Expected AccessDenied in response: {resp.text}"
        )
        print("PASS: test_form_field_not_in_policy_rejected")
    finally:
        delete_bucket(client, bucket)


def main():
    tests = [
        test_success_action_status_200,
        test_success_action_status_201,
        test_success_action_status_204,
        test_success_action_status_default,
        test_success_action_redirect,
        test_success_action_invalid_status,
        test_form_field_not_in_policy_rejected,
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
