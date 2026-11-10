//! Laws of the `Schedule` algebra, checked with proptest over random schedule *expressions*
//! and random step traces.
//!
//! Two schedules are equivalent when, fed the same trace of steps (consecutive attempts,
//! non-decreasing elapsed time), they make the same decisions up to and including the first
//! `None` (after which a retry loop stops asking). Traces carry an input at every step, so the
//! laws also cover the schedules that look at their input.

use std::time::Duration;

use counterpoint::schedule::{self, Schedule, ScheduleFor, Step};
use proptest::prelude::*;

// ----- a type-erased schedule, so proptest can build arbitrary expressions --------------------

trait Erased {
    fn next_erased(&mut self, input: &u8, step: Step) -> Option<Duration>;
    fn fresh_erased(&self) -> Box<dyn Erased>;
}

impl<S: ScheduleFor<u8> + 'static> Erased for S {
    fn next_erased(&mut self, input: &u8, step: Step) -> Option<Duration> {
        self.next(input, step)
    }
    fn fresh_erased(&self) -> Box<dyn Erased> {
        Box::new(self.fresh())
    }
}

struct Dyn(Box<dyn Erased>);

impl Schedule for Dyn {
    fn fresh(&self) -> Self {
        Dyn(self.0.fresh_erased())
    }
}

impl ScheduleFor<u8> for Dyn {
    fn next(&mut self, input: &u8, step: Step) -> Option<Duration> {
        self.0.next_erased(input, step)
    }
}

/// Schedule expressions.
#[derive(Debug, Clone)]
enum Expr {
    Stop,
    Forever,
    Recurs(u32),
    Spaced(u64),
    Exponential(u64),
    Fibonacci(u64),
    Within(u64),
    RecurWhile(u8),
    RecurUntil(u8),
    FromInput(u64),
    Contramap(Box<Expr>, u8),
    Union(Box<Expr>, Box<Expr>),
    Intersect(Box<Expr>, Box<Expr>),
    Then(Box<Expr>, Box<Expr>),
    Cap(Box<Expr>, u64),
}

fn ms(n: u64) -> Duration {
    Duration::from_millis(n)
}

impl Expr {
    fn build(&self) -> Dyn {
        fn d<S: ScheduleFor<u8> + 'static>(s: S) -> Dyn {
            Dyn(Box::new(s))
        }
        match self {
            Expr::Stop => d(schedule::stop()),
            Expr::Forever => d(schedule::forever()),
            Expr::Recurs(n) => d(schedule::recurs(*n)),
            Expr::Spaced(x) => d(schedule::spaced(ms(*x))),
            Expr::Exponential(x) => d(schedule::exponential(ms(*x))),
            Expr::Fibonacci(x) => d(schedule::fibonacci(ms(*x))),
            Expr::Within(x) => d(schedule::within(ms(*x))),
            Expr::RecurWhile(k) => {
                let k = *k;
                d(schedule::recur_while(move |x: &u8| *x < k))
            }
            Expr::RecurUntil(k) => {
                let k = *k;
                d(schedule::recur_until(move |x: &u8| *x >= k))
            }
            Expr::FromInput(k) => {
                let k = *k;
                d(schedule::from_input(move |x: &u8, _| {
                    (*x < 200).then(|| ms(k * u64::from(*x)))
                }))
            }
            Expr::Contramap(a, k) => {
                let k = *k;
                d(a.build().contramap(move |x: &u8| x.wrapping_add(k)))
            }
            Expr::Union(a, b) => d(a.build() | b.build()),
            Expr::Intersect(a, b) => d(a.build() & b.build()),
            Expr::Then(a, b) => d(a.build().then(b.build())),
            Expr::Cap(a, x) => d(a.build().cap(ms(*x))),
        }
    }
}

// `Dyn` is local, so give it the operators too.
impl<R: Schedule> std::ops::BitOr<R> for Dyn {
    type Output = schedule::Union<Dyn, R>;
    fn bitor(self, rhs: R) -> Self::Output {
        self.union(rhs)
    }
}
impl<R: Schedule> std::ops::BitAnd<R> for Dyn {
    type Output = schedule::Intersect<Dyn, R>;
    fn bitand(self, rhs: R) -> Self::Output {
        self.intersect(rhs)
    }
}

fn expr() -> impl Strategy<Value = Expr> {
    grow(prop_oneof![
        pure_leaf(),
        any::<u8>().prop_map(Expr::RecurWhile),
        any::<u8>().prop_map(Expr::RecurUntil),
        (0u64..20).prop_map(Expr::FromInput),
    ])
}

/// Expressions built only from schedules that ignore their input.
fn pure_expr() -> impl Strategy<Value = Expr> {
    grow(pure_leaf())
}

fn pure_leaf() -> BoxedStrategy<Expr> {
    prop_oneof![
        Just(Expr::Stop),
        Just(Expr::Forever),
        (0u32..6).prop_map(Expr::Recurs),
        (0u64..100).prop_map(Expr::Spaced),
        (1u64..50).prop_map(Expr::Exponential),
        (1u64..50).prop_map(Expr::Fibonacci),
        (0u64..500).prop_map(Expr::Within),
    ]
    .boxed()
}

fn grow(leaf: impl Strategy<Value = Expr> + 'static) -> impl Strategy<Value = Expr> {
    leaf.prop_recursive(4, 24, 2, |inner| {
        prop_oneof![
            (inner.clone(), inner.clone()).prop_map(|(a, b)| Expr::Union(a.into(), b.into())),
            (inner.clone(), inner.clone()).prop_map(|(a, b)| Expr::Intersect(a.into(), b.into())),
            (inner.clone(), inner.clone()).prop_map(|(a, b)| Expr::Then(a.into(), b.into())),
            (inner.clone(), 0u64..300).prop_map(|(a, x)| Expr::Cap(a.into(), x)),
            (inner, any::<u8>()).prop_map(|(a, k)| Expr::Contramap(a.into(), k)),
        ]
    })
}

/// A trace: consecutive attempts with non-decreasing elapsed time, each with an input.
fn trace() -> impl Strategy<Value = Vec<(u8, Step)>> {
    prop::collection::vec((any::<u8>(), 0u64..80), 1..14).prop_map(|items| {
        let mut elapsed = Duration::ZERO;
        items
            .into_iter()
            .enumerate()
            .map(|(i, (input, g))| {
                elapsed += ms(g);
                (
                    input,
                    Step {
                        attempt: i as u32,
                        elapsed,
                    },
                )
            })
            .collect()
    })
}

/// Decisions up to and including the first `None`.
fn run<S: ScheduleFor<u8>>(s: &S, steps: &[(u8, Step)]) -> Vec<Option<Duration>> {
    let mut s = s.fresh();
    let mut out = Vec::new();
    for (input, step) in steps {
        let d = s.next(input, *step);
        out.push(d);
        if d.is_none() {
            break;
        }
    }
    out
}

macro_rules! equiv {
    ($steps:expr, $lhs:expr, $rhs:expr) => {
        prop_assert_eq!(run(&$lhs, &$steps), run(&$rhs, &$steps));
    };
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    // ----- lattice: | is pointwise min, & is pointwise max ------------------------------

    #[test]
    fn union_associative(a in expr(), b in expr(), c in expr(), t in trace()) {
        equiv!(t, (a.build() | b.build()) | c.build(), a.build() | (b.build() | c.build()));
    }

    #[test]
    fn intersect_associative(a in expr(), b in expr(), c in expr(), t in trace()) {
        equiv!(t, (a.build() & b.build()) & c.build(), a.build() & (b.build() & c.build()));
    }

    #[test]
    fn union_and_intersect_commutative(a in expr(), b in expr(), t in trace()) {
        equiv!(t, a.build() | b.build(), b.build() | a.build());
        equiv!(t, a.build() & b.build(), b.build() & a.build());
    }

    #[test]
    fn idempotent(a in expr(), t in trace()) {
        equiv!(t, a.build() | a.build(), a.build());
        equiv!(t, a.build() & a.build(), a.build());
    }

    #[test]
    fn absorption(a in expr(), b in expr(), t in trace()) {
        equiv!(t, a.build() | (a.build() & b.build()), a.build());
        equiv!(t, a.build() & (a.build() | b.build()), a.build());
    }

    #[test]
    fn distributive(a in expr(), b in expr(), c in expr(), t in trace()) {
        equiv!(t, a.build() & (b.build() | c.build()), (a.build() & b.build()) | (a.build() & c.build()));
        equiv!(t, a.build() | (b.build() & c.build()), (a.build() | b.build()) & (a.build() | c.build()));
    }

    #[test]
    fn bounds(a in expr(), t in trace()) {
        equiv!(t, a.build() | schedule::stop(), a.build());
        equiv!(t, a.build() & schedule::forever(), a.build());
        equiv!(t, a.build() & schedule::stop(), schedule::stop());
        equiv!(t, a.build() | schedule::forever(), schedule::forever());
    }

    // ----- sequencing: a monoid with unit `stop` -----------------------------------------

    #[test]
    fn then_associative(a in expr(), b in expr(), c in expr(), t in trace()) {
        equiv!(t, a.build().then(b.build()).then(c.build()), a.build().then(b.build().then(c.build())));
    }

    #[test]
    fn then_unit(a in expr(), t in trace()) {
        equiv!(t, schedule::stop().then(a.build()), a.build());
        equiv!(t, a.build().then(schedule::stop()), a.build());
    }

    #[test]
    fn recurs_then_adds(m in 0u32..5, n in 0u32..5, t in trace()) {
        equiv!(t, schedule::recurs(m).then(schedule::recurs(n)), schedule::recurs(m + n));
    }

    // ----- inputs -------------------------------------------------------------------------

    #[test]
    fn contramap_identity(a in expr(), t in trace()) {
        equiv!(t, a.build().contramap(|x: &u8| *x), a.build());
    }

    #[test]
    fn contramap_composition(a in expr(), j in any::<u8>(), k in any::<u8>(), t in trace()) {
        let f = move |x: &u8| x.wrapping_add(j);
        let g = move |x: &u8| x.wrapping_mul(k);
        equiv!(t, a.build().contramap(f).contramap(g), a.build().contramap(move |x: &u8| f(&g(x))));
    }

    #[test]
    fn contramap_distributes(a in expr(), b in expr(), k in any::<u8>(), t in trace()) {
        let f = move |x: &u8| x.wrapping_add(k);
        equiv!(t, (a.build() | b.build()).contramap(f), a.build().contramap(f) | b.build().contramap(f));
        equiv!(t, (a.build() & b.build()).contramap(f), a.build().contramap(f) & b.build().contramap(f));
        equiv!(t, a.build().then(b.build()).contramap(f), a.build().contramap(f).then(b.build().contramap(f)));
    }

    #[test]
    fn recur_while_bounds(t in trace()) {
        equiv!(t, schedule::recur_while(|_: &u8| true), schedule::forever());
        equiv!(t, schedule::recur_while(|_: &u8| false), schedule::stop());
    }

    #[test]
    fn recur_until_is_the_negation_of_recur_while(k in any::<u8>(), t in trace()) {
        equiv!(t, schedule::recur_until(move |x: &u8| *x >= k), schedule::recur_while(move |x: &u8| *x < k));
    }

    #[test]
    fn recur_while_stops_exactly_when_the_predicate_fails(k in any::<u8>(), t in trace()) {
        let decisions = run(&schedule::recur_while(move |x: &u8| *x < k), &t);
        let stop = t.iter().position(|(x, _)| *x >= k);
        prop_assert_eq!(decisions.len(), stop.map_or(t.len(), |i| i + 1));
        prop_assert_eq!(decisions.last().unwrap().is_none(), stop.is_some());
    }

    #[test]
    fn from_input_decides_on_the_input(k in 0u64..20, t in trace()) {
        let f = move |x: &u8, _: Step| (*x < 200).then(|| ms(k * u64::from(*x)));
        let decisions = run(&schedule::from_input(f), &t);
        let expected: Vec<_> = t.iter().map(|(x, s)| f(x, *s)).collect();
        let n = decisions.len();
        prop_assert_eq!(decisions, expected[..n].to_vec());
    }

    #[test]
    fn schedules_without_input_ignore_it(a in pure_expr(), t in trace(), other in any::<u8>()) {
        let replaced: Vec<_> = t.iter().map(|(_, s)| (other, *s)).collect();
        prop_assert_eq!(run(&a.build(), &t), run(&a.build(), &replaced));
    }

    // ----- derived operations -----------------------------------------------------------

    #[test]
    fn cap_composes_by_min(a in expr(), x in 0u64..300, y in 0u64..300, t in trace()) {
        equiv!(t, a.build().cap(ms(x)).cap(ms(y)), a.build().cap(ms(x.min(y))));
    }

    #[test]
    fn take_is_intersection_with_recurs(a in expr(), n in 0u32..6, t in trace()) {
        equiv!(t, a.build().take(n), a.build() & schedule::recurs(n));
        equiv!(t, schedule::recurs(n), schedule::forever().take(n));
    }

    #[test]
    fn recurs_and_spaced_are_lattice_homomorphisms(m in 0u32..6, n in 0u32..6, x in 0u64..100, y in 0u64..100, t in trace()) {
        equiv!(t, schedule::recurs(m) & schedule::recurs(n), schedule::recurs(m.min(n)));
        equiv!(t, schedule::recurs(m) | schedule::recurs(n), schedule::recurs(m.max(n)));
        equiv!(t, schedule::spaced(ms(x)) | schedule::spaced(ms(y)), schedule::spaced(ms(x.min(y))));
        equiv!(t, schedule::spaced(ms(x)) & schedule::spaced(ms(y)), schedule::spaced(ms(x.max(y))));
    }

    #[test]
    fn zero_jitter_is_identity(a in expr(), t in trace()) {
        equiv!(t, a.build().jitter(0.0), a.build());
    }

    #[test]
    fn jitter_stays_in_range(a in expr(), f in 0.0f64..1.0, seed in any::<u64>(), t in trace()) {
        let plain = run(&a.build(), &t);
        let jittered = run(&a.build().jitter(f).seed(seed), &t);
        prop_assert_eq!(plain.len(), jittered.len());
        for (p, j) in plain.into_iter().zip(jittered) {
            match (p, j) {
                (None, None) => {}
                (Some(p), Some(j)) => {
                    let p = p.as_secs_f64();
                    let j = j.as_secs_f64();
                    prop_assert!(j >= p * (1.0 - f) - 1e-9 && j <= p * (1.0 + f) + 1e-9);
                }
                _ => prop_assert!(false, "jitter changed a stop decision"),
            }
        }
    }

    #[test]
    fn seeded_jitter_is_reproducible(a in expr(), seed in any::<u64>(), t in trace()) {
        let s = a.build().full_jitter().seed(seed);
        prop_assert_eq!(run(&s, &t), run(&s, &t));
    }
}

#[test]
fn documented_delays() {
    let s = schedule::exponential(ms(100)).cap(ms(1000)) & schedule::recurs(5);
    assert_eq!(
        s.delays().collect::<Vec<_>>(),
        [ms(100), ms(200), ms(400), ms(800), ms(1000)]
    );
    let f = schedule::fibonacci(ms(10)).take(6);
    assert_eq!(
        f.delays().collect::<Vec<_>>(),
        [ms(10), ms(10), ms(20), ms(30), ms(50), ms(80)]
    );
    let switch = schedule::spaced(ms(5))
        .take(2)
        .then(schedule::spaced(ms(50)).take(1));
    assert_eq!(switch.delays().collect::<Vec<_>>(), [ms(5), ms(5), ms(50)]);
    let budget = schedule::spaced(ms(30)).within(ms(100));
    assert_eq!(budget.delays().collect::<Vec<_>>(), [ms(30); 4]);
}
