//! Parallel combinators: products, races and bounded concurrency over collections.
//!
//! "Parallel" here means *concurrent within the current task*: branches are polled by the same
//! future (like `futures::join!`), so nothing is spawned, nothing needs to be `'static`, and
//! dropping the outer future cancels every branch.

use core::convert::Infallible;
use core::pin::pin;

use futures_util::future::{self, Either};
use futures_util::stream::{FuturesUnordered, StreamExt};

use crate::op::Op;

/// See [`Op::zip`].
#[derive(Clone)]
#[must_use = "ops do nothing until `.run().await`"]
pub struct Zip<A, B> {
    a: A,
    b: B,
}

impl<A, B> Zip<A, B> {
    pub(crate) fn new(a: A, b: B) -> Self {
        Self { a, b }
    }
}

impl<A, B> Op for Zip<A, B>
where
    A: Op,
    B: Op<Err = A::Err>,
{
    type Ok = (A::Ok, B::Ok);
    type Err = A::Err;

    async fn run(&mut self) -> Result<(A::Ok, B::Ok), A::Err> {
        future::try_join(self.a.run(), self.b.run()).await
    }
}

/// See [`Op::join`].
#[derive(Clone)]
#[must_use = "ops do nothing until `.run().await`"]
pub struct Join<A, B> {
    a: A,
    b: B,
}

impl<A, B> Join<A, B> {
    pub(crate) fn new(a: A, b: B) -> Self {
        Self { a, b }
    }
}

impl<A, B> Op for Join<A, B>
where
    A: Op,
    B: Op,
{
    type Ok = (Result<A::Ok, A::Err>, Result<B::Ok, B::Err>);
    type Err = Infallible;

    async fn run(&mut self) -> Result<Self::Ok, Infallible> {
        Ok(future::join(self.a.run(), self.b.run()).await)
    }
}

/// See [`Op::race`].
#[derive(Clone)]
#[must_use = "ops do nothing until `.run().await`"]
pub struct Race<A, B> {
    a: A,
    b: B,
}

impl<A, B> Race<A, B> {
    pub(crate) fn new(a: A, b: B) -> Self {
        Self { a, b }
    }
}

impl<A, B> Op for Race<A, B>
where
    A: Op,
    B: Op<Ok = A::Ok, Err = A::Err>,
{
    type Ok = A::Ok;
    type Err = A::Err;

    async fn run(&mut self) -> Result<A::Ok, A::Err> {
        let a = pin!(self.a.run());
        let b = pin!(self.b.run());
        // `select` polls the left future first, so exact ties go to `a`.
        match future::select(a, b).await {
            Either::Left((r, _loser)) => r,
            Either::Right((r, _loser)) => r,
        }
    }
}

// ----- N-ary ----------------------------------------------------------------------------------

async fn indexed<O: Op>(i: usize, op: &mut O) -> (usize, Result<O::Ok, O::Err>) {
    (i, op.run().await)
}

/// Run every op of a collection concurrently (optionally at most `limit` at a time) and
/// collect the values in input order. Fail-fast: the first error cancels the rest.
///
/// Built with [`all`].
#[derive(Clone)]
#[must_use = "ops do nothing until `.run().await`"]
pub struct All<O> {
    ops: Vec<O>,
    limit: usize,
}

/// Run all `ops` concurrently, fail-fast. See [`All`] and [`All::limit`].
pub fn all<I>(ops: I) -> All<I::Item>
where
    I: IntoIterator,
    I::Item: Op,
{
    All {
        ops: ops.into_iter().collect(),
        limit: usize::MAX,
    }
}

impl<O> All<O> {
    /// Run at most `limit` ops at the same time (bounded concurrency). Panics if `limit == 0`.
    pub fn limit(mut self, limit: usize) -> Self {
        assert!(limit > 0, "concurrency limit must be positive");
        self.limit = limit;
        self
    }

    /// Report every outcome instead of failing fast. See [`AllSettled`].
    pub fn settled(self) -> AllSettled<O> {
        AllSettled { inner: self }
    }
}

impl<O: Op> Op for All<O> {
    type Ok = Vec<O::Ok>;
    type Err = O::Err;

    async fn run(&mut self) -> Result<Vec<O::Ok>, O::Err> {
        let n = self.ops.len();
        let mut out: Vec<Option<O::Ok>> = core::iter::repeat_with(|| None).take(n).collect();
        let mut queue = self.ops.iter_mut().enumerate();
        let mut running = FuturesUnordered::new();
        for (i, op) in queue.by_ref().take(self.limit) {
            running.push(indexed(i, op));
        }
        while let Some((i, r)) = running.next().await {
            out[i] = Some(r?);
            if let Some((j, op)) = queue.next() {
                running.push(indexed(j, op));
            }
        }
        Ok(out
            .into_iter()
            .map(|v| v.expect("every slot is filled"))
            .collect())
    }
}

/// Like [`All`] but waits for every op and reports each outcome, in input order. Never fails.
#[derive(Clone)]
#[must_use = "ops do nothing until `.run().await`"]
pub struct AllSettled<O> {
    inner: All<O>,
}

impl<O: Op> Op for AllSettled<O> {
    type Ok = Vec<Result<O::Ok, O::Err>>;
    type Err = Infallible;

    async fn run(&mut self) -> Result<Self::Ok, Infallible> {
        let n = self.inner.ops.len();
        let mut out: Vec<Option<Result<O::Ok, O::Err>>> =
            core::iter::repeat_with(|| None).take(n).collect();
        let mut queue = self.inner.ops.iter_mut().enumerate();
        let mut running = FuturesUnordered::new();
        for (i, op) in queue.by_ref().take(self.inner.limit) {
            running.push(indexed(i, op));
        }
        while let Some((i, r)) = running.next().await {
            out[i] = Some(r);
            if let Some((j, op)) = queue.next() {
                running.push(indexed(j, op));
            }
        }
        Ok(out
            .into_iter()
            .map(|v| v.expect("every slot is filled"))
            .collect())
    }
}

/// Race a collection: the first op to finish wins and the rest are cancelled. An empty race
/// never completes (it is [`never()`](fn@crate::never)(crate::never), the identity of racing).
#[derive(Clone)]
#[must_use = "ops do nothing until `.run().await`"]
pub struct RaceAll<O> {
    ops: Vec<O>,
}

/// Race all `ops`. See [`RaceAll`].
pub fn race_all<I>(ops: I) -> RaceAll<I::Item>
where
    I: IntoIterator,
    I::Item: Op,
{
    RaceAll {
        ops: ops.into_iter().collect(),
    }
}

impl<O: Op> Op for RaceAll<O> {
    type Ok = O::Ok;
    type Err = O::Err;

    async fn run(&mut self) -> Result<O::Ok, O::Err> {
        let mut running: FuturesUnordered<_> = self
            .ops
            .iter_mut()
            .enumerate()
            .map(|(i, op)| indexed(i, op))
            .collect();
        match running.next().await {
            Some((_, r)) => r,
            None => core::future::pending().await,
        }
    }
}
