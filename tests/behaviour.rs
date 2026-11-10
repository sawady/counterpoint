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

/// Peak concurrency and running time of `all(n ops of 10ms).limit(limit)`.
async fn peak_of_bounded_all(n: usize, limit: usize) -> (usize, Duration) {
    let running = Rc::new(Cell::new(0usize));
    let peak = Rc::new(Cell::new(0usize));
    let ops = (0..n).map(|i| {
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
    let mut op = all(ops).limit(limit);
    let t0 = Instant::now();
    assert_eq!(op.run().await, Ok((0..n).collect::<Vec<_>>()));
    (peak.get(), t0.elapsed())
}

#[tokio::test(start_paused = true)]
async fn all_respects_the_concurrency_limit() {
    // ceil(n / limit) waves of 10ms. The second case goes through `FuturesUnordered`.
    assert_eq!(peak_of_bounded_all(10, 3).await, (3, ms(40)));
    assert_eq!(peak_of_bounded_all(100, 40).await, (40, ms(30)));
}

/// `all` of `n` slow ops zipped with a fast failure: every slow op must be cancelled.
async fn fail_fast_cancels(n: usize, limit: usize) {
    let dropped: Vec<_> = (0..n).map(|_| Rc::new(Cell::new(false))).collect();
    let ops = (0..n).map(|i| cancellable(100, Ok(i as i32), dropped[i].clone()));
    let failing = cancellable(5, Err(TErr::Code(1)), Rc::default());
    let mut op = all(ops).limit(limit).zip(failing);
    assert_eq!(op.run().await, Err(TErr::Code(1)));
    let started = n.min(limit);
    assert!(dropped[..started].iter().all(|d| d.get()));
}

#[tokio::test(start_paused = true)]
async fn all_fail_fast_cancels_the_rest() {
    fail_fast_cancels(4, usize::MAX).await; // one pinned slice
    fail_fast_cancels(40, usize::MAX).await; // `FuturesUnordered`
    fail_fast_cancels(10, 3).await; // pool of slots
    fail_fast_cancels(80, 40).await; // `FuturesUnordered` with a limit
}

#[tokio::test(start_paused = true)]
async fn an_error_inside_all_cancels_its_siblings() {
    for n in [4usize, 40] {
        let dropped: Vec<_> = (0..n).map(|_| Rc::new(Cell::new(false))).collect();
        let ops = (0..n).map(|i| {
            let out = if i == n - 1 {
                Err(TErr::Code(2))
            } else {
                Ok(i as i32)
            };
            let d = if i == n - 1 { 5 } else { 100 };
            cancellable(d, out, dropped[i].clone())
        });
        let mut op = all(ops);
        let t0 = Instant::now();
        assert_eq!(op.run().await, Err(TErr::Code(2)));
        assert_eq!(t0.elapsed(), ms(5));
        assert!(dropped[..n - 1].iter().all(|d| d.get()));
    }
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

// ----- breaker with a cool-down schedule --------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn breaker_cooldown_follows_the_schedule_and_resets_on_close() {
    let b = CircuitBreaker::with_schedule(1, schedule::exponential(ms(100)).take(2));
    let outcome = Rc::new(Cell::new(Err(TErr::Code(1))));
    let mut op = from_fn({
        let outcome = outcome.clone();
        move || {
            let outcome = outcome.clone();
            async move { outcome.get() }
        }
    })
    .with_breaker(b.clone());

    // First opening lasts 100ms, the second 200ms.
    assert_eq!(op.run().await, Err(TErr::Code(1)));
    assert_eq!(b.state(), BreakerState::Open);
    sleep(ms(99)).await;
    assert_eq!(b.state(), BreakerState::Open);
    sleep(ms(1)).await;
    assert_eq!(b.state(), BreakerState::HalfOpen);
    assert_eq!(op.run().await, Err(TErr::Code(1)), "failed probe");
    sleep(ms(199)).await;
    assert_eq!(b.state(), BreakerState::Open);
    sleep(ms(1)).await;
    assert_eq!(b.state(), BreakerState::HalfOpen);

    // A successful probe closes it and resets the schedule.
    outcome.set(Ok(1));
    assert_eq!(op.run().await, Ok(1));
    assert_eq!(b.state(), BreakerState::Closed);
    outcome.set(Err(TErr::Code(2)));
    assert_eq!(op.run().await, Err(TErr::Code(2)));
    sleep(ms(100)).await;
    assert_eq!(b.state(), BreakerState::HalfOpen);

    // Third consecutive opening: the schedule has stopped, so it stays open for good.
    assert_eq!(op.run().await, Err(TErr::Code(2)), "failed probe");
    sleep(ms(200)).await;
    assert_eq!(op.run().await, Err(TErr::Code(2)), "failed probe");
    sleep(ms(10_000_000)).await;
    assert_eq!(b.state(), BreakerState::Open);
    assert_eq!(op.run().await, Err(TErr::Open));
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

#[tokio::test(start_paused = true)]
async fn hedge_cancels_the_losers_and_launches_no_more() {
    let dropped = Rc::new(Cell::new(0u32));
    let launched = Rc::new(Cell::new(0u32));
    let (d, l) = (dropped.clone(), launched.clone());
    let mut op = from_fn(move || {
        let (d, l) = (d.clone(), l.clone());
        async move {
            let n = l.get();
            l.set(n + 1);
            let delay = if n == 0 { 1000 } else { 25 };
            struct CountDrop(Rc<Cell<u32>>);
            impl Drop for CountDrop {
                fn drop(&mut self) {
                    self.0.set(self.0.get() + 1);
                }
            }
            let guard = CountDrop(d);
            sleep(ms(delay)).await;
            std::mem::forget(guard);
            Ok::<_, TErr>(n)
        }
    })
    .hedge(schedule::spaced(ms(10)).take(5));
    let t0 = Instant::now();
    // Launches at 0, 10, 20, 30; the hedge from t=10 finishes at 35 and wins.
    assert_eq!(op.run().await, Ok(1));
    assert_eq!(t0.elapsed(), ms(35));
    assert_eq!(launched.get(), 4);
    assert_eq!(dropped.get(), 3);
    sleep(ms(1000)).await;
    assert_eq!(launched.get(), 4, "no hedge starts after the winner");
}

#[tokio::test(start_paused = true)]
async fn dropping_a_hedge_cancels_every_attempt() {
    let dropped = Rc::new(Cell::new(false));
    let d = dropped.clone();
    let mut op = from_fn(move || {
        let d = d.clone();
        async move {
            let guard = DropFlag(d);
            sleep(ms(1000)).await;
            std::mem::forget(guard);
            Ok::<_, TErr>(1)
        }
    })
    .hedge(schedule::spaced(ms(10)).take(2));
    assert!(tokio::time::timeout(ms(25), op.run()).await.is_err());
    assert!(dropped.get());
}

#[tokio::test(start_paused = true)]
async fn hedge_skips_the_timer_when_nothing_is_running() {
    let (op, calls) = scripted(vec![
        Leaf {
            delay_ms: 1,
            result: Err(TErr::Code(1)),
        },
        Leaf {
            delay_ms: 1,
            result: Ok(9),
        },
    ]);
    let mut op = op.hedge(schedule::spaced(ms(1000)).take(1));
    let t0 = Instant::now();
    assert_eq!(op.run().await, Ok(9));
    assert_eq!(t0.elapsed(), ms(2));
    assert_eq!(calls.get(), 2);
}

#[tokio::test(start_paused = true)]
async fn zip3_fails_fast_and_cancels_the_siblings() {
    let (b, c) = (Rc::new(Cell::new(false)), Rc::new(Cell::new(false)));
    let mut op = zip3(
        cancellable(5, Err(TErr::Code(4)), Rc::default()),
        cancellable(100, Ok(1), b.clone()),
        cancellable(100, Ok(2), c.clone()),
    );
    let t0 = Instant::now();
    assert_eq!(op.run().await, Err(TErr::Code(4)));
    assert_eq!(t0.elapsed(), ms(5));
    assert!(b.get() && c.get());
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_new_combinators_can_be_spawned() {
    let breaker = CircuitBreaker::with_schedule(5, schedule::exponential(ms(1)).full_jitter());
    let handle = tokio::spawn(async move {
        let leaf = from_fn(|| async { Ok::<i32, TErr>(1) });
        let mut op = zip3(leaf.clone(), leaf.clone(), leaf)
            .map(|(a, b, c)| a + b + c)
            .hedge(schedule::spaced(ms(50)).take(1))
            .with_breaker(breaker)
            .repeat(schedule::recur_until(|v: &i32| *v == 3))
            .retry(schedule::recurs(1) & schedule::recur_while(|e: &TErr| *e != TErr::Open));
        op.run().await
    });
    assert_eq!(handle.await.unwrap(), Ok(3));
}
