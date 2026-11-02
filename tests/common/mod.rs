//! Test helpers: scripted ops on tokio's virtual clock and an observational-equivalence check.
#![allow(dead_code)]

use std::cell::Cell;
use std::future::Future;
use std::rc::Rc;
use std::time::Duration;

use counterpoint::prelude::*;
use proptest::prelude::*;
use tokio::time::{Instant, sleep};

/// The error type used by every test.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TErr {
    Code(u8),
    Timeout,
    Open,
}

impl From<Elapsed> for TErr {
    fn from(_: Elapsed) -> Self {
        TErr::Timeout
    }
}

impl From<CircuitOpen> for TErr {
    fn from(_: CircuitOpen) -> Self {
        TErr::Open
    }
}

pub type Out<T = i32> = Result<T, TErr>;

pub fn ms(n: u64) -> Duration {
    Duration::from_millis(n)
}

/// Run a future to completion on a fresh current-thread runtime with a paused clock and return
/// its output together with the virtual time it took.
pub fn on_virtual_clock<F: Future>(f: F) -> (F::Output, Duration) {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .start_paused(true)
        .build()
        .unwrap();
    rt.block_on(async {
        let t0 = Instant::now();
        let out = f.await;
        (out, t0.elapsed())
    })
}

/// Observe one run of an op: its result and its (virtual) duration.
pub fn observe<O: Op>(mut op: O) -> (Result<O::Ok, O::Err>, Duration) {
    on_virtual_clock(async move { op.run().await })
}

/// A leaf op: waits `delay_ms` of virtual time, then returns `result`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Leaf {
    pub delay_ms: u64,
    pub result: Out,
}

impl Leaf {
    pub fn op(self) -> impl Op<Ok = i32, Err = TErr> + Clone {
        from_fn(move || async move {
            sleep(ms(self.delay_ms)).await;
            self.result
        })
    }

    pub fn ok(&self) -> bool {
        self.result.is_ok()
    }
}

pub fn out() -> impl Strategy<Value = Out> {
    prop_oneof![
        3 => (-50i32..50).prop_map(Ok),
        1 => (0u8..5).prop_map(|e| Err(TErr::Code(e))),
    ]
}

pub fn leaf() -> impl Strategy<Value = Leaf> {
    (0u64..60, out()).prop_map(|(delay_ms, result)| Leaf { delay_ms, result })
}

/// A Kleisli arrow `i32 -> Op`: waits, then adds `k` or fails.
#[derive(Debug, Clone, Copy)]
pub struct Arrow {
    pub delay_ms: u64,
    pub k: i32,
    pub fail: Option<u8>,
}

impl Arrow {
    pub async fn call(self, x: i32) -> Out {
        sleep(ms(self.delay_ms)).await;
        match self.fail {
            Some(e) => Err(TErr::Code(e)),
            None => Ok(x.wrapping_add(self.k)),
        }
    }
}

pub fn arrow() -> impl Strategy<Value = Arrow> {
    (0u64..60, -10i32..10, prop::option::weighted(0.25, 0u8..5))
        .prop_map(|(delay_ms, k, fail)| Arrow { delay_ms, k, fail })
}

/// An op that fails `failures` times (each run taking `delay_ms`) and then succeeds forever.
/// Returns the op and a counter of how many times it ran.
pub fn flaky(failures: u32, delay_ms: u64) -> (impl Op<Ok = u32, Err = TErr>, Rc<Cell<u32>>) {
    let calls = Rc::new(Cell::new(0u32));
    let c = calls.clone();
    let op = from_fn(move || {
        let c = c.clone();
        async move {
            sleep(ms(delay_ms)).await;
            let n = c.get();
            c.set(n + 1);
            if n < failures {
                Err(TErr::Code(n as u8))
            } else {
                Ok(n)
            }
        }
    });
    (op, calls)
}

/// Sets a flag when dropped: proves that a branch was cancelled.
pub struct DropFlag(pub Rc<Cell<bool>>);

impl Drop for DropFlag {
    fn drop(&mut self) {
        self.0.set(true);
    }
}
