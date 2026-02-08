# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

[Unreleased]: https://github.com/s3s-project/s3s/compare/v0.12.0...HEAD

### Docker

+ Migrated Docker releases from Docker Hub to GitHub Container Registry (ghcr.io)

## [v0.12.0] - 2025-12-22

[v0.12.0]: https://github.com/s3s-project/s3s/compare/v0.11.0...v0.12.0

Tracking in [#270](https://github.com/s3s-project/s3s/issues/270).

MSRV of this minor version: 1.86.0

### s3s

**BREAKING**: Architecture refactoring:
+ Make S3Service shared ([#9ccf2f9](https://github.com/s3s-project/s3s/commit/9ccf2f908ffbdf3f5636f838b8897fc621ced337))
+ Define http types in protocol module ([#7f8be8a](https://github.com/s3s-project/s3s/commit/7f8be8a1714963a9317cb97a7d3e4d0436bc5c2c))
+ Better route definition ([#c4247b3](https://github.com/s3s-project/s3s/commit/c4247b34595d0441254a530b817a4fa61197d098))
+ Move protocol types ([#6c6b066](https://github.com/s3s-project/s3s/commit/6c6b066edcd5ae68ecd1230b6cac22cc19c2674e))

**BREAKING**: Security improvements:
+ Add memory allocation limits to stream parsers ([#413](https://github.com/s3s-project/s3s/pull/413))
+ Fix unbounded memory allocation in http::body ([#407](https://github.com/s3s-project/s3s/pull/407))
+ Fix unbounded memory allocation in POST object ([#370](https://github.com/s3s-project/s3s/pull/370), [#390](https://github.com/s3s-project/s3s/pull/390))
+ Remove non-constant time PartialEq from SecretKey, use ConstantTimeEq ([#319](https://github.com/s3s-project/s3s/pull/319))


**BREAKING**: Content-Type validation changes ([#389](https://github.com/s3s-project/s3s/pull/389))
+ Allow custom content-type values
+ Allow empty content-type headers ([#365](https://github.com/s3s-project/s3s/pull/365))

**BREAKING**: Typed ETag support ([#349](https://github.com/s3s-project/s3s/pull/349), [#403](https://github.com/s3s-project/s3s/pull/403), [#410](https://github.com/s3s-project/s3s/pull/410))
+ Introduced strongly-typed `ETag` type replacing `String` for conditional request headers
+ Added `ETagCondition` type to support wildcard `*` in If-Match/If-None-Match headers
+ Implemented RFC 9110-compliant ETag comparison methods (strong and weak validation)
+ Fixed If-None-Match wildcard support ([#433](https://github.com/s3s-project/s3s/pull/433))

Configuration types now implement `Default`, `Serialize`, and `Deserialize` ([#429](https://github.com/s3s-project/s3s/pull/429), [#435](https://github.com/s3s-project/s3s/pull/435))
+ All Configuration types now derive or implement `Default` trait
+ Added serde support for all Configuration types and their dependencies

Signature verification enhancements:
+ AWS Signature V2 POST signature support ([#358](https://github.com/s3s-project/s3s/pull/358))
+ STS signature validation support ([#418](https://github.com/s3s-project/s3s/pull/418))
+ Normalize header values per AWS SigV4 specification ([#393](https://github.com/s3s-project/s3s/pull/393))
+ Fix status code for invalid x-amz-content-sha256 header ([#430](https://github.com/s3s-project/s3s/pull/430))
+ Handle multi-value headers in canonical requests ([#408](https://github.com/s3s-project/s3s/pull/408))
+ Fix single chunk upload signature validation ([#369](https://github.com/s3s-project/s3s/pull/369))
+ Add tests for PUT presigned URL signature verification ([#402](https://github.com/s3s-project/s3s/pull/402))

Protocol improvements:
+ Enhanced checksum support and content validation ([#371](https://github.com/s3s-project/s3s/pull/371))
+ Support streaming trailers ([#59d6fd9](https://github.com/s3s-project/s3s/commit/59d6fd973cf9237537954b3723e889f28e4fe833))
+ Improve error logs for HTTP parsing failures ([#366](https://github.com/s3s-project/s3s/pull/366))
+ Fix multipart optional content_type ([#355](https://github.com/s3s-project/s3s/pull/355))
+ Fix complete_multipart_upload keep_alive ([#348](https://github.com/s3s-project/s3s/pull/348))
+ Ignore empty headers ([#384](https://github.com/s3s-project/s3s/pull/384))
+ Improve TrailingHeaders ([#d4a9db2](https://github.com/s3s-project/s3s/commit/d4a9db2a7d519dc3b8109b14d3145ebdda640f06))
+ Host header fallback on HTTP/2 ([#44c1002](https://github.com/s3s-project/s3s/commit/44c100274c1494f30afe631d6112467bb701e23c), [#1746e26](https://github.com/s3s-project/s3s/commit/1746e2635bb65cc3045442fba1cf2fee5b6b3659))
+ Display invalid content-type content ([#386](https://github.com/s3s-project/s3s/pull/386))
+ Add xml_attr field and related functionality for XML serialization ([#299](https://github.com/s3s-project/s3s/pull/299))
+ Optimize StrEnum XML deserialization to reduce allocations ([#313](https://github.com/s3s-project/s3s/pull/313))
+ Differentiate Get and List operations by id parameter ([#392](https://github.com/s3s-project/s3s/pull/392), [#398](https://github.com/s3s-project/s3s/pull/398))
+ Return MalformedXML for empty XML body in operations requiring it ([#377](https://github.com/s3s-project/s3s/pull/377))
+ Enhance extract_host to return host from URI if available ([#431](https://github.com/s3s-project/s3s/pull/431))
+ Custom validation option via S3ServiceBuilder ([#342](https://github.com/s3s-project/s3s/pull/342))

Cryptography:
+ Use latest RustCrypto releases ([#354db52](https://github.com/s3s-project/s3s/commit/354db522718fbe59548887f6db1bf55c9cd2c2b5))
+ Extract checksum algorithms ([#09c9374](https://github.com/s3s-project/s3s/commit/09c9374fd6fa77e69f17336ed92845954de8a64e))
+ Use crc-fast instead of crc32fast & crc64fast-nvme ([#380](https://github.com/s3s-project/s3s/pull/380))

RFC 2047 support:
+ Add RFC 2047 non-ASCII header encoding/decoding support ([#405](https://github.com/s3s-project/s3s/pull/405))
+ Allow RFC2047-encoded metadata values ([#434](https://github.com/s3s-project/s3s/pull/434))


Examples:
+ Add HTTPS example with TLS support ([#409](https://github.com/s3s-project/s3s/pull/409))

### s3s-fs

+ Fix ListObjectsV2 response fields causing OpenDAL hang ([#351](https://github.com/s3s-project/s3s/pull/351))
+ Preserve standard object attributes ([#420](https://github.com/s3s-project/s3s/pull/420))
+ Make metadata file writes atomic ([#360](https://github.com/s3s-project/s3s/pull/360))
+ Fix checksum for range requests ([#285](https://github.com/s3s-project/s3s/pull/285))
+ Enforce multipart upload limits ([#281](https://github.com/s3s-project/s3s/pull/281))
+ Fix trailer checksum ([#ef0bd70](https://github.com/s3s-project/s3s/commit/ef0bd703878ab9ba868a16a2957ea96a5421e4f3))

### s3s-e2e

+ Add comprehensive test coverage with enabled advanced features ([#321](https://github.com/s3s-project/s3s/pull/321))
+ Add multipart upload checksum support to e2e tests ([#374](https://github.com/s3s-project/s3s/pull/374))
+ Add test_put_object_with_checksum_algorithm ([#6bf36f9](https://github.com/s3s-project/s3s/commit/6bf36f9275bfefbe797caf6d73215b57310ba7a0))

### codegen

+ Add MinIO feature support ([#5c460a8](https://github.com/s3s-project/s3s/commit/5c460a8c842abddd761fa30b0862b80282c5f4c6))
+ Fix optional object attributes ([#346](https://github.com/s3s-project/s3s/pull/346))
+ Parse checksum_algorithm_header ([#c8d42ed](https://github.com/s3s-project/s3s/commit/c8d42ed4c6f9fc08ff1b361a9e1caf3e9706f270))
+ Derive serde for Tagging ([#6faf16e](https://github.com/s3s-project/s3s/commit/6faf16ecc35e6e625bee3220a9edca6f4a6f641b))
+ Patch PartNumberMarker ([#f8f28ea](https://github.com/s3s-project/s3s/commit/f8f28ea9c39dc09c490b021b74187e9e7ce88a25))
+ Ignore EntityTooLarge 405 ([#0ed460e](https://github.com/s3s-project/s3s/commit/0ed460e55ab14cd0b70a67cf39bef0a7f71b6668))
+ Timestamp derive more ([#cdf9b15](https://github.com/s3s-project/s3s/commit/cdf9b1587d536cd123df84d3a24dcba08d0f02f8))
+ Fix CI failure by updating missing generated code from AWS data ([#323](https://github.com/s3s-project/s3s/pull/323))

### Testing

+ Add comprehensive error case tests for aws_chunked_stream ([#354](https://github.com/s3s-project/s3s/issues/354), [#423](https://github.com/s3s-project/s3s/pull/423))
+ Add OpenDAL compatibility test for S3 API integration ([#317](https://github.com/s3s-project/s3s/pull/317))
+ Add crate `s3s-wasm` for WebAssembly support ([#3c3d3cc](https://github.com/s3s-project/s3s/commit/3c3d3cc8fe2c2db3b721d6bf65dadaba1ce776fe), [#452f2e3](https://github.com/s3s-project/s3s/commit/452f2e3b847327a57e0af7357dd4875343c37899))

### Documentation

+ Deploy cargo docs to GitHub Pages for main branch ([#359](https://github.com/s3s-project/s3s/pull/359))

### Docker

+ Standalone, static-compiled Docker image for s3s-fs/e2e/proxy (AMD64 and ARM64) ([#334](https://github.com/s3s-project/s3s/pull/334))
+ Adjust Docker workflow for tag-based releases and weekly edge builds ([#422](https://github.com/s3s-project/s3s/pull/422))

### Dependencies

+ Upgrade crypto dependencies ([#379](https://github.com/s3s-project/s3s/pull/379), [#335](https://github.com/s3s-project/s3s/pull/335))
+ Remove unnecessary yanked dependencies ([#344](https://github.com/s3s-project/s3s/pull/344))

## [v0.11.0] - 2025-03-28

[v0.11.0]: https://github.com/Nugine/s3s/compare/v0.10.1...v0.11.0

Tracking in [#267](https://github.com/Nugine/s3s/issues/267).

MSRV of this minor version: 1.85.0

### s3s

**BREAKING**: Following the latest model definitions in [aws-sdk-rust](https://github.com/awslabs/aws-sdk-rust), `s3s::dto` is updated.
+ You may come across some type changes reported by rustc.
+ The migration is not hard but requires some time.

**BREAKING**: More request parameters are accepted via upgrading model definitions.
+ S3 preconditions ([#241](https://github.com/Nugine/s3s/issues/241))
+ PutObject write_offset_bytes ([#249](https://github.com/Nugine/s3s/issues/249))

**BREAKING**: Policy-based access control is supported in `s3s::access` ([#161](https://github.com/Nugine/s3s/issues/161))
+ Add `S3Access` trait for access control.
+ Add `S3ServiceBuilder::set_access`.
+ Move `S3Auth::check_access` to `S3Access::check`.

**BREAKING**: Multi-domain is supported in `s3s::host`. ([#175](https://github.com/Nugine/s3s/issues/175))
+ Add `S3Host` trait for parsing host header.
+ Change `S3ServiceBuilder::set_base_domain` to `S3ServiceBuilder::set_host`.
+ Add `SingleDomain` parser.
+ Add `MultiDomain` parser.

Custom route is supported in `s3s::route` ([#195](https://github.com/Nugine/s3s/issues/195))
+ Add `S3Route` trait for custom route protected by signature verification.
+ Add `S3ServiceBuilder::set_route`.
+ Signature v4 supports AWS STS requests ([#208](https://github.com/Nugine/s3s/pull/208))
+ Add example using [axum](https://github.com/tokio-rs/axum) web framework ([#263](https://github.com/Nugine/s3s/pull/263))

Unstable `minio` branch:
+ Add `minio` branch for MinIO compatibility.
+ This branch is automatically force-rebased to the latest `main` branch.

Other notable changes
+ feat(s3s): export xml module ([#189](https://github.com/Nugine/s3s/pull/189))
+ fix(s3s/ops): allow presigned url requests with up to 15 minutes clock skew ([#216](https://github.com/Nugine/s3s/pull/216))
+ handle fmt message with implicit arguments in s3_error macro ([#228](https://github.com/Nugine/s3s/pull/228))
+ feat(s3s/dto): ignore empty strings ([#244](https://github.com/Nugine/s3s/pull/244))
+ feat(model): extra error codes ([#255](https://github.com/Nugine/s3s/pull/255))
+ feat(s3s/checksum): add crc64nvme ([#256](https://github.com/Nugine/s3s/pull/256))
+ feat(s3s/xml): support xmlns ([#265](https://github.com/Nugine/s3s/pull/265))

### s3s-model

+ Add crate `s3s-model` for S3 model definitions.

### s3s-policy

+ Add crate `s3s-policy` for S3 policy language.
+ Add grammar model types for serialization and deserialization in `s3s_policy::model`.
+ Add `PatternSet` for matching multiple patterns in `s3s_policy::pattern`.

### s3s-test

+ Add crate `s3s-test` for custom test framework.

### s3s-e2e

+ Add crate `s3s-e2e` for S3 compatibility tests.
