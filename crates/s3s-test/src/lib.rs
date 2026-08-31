// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

//! A reusable test harness for S3-compatible services.
//!
//! The harness organizes tests into three levels:
//!
//! - **Suite**: one server configuration. The suite setup builds the server
//!   (or connects to an external endpoint) and produces the shared data
//!   consumed by all fixtures and cases.
//! - **Fixture**: a group of cases that share the same client and
//!   precondition. Fixtures may hold extra shared state beyond the suite data.
//! - **Case**: a single test function. Cases receive `Arc<Fixture>` and may
//!   run concurrently.
//!
//! # Server shapes
//!
//! The harness itself is transport-agnostic: it never constructs servers.
//! The suite setup is responsible for building the client. Common shapes:
//!
//! - **In-process (direct)**: build an `S3Service`, wrap it with
//!   `s3s_aws::Client`, and hand the resulting AWS SDK client to the suite.
//!   No network is involved.
//! - **In-process (TCP)**: start a `hyper` server on a random local port and
//!   point a client at it.
//! - **Remote endpoint**: read `AWS_ENDPOINT_URL` and credentials from the
//!   environment (e.g. via `aws_config::from_env`) and run the cases against
//!   any S3-compatible service.
//!
//! The same cases can run against any of these shapes.
//!
//! # Concurrency
//!
//! Concurrency is a global flag (`--concurrent` on the CLI). With
//! concurrency enabled, cases within one fixture run in parallel; fixtures
//! and suites still run sequentially. Concurrency safety is the
//! responsibility of each case: cases that may run concurrently must isolate
//! their resources (e.g. by using unique bucket or key names). Default is
//! sequential.
//!
//! # Quick start
//!
//! ```ignore
//! use s3s_test::tcx::TestContext;
//! use s3s_test::{Result, TestFixture, TestSuite};
//!
//! struct Server;
//!
//! impl TestSuite for Server {
//!     fn setup() -> impl Future<Output = Result<Self>> + Send + 'static {
//!         std::future::ready(Ok(Self))
//!     }
//! }
//!
//! struct Client;
//!
//! impl TestFixture<Server> for Client {
//!     fn setup(_: Arc<Server>) -> impl Future<Output = Result<Self>> + Send + 'static {
//!         std::future::ready(Ok(Self))
//!     }
//! }
//!
//! impl Client {
//!     async fn list_buckets(self: Arc<Self>) -> Result {
//!         Ok(())
//!     }
//! }
//!
//! fn register(tcx: &mut TestContext) {
//!     let mut suite = tcx.suite::<Server>("Server");
//!     let mut fixture = suite.fixture::<Client>("Client");
//!     fixture.case("list_buckets", Client::list_buckets);
//! }
//!
//! s3s_test::main!(register);
//! ```
//!
//! The `main!` macro generates the entry point with a fixed CLI
//! (`--filter`, `--list`, `--json`, `--run-ignored`, `--concurrent`). For a
//! custom entry point, use the library mode directly: `TestContext::new()`,
//! register, then drive the run with `cli::main` and `cli::Options`.

#![allow(
    clippy::missing_errors_doc, // TODO
    clippy::missing_panics_doc, // TODO
)]

mod error;
mod runner;
#[cfg(test)]
mod test_support;
mod traits;

pub mod build;
pub mod cli;
pub mod report;
pub mod tcx;

pub use self::error::{Failed, Result};
pub use self::tcx::TestContext;
pub use self::traits::*;
