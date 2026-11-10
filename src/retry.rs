//! Re-running an [`Op`] according to a [`Schedule`](crate::schedule::Schedule): on failure with [`Retry`], on success with
//! [`Repeat`].

use core::time::Duration;

use tokio::time::Instant;

use crate::op::Op;
use crate::schedule::{ScheduleFor, Step};

/// Decides whether an error is worth retrying. Implemented by [`Always`] and by any
/// `FnMut(&E) -> bool`.
pub trait RetryIf<E> {
    /// `true` to let the schedule decide, `false` to fail immediately.
    fn retryable(&mut self, error: &E) -> bool;
}

/// Retry every error (the default).
#[derive(Debug, Clone, Copy, Default)]
pub struct Always;

impl<E> RetryIf<E> for Always {
    fn retryable(&mut self, _: &E) -> bool {
        true
    }
}

impl<E, F: FnMut(&E) -> bool> RetryIf<E> for F {
    fn retryable(&mut self, error: &E) -> bool {
        self(error)
    }
}

/// Observes retries. Implemented by [`NoHook`] and by any `FnMut(&E, u32, Duration)`, which
/// receives the error, the 1-based retry number and the delay before it.
pub trait OnRetry<E> {
    /// Called right before sleeping ahead of a retry.
    fn on_retry(&mut self, error: &E, retry: u32, delay: Duration);
}

/// No retry hook (the default).
#[derive(Debug, Clone, Copy, Default)]
pub struct NoHook;

impl<E> OnRetry<E> for NoHook {
    fn on_retry(&mut self, _: &E, _: u32, _: Duration) {}
}

impl<E, F: FnMut(&E, u32, Duration)> OnRetry<E> for F {
    fn on_retry(&mut self, error: &E, retry: u32, delay: Duration) {
        self(error, retry, delay)
    }
}

/// See [`Op::retry`].
#[derive(Clone)]
#[must_use = "ops do nothing until `.run().await`"]
pub struct Retry<O, S, W = Always, H = NoHook> {
    op: O,
    schedule: S,
    when: W,
    hook: H,
}

impl<O, S> Retry<O, S> {
    pub(crate) fn new(op: O, schedule: S) -> Self {
        Retry {
            op,
            schedule,
            when: Always,
            hook: NoHook,
        }
    }
}

impl<O, S, W, H> Retry<O, S, W, H> {
    /// Only retry errors for which `pred` returns `true`; others fail immediately.
    ///
    /// Laws: `.when(|_| true) ≡` no filter; `m.retry(s).when(|_| false) ≡ m`.
    pub fn when<P>(self, pred: P) -> Retry<O, S, P, H> {
        Retry {
            op: self.op,
            schedule: self.schedule,
            when: pred,
            hook: self.hook,
        }
    }

    /// Call `hook(&error, retry_number, delay)` before each retry (logging, metrics…).
    pub fn on_retry<G>(self, hook: G) -> Retry<O, S, W, G> {
        Retry {
            op: self.op,
            schedule: self.schedule,
            when: self.when,
            hook,
        }
    }
}

impl<O, S, W, H> Op for Retry<O, S, W, H>
where
    O: Op,
    S: ScheduleFor<O::Err>,
    W: RetryIf<O::Err>,
    H: OnRetry<O::Err>,
{
    type Ok = O::Ok;
    type Err = O::Err;

    async fn run(&mut self) -> Result<O::Ok, O::Err> {
        let start = Instant::now();
        let mut schedule = self.schedule.fresh();
        let mut attempt: u32 = 0;
        loop {
            let err = match self.op.run().await {
                Ok(v) => return Ok(v),
                Err(e) => e,
            };
            if !self.when.retryable(&err) {
                return Err(err);
            }
            let step = Step {
                attempt,
                elapsed: start.elapsed(),
            };
            let Some(delay) = schedule.next(&err, step) else {
                crate::obs::event!("counterpoint.retry: giving up after {} retries", attempt);
                return Err(err);
            };
            attempt = attempt.saturating_add(1);
            self.hook.on_retry(&err, attempt, delay);
            crate::obs::event!("counterpoint.retry: retry {} in {:?}", attempt, delay);
            drop(err);
            if !delay.is_zero() {
                tokio::time::sleep(delay).await;
            }
        }
    }
}

/// See [`Op::repeat`].
#[derive(Clone)]
#[must_use = "ops do nothing until `.run().await`"]
pub struct Repeat<O, S> {
    op: O,
    schedule: S,
}

impl<O, S> Repeat<O, S> {
    pub(crate) fn new(op: O, schedule: S) -> Self {
        Repeat { op, schedule }
    }
}

impl<O, S> Op for Repeat<O, S>
where
    O: Op,
    S: ScheduleFor<O::Ok>,
{
    type Ok = O::Ok;
    type Err = O::Err;

    async fn run(&mut self) -> Result<O::Ok, O::Err> {
        let start = Instant::now();
        let mut schedule = self.schedule.fresh();
        let mut attempt: u32 = 0;
        loop {
            let value = self.op.run().await?;
            let step = Step {
                attempt,
                elapsed: start.elapsed(),
            };
            let Some(delay) = schedule.next(&value, step) else {
                return Ok(value);
            };
            attempt = attempt.saturating_add(1);
            crate::obs::event!("counterpoint.repeat: repetition {} in {:?}", attempt, delay);
            drop(value);
            if !delay.is_zero() {
                tokio::time::sleep(delay).await;
            }
        }
    }
}
