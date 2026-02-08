# s3s

[![Apache 2.0 licensed][license-badge]][license-url]
[![Unsafe Forbidden][unsafe-forbidden-badge]][unsafe-forbidden-url]

[license-badge]: https://img.shields.io/badge/license-Apache--2.0-blue.svg
[license-url]: ./LICENSE
[unsafe-forbidden-badge]: https://img.shields.io/badge/unsafe-forbidden-success.svg
[unsafe-forbidden-url]: https://github.com/rust-secure-code/safety-dance/

S3 Service Adapter


| crate                      |                                           version                                           |                                 docs                                 |
| :------------------------- | :-----------------------------------------------------------------------------------------: | :------------------------------------------------------------------: |
| [s3s](./crates/s3s/)       |    [![Crates.io](https://img.shields.io/crates/v/s3s.svg)](https://crates.io/crates/s3s)    |    [![Docs](https://docs.rs/s3s/badge.svg)](https://docs.rs/s3s/)    |
| [s3s-aws](./crates/s3s-aws/)       |    [![Crates.io](https://img.shields.io/crates/v/s3s-aws.svg)](https://crates.io/crates/s3s-aws)    |    [![Docs](https://docs.rs/s3s-aws/badge.svg)](https://docs.rs/s3s-aws/)    |
| [s3s-fs](./crates/s3s-fs/) | [![Crates.io](https://img.shields.io/crates/v/s3s-fs.svg)](https://crates.io/crates/s3s-fs) | [![Docs](https://docs.rs/s3s-fs/badge.svg)](https://docs.rs/s3s-fs/) |

📚 **[Development documentation](https://s3s-project.github.io/s3s/)** for the `main` branch is available on GitHub Pages.

This experimental project intends to offer an ergonomic adapter for building S3-compatible services.

`s3s` implements Amazon S3 REST API in the form of a generic [hyper](https://github.com/hyperium/hyper) service. S3-compatible services can focus on the S3 API itself and don't have to care about the HTTP layer.

`s3s-aws` provides useful types and integration with [`aws-sdk-s3`](https://crates.io/crates/aws-sdk-s3).

`s3s-fs` implements the S3 API based on file system, as a sample implementation. It is designed for integration testing, which can be used to [mock an S3 client](https://github.com/Nugine/s3s/blob/main/crates/s3s-fs/tests/it_aws.rs). It also provides a binary for debugging. [Play it!](./CONTRIBUTING.md#play-the-test-server)

## How it works

![architecture diagram](docs/arch/arch.svg)

The diagram above shows how `s3s` works. 

`s3s` converts HTTP requests to operation inputs before calling the user-defined service. 

`s3s` converts operation outputs or errors to HTTP responses after calling the user-defined service.

The data types, serialization and deserialization are generated from the smithy model in [aws-sdk-rust](https://github.com/awslabs/aws-sdk-rust) repository. We apply manual hacks to fix some problems in [smithy server codegen](https://smithy-lang.github.io/smithy-rs/design/server/overview.html) and make `s3s` ready to use now.

## Security

`S3Service` and other adapters in this project have no security protection. If they are exposed to the Internet directly, they may be **attacked**.

It is up to the user to implement security enhancements such as **HTTP body length limit**, rate limit and back pressure.

## Docker

Docker images are available at [GitHub Container Registry (GHCR)](https://github.com/s3s-project/s3s/pkgs/container/s3s).

See [Docker documentation](./docs/docker.md) for usage details.

## Contributing

+ [Development Guide](./CONTRIBUTING.md)

## Sponsor

We have a reward funds pool for contributors: <https://github.com/Nugine/s3s/issues/174>

If my open-source work has been helpful to you, please [sponsor me](https://github.com/Nugine#sponsor).

Every little bit helps. Thank you!
