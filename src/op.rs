//! The [`Op`] trait: a re-runnable, fallible async computation.

use core::future::Future;
use core::marker::PhantomData;
use core::time::Duration;

use crate::breaker::{Breaker, CircuitBreaker, CircuitOpen};
use crate::par::{Hedge, Join, Race, Zip};
use crate::retry::{Repeat, Retry};
use crate::schedule::ScheduleFor;
use crate::seq::{AndThen, Ensure, Map, MapErr, Or, OrElse};
use crate::time::{Elapsed, Timeout};

/// A *description* of a fallible async computation that can be run any number of times.
///
/// A Rust [`Future`] is a one-shot value: once polled to completion it is gone. Retrying,
/// falling back or racing needs something that can produce a *fresh* future on demand, so the
/// central abstraction of this crate is a re-runnable recipe, much like `IO` in cats-effect or
/// `ZIO` values in Scala.
///
/// Every combinator returns a new `Op`; nothing happens until [`Op::run`] is awaited. The
/// future returned by `run` borrows the op mutably, so a single op is never run twice
/// concurrently, and no boxing or `'static` bound is needed.
///
/// Dropping the future returned by `run` cancels everything that is in flight inside it
/// (parallel branches, timers, retries): all combinators are plain state machines that own
/// their children.
///
/// # Example
///
/// ```
/// use counterpoint::prelude::*;
/// use std::time::Duration;
///
/// # #[derive(Debug, PartialEq)] enum MyError { Timeout }
/// # impl From<Elapsed> for MyError { fn from(_: Elapsed) -> Self { MyError::Timeout } }
/// # #[tokio::main(flavor = "current_thread")] async fn main() {
/// let mut price = from_fn(|| async { Ok::<u32, MyError>(42) })
///     .timeout(Duration::from_millis(200))
///     .retry(schedule::exponential(Duration::from_millis(10)).take(3));
///
/// assert_eq!(price.run().await, Ok(42));
/// # }
/// ```
pub trait Op {
    /// The success type.
    type Ok;
    /// The error type.
    type Err;

    /// Run the computation once.
    fn run(&mut self) -> impl Future<Output = Result<Self::Ok, Self::Err>>;

    // ----- functor / monad ------------------------------------------------------------------

    /// Transform the success value.
    fn map<F, U>(self, f: F) -> Map<Self, F>
    where
        Self: Sized,
        F: FnMut(Self::Ok) -> U,
    {
        Map::new(self, f)
    }

    /// Transform the error value.
    fn map_err<F, E2>(self, f: F) -> MapErr<Self, F>
    where
        Self: Sized,
        F: FnMut(Self::Err) -> E2,
    {
        MapErr::new(self, f)
    }

    /// Sequential composition: run `self`, then feed its value to `f`. Short-circuits on error.
    ///
    /// Laws (monad): `succeed(a).and_then(f) ≡ f(a)`, `m.and_then(succeed) ≡ m`,
    /// `m.and_then(f).and_then(g) ≡ m.and_then(|x| f(x).and_then(g))`.
    fn and_then<F, U>(self, f: F) -> AndThen<Self, F>
    where
        Self: Sized,
        F: AsyncFnMut(Self::Ok) -> Result<U, Self::Err>,
    {
        AndThen::new(self, f)
    }

    /// Guard: fail with `err(&value)` unless `pred(&value)` holds.
    ///
    /// Laws: `m.ensure(|_| true, e) ≡ m`;
    /// `m.ensure(p, e).ensure(q, e) ≡ m.ensure(|x| p(x) && q(x), e)`.
    fn ensure<P, F>(self, pred: P, err: F) -> Ensure<Self, P, F>
    where
        Self: Sized,
        P: FnMut(&Self::Ok) -> bool,
        F: FnMut(&Self::Ok) -> Self::Err,
    {
        Ensure::new(self, pred, err)
    }

    // ----- alternatives ---------------------------------------------------------------------

    /// Fallback: if `self` fails, run `other` (the first error is discarded).
    ///
    /// Laws: associative; `fail(e).or(b) ≡ b`; `succeed(a).or(b) ≡ succeed(a)`.
    /// There is **no** right identity: `a.or(fail(e))` replaces `a`'s error with `e`.
    fn or<B>(self, other: B) -> Or<Self, B>
    where
        Self: Sized,
        B: Op<Ok = Self::Ok>,
    {
        Or::new(self, other)
    }

    /// Fallback that sees the error, e.g. to serve a cached value or translate the error.
    fn or_else<F, E2>(self, f: F) -> OrElse<Self, F>
    where
        Self: Sized,
        F: AsyncFnMut(Self::Err) -> Result<Self::Ok, E2>,
    {
        OrElse::new(self, f)
    }

    // ----- parallel -------------------------------------------------------------------------

    /// Parallel product, fail-fast: run both at once; the first error wins and cancels the
    /// other branch.
    ///
    /// Laws: associative up to tuple re-association, `succeed(())` is a unit up to
    /// projection, commutative up to swapping the pair; duration is the `max` of both.
    fn zip<B>(self, other: B) -> Zip<Self, B>
    where
        Self: Sized,
        B: Op<Err = Self::Err>,
    {
        Zip::new(self, other)
    }

    /// Parallel product that waits for both and reports both outcomes. Never fails.
    fn join<B>(self, other: B) -> Join<Self, B>
    where
        Self: Sized,
        B: Op,
    {
        Join::new(self, other)
    }

    /// Race: run both at once; whichever *finishes* first (success or error) wins and the
    /// loser is cancelled.
    ///
    /// Laws: associative, `never()` is the identity, commutative except on exact ties (ties
    /// go to the left operand); duration is the `min` of both.
    fn race<B>(self, other: B) -> Race<Self, B>
    where
        Self: Sized,
        B: Op<Ok = Self::Ok, Err = Self::Err>,
    {
        Race::new(self, other)
    }

    // ----- time & resilience ----------------------------------------------------------------

    /// Fail with [`Elapsed`] (converted into `Self::Err`) if the op takes longer than `limit`.
    /// The in-flight run is cancelled. `Duration::MAX` means "no limit" and adds no timer.
    ///
    /// Laws: `m.timeout(Duration::MAX) ≡ m`;
    /// `m.timeout(a).timeout(b) ≡ m.timeout(min(a, b))` (except on exact ties).
    fn timeout(self, limit: Duration) -> Timeout<Self>
    where
        Self: Sized,
        Self::Err: From<Elapsed>,
    {
        Timeout::new(self, limit)
    }

    /// Re-run on failure according to a [`Schedule`](crate::schedule::Schedule). The schedule
    /// sees each error, so it can stop on the ones that are not worth retrying.
    ///
    /// Laws: `m.retry(schedule::stop()) ≡ m` (and so `m.retry(recurs(0)) ≡ m`);
    /// `succeed(a).retry(s) ≡ succeed(a)`.
    fn retry<S>(self, schedule: S) -> Retry<Self, S>
    where
        Self: Sized,
        S: ScheduleFor<Self::Err>,
    {
        Retry::new(self, schedule)
    }

    /// Run again after each *success* according to a [`Schedule`](crate::schedule::Schedule), until the schedule stops or
    /// a run fails. Returns the last value, or the first error. The schedule sees each value,
    /// so polling is `op.repeat(spaced(d) & recur_until(|v: &V| v.is_done()))`.
    ///
    /// Laws: `m.repeat(schedule::stop()) ≡ m`; `fail(e).repeat(s) ≡ fail(e)`;
    /// `repeat` is `retry` with success and failure swapped;
    /// `m.repeat(recurs(a)).repeat(recurs(b))` runs `m` `(a + 1)(b + 1)` times.
    fn repeat<S>(self, schedule: S) -> Repeat<Self, S>
    where
        Self: Sized,
        S: ScheduleFor<Self::Ok>,
    {
        Repeat::new(self, schedule)
    }

    /// Hedged execution: start the op, and start another copy each time `schedule` says so
    /// while none has succeeded. The first *success* wins and cancels the others. An attempt
    /// that fails drops out; when none is left running, the next copy starts at once instead
    /// of waiting for its timer. If every attempt fails, the result is the last error to
    /// arrive.
    ///
    /// The schedule is asked for the delay until the next copy when the run starts and after
    /// each launch. Hedges run on clones of the op as it was when `hedge` was called, so the
    /// op must be [`Clone`]. A schedule that never stops launches copies until one succeeds:
    /// bound it, as in `spaced(d).take(2)`.
    ///
    /// Laws: `m.hedge(schedule::stop()) ≡ m`; `succeed(a).hedge(s) ≡ succeed(a)`; a success is
    /// never slower than the primary's; for attempts that succeed, one hedge after `d` is a
    /// `race` against the second attempt delayed by `d`.
    fn hedge<S>(self, schedule: S) -> Hedge<Self, S>
    where
        Self: Sized + Clone,
        S: ScheduleFor<()>,
    {
        Hedge::new(self, schedule)
    }

    /// Run through a shared [`CircuitBreaker`]. When the breaker is open the op is not run
    /// and fails with [`CircuitOpen`] (converted into `Self::Err`).
    fn with_breaker<S>(self, breaker: CircuitBreaker<S>) -> Breaker<Self, S>
    where
        Self: Sized,
        Self::Err: From<CircuitOpen>,
        S: ScheduleFor<()>,
    {
        Breaker::new(self, breaker)
    }
}

impl<O: Op + ?Sized> Op for &mut O {
    type Ok = O::Ok;
    type Err = O::Err;

    fn run(&mut self) -> impl Future<Output = Result<Self::Ok, Self::Err>> {
        (**self).run()
    }
}

// ----- constructors ---------------------------------------------------------------------------

/// An op built from an async closure (`|| async { .. }` or `async || { .. }`).
#[derive(Clone)]
#[must_use = "ops do nothing until `.run().await`"]
pub struct FromFn<F> {
    f: F,
}

/// Lift an async closure into an [`Op`]. The closure is called once per run.
pub fn from_fn<F, T, E>(f: F) -> FromFn<F>
where
    F: AsyncFnMut() -> Result<T, E>,
{
    FromFn { f }
}

impl<F, T, E> Op for FromFn<F>
where
    F: AsyncFnMut() -> Result<T, E>,
{
    type Ok = T;
    type Err = E;

    fn run(&mut self) -> impl Future<Output = Result<T, E>> {
        (self.f)()
    }
}

/// An op that immediately succeeds with a clone of the value. See [`succeed`].
#[must_use = "ops do nothing until `.run().await`"]
pub struct Succeed<T, E> {
    value: T,
    _err: PhantomData<fn() -> E>,
}

impl<T: Clone, E> Clone for Succeed<T, E> {
    fn clone(&self) -> Self {
        Succeed {
            value: self.value.clone(),
            _err: PhantomData,
        }
    }
}

/// The op that succeeds immediately with `value` (the `pure`/`return` of the monad).
pub fn succeed<T: Clone, E>(value: T) -> Succeed<T, E> {
    Succeed {
        value,
        _err: PhantomData,
    }
}

impl<T: Clone, E> Op for Succeed<T, E> {
    type Ok = T;
    type Err = E;

    async fn run(&mut self) -> Result<T, E> {
        Ok(self.value.clone())
    }
}

/// An op that immediately fails with a clone of the error. See [`fail`].
#[must_use = "ops do nothing until `.run().await`"]
pub struct Fail<T, E> {
    error: E,
    _ok: PhantomData<fn() -> T>,
}

impl<T, E: Clone> Clone for Fail<T, E> {
    fn clone(&self) -> Self {
        Fail {
            error: self.error.clone(),
            _ok: PhantomData,
        }
    }
}

/// The op that fails immediately with `error`. Left identity of [`Op::or`].
pub fn fail<T, E: Clone>(error: E) -> Fail<T, E> {
    Fail {
        error,
        _ok: PhantomData,
    }
}

impl<T, E: Clone> Op for Fail<T, E> {
    type Ok = T;
    type Err = E;

    async fn run(&mut self) -> Result<T, E> {
        Err(self.error.clone())
    }
}

/// An op that never completes. See [`never()`](fn@crate::never).
#[must_use = "ops do nothing until `.run().await`"]
pub struct Never<T, E> {
    _t: PhantomData<fn() -> (T, E)>,
}

impl<T, E> Clone for Never<T, E> {
    fn clone(&self) -> Self {
        Never { _t: PhantomData }
    }
}

/// The op that never completes: identity of [`Op::race`].
pub fn never<T, E>() -> Never<T, E> {
    Never { _t: PhantomData }
}

impl<T, E> Op for Never<T, E> {
    type Ok = T;
    type Err = E;

    async fn run(&mut self) -> Result<T, E> {
        core::future::pending().await
    }
}
