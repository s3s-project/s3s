// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

use crate::error::Result;

use std::future::Future;
use std::sync::Arc;

/// A test suite: one server configuration.
///
/// The suite setup builds the server (or connects to an external endpoint)
/// and returns the shared data consumed by all fixtures and cases.
/// Teardown runs only when setup succeeds.
pub trait TestSuite: Sized + Send + Sync + 'static {
    /// Sets up the suite and returns the shared suite data.
    fn setup() -> impl Future<Output = Result<Self>> + Send + 'static;

    /// Tears down the suite.
    fn teardown(self) -> impl Future<Output = Result> + Send + 'static {
        async { Ok(()) }
    }
}

/// A fixture: a group of cases that share the same client and precondition.
///
/// The fixture setup receives the shared suite data and may retain extra
/// state beyond the client. Teardown runs only when setup succeeds.
pub trait TestFixture<S: TestSuite>: Sized + Send + Sync + 'static {
    /// Sets up the fixture from the shared suite data.
    fn setup(suite: Arc<S>) -> impl Future<Output = Result<Self>> + Send + 'static;

    /// Tears down the fixture.
    fn teardown(self) -> impl Future<Output = Result> + Send + 'static {
        async { Ok(()) }
    }
}

/// A single test case.
///
/// The blanket implementation accepts any `async fn(&self, Arc<X>) -> Result`
/// method, so cases are usually written as regular async methods on the
/// fixture type.
pub trait TestCase<X, S>: Sized + Send + Sync + 'static
where
    Self: Sized + Send + Sync + 'static,
    X: TestFixture<S>,
    S: TestSuite,
{
    /// Runs the case against the fixture data.
    fn run(&self, fixture: Arc<X>) -> impl Future<Output = Result> + Send + 'static;
}

trait AsyncFn<'a, A> {
    type Output;
    type Future: Future<Output = Self::Output> + Send + 'a;

    fn call(&self, args: A) -> Self::Future;
}

impl<'a, F, U, O, A> AsyncFn<'a, (A,)> for F
where
    F: Fn(A) -> U,
    U: Future<Output = O> + Send + 'a,
{
    type Output = O;

    type Future = U;

    fn call(&self, args: (A,)) -> Self::Future {
        (self)(args.0)
    }
}

impl<C, X, S> TestCase<X, S> for C
where
    C: for<'a> AsyncFn<'a, (Arc<X>,), Output = Result>,
    C: Send + Sync + 'static,
    X: TestFixture<S>,
    S: TestSuite,
{
    fn run(&self, fixture: Arc<X>) -> impl Future<Output = Result> + Send + 'static {
        AsyncFn::call(self, (fixture,))
    }
}
