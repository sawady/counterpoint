//! Laws of the circuit breaker's cool-down schedule, checked on tokio's virtual clock against an
//! independent model of the state machine.

mod common;

use std::cell::Cell;
use std::rc::Rc;

use common::*;
use counterpoint::breaker::Verdict;
use counterpoint::prelude::*;
use counterpoint::schedule::Step;
use proptest::prelude::*;
use tokio::time::sleep;

/// What a call of the trace does once admitted.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Kind {
    Ok,
    /// Fails with `COUNTED`.
    Fail,
    /// Fails with `IGNORED`, which the classifying breaker does not count.
    Ignored,
    /// Fails with `BENIGN`, which a three-way classification may count as a success.
    Benign,
}

const COUNTED: TErr = TErr::Code(1);
const IGNORED: TErr = TErr::Code(2);
const BENIGN: TErr = TErr::Code(3);

/// One call of the trace: wait `gap_ms`, then make a call that ends at once.
#[derive(Debug, Clone, Copy)]
struct Call {
    gap_ms: u64,
    kind: Kind,
}

impl Call {
    fn outcome(self) -> Out {
        match self.kind {
            Kind::Ok => Ok(1),
            Kind::Fail => Err(COUNTED),
            Kind::Ignored => Err(IGNORED),
            Kind::Benign => Err(BENIGN),
        }
    }
}

fn calls() -> impl Strategy<Value = Vec<Call>> {
    let kind = prop_oneof![
        4 => Just(Kind::Ok),
        4 => Just(Kind::Fail),
        2 => Just(Kind::Ignored),
        2 => Just(Kind::Benign),
    ];
    prop::collection::vec(
        (0u64..120, kind).prop_map(|(gap_ms, kind)| Call { gap_ms, kind }),
        1..30,
    )
}

/// `(outcome of the call, state right after it)` for every call of the trace.
type Observed = Vec<(Out, BreakerState)>;

/// How the op under test classifies its errors for the breaker.
#[derive(Debug, Clone, Copy)]
enum Filter {
    /// Plain `with_breaker`: every error counts.
    None,
    /// `.when(|_| true)`.
    CountAll,
    /// `.when(|_| false)`.
    CountNone,
    /// `.when(|e| *e != IGNORED)`.
    SkipIgnored,
    /// `.when(p)` with a `bool` predicate given for `COUNTED`, `IGNORED` and `BENIGN`.
    Bool([bool; 3]),
    /// `.when(v)` with a `Verdict` given for `COUNTED`, `IGNORED` and `BENIGN`.
    Classify([Verdict; 3]),
}

fn verdict() -> impl Strategy<Value = Verdict> {
    prop_oneof![
        Just(Verdict::Failure),
        Just(Verdict::Success),
        Just(Verdict::Neutral)
    ]
}

/// Position of an error of the trace in a `[_; 3]` classification.
fn slot(e: &TErr) -> usize {
    match e {
        TErr::Code(c @ 1..=3) => usize::from(*c) - 1,
        other => panic!("the breaker asked about {other:?}"),
    }
}

fn replay<S: ScheduleFor<()>>(b: CircuitBreaker<S>, trace: &[Call], filter: Filter) -> Observed {
    let outcome: Rc<Cell<Out>> = Rc::new(Cell::new(Ok(0)));
    let o = outcome.clone();
    let op = from_fn(move || {
        let o = o.clone();
        async move { o.get() }
    });
    let handle = b.clone();
    on_virtual_clock(async move {
        let guarded = op.with_breaker(handle);
        match filter {
            Filter::None => drive(guarded, trace, &outcome, &b).await,
            Filter::CountAll => drive(guarded.when(|_: &TErr| true), trace, &outcome, &b).await,
            Filter::CountNone => drive(guarded.when(|_: &TErr| false), trace, &outcome, &b).await,
            Filter::SkipIgnored => {
                drive(guarded.when(|e: &TErr| *e != IGNORED), trace, &outcome, &b).await
            }
            Filter::Bool(p) => {
                drive(
                    guarded.when(move |e: &TErr| p[slot(e)]),
                    trace,
                    &outcome,
                    &b,
                )
                .await
            }
            Filter::Classify(v) => {
                drive(
                    guarded.when(move |e: &TErr| v[slot(e)]),
                    trace,
                    &outcome,
                    &b,
                )
                .await
            }
        }
    })
    .0
}

async fn drive<O: Op<Ok = i32, Err = TErr>, S>(
    mut op: O,
    trace: &[Call],
    outcome: &Cell<Out>,
    b: &CircuitBreaker<S>,
) -> Observed {
    let mut out = Vec::new();
    for call in trace {
        sleep(ms(call.gap_ms)).await;
        outcome.set(call.outcome());
        let r = op.run().await;
        out.push((r, b.state()));
    }
    out
}

/// Every error is a failure.
fn plain(kind: Kind) -> Verdict {
    if kind == Kind::Ok {
        Verdict::Success
    } else {
        Verdict::Failure
    }
}

/// `IGNORED` is neutral, every other error a failure.
fn skipping_ignored(kind: Kind) -> Verdict {
    if kind == Kind::Ignored {
        Verdict::Neutral
    } else {
        plain(kind)
    }
}

/// The breaker as a pure state machine over virtual milliseconds. Cool-downs come from a list;
/// past its end the schedule has stopped. `verdict` says what each admitted call means: a
/// failure counts (and reopens from half-open), a success resets the count (and closes from
/// half-open), and a neutral call leaves the state as it was (a probe that ends with one gives
/// its slot back).
fn model(
    threshold: u32,
    delays: &[u64],
    trace: &[Call],
    verdict: impl Fn(Kind) -> Verdict,
) -> Observed {
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
            _ => {
                match (state, verdict(call.kind)) {
                    (_, Verdict::Neutral) => {}
                    (M::Closed(_), Verdict::Success) => state = M::Closed(0),
                    (M::Closed(failures), Verdict::Failure) => {
                        state = if failures + 1 >= threshold {
                            trip(now)
                        } else {
                            M::Closed(failures + 1)
                        };
                    }
                    (M::HalfOpen, Verdict::Success) => {
                        state = M::Closed(0);
                        trips = 0;
                    }
                    (M::HalfOpen, Verdict::Failure) => state = trip(now),
                    (M::Open(_), _) => unreachable!(),
                }
                call.outcome()
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
        prop_assert_eq!(replay(b, &trace, Filter::None), model(threshold, &delays, &trace, plain));
    }

    #[test]
    fn a_spaced_schedule_is_the_fixed_cooldown(threshold in 1u32..4, cooldown in 0u64..150, trace in calls()) {
        prop_assert_eq!(
            replay(CircuitBreaker::with_schedule(threshold, schedule::spaced(ms(cooldown))), &trace, Filter::None),
            replay(CircuitBreaker::new(threshold, ms(cooldown)), &trace, Filter::None),
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

    // ----- error classification ---------------------------------------------------------

    #[test]
    fn a_classifying_breaker_matches_its_model(
        threshold in 1u32..4,
        delays in prop::collection::vec(0u64..150, 0..6),
        trace in calls(),
    ) {
        let b = CircuitBreaker::with_schedule(threshold, from_list(delays.clone()));
        prop_assert_eq!(replay(b, &trace, Filter::SkipIgnored), model(threshold, &delays, &trace, skipping_ignored));
    }

    #[test]
    fn counting_every_error_is_no_classification(threshold in 1u32..4, cooldown in 0u64..150, trace in calls()) {
        prop_assert_eq!(
            replay(CircuitBreaker::new(threshold, ms(cooldown)), &trace, Filter::CountAll),
            replay(CircuitBreaker::new(threshold, ms(cooldown)), &trace, Filter::None),
        );
    }

    #[test]
    fn counting_no_error_never_opens(threshold in 1u32..4, cooldown in 0u64..150, trace in calls()) {
        let expected: Observed = trace.iter().map(|c| (c.outcome(), BreakerState::Closed)).collect();
        prop_assert_eq!(replay(CircuitBreaker::new(threshold, ms(cooldown)), &trace, Filter::CountNone), expected);
    }

    #[test]
    fn ignored_errors_are_invisible_to_the_breaker(
        threshold in 1u32..4,
        delays in prop::collection::vec(0u64..150, 0..6),
        trace in calls(),
    ) {
        // Dropping the ignored calls from the trace (keeping the time they took) leaves the
        // outcome of every other call, and the state after it, unchanged.
        let b = CircuitBreaker::with_schedule(threshold, from_list(delays.clone()));
        let full = replay(b, &trace, Filter::SkipIgnored);
        let mut kept = Vec::new();
        let mut gap = 0;
        for c in &trace {
            gap += c.gap_ms;
            if c.kind != Kind::Ignored {
                kept.push(Call { gap_ms: gap, kind: c.kind });
                gap = 0;
            }
        }
        let b = CircuitBreaker::with_schedule(threshold, from_list(delays.clone()));
        let without = replay(b, &kept, Filter::SkipIgnored);
        let full_kept: Vec<_> = full
            .into_iter()
            .zip(&trace)
            .filter(|(_, c)| c.kind != Kind::Ignored)
            .map(|(o, _)| o)
            .collect();
        prop_assert_eq!(full_kept, without);
    }

    // ----- three-way classification -------------------------------------------------------

    #[test]
    fn a_three_way_classifying_breaker_matches_its_model(
        threshold in 1u32..4,
        delays in prop::collection::vec(0u64..150, 0..6),
        trace in calls(),
        verdicts in [verdict(), verdict(), verdict()],
    ) {
        let b = CircuitBreaker::with_schedule(threshold, from_list(delays.clone()));
        let classify = |k: Kind| match k {
            Kind::Ok => Verdict::Success,
            Kind::Fail => verdicts[0],
            Kind::Ignored => verdicts[1],
            Kind::Benign => verdicts[2],
        };
        prop_assert_eq!(
            replay(b, &trace, Filter::Classify(verdicts)),
            model(threshold, &delays, &trace, classify),
        );
    }

    #[test]
    fn an_error_counted_as_a_success_is_a_success_to_the_breaker(
        threshold in 1u32..4,
        delays in prop::collection::vec(0u64..150, 0..6),
        trace in calls(),
    ) {
        // Replacing the calls that end with `BENIGN` by successes leaves every state, and every
        // other outcome, unchanged; the replaced calls themselves still return their error.
        let verdicts = [Verdict::Failure, Verdict::Neutral, Verdict::Success];
        let b = CircuitBreaker::with_schedule(threshold, from_list(delays.clone()));
        let full = replay(b, &trace, Filter::Classify(verdicts));
        let healed: Vec<_> = trace
            .iter()
            .map(|c| Call { kind: if c.kind == Kind::Benign { Kind::Ok } else { c.kind }, ..*c })
            .collect();
        let b = CircuitBreaker::with_schedule(threshold, from_list(delays.clone()));
        let expected: Observed = replay(b, &healed, Filter::Classify(verdicts))
            .into_iter()
            .zip(&trace)
            .map(|((r, s), c)| match r {
                Ok(_) if c.kind == Kind::Benign => (Err(BENIGN), s),
                r => (r, s),
            })
            .collect();
        prop_assert_eq!(full, expected);
    }

    #[test]
    fn a_bool_predicate_is_its_verdict(
        threshold in 1u32..4,
        delays in prop::collection::vec(0u64..150, 0..6),
        trace in calls(),
        p in [any::<bool>(), any::<bool>(), any::<bool>()],
    ) {
        let b = CircuitBreaker::with_schedule(threshold, from_list(delays.clone()));
        let with_bool = replay(b, &trace, Filter::Bool(p));
        let b = CircuitBreaker::with_schedule(threshold, from_list(delays.clone()));
        prop_assert_eq!(with_bool, replay(b, &trace, Filter::Classify(p.map(Verdict::from))));
    }

    #[test]
    fn a_half_open_probe_follows_its_verdict(
        threshold in 1u32..4,
        cooldown in 1u64..150,
        probe in prop_oneof![Just(Kind::Fail), Just(Kind::Ignored), Just(Kind::Benign)],
        verdicts in [verdict(), verdict(), verdict()],
    ) {
        // Trip the breaker with counted failures, wait for half-open, then probe: a failure
        // reopens, a success closes, and a neutral probe leaves it half-open with the slot free
        // (the next call is admitted as the probe).
        let mut trace: Vec<_> = (0..threshold).map(|_| Call { gap_ms: 0, kind: Kind::Fail }).collect();
        trace.push(Call { gap_ms: cooldown, kind: probe });
        trace.push(Call { gap_ms: 0, kind: Kind::Ok });
        let verdicts = [Verdict::Failure, verdicts[1], verdicts[2]];
        let b = CircuitBreaker::new(threshold, ms(cooldown));
        let observed = replay(b, &trace, Filter::Classify(verdicts));
        let (probed, after_probe) = observed[threshold as usize];
        prop_assert_eq!(probed, probe_outcome(probe));
        let v = verdicts[slot(&probe_outcome(probe).unwrap_err())];
        let (expected_state, next) = match v {
            Verdict::Failure => (BreakerState::Open, (Err(TErr::Open), BreakerState::Open)),
            Verdict::Success => (BreakerState::Closed, (Ok(1), BreakerState::Closed)),
            Verdict::Neutral => (BreakerState::HalfOpen, (Ok(1), BreakerState::Closed)),
        };
        prop_assert_eq!(after_probe, expected_state);
        prop_assert_eq!(observed[threshold as usize + 1], next);
    }
}

fn probe_outcome(kind: Kind) -> Out {
    Call { gap_ms: 0, kind }.outcome()
}
