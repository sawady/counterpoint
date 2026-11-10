//! A minimal circuit breaker, shareable between ops and tasks.
//!
//! States: **closed** (calls go through; consecutive failures are counted), **open** (calls
//! fail fast with [`CircuitOpen`] until the cool-down passes) and **half-open** (exactly one
//! probe call is let through; success closes the circuit, failure opens it again).
//!
//! Cancelling a probe (dropping its future) releases the half-open slot, so a cancelled probe
//! never wedges the breaker.
//!
//! By default every error counts as a failure. [`Breaker::when`] picks the ones that do: the
//! others are neutral, as if the call had not been made.
//!
//! The cool-down is a [`Schedule`](crate::schedule::Schedule): [`CircuitBreaker::new`] uses a
//! fixed one and [`CircuitBreaker::with_schedule`] accepts any, so a breaker can back off while
//! a dependency stays down.

use core::fmt;
use core::time::Duration;
use std::sync::{Arc, Mutex, MutexGuard};

use tokio::time::Instant;

use crate::op::Op;
use crate::retry::Always;
use crate::schedule::{self, ScheduleFor, Spaced, Step};

/// Decides whether an error counts as a failure for the breaker. Implemented by
/// [`Always`] (every error counts, the default) and by any
/// `FnMut(&E) -> bool`.
pub trait CountIf<E> {
    /// `true` if `error` is a failure of the dependency, `false` to leave the breaker as it was.
    fn counts(&mut self, error: &E) -> bool;
}

impl<E> CountIf<E> for Always {
    fn counts(&mut self, _: &E) -> bool {
        true
    }
}

impl<E, F: FnMut(&E) -> bool> CountIf<E> for F {
    fn counts(&mut self, error: &E) -> bool {
        self(error)
    }
}

/// How a call through the breaker ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Outcome {
    Success,
    Failure,
    /// An error that does not count: neither a success nor a failure.
    Ignored,
}

/// Error returned when the breaker rejects a call. Convert it into your error type with
/// `From`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct CircuitOpen;

impl fmt::Display for CircuitOpen {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("circuit breaker is open")
    }
}

impl std::error::Error for CircuitOpen {}

/// Observable state of a [`CircuitBreaker`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BreakerState {
    /// Calls go through.
    Closed,
    /// Calls are rejected until the cool-down passes.
    Open,
    /// The next call is a probe.
    HalfOpen,
}

#[derive(Debug)]
enum State {
    Closed {
        failures: u32,
    },
    /// `until` is `None` when the cool-down schedule stopped: open for good.
    Open {
        until: Option<Instant>,
    },
    HalfOpen {
        probing: bool,
    },
}

#[derive(Debug)]
struct Inner<S> {
    state: State,
    threshold: u32,
    cooldown: S,
    /// Trips since the circuit was last closed.
    trips: u32,
    /// When the circuit first opened since it was last closed.
    first_trip: Option<Instant>,
}

impl<S> Inner<S> {
    fn refresh(&mut self) {
        if let State::Open { until: Some(until) } = self.state
            && Instant::now() >= until
        {
            self.state = State::HalfOpen { probing: false };
            crate::obs::event!("counterpoint.breaker: half-open");
        }
    }
}

impl<S: ScheduleFor<()>> Inner<S> {
    fn open(&mut self) {
        let now = Instant::now();
        let first_trip = *self.first_trip.get_or_insert(now);
        let step = Step {
            attempt: self.trips,
            elapsed: now.saturating_duration_since(first_trip),
        };
        let cooldown = self.cooldown.next(&(), step);
        self.trips = self.trips.saturating_add(1);
        crate::obs::event!("counterpoint.breaker: open for {:?}", cooldown);
        let until = cooldown.map(|d| {
            now.checked_add(d)
                .unwrap_or_else(|| now + Duration::from_secs(86_400 * 365 * 30))
        });
        self.state = State::Open { until };
    }

    fn close(&mut self) {
        self.state = State::Closed { failures: 0 };
        self.cooldown = self.cooldown.fresh();
        self.trips = 0;
        self.first_trip = None;
        crate::obs::event!("counterpoint.breaker: closed");
    }
}

/// A shared circuit breaker. Cloning it gives another handle to the same breaker.
#[derive(Debug)]
pub struct CircuitBreaker<S = Spaced> {
    inner: Arc<Mutex<Inner<S>>>,
}

impl<S> Clone for CircuitBreaker<S> {
    fn clone(&self) -> Self {
        CircuitBreaker {
            inner: self.inner.clone(),
        }
    }
}

impl CircuitBreaker {
    /// Open after `failure_threshold` consecutive failures; stay open for `cooldown`.
    /// Panics if `failure_threshold == 0`.
    pub fn new(failure_threshold: u32, cooldown: Duration) -> Self {
        Self::with_schedule(failure_threshold, schedule::spaced(cooldown))
    }
}

impl<S: ScheduleFor<()>> CircuitBreaker<S> {
    /// Open after `failure_threshold` consecutive failures and let `cooldown` decide how long
    /// each opening lasts. Panics if `failure_threshold == 0`.
    ///
    /// The schedule is asked once per trip: `attempt` counts the earlier trips since the circuit
    /// was last closed and `elapsed` is the time since the first of them. When it stops, the
    /// circuit stays open for good. A successful probe closes the circuit and resets the
    /// schedule.
    ///
    /// Laws: `with_schedule(n, spaced(d)) ≡ new(n, d)`; the `k`-th consecutive opening lasts the
    /// `k`-th delay of the schedule.
    ///
    /// ```
    /// use counterpoint::prelude::*;
    /// use std::time::Duration;
    ///
    /// let ms = Duration::from_millis;
    /// let breaker = CircuitBreaker::with_schedule(5, schedule::exponential(ms(100)).cap(ms(5_000)));
    /// # let _ = breaker;
    /// ```
    pub fn with_schedule(failure_threshold: u32, cooldown: S) -> Self {
        assert!(failure_threshold > 0, "failure threshold must be positive");
        CircuitBreaker {
            inner: Arc::new(Mutex::new(Inner {
                state: State::Closed { failures: 0 },
                threshold: failure_threshold,
                cooldown,
                trips: 0,
                first_trip: None,
            })),
        }
    }
}

impl<S> CircuitBreaker<S> {
    fn lock(&self) -> MutexGuard<'_, Inner<S>> {
        // The only user code that runs under the lock is the cool-down schedule, and the state
        // is written after it returns, so a poisoned lock still holds a valid state.
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The current state.
    pub fn state(&self) -> BreakerState {
        let mut inner = self.lock();
        inner.refresh();
        match inner.state {
            State::Closed { .. } => BreakerState::Closed,
            State::Open { .. } => BreakerState::Open,
            State::HalfOpen { .. } => BreakerState::HalfOpen,
        }
    }

    fn acquire(&self) -> Result<Permit<'_, S>, CircuitOpen> {
        let mut inner = self.lock();
        inner.refresh();
        let probe = match &mut inner.state {
            State::Closed { .. } => false,
            State::Open { .. } | State::HalfOpen { probing: true } => return Err(CircuitOpen),
            State::HalfOpen { probing } => {
                *probing = true;
                true
            }
        };
        Ok(Permit {
            breaker: self,
            probe,
            settled: false,
        })
    }
}

struct Permit<'a, S> {
    breaker: &'a CircuitBreaker<S>,
    probe: bool,
    settled: bool,
}

impl<S: ScheduleFor<()>> Permit<'_, S> {
    fn settle(mut self, outcome: Outcome) {
        if outcome == Outcome::Ignored {
            // Leave the state as it was; dropping the permit gives a probe's slot back, exactly
            // as when the probe is cancelled.
            return;
        }
        self.settled = true;
        let mut guard = self.breaker.lock();
        let inner = &mut *guard;
        match (&mut inner.state, outcome == Outcome::Success) {
            (State::Closed { failures }, true) => *failures = 0,
            (State::Closed { failures }, false) => {
                *failures += 1;
                if *failures >= inner.threshold {
                    inner.open();
                }
            }
            (State::HalfOpen { .. }, true) if self.probe => inner.close(),
            (State::HalfOpen { .. }, false) if self.probe => inner.open(),
            // Late results of calls admitted before the breaker opened are ignored.
            _ => {}
        }
    }
}

impl<S> Drop for Permit<'_, S> {
    fn drop(&mut self) {
        if !self.settled && self.probe {
            let mut inner = self.breaker.lock();
            if let State::HalfOpen { probing } = &mut inner.state {
                *probing = false;
            }
        }
    }
}

/// See [`Op::with_breaker`].
#[must_use = "ops do nothing until `.run().await`"]
pub struct Breaker<O, S = Spaced, W = Always> {
    op: O,
    breaker: CircuitBreaker<S>,
    when: W,
}

impl<O: Clone, S, W: Clone> Clone for Breaker<O, S, W> {
    fn clone(&self) -> Self {
        Breaker {
            op: self.op.clone(),
            breaker: self.breaker.clone(),
            when: self.when.clone(),
        }
    }
}

impl<O, S> Breaker<O, S> {
    pub(crate) fn new(op: O, breaker: CircuitBreaker<S>) -> Self {
        Breaker {
            op,
            breaker,
            when: Always,
        }
    }
}

impl<O, S, W> Breaker<O, S, W> {
    /// Only count errors for which `pred` returns `true` as failures. The others are neutral:
    /// they do not add to the consecutive failures, do not reset them, and a probe that ends
    /// with one gives its half-open slot back, so the next call is the probe. Use it for errors
    /// that say nothing about the health of the dependency, such as a rejected request.
    ///
    /// The classification belongs to the op, not to the breaker: ops sharing a breaker may
    /// classify differently. Rejections with [`CircuitOpen`] are not seen by `pred`.
    ///
    /// Laws: `.when(|_| true) ≡` no filter; with `.when(|_| false)` a closed breaker never
    /// opens; removing the calls that end with an uncounted error from a trace changes nothing
    /// for the other calls.
    ///
    /// ```
    /// use counterpoint::prelude::*;
    /// use std::time::Duration;
    ///
    /// #[derive(Debug, Clone, PartialEq)]
    /// enum Error { Unavailable, BadRequest, Open }
    /// impl From<CircuitOpen> for Error { fn from(_: CircuitOpen) -> Self { Error::Open } }
    ///
    /// # #[tokio::main(flavor = "current_thread")] async fn main() {
    /// let breaker = CircuitBreaker::new(1, Duration::from_secs(10));
    /// let mut call = from_fn(|| async { Err::<(), _>(Error::BadRequest) })
    ///     .with_breaker(breaker.clone())
    ///     .when(|e: &Error| *e == Error::Unavailable);
    /// assert_eq!(call.run().await, Err(Error::BadRequest));
    /// assert_eq!(breaker.state(), BreakerState::Closed);
    /// # }
    /// ```
    pub fn when<P>(self, pred: P) -> Breaker<O, S, P> {
        Breaker {
            op: self.op,
            breaker: self.breaker,
            when: pred,
        }
    }
}

impl<O, S, W> Op for Breaker<O, S, W>
where
    O: Op,
    O::Err: From<CircuitOpen>,
    S: ScheduleFor<()>,
    W: CountIf<O::Err>,
{
    type Ok = O::Ok;
    type Err = O::Err;

    async fn run(&mut self) -> Result<O::Ok, O::Err> {
        let permit = self.breaker.acquire()?;
        let result = self.op.run().await;
        let outcome = match &result {
            Ok(_) => Outcome::Success,
            Err(e) if self.when.counts(e) => Outcome::Failure,
            Err(_) => Outcome::Ignored,
        };
        permit.settle(outcome);
        result
    }
}
