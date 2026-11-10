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
        // Model: success iff every leaf succeeds, taking the max duration; otherwise the
        // earliest failure wins and cancels the rest.
        let (r, t) = observe(all(leaves.iter().map(|l| l.op())));
        let first_failure = leaves.iter().filter(|l| !l.ok()).map(|l| l.delay_ms).min();
        match first_failure {
            None => {
                prop_assert_eq!(r, Ok(leaves.iter().map(|l| l.result.unwrap()).collect::<Vec<_>>()));
                prop_assert_eq!(t, ms(leaves.iter().map(|l| l.delay_ms).max().unwrap_or(0)));
            }
            Some(d) => {
                prop_assert_eq!(t, ms(d));
                let at_d: Vec<_> = leaves.iter().filter(|l| !l.ok() && l.delay_ms == d).collect();
                if at_d.len() == 1 {
                    prop_assert_eq!(r, Err(at_d[0].result.unwrap_err()));
                }
            }
        }
    }

    #[test]
    fn bounded_all_is_list_scheduling(leaves in prop::collection::vec(leaf(), 0..7), limit in 1usize..4) {
        let leaves: Vec<_> = leaves.into_iter().map(|l| Leaf { result: Ok(l.delay_ms as i32), ..l }).collect();
        let (r, t) = observe(all(leaves.iter().map(|l| l.op())).limit(limit));
        // Greedy list scheduling in input order on `limit` slots.
        let mut slots = vec![0u64; limit];
        for l in &leaves {
            let s = slots.iter_mut().min().unwrap();
            *s += l.delay_ms;
        }
        prop_assert_eq!(r, Ok(leaves.iter().map(|l| l.delay_ms as i32).collect::<Vec<_>>()));
        prop_assert_eq!(t, ms(slots.into_iter().max().unwrap_or(0)));
    }

    #[test]
    fn settled_reports_everything(leaves in prop::collection::vec(leaf(), 0..7), limit in 1usize..4) {
        let (r, _) = observe(all(leaves.iter().map(|l| l.op())).limit(limit).settled());
        prop_assert_eq!(r, Ok(leaves.iter().map(|l| l.result).collect::<Vec<_>>()));
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
        let ds = [a.delay_ms, b.delay_ms, c.delay_ms];
        prop_assume!(ds[0] != ds[1] && ds[1] != ds[2] && ds[0] != ds[2]);
        same(race_all([a.op(), b.op(), c.op()]), a.op().race(b.op()).race(c.op()))?;
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
}
