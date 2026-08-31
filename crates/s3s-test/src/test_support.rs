// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

#![cfg(test)]
#![allow(clippy::missing_errors_doc, clippy::missing_panics_doc)]

use crate::error::Failed;
use crate::error::Result;
use crate::traits::TestFixture;
use crate::traits::TestSuite;

use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

/// A mock suite that succeeds and tracks whether setup and teardown ran.
pub struct MockSuite {
    pub teardown_called: AtomicBool,
}

impl TestSuite for MockSuite {
    fn setup() -> impl Future<Output = Result<Self>> + Send + 'static {
        std::future::ready(Ok(Self {
            teardown_called: AtomicBool::new(false),
        }))
    }

    fn teardown(self) -> impl Future<Output = Result> + Send + 'static {
        std::future::ready({
            self.teardown_called.store(true, Ordering::SeqCst);
            Ok(())
        })
    }
}

/// A mock suite whose setup fails.
pub struct FailSetupSuite;

impl TestSuite for FailSetupSuite {
    fn setup() -> impl Future<Output = Result<Self>> + Send + 'static {
        std::future::ready(Err(Failed::from_string("setup failed")))
    }
}

/// A mock suite whose teardown fails.
pub struct FailTeardownSuite;

impl TestSuite for FailTeardownSuite {
    fn setup() -> impl Future<Output = Result<Self>> + Send + 'static {
        std::future::ready(Ok(Self))
    }

    fn teardown(self) -> impl Future<Output = Result> + Send + 'static {
        std::future::ready(Err(Failed::from_string("teardown failed")))
    }
}

/// A mock suite that uses the default teardown.
pub struct PlainSuite;

impl TestSuite for PlainSuite {
    fn setup() -> impl Future<Output = Result<Self>> + Send + 'static {
        std::future::ready(Ok(Self))
    }
}

/// A mock fixture with no extra state.
pub struct MockFixture;

impl TestFixture<MockSuite> for MockFixture {
    fn setup(_: Arc<MockSuite>) -> impl Future<Output = Result<Self>> + Send + 'static {
        std::future::ready(Ok(Self))
    }
}

impl TestFixture<FailSetupSuite> for MockFixture {
    fn setup(_: Arc<FailSetupSuite>) -> impl Future<Output = Result<Self>> + Send + 'static {
        std::future::ready(Ok(Self))
    }
}

impl TestFixture<FailTeardownSuite> for MockFixture {
    fn setup(_: Arc<FailTeardownSuite>) -> impl Future<Output = Result<Self>> + Send + 'static {
        std::future::ready(Ok(Self))
    }
}

impl TestFixture<PlainSuite> for MockFixture {
    fn setup(_: Arc<PlainSuite>) -> impl Future<Output = Result<Self>> + Send + 'static {
        std::future::ready(Ok(Self))
    }
}

/// Registers a single case into a fresh context with the [`MockSuite`].
pub fn register_case(
    name: &str,
    case: impl crate::traits::TestCase<MockFixture, MockSuite> + 'static,
) -> crate::tcx::TestContext {
    register_case_with_tags(name, case, &[])
}

/// Registers a single case with tags into a fresh context with the
/// [`MockSuite`].
pub fn register_case_with_tags(
    name: &str,
    case: impl crate::traits::TestCase<MockFixture, MockSuite> + 'static,
    tags: &[crate::tcx::CaseTag],
) -> crate::tcx::TestContext {
    let mut tcx = crate::tcx::TestContext::new();
    let mut suite = tcx.suite::<MockSuite>("suite");
    let mut fixture = suite.fixture::<MockFixture>("fixture");
    let mut builder = fixture.case(name, case);
    for tag in tags {
        builder.tag(tag.clone());
    }
    tcx
}

/// Runs the context sequentially and returns the report of the single case.
pub async fn run_single_case(tcx: &mut crate::tcx::TestContext) -> crate::report::CaseReport {
    let report = crate::runner::run(tcx, false).await;
    let mut suites = report.suites;
    let suite = suites.pop().expect("one suite");
    let mut fixtures = suite.fixtures;
    let mut fixture = fixtures.pop().expect("one fixture");
    fixture.cases.pop().expect("one case")
}
