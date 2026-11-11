//! Algebraic laws of the `Op` combinators, checked with proptest on tokio's virtual clock.
//!
//! `lhs ≡ rhs` is checked by running each side on a fresh paused-clock runtime and comparing
//! the result *and* the elapsed virtual time.

mod common;

use common::*;
use counterpoint::prelude::*;
use proptest::prelude::*;

fn same<A: Op, B: Op<Ok = A::Ok, Err = A::Err>>(a: A, b: B) -> Result<(), TestCaseError>
where
    A::Ok: PartialEq + std::fmt::Debug,
    A::Err: PartialEq + std::fmt::Debug,
{
    prop_assert_eq!(observe(a), observe(b));
    Ok(())
}

fn unit() -> impl Op<Ok = (), Err = TErr> {
    succeed(())
}

/// Model of `all`: success iff every leaf succeeds, taking the max duration; otherwise the
/// earliest failure wins and cancels the rest.
fn all_model(leaves: &[Leaf]) -> Result<(), TestCaseError> {
    let (r, t) = observe(all(leaves.iter().map(|l| l.op())));
    let first_failure = leaves.iter().filter(|l| !l.ok()).map(|l| l.delay_ms).min();
    match first_failure {
        None => {
            prop_assert_eq!(
                r,
                Ok(leaves.iter().map(|l| l.result.unwrap()).collect::<Vec<_>>())
            );
            prop_assert_eq!(t, ms(leaves.iter().map(|l| l.delay_ms).max().unwrap_or(0)));
        }
        Some(d) => {
            prop_assert_eq!(t, ms(d));
            let at_d: Vec<_> = leaves
                .iter()
                .filter(|l| !l.ok() && l.delay_ms == d)
                .collect();
            if at_d.len() == 1 {
                prop_assert_eq!(r, Err(at_d[0].result.unwrap_err()));
            }
        }
    }
    Ok(())
}

/// Model of `race_all` over a non-empty collection: the fastest leaf wins, at its delay. With
/// `ties_to_first`, an exact tie goes to the earliest leaf; otherwise the result is one of the
/// tied leaves.
fn race_all_model(leaves: &[Leaf], ties_to_first: bool) -> Result<(), TestCaseError> {
    let (r, t) = observe(race_all(leaves.iter().map(|l| l.op())));
    let d = leaves.iter().map(|l| l.delay_ms).min().expect("non-empty");
    prop_assert_eq!(t, ms(d));
    let mut tied = leaves.iter().filter(|l| l.delay_ms == d).map(|l| l.result);
    if ties_to_first {
        prop_assert_eq!(Some(r), tied.next());
    } else {
        prop_assert!(tied.any(|x| x == r));
    }
    Ok(())
}

/// Bounded `all` of successful leaves: values in input order, and the makespan of greedy list
/// scheduling in input order on `limit` slots.
fn list_scheduling(leaves: Vec<Leaf>, limit: usize) -> Result<(), TestCaseError> {
    let leaves: Vec<_> = leaves
        .into_iter()
        .map(|l| Leaf {
            result: Ok(l.delay_ms as i32),
            ..l
        })
        .collect();
    let (r, t) = observe(all(leaves.iter().map(|l| l.op())).limit(limit));
    let mut slots = vec![0u64; limit];
    for l in &leaves {
        let s = slots.iter_mut().min().unwrap();
        *s += l.delay_ms;
    }
    prop_assert_eq!(
        r,
        Ok(leaves.iter().map(|l| l.delay_ms as i32).collect::<Vec<_>>())
    );
    prop_assert_eq!(t, ms(slots.into_iter().max().unwrap_or(0)));
    Ok(())
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    // ----- functor & monad --------------------------------------------------------------

    #[test]
    fn map_identity(a in leaf()) {
        same(a.op().map(|x| x), a.op())?;
    }

    #[test]
    fn map_composition(a in leaf(), k in -5i32..5) {
        let f = move |x: i32| x.wrapping_mul(3);
        let g = move |x: i32| x.wrapping_add(k);
        same(a.op().map(f).map(g), a.op().map(move |x| g(f(x))))?;
    }

    #[test]
    fn monad_left_identity(x in -50i32..50, f in arrow()) {
        same(
            succeed::<i32, TErr>(x).and_then(move |v| f.call(v)),
            from_fn(move || f.call(x)),
        )?;
    }

    #[test]
    fn monad_right_identity(m in leaf()) {
        same(m.op().and_then(|v| async move { Ok(v) }), m.op())?;
    }

    #[test]
    fn monad_associativity(m in leaf(), f in arrow(), g in arrow()) {
        same(
            m.op().and_then(move |v| f.call(v)).and_then(move |v| g.call(v)),
            m.op().and_then(move |v| async move { g.call(f.call(v).await?).await }),
        )?;
    }

    #[test]
    fn sequencing_adds_durations(m in leaf(), f in arrow()) {
        let (r, t) = observe(m.op().and_then(move |v| f.call(v)));
        if m.ok() {
            prop_assert_eq!(t, ms(m.delay_ms + f.delay_ms));
        } else {
            prop_assert_eq!(r, m.result);
            prop_assert_eq!(t, ms(m.delay_ms));
        }
    }

    // ----- parallel product -------------------------------------------------------------

    #[test]
    fn zip_associativity(a in leaf(), b in leaf(), c in leaf()) {
        same(
            a.op().zip(b.op()).zip(c.op()).map(|((x, y), z)| (x, y, z)),
            a.op().zip(b.op().zip(c.op())).map(|(x, (y, z))| (x, y, z)),
        )?;
    }

    #[test]
    fn zip_unit(a in leaf()) {
        same(a.op().zip(unit()).map(|(x, ())| x), a.op())?;
        same(unit().zip(a.op()).map(|((), x)| x), a.op())?;
    }

    #[test]
    fn zip_commutativity(a in leaf(), b in leaf()) {
        // If both fail at the very same instant the left error wins: a tie.
        prop_assume!(a.ok() || b.ok() || a.delay_ms != b.delay_ms);
        same(a.op().zip(b.op()), b.op().zip(a.op()).map(|(y, x)| (x, y)))?;
    }

    #[test]
    fn zip_takes_the_max_and_fails_fast(a in leaf(), b in leaf()) {
        let (r, t) = observe(a.op().zip(b.op()));
        match (a.result, b.result) {
            (Ok(x), Ok(y)) => {
                prop_assert_eq!(r, Ok((x, y)));
                prop_assert_eq!(t, ms(a.delay_ms.max(b.delay_ms)));
            }
            (Err(_), Ok(_)) => prop_assert_eq!(t, ms(a.delay_ms)),
            (Ok(_), Err(_)) => prop_assert_eq!(t, ms(b.delay_ms)),
            (Err(_), Err(_)) => prop_assert_eq!(t, ms(a.delay_ms.min(b.delay_ms))),
        }
    }

    #[test]
    fn join_never_fails_and_takes_the_max(a in leaf(), b in leaf()) {
        let (r, t) = observe(a.op().join(b.op()));
        prop_assert_eq!(r, Ok((a.result, b.result)));
        prop_assert_eq!(t, ms(a.delay_ms.max(b.delay_ms)));
    }

    #[test]
    fn all_matches_its_model(leaves in prop::collection::vec(leaf(), 0..7)) {
        all_model(&leaves)?;
    }

    #[test]
    fn all_matches_its_model_past_the_small_path(leaves in prop::collection::vec(leaf(), 10..30)) {
        // Crosses the size at which `all` switches from polling a slice to `FuturesUnordered`.
        all_model(&leaves)?;
    }

    #[test]
    fn all_with_a_limit_of_at_least_its_length_is_all(leaves in prop::collection::vec(leaf(), 0..30), extra in 0usize..3) {
        same(all(leaves.iter().map(|l| l.op())).limit(leaves.len().max(1) + extra), all(leaves.iter().map(|l| l.op())))?;
    }

    #[test]
    fn bounded_all_is_list_scheduling(leaves in prop::collection::vec(leaf(), 0..7), limit in 1usize..4) {
        list_scheduling(leaves, limit)?;
    }

    #[test]
    fn bounded_all_is_list_scheduling_with_wide_limits(leaves in prop::collection::vec(leaf(), 20..40), limit in 12usize..24) {
        // Limits on both sides of the switch to `FuturesUnordered`.
        list_scheduling(leaves, limit)?;
    }

    #[test]
    fn settled_reports_everything(leaves in prop::collection::vec(leaf(), 0..7), limit in 1usize..4) {
        let (r, _) = observe(all(leaves.iter().map(|l| l.op())).limit(limit).settled());
        prop_assert_eq!(r, Ok(leaves.iter().map(|l| l.result).collect::<Vec<_>>()));
    }

    #[test]
    fn settled_reports_everything_and_takes_the_max(leaves in prop::collection::vec(leaf(), 0..30)) {
        let (r, t) = observe(all(leaves.iter().map(|l| l.op())).settled());
        prop_assert_eq!(r, Ok(leaves.iter().map(|l| l.result).collect::<Vec<_>>()));
        prop_assert_eq!(t, ms(leaves.iter().map(|l| l.delay_ms).max().unwrap_or(0)));
    }

    // ----- race -------------------------------------------------------------------------

    #[test]
    fn race_identity(a in leaf()) {
        same(a.op().race(never()), a.op())?;
        same(never().race(a.op()), a.op())?;
    }

    #[test]
    fn race_commutativity_except_ties(a in leaf(), b in leaf()) {
        prop_assume!(a.delay_ms != b.delay_ms);
        same(a.op().race(b.op()), b.op().race(a.op()))?;
    }

    #[test]
    fn race_associativity(a in leaf(), b in leaf(), c in leaf()) {
        same(a.op().race(b.op()).race(c.op()), a.op().race(b.op().race(c.op())))?;
    }

    #[test]
    fn race_takes_the_min(a in leaf(), b in leaf()) {
        let (r, t) = observe(a.op().race(b.op()));
        prop_assert_eq!(t, ms(a.delay_ms.min(b.delay_ms)));
        let winner = if b.delay_ms < a.delay_ms { b } else { a };
        prop_assert_eq!(r, winner.result);
    }

    #[test]
    fn race_all_is_iterated_race(a in leaf(), b in leaf(), c in leaf()) {
        // Ties included: on the pinned-slice path they go to the earliest op, like `race`.
        same(race_all([a.op(), b.op(), c.op()]), a.op().race(b.op()).race(c.op()))?;
    }

    #[test]
    fn race_all_matches_its_model(leaves in prop::collection::vec(leaf(), 1..=16)) {
        race_all_model(&leaves, true)?;
    }

    #[test]
    fn race_all_matches_its_model_past_the_small_path(leaves in prop::collection::vec(leaf(), 10..40)) {
        // Crosses the size at which `race_all` switches from a slice to `FuturesUnordered`.
        race_all_model(&leaves, leaves.len() <= 16)?;
    }

    #[test]
    fn empty_race_all_is_the_identity_of_race(a in leaf()) {
        same(race_all(Vec::<Leaf>::new().into_iter().map(|l| l.op())).race(a.op()), a.op())?;
        same(a.op().race(race_all(Vec::<Leaf>::new().into_iter().map(|l| l.op()))), a.op())?;
    }

    // ----- alternatives & guards ---------------------------------------------------------

    #[test]
    fn or_associativity(a in leaf(), b in leaf(), c in leaf()) {
        same(a.op().or(b.op()).or(c.op()), a.op().or(b.op().or(c.op())))?;
    }

    #[test]
    fn or_left_identity_and_left_zero(e in 0u8..5, x in -50i32..50, b in leaf()) {
        same(fail::<i32, TErr>(TErr::Code(e)).or(b.op()), b.op())?;
        same(succeed::<i32, TErr>(x).or(b.op()), succeed(x))?;
    }

    #[test]
    fn ensure_true_is_identity(a in leaf()) {
        same(a.op().ensure(|_| true, |_| TErr::Code(9)), a.op())?;
    }

    #[test]
    fn ensure_conjunction(a in leaf(), p in -50i32..50, q in -50i32..50) {
        let err = |x: &i32| TErr::Code((x.rem_euclid(5)) as u8);
        same(
            a.op().ensure(move |x| *x >= p, err).ensure(move |x| *x <= q, err),
            a.op().ensure(move |x| *x >= p && *x <= q, err),
        )?;
    }

    // ----- time -------------------------------------------------------------------------

    #[test]
    fn timeout_infinity_is_identity(a in leaf()) {
        same(a.op().timeout(std::time::Duration::MAX), a.op())?;
    }

    #[test]
    fn timeout_longer_than_the_op_is_identity(a in leaf(), extra in 1u64..50) {
        same(a.op().timeout(ms(a.delay_ms + extra)), a.op())?;
    }

    #[test]
    fn timeout_shorter_than_the_op_fails_at_the_deadline(a in leaf(), t in 0u64..60) {
        prop_assume!(t < a.delay_ms);
        prop_assert_eq!(observe(a.op().timeout(ms(t))), (Err(TErr::Timeout), ms(t)));
    }

    #[test]
    fn nested_timeouts_take_the_min(a in leaf(), t1 in 0u64..70, t2 in 0u64..70) {
        prop_assume!(a.delay_ms != t1 && a.delay_ms != t2);
        same(a.op().timeout(ms(t1)).timeout(ms(t2)), a.op().timeout(ms(t1.min(t2))))?;
    }

    // ----- retry ------------------------------------------------------------------------

    #[test]
    fn retry_stop_is_identity(a in leaf()) {
        same(a.op().retry(schedule::stop()), a.op())?;
        same(a.op().retry(schedule::recurs(0)), a.op())?;
    }

    #[test]
    fn retry_never_reruns_a_success(x in -50i32..50, n in 0u32..5) {
        same(succeed::<i32, TErr>(x).retry(schedule::recurs(n)), succeed(x))?;
    }

    #[test]
    fn retry_when_false_is_identity(a in leaf(), n in 0u32..5) {
        same(a.op().retry(schedule::recurs(n)).when(|_: &TErr| false), a.op())?;
    }

    #[test]
    fn retry_runs_at_most_n_plus_one_times(failures in 0u32..8, n in 0u32..8, d in 0u64..20, gap in 0u64..20) {
        let (op, calls) = flaky(failures, d);
        let (r, t) = observe(op.retry(schedule::spaced(ms(gap)) & schedule::recurs(n)));
        let expected_calls = failures.min(n) + 1;
        prop_assert_eq!(calls.get(), expected_calls);
        prop_assert_eq!(r.is_ok(), failures <= n);
        let c = u64::from(expected_calls);
        prop_assert_eq!(t, ms(c * d + (c - 1) * gap));
    }

    #[test]
    fn nested_retries_multiply(m in 0u32..4, n in 0u32..4) {
        let (op, calls) = flaky(u32::MAX, 1);
        let _ = observe(op.retry(schedule::recurs(m)).retry(schedule::recurs(n)));
        prop_assert_eq!(calls.get(), (m + 1) * (n + 1));
    }

    // ----- flat products ----------------------------------------------------------------

    #[test]
    fn zip3_is_nested_zip(a in leaf(), b in leaf(), c in leaf()) {
        same(
            zip3(a.op(), b.op(), c.op()),
            a.op().zip(b.op()).zip(c.op()).map(|((x, y), z)| (x, y, z)),
        )?;
    }

    #[test]
    fn zip4_extends_zip3(a in leaf(), b in leaf(), c in leaf(), d in leaf()) {
        same(
            zip4(a.op(), b.op(), c.op(), d.op()),
            zip3(a.op(), b.op(), c.op()).zip(d.op()).map(|((x, y, z), w)| (x, y, z, w)),
        )?;
    }

    #[test]
    fn zip5_extends_zip4(a in leaf(), b in leaf(), c in leaf(), d in leaf(), e in leaf()) {
        same(
            zip5(a.op(), b.op(), c.op(), d.op(), e.op()),
            zip4(a.op(), b.op(), c.op(), d.op()).zip(e.op()).map(|((x, y, z, w), v)| (x, y, z, w, v)),
        )?;
    }

    #[test]
    fn zip6_extends_zip5(a in leaf(), b in leaf(), c in leaf(), d in leaf(), e in leaf(), f in leaf()) {
        same(
            zip6(a.op(), b.op(), c.op(), d.op(), e.op(), f.op()),
            zip5(a.op(), b.op(), c.op(), d.op(), e.op()).zip(f.op())
                .map(|((x, y, z, w, v), u)| (x, y, z, w, v, u)),
        )?;
    }

    #[test]
    fn zip7_extends_zip6(a in leaf(), b in leaf(), c in leaf(), d in leaf(), e in leaf(), f in leaf(), g in leaf()) {
        same(
            zip7(a.op(), b.op(), c.op(), d.op(), e.op(), f.op(), g.op()),
            zip6(a.op(), b.op(), c.op(), d.op(), e.op(), f.op()).zip(g.op())
                .map(|((x, y, z, w, v, u), t)| (x, y, z, w, v, u, t)),
        )?;
    }

    #[test]
    fn zip8_extends_zip7(a in leaf(), b in leaf(), c in leaf(), d in leaf(), e in leaf(), f in leaf(), g in leaf(), h in leaf()) {
        same(
            zip8(a.op(), b.op(), c.op(), d.op(), e.op(), f.op(), g.op(), h.op()),
            zip7(a.op(), b.op(), c.op(), d.op(), e.op(), f.op(), g.op()).zip(h.op())
                .map(|((x, y, z, w, v, u, t), s)| (x, y, z, w, v, u, t, s)),
        )?;
    }

    #[test]
    fn zip3_takes_the_max_and_fails_fast(a in leaf(), b in leaf(), c in leaf()) {
        let leaves = [a, b, c];
        let (r, t) = observe(zip3(a.op(), b.op(), c.op()));
        let first_failure = leaves.iter().filter(|l| !l.ok()).map(|l| l.delay_ms).min();
        match first_failure {
            None => {
                prop_assert_eq!(r, Ok((a.result.unwrap(), b.result.unwrap(), c.result.unwrap())));
                prop_assert_eq!(t, ms(leaves.iter().map(|l| l.delay_ms).max().unwrap()));
            }
            Some(d) => {
                prop_assert!(r.is_err());
                prop_assert_eq!(t, ms(d));
            }
        }
    }

    // ----- retry with input --------------------------------------------------------------

    #[test]
    fn retry_with_recur_while_is_when(script in prop::collection::vec(leaf(), 1..6), n in 0u32..6, gap in 0u64..20, limit in 0u8..5) {
        let p = move |e: &TErr| matches!(e, TErr::Code(c) if *c < limit);
        let s = schedule::spaced(ms(gap)) & schedule::recurs(n);
        let (lhs, lhs_calls) = scripted(script.clone());
        let (rhs, rhs_calls) = scripted(script);
        let left = observe(lhs.retry(s & schedule::recur_while(p)));
        let right = observe(rhs.retry(s).when(p));
        prop_assert_eq!(left, right);
        prop_assert_eq!(lhs_calls.get(), rhs_calls.get());
    }

    #[test]
    fn retry_waits_what_the_error_says(codes in prop::collection::vec(0u8..5, 1..6), d in 0u64..20, factor in 0u64..15) {
        let script: Vec<_> = codes.iter().map(|c| Leaf { delay_ms: d, result: Err(TErr::Code(*c)) }).collect();
        let n = codes.len();
        let (op, calls) = scripted(script);
        let wait = move |e: &TErr, _: schedule::Step| match e {
            TErr::Code(c) => Some(ms(factor * u64::from(*c))),
            _ => None,
        };
        let (r, t) = observe(op.retry(schedule::from_input(wait).take(n as u32 - 1)));
        prop_assert_eq!(r, Err(TErr::Code(codes[n - 1])));
        prop_assert_eq!(calls.get(), n);
        let waits: u64 = codes[..n - 1].iter().map(|c| factor * u64::from(*c)).sum();
        prop_assert_eq!(t, ms(n as u64 * d + waits));
    }

    // ----- repeat -----------------------------------------------------------------------

    #[test]
    fn repeat_stop_is_identity(a in leaf()) {
        same(a.op().repeat(schedule::stop()), a.op())?;
        same(a.op().repeat(schedule::recurs(0)), a.op())?;
    }

    #[test]
    fn repeat_never_reruns_a_failure(a in leaf(), n in 0u32..5) {
        let failing = Leaf { result: Err(TErr::Code(1)), ..a };
        same(failing.op().repeat(schedule::recurs(n)), failing.op())?;
        same(fail::<i32, TErr>(TErr::Code(2)).repeat(schedule::recurs(n)), fail(TErr::Code(2)))?;
    }

    #[test]
    fn repeat_is_retry_with_outcomes_swapped(script in prop::collection::vec(leaf(), 1..6), n in 0u32..6, gap in 0u64..20) {
        let s = schedule::spaced(ms(gap)) & schedule::recurs(n);
        let (lhs, lhs_calls) = scripted(script.clone());
        let (rhs, rhs_calls) = scripted(script);
        let (l, lt) = observe(lhs.repeat(s));
        let (r, rt) = observe(Flip(rhs).retry(s));
        let flipped = match r {
            Ok(e) => Err(e),
            Err(v) => Ok(v),
        };
        prop_assert_eq!((l, lt), (flipped, rt));
        prop_assert_eq!(lhs_calls.get(), rhs_calls.get());
    }

    #[test]
    fn repeat_runs_n_plus_one_times_and_returns_the_last_value(n in 0u32..8, d in 0u64..20, gap in 0u64..20) {
        let script: Vec<_> = (0..=n as i32).map(|v| Leaf { delay_ms: d, result: Ok(v) }).collect();
        let (op, calls) = scripted(script);
        let (r, t) = observe(op.repeat(schedule::spaced(ms(gap)) & schedule::recurs(n)));
        prop_assert_eq!(r, Ok(n as i32));
        prop_assert_eq!(calls.get(), n as usize + 1);
        let c = u64::from(n) + 1;
        prop_assert_eq!(t, ms(c * d + (c - 1) * gap));
    }

    #[test]
    fn repeat_stops_at_the_first_error(oks in 0usize..5, extra in 0u32..5, code in 0u8..5) {
        let mut script = vec![Leaf { delay_ms: 1, result: Ok(0) }; oks];
        script.push(Leaf { delay_ms: 1, result: Err(TErr::Code(code)) });
        let (op, calls) = scripted(script);
        let (r, _) = observe(op.repeat(schedule::recurs(oks as u32 + extra)));
        prop_assert_eq!(r, Err(TErr::Code(code)));
        prop_assert_eq!(calls.get(), oks + 1);
    }

    #[test]
    fn nested_repeats_multiply(m in 0u32..4, n in 0u32..4) {
        let (op, calls) = scripted(vec![Leaf { delay_ms: 1, result: Ok(1) }]);
        let _ = observe(op.repeat(schedule::recurs(m)).repeat(schedule::recurs(n)));
        prop_assert_eq!(calls.get() as u32, (m + 1) * (n + 1));
    }

    #[test]
    fn repeat_then_adds(a in leaf(), m in 0u32..4, n in 0u32..4) {
        same(
            a.op().repeat(schedule::recurs(m).then(schedule::recurs(n))),
            a.op().repeat(schedule::recurs(m + n)),
        )?;
    }

    #[test]
    fn repeat_until_is_polling(values in prop::collection::vec(0i32..20, 1..8), goal in 0i32..20, d in 0u64..10, gap in 0u64..10) {
        let script: Vec<_> = values.iter().map(|v| Leaf { delay_ms: d, result: Ok(*v) }).collect();
        let (op, calls) = scripted(script);
        let done = move |v: &i32| *v >= goal;
        let limit = values.len() as u32 - 1;
        let s = schedule::spaced(ms(gap)) & schedule::recur_until(done) & schedule::recurs(limit);
        let (r, t) = observe(op.repeat(s));
        let hit = values.iter().position(done);
        let runs = hit.map_or(values.len(), |i| i + 1);
        prop_assert_eq!(r, Ok(values[runs - 1]));
        prop_assert_eq!(calls.get(), runs);
        let c = runs as u64;
        prop_assert_eq!(t, ms(c * d + (c - 1) * gap));
    }

    // ----- hedge ------------------------------------------------------------------------

    #[test]
    fn hedge_stop_is_identity(a in leaf()) {
        same(a.op().hedge(schedule::stop()), a.op())?;
        same(a.op().hedge(schedule::recurs(0)), a.op())?;
    }

    #[test]
    fn hedge_never_reruns_an_immediate_success(x in -50i32..50, d in 1u64..20, n in 0u32..4) {
        same(succeed::<i32, TErr>(x).hedge(schedule::spaced(ms(d)).take(n)), succeed(x))?;
    }

    #[test]
    fn hedge_matches_its_model(
        script in prop::collection::vec(leaf(), 1..6),
        delays in prop::collection::vec(0u64..40, 0..5),
    ) {
        let attempts = script.len().min(delays.len() + 1);
        let ds = delays.clone();
        let policy = schedule::from_fn(move |s: schedule::Step| ds.get(s.attempt as usize).map(|d| ms(*d)));
        let (op, calls) = scripted(script.clone());
        let (r, t) = observe(op.hedge(policy.take(attempts as u32 - 1)));
        let model = hedge_model(&script[..attempts], &delays, |_| true);
        check_hedge_model(r, t, calls.get(), model)?;
    }

    #[test]
    fn hedge_success_is_never_slower_than_the_primary(
        script in prop::collection::vec(leaf(), 1..6),
        d in 0u64..40,
    ) {
        let primary = script[0];
        prop_assume!(primary.ok());
        let (op, _) = scripted(script.clone());
        let (r, t) = observe(op.hedge(schedule::spaced(ms(d)).take(script.len() as u32 - 1)));
        prop_assert!(r.is_ok());
        prop_assert!(t <= ms(primary.delay_ms));
    }

    #[test]
    fn hedge_of_successes_is_a_delayed_race(a in leaf(), b in leaf(), d in 0u64..40) {
        let a = Leaf { result: Ok(1), ..a };
        let b = Leaf { result: Ok(2), ..b };
        // On an exact tie the primary is polled first in both, but keep the law tie-free.
        prop_assume!(a.delay_ms != d + b.delay_ms);
        let delayed = Leaf { delay_ms: d + b.delay_ms, ..b };
        let (op, _) = scripted(vec![a, b]);
        same(op.hedge(schedule::spaced(ms(d)).take(1)), a.op().race(delayed.op()))?;
    }

    // ----- hedge with error classification -----------------------------------------------

    #[test]
    fn classified_hedge_matches_its_model(
        script in prop::collection::vec(leaf(), 1..6),
        delays in prop::collection::vec(0u64..40, 0..5),
        mask in any::<u8>(),
    ) {
        let attempts = script.len().min(delays.len() + 1);
        let ds = delays.clone();
        let policy = schedule::from_fn(move |s: schedule::Step| ds.get(s.attempt as usize).map(|d| ms(*d)));
        let (op, calls) = scripted(script.clone());
        let (r, t) = observe(op.hedge(policy.take(attempts as u32 - 1)).when(retryable_unless(mask)));
        let model = hedge_model(&script[..attempts], &delays, retryable_unless(mask));
        check_hedge_model(r, t, calls.get(), model)?;
    }

    #[test]
    fn hedging_every_error_is_no_classification(
        script in prop::collection::vec(leaf(), 1..6),
        d in 0u64..40,
    ) {
        let s = schedule::spaced(ms(d)).take(script.len() as u32 - 1);
        let (lhs, lhs_calls) = scripted(script.clone());
        let (rhs, rhs_calls) = scripted(script.clone());
        same(lhs.hedge(s).when(|_: &TErr| true), rhs.hedge(s))?;
        prop_assert_eq!(lhs_calls.get(), rhs_calls.get());
    }

    #[test]
    fn classified_hedge_stop_is_identity(a in leaf(), mask in any::<u8>()) {
        same(a.op().hedge(schedule::stop()).when(retryable_unless(mask)), a.op())?;
    }

    #[test]
    fn hedging_no_error_is_a_delayed_race(a in leaf(), b in leaf(), d in 0u64..40) {
        // With every error final, the first attempt to finish wins, whatever its outcome.
        prop_assume!(a.delay_ms != d + b.delay_ms);
        let delayed = Leaf { delay_ms: d + b.delay_ms, ..b };
        let (op, _) = scripted(vec![a, b]);
        same(
            op.hedge(schedule::spaced(ms(d)).take(1)).when(|_: &TErr| false),
            a.op().race(delayed.op()),
        )?;
    }

    #[test]
    fn a_final_error_launches_no_more_copies(
        delay in 0u64..40,
        gap in 1u64..40,
        extra in 1u32..4,
    ) {
        // The primary fails for good before the first hedge is due: nothing else starts.
        let primary = Leaf { delay_ms: delay, result: Err(TErr::Code(0)) };
        let (op, calls) = scripted(vec![primary, Leaf { delay_ms: 0, result: Ok(1) }]);
        let (r, t) = observe(op.hedge(schedule::spaced(ms(delay + gap)).take(extra)).when(retryable_unless(1)));
        prop_assert_eq!(r, Err(TErr::Code(0)));
        prop_assert_eq!(t, ms(delay));
        prop_assert_eq!(calls.get(), 1);
    }
}

#[derive(Debug)]
enum Expect {
    Exactly(Out),
    /// Several failures tied for last: any of them.
    AnyError,
    /// A success and a final error, or two different final errors, tied: either.
    Any,
}

struct HedgeOutcome {
    result: Expect,
    time: u64,
    launched: usize,
}

/// An independent model of hedging, as a discrete-event simulation over virtual milliseconds.
/// Errors for which `retryable` is false are final, like a success.
fn hedge_model(script: &[Leaf], delays: &[u64], retryable: impl Fn(&TErr) -> bool) -> HedgeOutcome {
    let mut now = 0u64;
    let mut launched = 1usize;
    let mut running = vec![(script[0].delay_ms, script[0].result)];
    let mut timer = delays.first().copied();
    let mut last = Expect::AnyError;
    loop {
        let next_finish = running.iter().map(|(t, _)| *t).min();
        let due = timer.filter(|_| launched < script.len());
        let launch_at = match (next_finish, due) {
            (None, None) => {
                return HedgeOutcome {
                    result: last,
                    time: now,
                    launched,
                };
            }
            // Nothing is running: the next copy starts at once.
            (None, Some(_)) => Some(now),
            // Completions are observed before a timer firing at the same instant.
            (Some(f), Some(t)) if t < f => Some(t),
            (Some(_), _) => None,
        };
        if let Some(at) = launch_at {
            now = at;
            running.push((now + script[launched].delay_ms, script[launched].result));
            launched += 1;
            timer = delays.get(launched - 1).map(|d| now + d);
            continue;
        }
        now = next_finish.expect("a completion is next");
        let (done, rest): (Vec<_>, Vec<_>) = running.into_iter().partition(|(t, _)| *t == now);
        running = rest;
        let fatal: Vec<Out> = done
            .iter()
            .map(|(_, r)| *r)
            .filter(|r| r.is_err_and(|e| !retryable(&e)))
            .collect();
        let success = done.iter().find(|(_, r)| r.is_ok()).map(|(_, r)| *r);
        let result = match (success, fatal.first()) {
            (Some(ok), None) => Some(Expect::Exactly(ok)),
            (None, Some(&e)) if fatal.iter().all(|f| *f == e) => Some(Expect::Exactly(e)),
            (None, None) => None,
            _ => Some(Expect::Any),
        };
        if let Some(result) = result {
            return HedgeOutcome {
                result,
                time: now,
                launched,
            };
        }
        last = if done.len() == 1 {
            Expect::Exactly(done[0].1)
        } else {
            Expect::AnyError
        };
    }
}

/// Error codes whose bit is set in `mask` are not worth hedging.
fn retryable_unless(mask: u8) -> impl Fn(&TErr) -> bool + Copy {
    move |e: &TErr| match e {
        TErr::Code(c) => mask & (1 << c) == 0,
        _ => true,
    }
}

fn check_hedge_model(
    r: Out,
    t: std::time::Duration,
    launched: usize,
    model: HedgeOutcome,
) -> Result<(), TestCaseError> {
    prop_assert_eq!(t, ms(model.time));
    prop_assert_eq!(launched, model.launched);
    match (r, model.result) {
        (r, Expect::Exactly(w)) => prop_assert_eq!(r, w),
        (Err(_), Expect::AnyError) | (_, Expect::Any) => {}
        (r, m) => prop_assert!(false, "got {:?}, the model says {:?}", r, m),
    }
    Ok(())
}
