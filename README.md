# s3s

[![Apache 2.0 licensed][license-badge]][license-url]
[![Unsafe Forbidden][unsafe-forbidden-badge]][unsafe-forbidden-url]

[license-badge]: https://img.shields.io/badge/license-Apache--2.0-blue.svg
[license-url]: ./LICENSE
[unsafe-forbidden-badge]: https://img.shields.io/badge/unsafe-forbidden-success.svg
[unsafe-forbidden-url]: https://github.com/rust-secure-code/safety-dance/

S3 Service Adapter


| crate                                    |                                                  version                                                  |                                        docs                                        |
| :--------------------------------------- | :-------------------------------------------------------------------------------------------------------: | :--------------------------------------------------------------------------------: |
| [s3s](./crates/s3s/)                     |           [![Crates.io](https://img.shields.io/crates/v/s3s.svg)](https://crates.io/crates/s3s)           |           [![Docs](https://docs.rs/s3s/badge.svg)](https://docs.rs/s3s/)           |
| [s3s-aws](./crates/s3s-aws/)             |       [![Crates.io](https://img.shields.io/crates/v/s3s-aws.svg)](https://crates.io/crates/s3s-aws)       |       [![Docs](https://docs.rs/s3s-aws/badge.svg)](https://docs.rs/s3s-aws/)       |
| [s3s-multipart](./crates/s3s-multipart/) | [![Crates.io](https://img.shields.io/crates/v/s3s-multipart.svg)](https://crates.io/crates/s3s-multipart) | [![Docs](https://docs.rs/s3s-multipart/badge.svg)](https://docs.rs/s3s-multipart/) |
| [s3s-chunked](./crates/s3s-chunked/)     |   [![Crates.io](https://img.shields.io/crates/v/s3s-chunked.svg)](https://crates.io/crates/s3s-chunked)   |   [![Docs](https://docs.rs/s3s-chunked/badge.svg)](https://docs.rs/s3s-chunked/)   |
| [s3s-sigv2](./crates/s3s-sigv2/)         |     [![Crates.io](https://img.shields.io/crates/v/s3s-sigv2.svg)](https://crates.io/crates/s3s-sigv2)     |     [![Docs](https://docs.rs/s3s-sigv2/badge.svg)](https://docs.rs/s3s-sigv2/)     |
| [s3s-sigv4](./crates/s3s-sigv4/)         |     [![Crates.io](https://img.shields.io/crates/v/s3s-sigv4.svg)](https://crates.io/crates/s3s-sigv4)     |     [![Docs](https://docs.rs/s3s-sigv4/badge.svg)](https://docs.rs/s3s-sigv4/)     |
| [s3s-rfc2047](./crates/s3s-rfc2047/)     |   [![Crates.io](https://img.shields.io/crates/v/s3s-rfc2047.svg)](https://crates.io/crates/s3s-rfc2047)   |   [![Docs](https://docs.rs/s3s-rfc2047/badge.svg)](https://docs.rs/s3s-rfc2047/)   |
| [s3s-fs](./crates/s3s-fs/)               |        [![Crates.io](https://img.shields.io/crates/v/s3s-fs.svg)](https://crates.io/crates/s3s-fs)        |        [![Docs](https://docs.rs/s3s-fs/badge.svg)](https://docs.rs/s3s-fs/)        |
| [s3s-http3](./crates/s3s-http3/)         |     [![Crates.io](https://img.shields.io/crates/v/s3s-http3.svg)](https://crates.io/crates/s3s-http3)     |     [![Docs](https://docs.rs/s3s-http3/badge.svg)](https://docs.rs/s3s-http3/)     |
| [s3s-model](./crates/s3s-model/)         |     [![Crates.io](https://img.shields.io/crates/v/s3s-model.svg)](https://crates.io/crates/s3s-model)     |     [![Docs](https://docs.rs/s3s-model/badge.svg)](https://docs.rs/s3s-model/)     |
| [s3s-policy](./crates/s3s-policy/)       |    [![Crates.io](https://img.shields.io/crates/v/s3s-policy.svg)](https://crates.io/crates/s3s-policy)    |    [![Docs](https://docs.rs/s3s-policy/badge.svg)](https://docs.rs/s3s-policy/)    |
| [s3s-proxy](./crates/s3s-proxy/)         |     [![Crates.io](https://img.shields.io/crates/v/s3s-proxy.svg)](https://crates.io/crates/s3s-proxy)     |                                 n/a (binary crate)                                 |
| [s3s-test](./crates/s3s-test/)           |      [![Crates.io](https://img.shields.io/crates/v/s3s-test.svg)](https://crates.io/crates/s3s-test)      |      [![Docs](https://docs.rs/s3s-test/badge.svg)](https://docs.rs/s3s-test/)      |
| [s3s-e2e](./crates/s3s-e2e/)             |       [![Crates.io](https://img.shields.io/crates/v/s3s-e2e.svg)](https://crates.io/crates/s3s-e2e)       |                                 n/a (binary crate)                                 |
| [s3s-wasm](./crates/s3s-wasm/)           |                                         not published (internal)                                          |                                        n/a                                         |

📚 **[Development documentation](https://s3s-project.github.io/s3s/)** for the `main` branch is available on GitHub Pages.

This experimental project intends to offer an ergonomic adapter for building S3-compatible services.

`s3s` implements Amazon S3 REST API in the form of a generic [hyper](https://github.com/hyperium/hyper) service. S3-compatible services can focus on the S3 API itself and don't have to care about the HTTP layer.

`s3s-aws` provides useful types and integration with [`aws-sdk-s3`](https://crates.io/crates/aws-sdk-s3).

`s3s-multipart` is a general-purpose asynchronous streaming parser for `multipart/form-data`. It is not tied to S3, and `s3s` builds on it for `POST Object` form uploads.
- Parsing: it consumes a stream of `bytes::Bytes` chunks and yields parts with their headers and data.
- S3 integration: [`crates/s3s/src/http/multipart.rs`](./crates/s3s/src/http/multipart.rs) keeps the S3 contract on top of the parser.
- Protocol tests: [`crates/s3s-multipart/tests/protocol/`](./crates/s3s-multipart/tests/protocol/).
- Benchmarks: [`crates/s3s-multipart/benches/`](./crates/s3s-multipart/benches/) — `parse_throughput`, `take_data_stream`, and `vs_multer`, which compares it with [`multer`](https://crates.io/crates/multer).
- Fuzzing: [`fuzz/fuzz_targets/multipart_parser.rs`](./fuzz/fuzz_targets/multipart_parser.rs).
- RFC differences: deliberate differences from RFC 2046 section 5.1 and RFC 7578 are documented in the [Compatibility notes](https://docs.rs/s3s-multipart/latest/s3s_multipart/#compatibility-notes).

`s3s-rfc2047` provides RFC 2047 MIME encoded-word encoding and decoding for non-ASCII header values.

`s3s-fs` implements the S3 API based on file system, as a sample implementation. It is designed for integration testing, which can be used to [mock an S3 client](https://github.com/Nugine/s3s/blob/main/crates/s3s-fs/tests/it_aws.rs). It also provides a binary for debugging. [Play it!](./CONTRIBUTING.md#play-the-test-server)

The same file system can also be served over HTTP/3: build the binary with the optional `http3` feature and pass `--http3`.

```sh
cargo install s3s-fs --features binary,http3
s3s-fs --http3 --port 8014 /data
```

- `http://host:port` keeps serving HTTP/1.1 and HTTP/2 over TCP, while `https://host:port` serves HTTP/3 on the same port.
- The server does not send `Alt-Svc`, so an HTTP/3 client connects explicitly.
- Without `--cert` and `--key` a self-signed certificate is generated; `--cert-out` writes the certificate a client has to trust. All three options require `--http3`.
- A default build has no HTTP/3 dependencies, and `s3s-fs` is published after `s3s-http3`.

`s3s-http3` is an experimental, opt-in HTTP/3 transport: it serves an `S3Service`, or any [`tower::Service`](https://crates.io/crates/tower), over QUIC, so the same S3 API is reachable over UDP with TLS 1.3 and the `h3` ALPN protocol. The adapter is server-side only, and its API may change while the HTTP/3 ecosystem evolves. Runnable servers are in `crates/s3s-fs/examples/http3-server.rs` and `crates/s3s-http3/examples/serve-with.rs`.

The other workspace members are supporting crates:
- `s3s-chunked` — the `aws-chunked` streaming request-body decoder, initialized as a placeholder while the implementation is under development.
- `s3s-sigv2`, `s3s-sigv4` — AWS Signature Version 2 and Version 4 parsing, canonicalization and signing.
- `s3s-model` — the S3 protocol model used by the code generator: the Smithy model types and the S3 error codes.
- `s3s-policy` — the S3 policy language model with wildcard pattern matching.
- `s3s-proxy` — a proxy implementation used by the end-to-end tests.
- `s3s-test` — a reusable test harness for S3-compatible services.
- `s3s-e2e` — the end-to-end test runner built on it.
- `s3s-wasm` — an internal crate (`publish = false`) that runs `s3s` under WebAssembly in its test suite.

## How it works

![architecture diagram](docs/arch/arch.svg)

The diagram above shows how `s3s` works. 

`s3s` converts HTTP requests to operation inputs before calling the user-defined service. 

`s3s` converts operation outputs or errors to HTTP responses after calling the user-defined service.

The data types, serialization and deserialization are generated from the smithy model in [aws-sdk-rust](https://github.com/awslabs/aws-sdk-rust) repository. We apply manual hacks to fix some problems in [smithy server codegen](https://smithy-lang.github.io/smithy-rs/design/server/overview.html) and make `s3s` ready to use now.

## Security

`S3Service` and other adapters in this project are not a complete security boundary. If they are exposed to the Internet directly, they may be **attacked**.

It is up to the user to implement security enhancements such as **HTTP body length limits**, object-size limits, rate limits and back pressure.

**Authentication is required for production deployments.** Without calling `set_auth`, the service accepts anonymous (unsigned) requests and skips authorization entirely: every S3 operation is open to any client that can reach the service, and signed requests fail with `NotImplemented` because no authentication provider is configured. A forgotten `set_auth` turns the service into a publicly readable and writable endpoint.

For streaming uploads (`PUT Object`, `UploadPart`), `s3s` applies a default 5 GiB object-size limit matching the AWS single-PUT limit; set `S3Config::put_object_max_size` to `None` to disable it and enforce deployment-specific caps in the `S3` implementation. For production, set it explicitly even though the default is already 5 GiB. `POST Object` keeps using `S3Config::post_object_max_file_size`.

List-type responses (`ListObjects`, `ListBuckets`, ...) are serialized in full by `s3s`: their memory usage grows with the number of entries the `S3` implementation returns. Implementations should paginate (`max-keys` / continuation tokens) and deployments should bound response sizes.

## Docker

Docker images are available at [GitHub Container Registry (GHCR)](https://github.com/s3s-project/s3s/pkgs/container/s3s).

See [Docker documentation](./docs/docker.md) for usage details.

## Contributing

+ [Development Guide](./CONTRIBUTING.md)

## Sponsor

We have a reward funds pool for contributors: <https://github.com/Nugine/s3s/issues/174>

If my open-source work has been helpful to you, please [sponsor me](https://github.com/Nugine#sponsor).

Every little bit helps. Thank you!
