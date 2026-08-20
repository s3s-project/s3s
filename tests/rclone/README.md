# rclone S3 integration test

This test exercises an S3 endpoint through the official rclone client. It is a
client-workflow smoke test that complements protocol-focused suites such as
MinIO MinT and Ceph s3-tests.

The runner creates a uniquely named temporary bucket and verifies:

- recursive upload, listing, download, and byte-for-byte content checks;
- empty objects, nested keys, spaces, Unicode, and small-object fan-out;
- a forced three-part multipart upload for a 12 MiB object;
- a ranged read from the multipart object;
- remote-to-remote object copy and single-object deletion; and
- bucket cleanup after success or failure.

The default client image and expected version are pinned in
[`scripts/rclone.env`](../../scripts/rclone.env). The Docker wrapper uses host
networking so it can reach an S3 server listening on the host.

## Run against s3s-fs

Install `s3s-fs`, ensure Docker is running, and use the self-contained wrapper:

```bash
just install s3s-fs
./scripts/e2e-rclone-fs.sh
```

## Run against another S3 endpoint

Set standard AWS connection variables and invoke the reusable runner:

```bash
export AWS_ENDPOINT_URL=http://localhost:9000
export AWS_ACCESS_KEY_ID=example-access-key
export AWS_SECRET_ACCESS_KEY=example-secret-key
export AWS_REGION=us-east-1
./scripts/e2e-rclone.sh
```

Optional variables include `AWS_SESSION_TOKEN`, `RCLONE_S3_PROVIDER`,
`RCLONE_S3_FORCE_PATH_STYLE`, `RCLONE_TEST_BUCKET`, and
`RCLONE_E2E_TIMEOUT`. `RCLONE_LOW_LEVEL_RETRIES` controls retry attempts for
individual S3 requests. Set `RCLONE_KEEP_WORKDIR=1` to retain local artifacts
for debugging. To try a different client release, override
`RCLONE_IMAGE_REF` and `RCLONE_VERSION_LINE` together. A custom
`RCLONE_TEST_BUCKET` must name a new, disposable bucket because the test
removes it when finished.
