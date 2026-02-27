use crate::error::Failed;
use crate::error::Result;
use crate::traits::TestCase;
use crate::traits::TestFixture;
use crate::traits::TestSuite;

use std::any::type_name;
use std::future::Future;
use std::marker::PhantomData;
use std::ops::Not;
use std::pin::Pin;
use std::sync::Arc;

use indexmap::IndexMap;
use regex::RegexSet;

pub(crate) type ArcAny = Arc<dyn std::any::Any + Send + Sync + 'static>;
type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

type SuiteSetupFn = Box<dyn Fn() -> BoxFuture<'static, Result<ArcAny, Failed>>>;
type SuiteTeardownFn = Box<dyn Fn(ArcAny) -> BoxFuture<'static, Result>>;

type FixtureSetupFn = Box<dyn Fn(ArcAny) -> BoxFuture<'static, Result<ArcAny, Failed>>>;
type FixtureTeardownFn = Box<dyn Fn(ArcAny) -> BoxFuture<'static, Result>>;

type CaseRunFn = Box<dyn Fn(ArcAny) -> BoxFuture<'static, Result>>;

pub struct TestContext {
    pub(crate) suites: IndexMap<String, SuiteInfo>,
}

pub(crate) struct SuiteInfo {
    pub(crate) name: String,
    // pub(crate) type_id: TypeId,
    pub(crate) setup: SuiteSetupFn,
    pub(crate) teardown: SuiteTeardownFn,
    pub(crate) fixtures: IndexMap<String, FixtureInfo>,
}

pub(crate) struct FixtureInfo {
    pub(crate) name: String,
    // pub(crate) type_id: TypeId,
    pub(crate) setup: FixtureSetupFn,
    pub(crate) teardown: FixtureTeardownFn,
    pub(crate) cases: IndexMap<String, CaseInfo>,
}

pub(crate) struct CaseInfo {
    pub(crate) name: String,
    pub(crate) run: CaseRunFn,
    pub(crate) tags: Vec<CaseTag>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CaseTag {
    Ignored,
    ShouldPanic,
}

fn wrap<T: Send + Sync + 'static>(x: T) -> ArcAny {
    Arc::new(x)
}

fn downcast<T: Send + Sync + 'static>(any: ArcAny) -> Arc<T> {
    Arc::downcast(any).unwrap()
}

fn unwrap<T: Send + Sync + 'static>(any: ArcAny) -> Result<T> {
    match Arc::try_unwrap(downcast::<T>(any)) {
        Ok(x) => Ok(x),
        Err(_) => Err(Failed::from_string(format!("Arc<{}> is leaked", type_name::<T>()))),
    }
}

impl TestContext {
    pub(crate) fn new() -> Self {
        Self { suites: IndexMap::new() }
    }

    pub fn suite<S: TestSuite>(&mut self, name: impl Into<String>) -> SuiteBuilder<'_, S> {
        let name = name.into();
        if !self.suites.contains_key(&name) {
            self.suites.insert(
                name.clone(),
                SuiteInfo {
                    name: name.clone(),
                    // type_id: TypeId::of::<S>(),
                    setup: Box::new(|| Box::pin(async { S::setup().await.map(wrap) })),
                    teardown: Box::new(|any| Box::pin(async move { S::teardown(unwrap(any)?).await })),
                    fixtures: IndexMap::new(),
                },
            );
        }
        SuiteBuilder {
            suite: &mut self.suites[&name],
            _marker: PhantomData,
        }
    }

    pub fn filter(&mut self, filter_set: &RegexSet) {
        self.suites.retain(|_, suite| {
            suite.fixtures.retain(|_, fixture| {
                fixture.cases.retain(|_, case| {
                    let id = format!("{}/{}/{}", suite.name, fixture.name, case.name);
                    filter_set.is_match(&id)
                });
                fixture.cases.is_empty().not()
            });
            suite.fixtures.is_empty().not()
        });
    }

    pub fn include_ignored(&mut self) {
        for suite in self.suites.values_mut() {
            for fixture in suite.fixtures.values_mut() {
                for case in fixture.cases.values_mut() {
                    case.tags.retain(|t| *t != CaseTag::Ignored);
                }
            }
        }
    }
}

pub struct SuiteBuilder<'a, S> {
    suite: &'a mut SuiteInfo,
    _marker: PhantomData<S>,
}

impl<S: TestSuite> SuiteBuilder<'_, S> {
    pub fn fixture<X: TestFixture<S>>(&mut self, name: impl Into<String>) -> FixtureBuilder<'_, X, S> {
        let name = name.into();
        if !self.suite.fixtures.contains_key(&name) {
            self.suite.fixtures.insert(
                name.clone(),
                FixtureInfo {
                    name: name.clone(),
                    // type_id: TypeId::of::<X>(),
                    setup: Box::new(|any| Box::pin(async move { X::setup(downcast(any)).await.map(wrap) })),
                    teardown: Box::new(|any| Box::pin(async move { X::teardown(unwrap(any)?).await })),
                    cases: IndexMap::new(),
                },
            );
        }
        FixtureBuilder {
            fixture: &mut self.suite.fixtures[&name],
            _marker: PhantomData,
        }
    }
}

pub struct FixtureBuilder<'a, X, S> {
    fixture: &'a mut FixtureInfo,
    _marker: PhantomData<(X, S)>,
}

impl<X, S> FixtureBuilder<'_, X, S>
where
    X: TestFixture<S>,
    S: TestSuite,
{
    pub fn case<C: TestCase<X, S>>(&mut self, name: impl Into<String>, case: C) -> CaseBuilder<'_, C, X, S> {
        let name = name.into();
        self.fixture.cases.insert(
            name.clone(),
            CaseInfo {
                name: name.clone(),
                run: Box::new(move |any| Box::pin(case.run(downcast(any)))),
                tags: Vec::new(),
            },
        );
        CaseBuilder {
            case: &mut self.fixture.cases[&name],
            _marker: PhantomData,
        }
    }
}

pub struct CaseBuilder<'a, C, X, S> {
    case: &'a mut CaseInfo,
    _marker: PhantomData<(C, X, S)>,
}

impl<C, X, S> CaseBuilder<'_, C, X, S> {
    pub fn tag(&mut self, tag: CaseTag) -> &mut Self {
        self.case.tags.push(tag);
        self
    }
}
