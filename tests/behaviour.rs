//! Behavioural tests: cancellation (drop-safety), bounded concurrency, circuit breaker, hooks,
//! and `Send`-ness of composed ops. All run on tokio's paused clock.

mod common;

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use common::*;
use counterpoint::prelude::*;
use tokio::time::{Instant, sleep};

/// An op that sleeps `d` and returns `v`, flagging `dropped` if cancelled before finishing.
fn cancellable(d: u64, v: Out, dropped: Rc<Cell<bool>>) -> impl Op<Ok = i32, Err = TErr> {
    from_fn(move || {
        let dropped = dropped.clone();
        async move {
            let guard = DropFlag(dropped);
            sleep(ms(d)).await;
            std::mem::forget(guard);
            v
        }
    })
}

#[tokio::test(start_paused = true)]
async fn race_cancels_the_loser() {
    let lost = Rc::new(Cell::new(false));
    let won = Rc::new(Cell::new(false));
    let mut op = cancellable(10, Ok(1), won.clone()).race(cancellable(50, Ok(2), lost.clone()));
    let t0 = Instant::now();
    assert_eq!(op.run().await, Ok(1));
    assert_eq!(t0.elapsed(), ms(10));
    assert!(lost.get(), "loser must be dropped");
    assert!(!won.get(), "winner ran to completion");
}

#[tokio::test(start_paused = true)]
async fn zip_fails_fast_and_cancels_the_sibling() {
    let sibling = Rc::new(Cell::new(false));
    let mut op = cancellable(10, Err(TErr::Code(7)), Rc::default()).zip(cancellable(
        1000,
        Ok(2),
        sibling.clone(),
    ));
    let t0 = Instant::now();
    assert_eq!(op.run().await, Err(TErr::Code(7)));
    assert_eq!(t0.elapsed(), ms(10));
    assert!(sibling.get());
}

#[tokio::test(start_paused = true)]
async fn timeout_cancels_the_inner_op() {
    let inner = Rc::new(Cell::new(false));
    let mut op = cancellable(100, Ok(1), inner.clone()).timeout(ms(30));
    assert_eq!(op.run().await, Err(TErr::Timeout));
    assert!(inner.get());
}

#[tokio::test(start_paused = true)]
async fn dropping_a_retry_mid_backoff_stops_it() {
    let (op, calls) = flaky(u32::MAX, 1);
    let mut op = op.retry(schedule::spaced(ms(100)));
    let r = tokio::time::timeout(ms(250), op.run()).await;
    assert!(r.is_err());
    // runs at t=0, 101, 202; the run that would start at 303 never happens.
    assert_eq!(calls.get(), 3);
    sleep(ms(1000)).await;
    assert_eq!(calls.get(), 3);
}

#[tokio::test(start_paused = true)]
async fn all_respects_the_concurrency_limit() {
    let running = Rc::new(Cell::new(0usize));
    let peak = Rc::new(Cell::new(0usize));
    let ops = (0..10).map(|i| {
        let (running, peak) = (running.clone(), peak.clone());
        from_fn(move || {
            let (running, peak) = (running.clone(), peak.clone());
            async move {
                running.set(running.get() + 1);
                peak.set(peak.get().max(running.get()));
                sleep(ms(10)).await;
                running.set(running.get() - 1);
                Ok::<_, TErr>(i)
            }
        })
    });
    let mut op = all(ops).limit(3);
    let t0 = Instant::now();
    assert_eq!(op.run().await, Ok((0..10).collect::<Vec<_>>()));
    assert_eq!(peak.get(), 3);
    assert_eq!(t0.elapsed(), ms(40)); // ceil(10 / 3) waves of 10ms
}

#[tokio::test(start_paused = true)]
async fn all_fail_fast_cancels_the_rest() {
    let dropped: Vec<_> = (0..4).map(|_| Rc::new(Cell::new(false))).collect();
    let ops = (0..4).map(|i| cancellable(100, Ok(i), dropped[i as usize].clone()));
    let failing = cancellable(5, Err(TErr::Code(1)), Rc::default());
    let mut op = all(ops).zip(failing);
    assert_eq!(op.run().await, Err(TErr::Code(1)));
    assert!(dropped.iter().all(|d| d.get()));
}

#[tokio::test(start_paused = true)]
async fn race_all_picks_the_fastest() {
    let mut op = race_all([30u64, 10, 20].map(|d| {
        from_fn(move || async move {
            sleep(ms(d)).await;
            Ok::<_, TErr>(d)
        })
    }));
    assert_eq!(op.run().await, Ok(10));
}

#[tokio::test(start_paused = true)]
async fn retry_hook_and_filter() {
    let log = Rc::new(RefCell::new(Vec::new()));
    let l = log.clone();
    let (op, calls) = flaky(3, 0);
    let mut op = op
        .retry(schedule::exponential(ms(10)).take(5))
        .on_retry(move |e: &TErr, n, d| l.borrow_mut().push((*e, n, d)));
    assert_eq!(op.run().await, Ok(3));
    assert_eq!(calls.get(), 4);
    assert_eq!(
        *log.borrow(),
        [
            (TErr::Code(0), 1, ms(10)),
            (TErr::Code(1), 2, ms(20)),
            (TErr::Code(2), 3, ms(40)),
        ]
    );

    // Errors that are not retryable fail immediately.
    let (op, calls) = flaky(3, 0);
    let mut op = op
        .retry(schedule::forever())
        .when(|e: &TErr| *e != TErr::Code(1));
    assert_eq!(op.run().await, Err(TErr::Code(1)));
    assert_eq!(calls.get(), 2);
}

#[tokio::test(start_paused = true)]
async fn or_else_serves_a_fallback_value() {
    let mut op = fail::<i32, TErr>(TErr::Code(3)).or_else(async |e| match e {
        TErr::Code(c) => Ok::<i32, TErr>(-i32::from(c)),
        other => Err(other),
    });
    assert_eq!(op.run().await, Ok(-3));
}

#[tokio::test(start_paused = true)]
async fn and_then_passes_values() {
    let mut op = succeed::<i32, TErr>(20)
        .and_then(async |x| Ok(x + 1))
        .and_then(async |x| Ok(x * 2))
        .ensure(|x| *x == 42, |_| TErr::Code(0));
    assert_eq!(op.run().await, Ok(42));
}

// ----- circuit breaker ------------------------------------------------------------------------

fn breaker_op(b: &CircuitBreaker, outcome: Rc<Cell<Out>>, d: u64) -> impl Op<Ok = i32, Err = TErr> {
    from_fn(move || {
        let outcome = outcome.clone();
        async move {
            sleep(ms(d)).await;
            outcome.get()
        }
    })
    .with_breaker(b.clone())
}

#[tokio::test(start_paused = true)]
async fn breaker_opens_half_opens_and_closes() {
    let b = CircuitBreaker::new(2, ms(100));
    let outcome = Rc::new(Cell::new(Err(TErr::Code(1))));
    let mut op = breaker_op(&b, outcome.clone(), 1);

    assert_eq!(op.run().await, Err(TErr::Code(1)));
    assert_eq!(b.state(), BreakerState::Closed);
    assert_eq!(op.run().await, Err(TErr::Code(1)));
    assert_eq!(b.state(), BreakerState::Open);
    assert_eq!(op.run().await, Err(TErr::Open), "fails fast while open");

    sleep(ms(100)).await;
    assert_eq!(b.state(), BreakerState::HalfOpen);
    assert_eq!(op.run().await, Err(TErr::Code(1)), "failed probe");
    assert_eq!(b.state(), BreakerState::Open);

    sleep(ms(100)).await;
    outcome.set(Ok(5));
    assert_eq!(op.run().await, Ok(5), "successful probe");
    assert_eq!(b.state(), BreakerState::Closed);
}

#[tokio::test(start_paused = true)]
async fn half_open_admits_a_single_probe_and_cancelled_probes_release_it() {
    let b = CircuitBreaker::new(1, ms(10));
    let outcome = Rc::new(Cell::new(Err(TErr::Code(1))));
    let _ = breaker_op(&b, outcome.clone(), 0).run().await;
    sleep(ms(10)).await;
    outcome.set(Ok(1));

    // Two concurrent calls while half-open: only one probe goes through.
    let (r1, r2) = breaker_op(&b, outcome.clone(), 50)
        .join(breaker_op(&b, outcome.clone(), 50))
        .run()
        .await
        .unwrap();
    assert_eq!((r1, r2), (Ok(1), Err(TErr::Open)));

    // Re-open, then cancel the probe: the breaker must not stay wedged.
    outcome.set(Err(TErr::Code(1)));
    let _ = breaker_op(&b, outcome.clone(), 0).run().await;
    sleep(ms(10)).await;
    let mut probe = breaker_op(&b, outcome.clone(), 1000).timeout(ms(5));
    assert_eq!(probe.run().await, Err(TErr::Timeout));
    assert_eq!(b.state(), BreakerState::HalfOpen);
    outcome.set(Ok(2));
    assert_eq!(breaker_op(&b, outcome.clone(), 0).run().await, Ok(2));
}

// ----- repeat, hedge, flat products --------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn dropping_a_repeat_mid_delay_stops_it() {
    let (op, calls) = scripted(vec![Leaf {
        delay_ms: 1,
        result: Ok(1),
    }]);
    let mut op = op.repeat(schedule::spaced(ms(100)));
    assert!(tokio::time::timeout(ms(250), op.run()).await.is_err());
    assert_eq!(calls.get(), 3); // runs at t=0, 101, 202
    sleep(ms(1000)).await;
    assert_eq!(calls.get(), 3);
}

// ----- Send -----------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn composed_ops_can_be_spawned() {
    let calls = Arc::new(AtomicUsize::new(0));
    let c = calls.clone();
    let breaker = CircuitBreaker::new(5, Duration::from_secs(1));
    let handle = tokio::spawn(async move {
        let mut op = from_fn(move || {
            let c = c.clone();
            async move {
                if c.fetch_add(1, Ordering::SeqCst) < 2 {
                    Err(TErr::Code(0))
                } else {
                    Ok(1)
                }
            }
        })
        .timeout(ms(50))
        .with_breaker(breaker)
        .retry(schedule::exponential(ms(1)).full_jitter().take(5))
        .zip(succeed(2))
        .race(never())
        .and_then(async |(a, b)| Ok(a + b));
        op.run().await
    });
    assert_eq!(handle.await.unwrap(), Ok(3));
    assert_eq!(calls.load(Ordering::SeqCst), 3);
}
