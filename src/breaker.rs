//! A minimal circuit breaker, shareable between ops and tasks.
//!
//! States: **closed** (calls go through; consecutive failures are counted), **open** (calls
//! fail fast with [`CircuitOpen`] until the cool-down passes) and **half-open** (exactly one
//! probe call is let through; success closes the circuit, failure opens it again).
//!
//! Cancelling a probe (dropping its future) releases the half-open slot, so a cancelled probe
//! never wedges the breaker.

use core::fmt;
use core::time::Duration;
use std::sync::{Arc, Mutex, MutexGuard};

use tokio::time::Instant;

use crate::op::Op;

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
    Closed { failures: u32 },
    Open { until: Instant },
    HalfOpen { probing: bool },
}

#[derive(Debug)]
struct Inner {
    state: State,
    threshold: u32,
    cooldown: Duration,
}

impl Inner {
    fn refresh(&mut self) {
        if let State::Open { until } = self.state
            && Instant::now() >= until
        {
            self.state = State::HalfOpen { probing: false };
            crate::obs::event!("counterpoint.breaker: half-open");
        }
    }

    fn open(&mut self) {
        let until = Instant::now()
            .checked_add(self.cooldown)
            .unwrap_or_else(|| Instant::now() + Duration::from_secs(86_400 * 365 * 30));
        self.state = State::Open { until };
        crate::obs::event!("counterpoint.breaker: open for {:?}", self.cooldown);
    }
}

/// A shared circuit breaker. Cloning it gives another handle to the same breaker.
#[derive(Debug, Clone)]
pub struct CircuitBreaker {
    inner: Arc<Mutex<Inner>>,
}

impl CircuitBreaker {
    /// Open after `failure_threshold` consecutive failures; stay open for `cooldown`.
    /// Panics if `failure_threshold == 0`.
    pub fn new(failure_threshold: u32, cooldown: Duration) -> Self {
        assert!(failure_threshold > 0, "failure threshold must be positive");
        CircuitBreaker {
            inner: Arc::new(Mutex::new(Inner {
                state: State::Closed { failures: 0 },
                threshold: failure_threshold,
                cooldown,
            })),
        }
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        // No user code runs under the lock, so a poisoned lock still holds a valid state.
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

    fn acquire(&self) -> Result<Permit<'_>, CircuitOpen> {
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

struct Permit<'a> {
    breaker: &'a CircuitBreaker,
    probe: bool,
    settled: bool,
}

impl Permit<'_> {
    fn settle(mut self, success: bool) {
        self.settled = true;
        let mut guard = self.breaker.lock();
        let inner = &mut *guard;
        match (&mut inner.state, success) {
            (State::Closed { failures }, true) => *failures = 0,
            (State::Closed { failures }, false) => {
                *failures += 1;
                if *failures >= inner.threshold {
                    inner.open();
                }
            }
            (State::HalfOpen { .. }, true) if self.probe => {
                inner.state = State::Closed { failures: 0 };
                crate::obs::event!("counterpoint.breaker: closed");
            }
            (State::HalfOpen { .. }, false) if self.probe => inner.open(),
            // Late results of calls admitted before the breaker opened are ignored.
            _ => {}
        }
    }
}

impl Drop for Permit<'_> {
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
#[derive(Clone)]
#[must_use = "ops do nothing until `.run().await`"]
pub struct Breaker<O> {
    op: O,
    breaker: CircuitBreaker,
}

impl<O> Breaker<O> {
    pub(crate) fn new(op: O, breaker: CircuitBreaker) -> Self {
        Breaker { op, breaker }
    }
}

impl<O> Op for Breaker<O>
where
    O: Op,
    O::Err: From<CircuitOpen>,
{
    type Ok = O::Ok;
    type Err = O::Err;

    async fn run(&mut self) -> Result<O::Ok, O::Err> {
        let permit = self.breaker.acquire()?;
        let result = self.op.run().await;
        permit.settle(result.is_ok());
        result
    }
}
