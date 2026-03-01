#!/usr/bin/env python3
"""
Test PutObject behavior when Content-Length is NOT specified.

This script tests:
1. PutObject via boto3 (which normally sets Content-Length)
2. PutObject via raw HTTP with Transfer-Encoding: chunked (no Content-Length)
3. PutObject via raw HTTP with no Content-Length and no body
4. PutObject via raw HTTP with body but explicitly no Content-Length

Usage:
    python3 tests/boto3/put_object_no_content_length.py [endpoint_url]

Default endpoint: http://localhost:9000 (MinIO)

Environment variables:
    AWS_ENDPOINT_URL     - S3 endpoint URL
    AWS_ACCESS_KEY_ID    - Access key (default: minioadmin)
    AWS_SECRET_ACCESS_KEY- Secret key (default: minioadmin)
    AWS_DEFAULT_REGION   - Region (default: us-east-1)
"""

import hashlib
import hmac
import os
import socket
import sys
import uuid
from datetime import datetime, timezone
from urllib.parse import quote, urlparse

import boto3
import requests
from botocore.config import Config

ENDPOINT_URL = os.environ.get("AWS_ENDPOINT_URL", "http://localhost:9000")
ACCESS_KEY = os.environ.get("AWS_ACCESS_KEY_ID", "minioadmin")
SECRET_KEY = os.environ.get("AWS_SECRET_ACCESS_KEY", "minioadmin")
REGION = os.environ.get("AWS_DEFAULT_REGION", "us-east-1")

if len(sys.argv) > 1:
    ENDPOINT_URL = sys.argv[1]

BUCKET = f"test-put-no-cl-{uuid.uuid4().hex[:8]}"

results = []


def log(msg):
    print(f"  {msg}")


def record(test_name, status, detail=""):
    results.append({"test": test_name, "status": status, "detail": detail})
    icon = "✓" if status == "PASS" else ("✗" if status == "FAIL" else "⚠")
    print(f"  [{icon}] {test_name}: {status} {detail}")


def make_client():
    return boto3.client(
        "s3",
        endpoint_url=ENDPOINT_URL,
        aws_access_key_id=ACCESS_KEY,
        aws_secret_access_key=SECRET_KEY,
        region_name=REGION,
        config=Config(signature_version="s3v4"),
    )


def setup():
    """Create test bucket."""
    client = make_client()
    try:
        client.create_bucket(Bucket=BUCKET)
        log(f"Created bucket: {BUCKET}")
    except Exception as e:
        log(f"Bucket creation: {e}")


def cleanup():
    """Delete test bucket and all objects."""
    client = make_client()
    try:
        resp = client.list_objects_v2(Bucket=BUCKET)
        for obj in resp.get("Contents", []):
            client.delete_object(Bucket=BUCKET, Key=obj["Key"])
    except Exception:
        pass
    try:
        client.delete_bucket(Bucket=BUCKET)
        log(f"Deleted bucket: {BUCKET}")
    except Exception:
        pass


# --- SigV4 signing helpers ---


def sign(key, msg):
    return hmac.new(key, msg.encode("utf-8"), hashlib.sha256).digest()


def get_signature_key(key, date_stamp, region, service):
    k_date = sign(("AWS4" + key).encode("utf-8"), date_stamp)
    k_region = sign(k_date, region)
    k_service = sign(k_region, service)
    k_signing = sign(k_service, "aws4_request")
    return k_signing


def make_sigv4_headers(method, url, headers, payload=b"", content_sha256=None):
    """
    Generate AWS Signature V4 headers for a request.
    Returns a dict of headers to add.
    """
    parsed = urlparse(url)
    host = parsed.netloc
    uri = quote(parsed.path, safe="/")
    query = parsed.query

    t = datetime.now(timezone.utc)
    amz_date = t.strftime("%Y%m%dT%H%M%SZ")
    date_stamp = t.strftime("%Y%m%d")

    if content_sha256 is None:
        content_sha256 = hashlib.sha256(payload).hexdigest()

    # Build canonical headers
    canonical_headers_dict = {
        "host": host,
        "x-amz-content-sha256": content_sha256,
        "x-amz-date": amz_date,
    }
    # Add content-length if present
    if "content-length" in headers:
        canonical_headers_dict["content-length"] = str(headers["content-length"])
    # Add content-type if present
    if "content-type" in headers:
        canonical_headers_dict["content-type"] = headers["content-type"]
    # Add transfer-encoding if present
    if "transfer-encoding" in headers:
        canonical_headers_dict["transfer-encoding"] = headers["transfer-encoding"]

    signed_header_keys = sorted(canonical_headers_dict.keys())
    signed_headers = ";".join(signed_header_keys)
    canonical_headers = ""
    for k in signed_header_keys:
        canonical_headers += f"{k}:{canonical_headers_dict[k]}\n"

    canonical_request = (
        f"{method}\n"
        f"{uri}\n"
        f"{query}\n"
        f"{canonical_headers}\n"
        f"{signed_headers}\n"
        f"{content_sha256}"
    )

    credential_scope = f"{date_stamp}/{REGION}/s3/aws4_request"
    string_to_sign = (
        f"AWS4-HMAC-SHA256\n"
        f"{amz_date}\n"
        f"{credential_scope}\n"
        f"{hashlib.sha256(canonical_request.encode('utf-8')).hexdigest()}"
    )

    signing_key = get_signature_key(SECRET_KEY, date_stamp, REGION, "s3")
    signature = hmac.new(
        signing_key, string_to_sign.encode("utf-8"), hashlib.sha256
    ).hexdigest()

    authorization = (
        f"AWS4-HMAC-SHA256 "
        f"Credential={ACCESS_KEY}/{credential_scope}, "
        f"SignedHeaders={signed_headers}, "
        f"Signature={signature}"
    )

    return {
        "x-amz-date": amz_date,
        "x-amz-content-sha256": content_sha256,
        "Authorization": authorization,
    }


# --- Test cases ---


def test_1_normal_put_with_content_length():
    """Normal PutObject with Content-Length header (baseline)."""
    test_name = "1. Normal PUT with Content-Length"
    try:
        client = make_client()
        body = b"hello world"
        client.put_object(Bucket=BUCKET, Key="test-normal.txt", Body=body)
        # Verify
        resp = client.get_object(Bucket=BUCKET, Key="test-normal.txt")
        data = resp["Body"].read()
        assert data == body, f"Data mismatch: {data!r}"
        record(test_name, "PASS", f"Uploaded {len(body)} bytes successfully")
    except Exception as e:
        record(test_name, "FAIL", str(e))


def test_2_put_with_chunked_transfer_encoding():
    """PutObject with Transfer-Encoding: chunked, no Content-Length header.
    Expected: 411 MissingContentLength (MinIO requires Content-Length)."""
    test_name = "2. PUT with chunked Transfer-Encoding (no Content-Length)"
    try:
        body = b"chunked test data"
        url = f"{ENDPOINT_URL}/{BUCKET}/test-chunked.txt"
        payload_hash = hashlib.sha256(body).hexdigest()

        headers = {
            "transfer-encoding": "chunked",
            "content-type": "application/octet-stream",
        }
        sig_headers = make_sigv4_headers(
            "PUT", url, headers, body, content_sha256=payload_hash
        )
        headers.update(sig_headers)

        # Use requests with chunked encoding
        def gen():
            yield body

        resp = requests.put(url, headers=headers, data=gen(), timeout=30)
        log(f"  Response: {resp.status_code} {resp.text[:200] if resp.text else ''}")

        if resp.status_code == 411:
            record(test_name, "PASS", "HTTP 411 MissingContentLength (expected)")
        elif resp.status_code in (200, 201):
            # Verify
            client = make_client()
            get_resp = client.get_object(Bucket=BUCKET, Key="test-chunked.txt")
            data = get_resp["Body"].read()
            if data == body:
                record(
                    test_name,
                    "FAIL",
                    f"HTTP {resp.status_code} accepted (should reject with 411)",
                )
            else:
                record(
                    test_name,
                    "FAIL",
                    f"HTTP {resp.status_code} but data mismatch: {data!r} != {body!r}",
                )
        else:
            record(test_name, "FAIL", f"HTTP {resp.status_code}: {resp.text[:200]}")
    except Exception as e:
        record(test_name, "FAIL", str(e))


def test_3_put_empty_body_no_content_length():
    """PutObject with empty body and no Content-Length header."""
    test_name = "3. PUT empty body (no Content-Length)"
    try:
        url = f"{ENDPOINT_URL}/{BUCKET}/test-empty.txt"
        payload_hash = hashlib.sha256(b"").hexdigest()

        headers = {
            "content-type": "application/octet-stream",
        }
        sig_headers = make_sigv4_headers(
            "PUT", url, headers, b"", content_sha256=payload_hash
        )
        headers.update(sig_headers)

        # Send with empty body - no Content-Length
        resp = requests.put(url, headers=headers, data=b"", timeout=30)
        log(f"  Response: {resp.status_code} {resp.text[:200] if resp.text else ''}")

        if resp.status_code in (200, 201):
            client = make_client()
            get_resp = client.get_object(Bucket=BUCKET, Key="test-empty.txt")
            data = get_resp["Body"].read()
            if data == b"":
                record(
                    test_name, "PASS", f"HTTP {resp.status_code}, empty object created"
                )
            else:
                record(
                    test_name,
                    "FAIL",
                    f"HTTP {resp.status_code} but data not empty: {data!r}",
                )
        else:
            record(test_name, "FAIL", f"HTTP {resp.status_code}: {resp.text[:200]}")
    except Exception as e:
        record(test_name, "FAIL", str(e))


def test_4_put_with_unsigned_payload_no_content_length():
    """PutObject with UNSIGNED-PAYLOAD and no Content-Length header using chunked transfer.
    Expected: 411 MissingContentLength (MinIO requires Content-Length)."""
    test_name = "4. PUT UNSIGNED-PAYLOAD chunked (no Content-Length)"
    try:
        body = b"unsigned payload test"
        url = f"{ENDPOINT_URL}/{BUCKET}/test-unsigned-chunked.txt"

        headers = {
            "transfer-encoding": "chunked",
            "content-type": "application/octet-stream",
        }
        sig_headers = make_sigv4_headers(
            "PUT", url, headers, b"", content_sha256="UNSIGNED-PAYLOAD"
        )
        headers.update(sig_headers)

        def gen():
            yield body

        resp = requests.put(url, headers=headers, data=gen(), timeout=30)
        log(f"  Response: {resp.status_code} {resp.text[:200] if resp.text else ''}")

        if resp.status_code == 411:
            record(test_name, "PASS", "HTTP 411 MissingContentLength (expected)")
        elif resp.status_code in (200, 201):
            record(
                test_name,
                "FAIL",
                f"HTTP {resp.status_code} accepted (should reject with 411)",
            )
        else:
            record(test_name, "FAIL", f"HTTP {resp.status_code}: {resp.text[:200]}")
    except Exception as e:
        record(test_name, "FAIL", str(e))


def test_5_raw_socket_put_no_content_length():
    """
    PutObject via raw socket: Send a PUT request with body but NO Content-Length
    and NO Transfer-Encoding headers. HTTP/1.1 requires one or the other for
    requests with a body.
    """
    test_name = "5. Raw socket PUT with body, no Content-Length, no Transfer-Encoding"
    try:
        body = b"raw socket test"
        parsed = urlparse(ENDPOINT_URL)
        host = parsed.hostname
        port = parsed.port or 80
        path = f"/{BUCKET}/test-raw.txt"

        payload_hash = hashlib.sha256(body).hexdigest()

        headers = {}
        sig_headers = make_sigv4_headers(
            "PUT", f"{ENDPOINT_URL}{path}", headers, body, content_sha256=payload_hash
        )

        # Build raw HTTP request without Content-Length
        req_lines = [
            f"PUT {path} HTTP/1.1",
            f"Host: {parsed.netloc}",
            f"x-amz-date: {sig_headers['x-amz-date']}",
            f"x-amz-content-sha256: {sig_headers['x-amz-content-sha256']}",
            f"Authorization: {sig_headers['Authorization']}",
            "Content-Type: application/octet-stream",
            "",
            "",
        ]
        raw_request = "\r\n".join(req_lines).encode("utf-8") + body

        sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        sock.settimeout(10)
        sock.connect((host, port))
        sock.sendall(raw_request)

        # Read response
        response = b""
        try:
            while True:
                chunk = sock.recv(4096)
                if not chunk:
                    break
                response += chunk
        except socket.timeout:
            pass
        finally:
            sock.close()

        resp_str = response.decode("utf-8", errors="replace")
        status_line = resp_str.split("\r\n")[0] if resp_str else "NO RESPONSE"
        log(f"  Response: {status_line}")
        log(f"  Full response (first 500 chars): {resp_str[:500]}")

        if "200" in status_line:
            record(test_name, "PASS", f"Server accepted: {status_line}")
        elif "411" in status_line:
            record(
                test_name,
                "INFO",
                f"Server requires Content-Length (411): {status_line}",
            )
        elif "400" in status_line:
            record(
                test_name, "INFO", f"Server returned Bad Request (400): {status_line}"
            )
        elif "403" in status_line:
            record(test_name, "INFO", f"Server returned Forbidden (403): {status_line}")
        else:
            record(test_name, "INFO", f"Server response: {status_line}")
    except Exception as e:
        record(test_name, "FAIL", str(e))


def test_6_put_zero_content_length():
    """PutObject with Content-Length: 0 but non-empty body (should create empty object)."""
    test_name = "6. PUT with Content-Length: 0"
    try:
        url = f"{ENDPOINT_URL}/{BUCKET}/test-zero-cl.txt"
        payload_hash = hashlib.sha256(b"").hexdigest()

        headers = {
            "content-length": "0",
            "content-type": "application/octet-stream",
        }
        sig_headers = make_sigv4_headers(
            "PUT", url, headers, b"", content_sha256=payload_hash
        )
        headers.update(sig_headers)

        resp = requests.put(url, headers=headers, timeout=30)
        log(f"  Response: {resp.status_code} {resp.text[:200] if resp.text else ''}")

        if resp.status_code in (200, 201):
            client = make_client()
            get_resp = client.get_object(Bucket=BUCKET, Key="test-zero-cl.txt")
            data = get_resp["Body"].read()
            record(
                test_name, "PASS", f"HTTP {resp.status_code}, stored {len(data)} bytes"
            )
        else:
            record(test_name, "FAIL", f"HTTP {resp.status_code}: {resp.text[:200]}")
    except Exception as e:
        record(test_name, "FAIL", str(e))


def test_7_put_via_requests_no_content_length():
    """
    PutObject via requests library, trying to avoid Content-Length.
    requests by default adds Content-Length for known-length bodies.
    We use a generator to force chunked transfer-encoding.
    Expected: 411 MissingContentLength (MinIO requires Content-Length).
    """
    test_name = "7. PUT via requests generator (forces chunked)"
    try:
        body = b"generator chunked body content"
        url = f"{ENDPOINT_URL}/{BUCKET}/test-requests-chunked.txt"

        # Use UNSIGNED-PAYLOAD since we don't know final size
        headers = {
            "content-type": "application/octet-stream",
        }
        sig_headers = make_sigv4_headers(
            "PUT", url, headers, b"", content_sha256="UNSIGNED-PAYLOAD"
        )
        headers.update(sig_headers)

        def gen():
            yield body

        resp = requests.put(url, headers=headers, data=gen(), timeout=30)
        log(f"  Response: {resp.status_code} {resp.text[:200] if resp.text else ''}")

        if resp.status_code == 411:
            record(test_name, "PASS", "HTTP 411 MissingContentLength (expected)")
        elif resp.status_code in (200, 201):
            record(
                test_name,
                "FAIL",
                f"HTTP {resp.status_code} accepted (should reject with 411)",
            )
        else:
            record(test_name, "FAIL", f"HTTP {resp.status_code}: {resp.text[:200]}")
    except Exception as e:
        record(test_name, "FAIL", str(e))


def main():
    print(f"\n{'=' * 60}")
    print("PutObject without Content-Length - Behavior Test")
    print(f"Endpoint: {ENDPOINT_URL}")
    print(f"Bucket: {BUCKET}")
    print(f"{'=' * 60}\n")

    setup()

    try:
        print("\n--- Running tests ---\n")
        test_1_normal_put_with_content_length()
        test_2_put_with_chunked_transfer_encoding()
        test_3_put_empty_body_no_content_length()
        test_4_put_with_unsigned_payload_no_content_length()
        test_5_raw_socket_put_no_content_length()
        test_6_put_zero_content_length()
        test_7_put_via_requests_no_content_length()
    finally:
        print("\n--- Cleanup ---\n")
        cleanup()

    print(f"\n{'=' * 60}")
    print("Summary:")
    print(f"{'=' * 60}")
    for r in results:
        icon = "✓" if r["status"] == "PASS" else ("✗" if r["status"] == "FAIL" else "ℹ")
        print(f"  [{icon}] {r['test']}: {r['status']}")
        if r["detail"]:
            print(f"      {r['detail']}")
    print()

    # Return non-zero if any test failed
    failed = sum(1 for r in results if r["status"] == "FAIL")
    return failed


if __name__ == "__main__":
    sys.exit(main())
