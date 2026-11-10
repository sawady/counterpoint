//! Laws of the circuit breaker's cool-down schedule, checked on tokio's virtual clock against an
//! independent model of the state machine.

mod common;

use std::cell::Cell;
use std::rc::Rc;

use common::*;
use counterpoint::prelude::*;
use counterpoint::schedule::Step;
use proptest::prelude::*;
use tokio::time::sleep;

/// One call of the trace: wait `gap_ms`, then make a call that fails or succeeds at once.
#[derive(Debug, Clone, Copy)]
struct Call {
    gap_ms: u64,
    ok: bool,
}

fn calls() -> impl Strategy<Value = Vec<Call>> {
    prop::collection::vec(
        (0u64..120, prop::bool::weighted(0.4)).prop_map(|(gap_ms, ok)| Call { gap_ms, ok }),
        1..30,
    )
}

/// `(outcome of the call, state right after it)` for every call of the trace.
type Observed = Vec<(Out, BreakerState)>;

fn replay<S: ScheduleFor<()>>(b: CircuitBreaker<S>, trace: &[Call]) -> Observed {
    on_virtual_clock(async {
        let outcome: Rc<Cell<Out>> = Rc::new(Cell::new(Ok(0)));
        let o = outcome.clone();
        let mut op = from_fn(move || {
            let o = o.clone();
            async move { o.get() }
        })
        .with_breaker(b.clone());
        let mut out = Vec::new();
        for call in trace {
            sleep(ms(call.gap_ms)).await;
            outcome.set(if call.ok { Ok(1) } else { Err(TErr::Code(1)) });
            let r = op.run().await;
            out.push((r, b.state()));
        }
        out
    })
    .0
}

/// The breaker as a pure state machine over virtual milliseconds. Cool-downs come from a list;
/// past its end the schedule has stopped.
fn model(threshold: u32, delays: &[u64], trace: &[Call]) -> Observed {
    #[derive(Clone, Copy)]
    enum M {
        Closed(u32),
        Open(Option<u64>),
        HalfOpen,
    }
    let mut state = M::Closed(0);
    let mut trips = 0usize;
    let mut now = 0u64;
    let mut out = Vec::new();
    let view = |s: M, now: u64| match s {
        M::Closed(_) => BreakerState::Closed,
        M::Open(Some(u)) if now >= u => BreakerState::HalfOpen,
        M::Open(_) => BreakerState::Open,
        M::HalfOpen => BreakerState::HalfOpen,
    };
    for call in trace {
        now += call.gap_ms;
        if let M::Open(Some(u)) = state
            && now >= u
        {
            state = M::HalfOpen;
        }
        let mut trip = |at: u64| {
            let open = M::Open(delays.get(trips).map(|d| at + d));
            trips += 1;
            open
        };
        let result = match state {
            M::Open(_) => Err(TErr::Open),
            M::Closed(failures) => {
                if call.ok {
                    state = M::Closed(0);
                    Ok(1)
                } else {
                    state = if failures + 1 >= threshold {
                        trip(now)
                    } else {
                        M::Closed(failures + 1)
                    };
                    Err(TErr::Code(1))
                }
            }
            M::HalfOpen => {
                if call.ok {
                    state = M::Closed(0);
                    trips = 0;
                    Ok(1)
                } else {
                    state = trip(now);
                    Err(TErr::Code(1))
                }
            }
        };
        out.push((result, view(state, now)));
    }
    out
}

fn from_list(delays: Vec<u64>) -> impl ScheduleFor<()> + Clone {
    schedule::from_fn(move |s: Step| delays.get(s.attempt as usize).map(|d| ms(*d)))
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    #[test]
    fn breaker_matches_its_model(
        threshold in 1u32..4,
        delays in prop::collection::vec(0u64..150, 0..6),
        trace in calls(),
    ) {
        let b = CircuitBreaker::with_schedule(threshold, from_list(delays.clone()));
        prop_assert_eq!(replay(b, &trace), model(threshold, &delays, &trace));
    }

    #[test]
    fn a_spaced_schedule_is_the_fixed_cooldown(threshold in 1u32..4, cooldown in 0u64..150, trace in calls()) {
        prop_assert_eq!(
            replay(CircuitBreaker::with_schedule(threshold, schedule::spaced(ms(cooldown))), &trace),
            replay(CircuitBreaker::new(threshold, ms(cooldown)), &trace),
        );
    }

    #[test]
    fn openings_last_the_schedule_delays(base in 1u64..50, n in 1u32..6) {
        let delays: Vec<_> = (schedule::exponential(ms(base)) & schedule::recurs(n)).delays().collect();
        let (openings, _) = on_virtual_clock(async {
            let b = CircuitBreaker::with_schedule(1, schedule::exponential(ms(base)) & schedule::recurs(n));
            let mut op = from_fn(|| async { Err::<i32, TErr>(TErr::Code(1)) }).with_breaker(b.clone());
            let mut openings = Vec::new();
            for _ in 0..n {
                assert_eq!(op.run().await, Err(TErr::Code(1)));
                let t0 = tokio::time::Instant::now();
                while b.state() == BreakerState::Open {
                    sleep(ms(1)).await;
                }
                openings.push(t0.elapsed());
            }
            openings
        });
        prop_assert_eq!(openings, delays);
    }

    #[test]
    fn a_stopped_schedule_stays_open(wait in 0u64..1_000_000) {
        let (r, _) = on_virtual_clock(async {
            let b = CircuitBreaker::with_schedule(1, schedule::stop());
            let mut op = from_fn(|| async { Err::<i32, TErr>(TErr::Code(1)) }).with_breaker(b.clone());
            assert_eq!(op.run().await, Err(TErr::Code(1)));
            sleep(ms(wait)).await;
            (b.state(), op.run().await)
        });
        prop_assert_eq!(r, (BreakerState::Open, Err(TErr::Open)));
    }
}
