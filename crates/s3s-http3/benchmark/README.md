# HTTP/2 TLS and HTTP/3 benchmarks

Requires `just`, `hyperfine`, an HTTP/3-capable curl with `--parallel-max-host`
and `%header{}` support, OpenSSL, and standard GNU/Linux tools. The examples
use local self-signed TLS and an unauthenticated, disposable filesystem backend.

Run from the repository root:

```sh
# Small GET/PUT correctness check; starts and stops both servers.
just --justfile crates/s3s-http3/benchmark/justfile bench-check

# GET and PUT, 16 and 64 MiB objects, concurrency 1 and 8.
# Eight requests per batch; starts/stops the servers automatically.
just --justfile crates/s3s-http3/benchmark/justfile bench-large
```

For a custom comparison, start the servers, run the desired cases, then stop:

```sh
just --justfile crates/s3s-http3/benchmark/justfile bench-up

# Arguments: request count, concurrency limit, GET|PUT, object size in MiB.
just --justfile crates/s3s-http3/benchmark/justfile bench-throughput 8 8 GET 64
just --justfile crates/s3s-http3/benchmark/justfile bench-throughput 8 8 PUT 64
just --justfile crates/s3s-http3/benchmark/justfile bench-throughput 2 2 PUT 256

just --justfile crates/s3s-http3/benchmark/justfile bench-down
```

Object size accepts integer MiB from 1 to 1024. Existing calls such as
`bench-throughput 200 16` still mean 200 GETs of a 1 MiB object.
`bench-http3` retains the original small-latency/GET suite. `bench-large N C`
changes the larger matrix's request count and maximum concurrency.

## What gets measured

- One connection per protocol, with up to the selected number of concurrent
  streams. One warmup and five measured batches, H2 followed by H3.
- Each batch starts a new curl process. Times include command/harness startup,
  connection establishment, application processing, and transfer completion.
- GET repeatedly reads one seeded object. PUT streams the same payload file to
  a distinct object key for every request; subsequent batches overwrite that
  same bounded set. This avoids simultaneous writes to the same object.
- Both use TLS, no proxy, and an explicit empty `Expect` header to avoid an
  incidental 100-continue wait. Transport/socket defaults are not tuned.
- PUT includes filesystem writes, MD5 calculation, and metadata work. The
  sample backend flushes its userspace writer but does not call `fsync`, so
  these are not durable-storage throughput measurements. GET may hit page cache.
- Hyperfine CPU times belong to the client/harness, not the server.

Every warmup and timed batch checks the protocol, connection count, response
count, HTTP status, and transferred byte count. PUT also checks each returned
ETag against the source file's MD5. After timing, one uploaded object per
protocol is downloaded and hashed to check persistence; that readback is
outside the timing interval. Validation failures stop the benchmark.

## Results and disk usage

Each comparison prints a fresh directory under `target/http3-bench`, for example
`PUT-64m-8-8-XXXXXX`. It contains `timing.json`, per-batch request/stderr logs,
the payload checksum, and PUT readback checksums. Results are not overwritten.
Convert batch duration to MiB/s with `request_count * size_mib / seconds`.

Fixtures and seeded GET objects are retained for reuse. PUT objects use a
run-specific prefix and are deleted on exit; only that run's generated keys
are targeted. The runner checks a conservative disk budget, including atomic
overwrite temporary files and a 1 GiB reserve. Cleanup failures are reported.
Avoid overlapping benchmark runs against the same server ports.

The lower-level `bench-load` remains available for diagnostics:

```sh
# protocol, count, concurrency, destination, method, size MiB, PUT prefix
just --justfile crates/s3s-http3/benchmark/justfile bench-load h3 8 8 127.0.0.1 GET 64
```

`bench-load` does not seed data, apply the runner's disk budget, or clean up
uploads. Its PUT mode overwrites `bench/put-PREFIX-INDEX` keys; use a unique
prefix and handle cleanup when calling it directly. Use `rtk proxy just` when
capturing its full records, since compressed output can omit records.
