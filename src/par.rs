//! Parallel combinators: products, races and bounded concurrency over collections.
//!
//! "Parallel" here means *concurrent within the current task*: branches are polled by the same
//! future (like `futures::join!`), so nothing is spawned, nothing needs to be `'static`, and
//! dropping the outer future cancels every branch.

use core::convert::Infallible;
use core::future::{Future, poll_fn};
use core::pin::pin;
use core::task::{Context, Poll};

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

macro_rules! zip_arity {
    ($(#[$doc:meta])* $name:ident $ctor:ident: $A:ident $a:ident $(, $T:ident $t:ident)+) => {
        /// A flat parallel product of ops: see the function of the same name in lowercase.
        #[derive(Clone)]
        #[must_use = "ops do nothing until `.run().await`"]
        pub struct $name<$A, $($T),+> {
            $a: $A,
            $($t: $T,)+
        }

        $(#[$doc])*
        #[allow(clippy::too_many_arguments)]
        pub fn $ctor<$A, $($T),+>($a: $A, $($t: $T),+) -> $name<$A, $($T),+>
        where
            $A: Op,
            $($T: Op<Err = $A::Err>,)+
        {
            $name { $a, $($t),+ }
        }

        impl<$A, $($T),+> Op for $name<$A, $($T),+>
        where
            $A: Op,
            $($T: Op<Err = $A::Err>,)+
        {
            type Ok = ($A::Ok, $($T::Ok),+);
            type Err = $A::Err;

            async fn run(&mut self) -> Result<Self::Ok, $A::Err> {
                let mut $a = pin!(future::try_maybe_done(self.$a.run()));
                $(let mut $t = pin!(future::try_maybe_done(self.$t.run()));)+
                poll_fn(move |cx: &mut Context<'_>| -> Poll<Result<Self::Ok, $A::Err>> {
                    let mut done = $a.as_mut().poll(cx)?.is_ready();
                    $(done &= $t.as_mut().poll(cx)?.is_ready();)+
                    if !done {
                        return Poll::Pending;
                    }
                    Poll::Ready(Ok((
                        $a.as_mut().take_output().expect("polled to completion"),
                        $($t.as_mut().take_output().expect("polled to completion"),)+
                    )))
                })
                .await
            }
        }
    };
}

zip_arity! {
    /// Parallel product of 3 ops with a flat result, fail-fast like [`Op::zip`].
    /// Law: `zip3(a, b, c) ≡ a.zip(b).zip(c).map(|((a, b), c)| (a, b, c))`.
    Zip3 zip3: A a, B b, C c
}
zip_arity! {
    /// Parallel product of 4 ops with a flat result, fail-fast like [`Op::zip`].
    /// Law: `zip4(a, b, c, d) ≡ zip3(a, b, c).zip(d)`, flattened.
    Zip4 zip4: A a, B b, C c, D d
}
zip_arity! {
    /// Parallel product of 5 ops with a flat result, fail-fast like [`Op::zip`].
    /// Law: `zip5(.., e) ≡ zip4(..).zip(e)`, flattened.
    Zip5 zip5: A a, B b, C c, D d, E e
}
zip_arity! {
    /// Parallel product of 6 ops with a flat result, fail-fast like [`Op::zip`].
    /// Law: `zip6(.., f) ≡ zip5(..).zip(f)`, flattened.
    Zip6 zip6: A a, B b, C c, D d, E e, F f
}
zip_arity! {
    /// Parallel product of 7 ops with a flat result, fail-fast like [`Op::zip`].
    /// Law: `zip7(.., g) ≡ zip6(..).zip(g)`, flattened.
    Zip7 zip7: A a, B b, C c, D d, E e, F f, G g
}
zip_arity! {
    /// Parallel product of 8 ops with a flat result, fail-fast like [`Op::zip`].
    /// Law: `zip8(.., h) ≡ zip7(..).zip(h)`, flattened.
    Zip8 zip8: A a, B b, C c, D d, E e, F f, G g, H h
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
