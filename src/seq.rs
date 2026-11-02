//! Sequential combinators: map, bind, guards and fallbacks.

use crate::op::Op;

/// See [`Op::map`].
#[derive(Clone)]
#[must_use = "ops do nothing until `.run().await`"]
pub struct Map<O, F> {
    op: O,
    f: F,
}

impl<O, F> Map<O, F> {
    pub(crate) fn new(op: O, f: F) -> Self {
        Self { op, f }
    }
}

impl<O, F, U> Op for Map<O, F>
where
    O: Op,
    F: FnMut(O::Ok) -> U,
{
    type Ok = U;
    type Err = O::Err;

    async fn run(&mut self) -> Result<U, O::Err> {
        self.op.run().await.map(&mut self.f)
    }
}

/// See [`Op::map_err`].
#[derive(Clone)]
#[must_use = "ops do nothing until `.run().await`"]
pub struct MapErr<O, F> {
    op: O,
    f: F,
}

impl<O, F> MapErr<O, F> {
    pub(crate) fn new(op: O, f: F) -> Self {
        Self { op, f }
    }
}

impl<O, F, E2> Op for MapErr<O, F>
where
    O: Op,
    F: FnMut(O::Err) -> E2,
{
    type Ok = O::Ok;
    type Err = E2;

    async fn run(&mut self) -> Result<O::Ok, E2> {
        self.op.run().await.map_err(&mut self.f)
    }
}

/// See [`Op::and_then`].
#[derive(Clone)]
#[must_use = "ops do nothing until `.run().await`"]
pub struct AndThen<O, F> {
    op: O,
    f: F,
}

impl<O, F> AndThen<O, F> {
    pub(crate) fn new(op: O, f: F) -> Self {
        Self { op, f }
    }
}

impl<O, F, U> Op for AndThen<O, F>
where
    O: Op,
    F: AsyncFnMut(O::Ok) -> Result<U, O::Err>,
{
    type Ok = U;
    type Err = O::Err;

    async fn run(&mut self) -> Result<U, O::Err> {
        let a = self.op.run().await?;
        (self.f)(a).await
    }
}

/// See [`Op::ensure`].
#[derive(Clone)]
#[must_use = "ops do nothing until `.run().await`"]
pub struct Ensure<O, P, F> {
    op: O,
    pred: P,
    err: F,
}

impl<O, P, F> Ensure<O, P, F> {
    pub(crate) fn new(op: O, pred: P, err: F) -> Self {
        Self { op, pred, err }
    }
}

impl<O, P, F> Op for Ensure<O, P, F>
where
    O: Op,
    P: FnMut(&O::Ok) -> bool,
    F: FnMut(&O::Ok) -> O::Err,
{
    type Ok = O::Ok;
    type Err = O::Err;

    async fn run(&mut self) -> Result<O::Ok, O::Err> {
        let v = self.op.run().await?;
        if (self.pred)(&v) {
            Ok(v)
        } else {
            Err((self.err)(&v))
        }
    }
}

/// See [`Op::or`].
#[derive(Clone)]
#[must_use = "ops do nothing until `.run().await`"]
pub struct Or<A, B> {
    a: A,
    b: B,
}

impl<A, B> Or<A, B> {
    pub(crate) fn new(a: A, b: B) -> Self {
        Self { a, b }
    }
}

impl<A, B> Op for Or<A, B>
where
    A: Op,
    B: Op<Ok = A::Ok>,
{
    type Ok = A::Ok;
    type Err = B::Err;

    async fn run(&mut self) -> Result<A::Ok, B::Err> {
        match self.a.run().await {
            Ok(v) => Ok(v),
            Err(_) => {
                crate::obs::event!("counterpoint.or: primary failed, running fallback");
                self.b.run().await
            }
        }
    }
}

/// See [`Op::or_else`].
#[derive(Clone)]
#[must_use = "ops do nothing until `.run().await`"]
pub struct OrElse<O, F> {
    op: O,
    f: F,
}

impl<O, F> OrElse<O, F> {
    pub(crate) fn new(op: O, f: F) -> Self {
        Self { op, f }
    }
}

impl<O, F, E2> Op for OrElse<O, F>
where
    O: Op,
    F: AsyncFnMut(O::Err) -> Result<O::Ok, E2>,
{
    type Ok = O::Ok;
    type Err = E2;

    async fn run(&mut self) -> Result<O::Ok, E2> {
        match self.op.run().await {
            Ok(v) => Ok(v),
            Err(e) => (self.f)(e).await,
        }
    }
}
