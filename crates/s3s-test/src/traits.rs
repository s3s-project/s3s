// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 Nugine

use crate::error::Result;

use std::future::Future;
use std::sync::Arc;

pub trait TestSuite: Sized + Send + Sync + 'static {
    fn setup() -> impl Future<Output = Result<Self>> + Send + 'static;

    fn teardown(self) -> impl Future<Output = Result> + Send + 'static {
        async { Ok(()) }
    }
}

pub trait TestFixture<S: TestSuite>: Sized + Send + Sync + 'static {
    fn setup(suite: Arc<S>) -> impl Future<Output = Result<Self>> + Send + 'static;

    fn teardown(self) -> impl Future<Output = Result> + Send + 'static {
        async { Ok(()) }
    }
}

pub trait TestCase<X, S>: Sized + Send + Sync + 'static
where
    Self: Sized + Send + Sync + 'static,
    X: TestFixture<S>,
    S: TestSuite,
{
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
