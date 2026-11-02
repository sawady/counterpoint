//! Retry schedules as a small algebra.
//!
//! A [`Schedule`] decides, after each failed attempt, whether to try again and how long to
//! wait first. Its decision at each step is a value of `Option<Duration>`, where `None` means
//! "stop". Reading `None` as an *infinite* delay, decisions are totally ordered, and the two
//! basic ways of combining schedules are the pointwise lattice operations:
//!
//! | combinator            | operator | continues when…   | waits     | identity    | zero        |
//! |-----------------------|----------|-------------------|-----------|-------------|-------------|
//! | [`Schedule::union`]     | `a \| b` | either continues  | the `min` | [`stop`]    | [`forever`] |
//! | [`Schedule::intersect`] | `a & b`  | both continue     | the `max` | [`forever`] | [`stop`]    |
//!
//! Because `min`/`max` over a total order form a distributive lattice, deterministic
//! schedules satisfy associativity, commutativity, idempotence, absorption and distributivity
//! for `|` and `&` (all checked by the property tests in `tests/schedule_laws.rs`).
//! [`Schedule::then`] is a third, sequential composition that forms a monoid with [`stop`].
//!
//! ```
//! use counterpoint::schedule::{self, Schedule};
//! use std::time::Duration;
//!
//! let ms = Duration::from_millis;
//! // Exponential backoff from 100ms, capped at 1s, at most 5 retries.
//! let s = schedule::exponential(ms(100)).cap(ms(1000)) & schedule::recurs(5);
//! let delays: Vec<_> = s.delays().collect();
//! assert_eq!(delays, [ms(100), ms(200), ms(400), ms(800), ms(1000)]);
//! ```

use core::ops::{BitAnd, BitOr};
use core::time::Duration;
use std::hash::{BuildHasher, RandomState};

/// What a schedule sees when asked for its next decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Step {
    /// Number of retries already performed (0 when deciding after the first failure).
    pub attempt: u32,
    /// Time since the first attempt started.
    pub elapsed: Duration,
}

/// A retry policy. See the [module docs](self) for the algebra.
pub trait Schedule {
    /// Decide after a failure: `Some(delay)` to retry after `delay`, `None` to give up.
    fn next(&mut self, step: Step) -> Option<Duration>;

    /// A copy of this schedule in its initial state. Retrying calls this once per run, so a
    /// schedule value can be reused. Unseeded jitter draws a new seed here.
    fn fresh(&self) -> Self
    where
        Self: Sized;

    /// Continue while *either* continues, waiting the shorter delay. Also written `a | b`.
    fn union<T: Schedule>(self, other: T) -> Union<Self, T>
    where
        Self: Sized,
    {
        Union { a: self, b: other }
    }

    /// Continue while *both* continue, waiting the longer delay. Also written `a & b`.
    fn intersect<T: Schedule>(self, other: T) -> Intersect<Self, T>
    where
        Self: Sized,
    {
        Intersect { a: self, b: other }
    }

    /// Run `self` until it stops, then `other` (which sees attempts counted from its start).
    fn then<T: Schedule>(self, other: T) -> Then<Self, T>
    where
        Self: Sized,
    {
        Then {
            a: self,
            b: other,
            switched_at: None,
        }
    }

    /// At most `n` retries: `self & recurs(n)`.
    fn take(self, n: u32) -> Intersect<Self, Recurs>
    where
        Self: Sized,
    {
        self.intersect(recurs(n))
    }

    /// Stop once `budget` has elapsed since the first attempt: `self & within(budget)`.
    fn within(self, budget: Duration) -> Intersect<Self, Within>
    where
        Self: Sized,
    {
        self.intersect(within(budget))
    }

    /// Never wait longer than `max`. Law: `s.cap(a).cap(b) ≡ s.cap(min(a, b))`.
    fn cap(self, max: Duration) -> Cap<Self>
    where
        Self: Sized,
    {
        Cap { inner: self, max }
    }

    /// Multiply each delay by a random factor in `[1 - factor, 1 + factor]` (`factor` is
    /// clamped to `[0, 1]`). Law: `s.jitter(0.0) ≡ s`.
    fn jitter(self, factor: f64) -> Jitter<Self>
    where
        Self: Sized,
    {
        let f = if factor.is_nan() {
            0.0
        } else {
            factor.clamp(0.0, 1.0)
        };
        Jitter::new(self, 1.0 - f, 1.0 + f)
    }

    /// "Full jitter": each delay becomes uniform in `[0, delay]`.
    fn full_jitter(self) -> Jitter<Self>
    where
        Self: Sized,
    {
        Jitter::new(self, 0.0, 1.0)
    }

    /// The delays this schedule would produce if every attempt took no time. Handy for docs
    /// and tests; it is infinite for schedules that never stop.
    fn delays(&self) -> Delays<Self>
    where
        Self: Sized,
    {
        Delays {
            s: self.fresh(),
            step: Step::default(),
            done: false,
        }
    }
}

/// Iterator returned by [`Schedule::delays`].
#[derive(Debug, Clone)]
pub struct Delays<S> {
    s: S,
    step: Step,
    done: bool,
}

impl<S: Schedule> Iterator for Delays<S> {
    type Item = Duration;

    fn next(&mut self) -> Option<Duration> {
        if self.done {
            return None;
        }
        match self.s.next(self.step) {
            Some(d) => {
                self.step.attempt = self.step.attempt.saturating_add(1);
                self.step.elapsed = self.step.elapsed.saturating_add(d);
                Some(d)
            }
            None => {
                self.done = true;
                None
            }
        }
    }
}

// ----- constructors ---------------------------------------------------------------------------

/// Never retry. Identity of `|` and of [`Schedule::then`]; zero of `&`.
#[derive(Debug, Clone, Copy, Default)]
pub struct Stop;

/// Never retry.
pub fn stop() -> Stop {
    Stop
}

impl Schedule for Stop {
    fn next(&mut self, _: Step) -> Option<Duration> {
        None
    }
    fn fresh(&self) -> Self {
        Stop
    }
}

/// Retry forever, immediately. Identity of `&`; zero of `|`.
#[derive(Debug, Clone, Copy, Default)]
pub struct Forever;

/// Retry forever without waiting.
pub fn forever() -> Forever {
    Forever
}

impl Schedule for Forever {
    fn next(&mut self, _: Step) -> Option<Duration> {
        Some(Duration::ZERO)
    }
    fn fresh(&self) -> Self {
        Forever
    }
}

/// Retry `n` times without waiting. See [`recurs`].
#[derive(Debug, Clone, Copy)]
pub struct Recurs(u32);

/// Retry `n` times without waiting. `recurs(0) ≡ stop()`; `recurs(a) & recurs(b) ≡
/// recurs(min(a, b))`.
pub fn recurs(n: u32) -> Recurs {
    Recurs(n)
}

impl Schedule for Recurs {
    fn next(&mut self, step: Step) -> Option<Duration> {
        (step.attempt < self.0).then_some(Duration::ZERO)
    }
    fn fresh(&self) -> Self {
        *self
    }
}

/// Retry forever with a fixed delay. See [`spaced`].
#[derive(Debug, Clone, Copy)]
pub struct Spaced(Duration);

/// Retry forever, waiting `d` each time. Law: `spaced(a) | spaced(b) ≡ spaced(min(a, b))`.
pub fn spaced(d: Duration) -> Spaced {
    Spaced(d)
}

impl Schedule for Spaced {
    fn next(&mut self, _: Step) -> Option<Duration> {
        Some(self.0)
    }
    fn fresh(&self) -> Self {
        *self
    }
}

/// Exponential backoff. See [`exponential`].
#[derive(Debug, Clone, Copy)]
pub struct Exponential {
    base: Duration,
    factor: f64,
}

/// Retry forever, waiting `base * 2^attempt` (saturating). Change the factor with
/// [`Exponential::factor`].
pub fn exponential(base: Duration) -> Exponential {
    Exponential { base, factor: 2.0 }
}

impl Exponential {
    /// Use `factor` instead of 2 (values below 1 are treated as 1).
    pub fn factor(mut self, factor: f64) -> Self {
        self.factor = if factor.is_nan() {
            1.0
        } else {
            factor.max(1.0)
        };
        self
    }
}

impl Schedule for Exponential {
    fn next(&mut self, step: Step) -> Option<Duration> {
        let exp = i32::try_from(step.attempt).unwrap_or(i32::MAX);
        let secs = self.base.as_secs_f64() * self.factor.powi(exp);
        Some(Duration::try_from_secs_f64(secs).unwrap_or(Duration::MAX))
    }
    fn fresh(&self) -> Self {
        *self
    }
}

/// Fibonacci backoff. See [`fibonacci`].
#[derive(Debug, Clone, Copy)]
pub struct Fibonacci(Duration);

/// Retry forever, waiting `base * fib(attempt + 1)`: `base, base, 2·base, 3·base, 5·base, …`.
pub fn fibonacci(base: Duration) -> Fibonacci {
    Fibonacci(base)
}

impl Schedule for Fibonacci {
    fn next(&mut self, step: Step) -> Option<Duration> {
        let (mut a, mut b) = (1u32, 1u32);
        for _ in 0..step.attempt {
            (a, b) = (b, a.saturating_add(b));
            if a == u32::MAX {
                break;
            }
        }
        Some(self.0.checked_mul(a).unwrap_or(Duration::MAX))
    }
    fn fresh(&self) -> Self {
        *self
    }
}

/// Retry immediately while less than a time budget has elapsed. See [`within`].
#[derive(Debug, Clone, Copy)]
pub struct Within(Duration);

/// Retry immediately while less than `budget` has elapsed since the first attempt. Meant to
/// be intersected with a delaying schedule: `exponential(..) & within(budget)`.
pub fn within(budget: Duration) -> Within {
    Within(budget)
}

impl Schedule for Within {
    fn next(&mut self, step: Step) -> Option<Duration> {
        (step.elapsed < self.0).then_some(Duration::ZERO)
    }
    fn fresh(&self) -> Self {
        *self
    }
}

/// A custom schedule from a function of the [`Step`]. See [`from_fn`].
#[derive(Debug, Clone, Copy)]
pub struct FromFn<F>(F);

/// A custom schedule from a closure. The closure is cloned on [`Schedule::fresh`].
pub fn from_fn<F>(f: F) -> FromFn<F>
where
    F: FnMut(Step) -> Option<Duration> + Clone,
{
    FromFn(f)
}

impl<F> Schedule for FromFn<F>
where
    F: FnMut(Step) -> Option<Duration> + Clone,
{
    fn next(&mut self, step: Step) -> Option<Duration> {
        (self.0)(step)
    }
    fn fresh(&self) -> Self {
        FromFn(self.0.clone())
    }
}

// ----- combinators ----------------------------------------------------------------------------

/// See [`Schedule::union`].
#[derive(Debug, Clone, Copy)]
pub struct Union<A, B> {
    a: A,
    b: B,
}

impl<A: Schedule, B: Schedule> Schedule for Union<A, B> {
    fn next(&mut self, step: Step) -> Option<Duration> {
        // Both sides always step, so stateful children stay in sync.
        match (self.a.next(step), self.b.next(step)) {
            (Some(x), Some(y)) => Some(x.min(y)),
            (x, None) => x,
            (None, y) => y,
        }
    }
    fn fresh(&self) -> Self {
        Union {
            a: self.a.fresh(),
            b: self.b.fresh(),
        }
    }
}

/// See [`Schedule::intersect`].
#[derive(Debug, Clone, Copy)]
pub struct Intersect<A, B> {
    a: A,
    b: B,
}

impl<A: Schedule, B: Schedule> Schedule for Intersect<A, B> {
    fn next(&mut self, step: Step) -> Option<Duration> {
        match (self.a.next(step), self.b.next(step)) {
            (Some(x), Some(y)) => Some(x.max(y)),
            _ => None,
        }
    }
    fn fresh(&self) -> Self {
        Intersect {
            a: self.a.fresh(),
            b: self.b.fresh(),
        }
    }
}

/// See [`Schedule::then`].
#[derive(Debug, Clone, Copy)]
pub struct Then<A, B> {
    a: A,
    b: B,
    switched_at: Option<u32>,
}

impl<A: Schedule, B: Schedule> Schedule for Then<A, B> {
    fn next(&mut self, step: Step) -> Option<Duration> {
        let offset = match self.switched_at {
            Some(k) => k,
            None => match self.a.next(step) {
                Some(d) => return Some(d),
                None => {
                    self.switched_at = Some(step.attempt);
                    step.attempt
                }
            },
        };
        self.b.next(Step {
            attempt: step.attempt - offset,
            elapsed: step.elapsed,
        })
    }
    fn fresh(&self) -> Self {
        Then {
            a: self.a.fresh(),
            b: self.b.fresh(),
            switched_at: None,
        }
    }
}

/// See [`Schedule::cap`].
#[derive(Debug, Clone, Copy)]
pub struct Cap<S> {
    inner: S,
    max: Duration,
}

impl<S: Schedule> Schedule for Cap<S> {
    fn next(&mut self, step: Step) -> Option<Duration> {
        self.inner.next(step).map(|d| d.min(self.max))
    }
    fn fresh(&self) -> Self {
        Cap {
            inner: self.inner.fresh(),
            max: self.max,
        }
    }
}

/// See [`Schedule::jitter`] and [`Schedule::full_jitter`].
#[derive(Debug, Clone, Copy)]
pub struct Jitter<S> {
    inner: S,
    lo: f64,
    hi: f64,
    seed: Option<u64>,
    rng: SplitMix64,
}

impl<S> Jitter<S> {
    fn new(inner: S, lo: f64, hi: f64) -> Self {
        Jitter {
            inner,
            lo,
            hi,
            seed: None,
            rng: SplitMix64::random(),
        }
    }

    /// Make the jitter deterministic: every run starts from the same seed.
    pub fn seed(mut self, seed: u64) -> Self {
        self.seed = Some(seed);
        self.rng = SplitMix64(seed);
        self
    }
}

impl<S: Schedule> Schedule for Jitter<S> {
    fn next(&mut self, step: Step) -> Option<Duration> {
        let d = self.inner.next(step)?;
        let r = self.rng.next_f64();
        let factor = self.lo + (self.hi - self.lo) * r;
        Some(Duration::try_from_secs_f64(d.as_secs_f64() * factor).unwrap_or(Duration::MAX))
    }
    fn fresh(&self) -> Self {
        Jitter {
            inner: self.inner.fresh(),
            lo: self.lo,
            hi: self.hi,
            seed: self.seed,
            rng: self.seed.map_or_else(SplitMix64::random, SplitMix64),
        }
    }
}

/// SplitMix64 (public-domain PRNG by Sebastiano Vigna): tiny, fast, good enough for jitter.
#[derive(Debug, Clone, Copy)]
struct SplitMix64(u64);

impl SplitMix64 {
    fn random() -> Self {
        // `RandomState` is seeded per instance from OS randomness (then incremented), which
        // is all we need to decorrelate retries across calls and processes.
        SplitMix64(RandomState::new().hash_one(0x5eed_u64))
    }

    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Uniform in `[0, 1)`.
    fn next_f64(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 * (1.0 / (1u64 << 53) as f64)
    }
}

// ----- operators ------------------------------------------------------------------------------

macro_rules! impl_ops {
    ($([$($g:ident),*] $t:ty),* $(,)?) => {$(
        impl<$($g,)* Rhs: Schedule> BitOr<Rhs> for $t where $t: Schedule {
            type Output = Union<Self, Rhs>;
            fn bitor(self, rhs: Rhs) -> Self::Output {
                self.union(rhs)
            }
        }
        impl<$($g,)* Rhs: Schedule> BitAnd<Rhs> for $t where $t: Schedule {
            type Output = Intersect<Self, Rhs>;
            fn bitand(self, rhs: Rhs) -> Self::Output {
                self.intersect(rhs)
            }
        }
    )*};
}

impl_ops!(
    [] Stop,
    [] Forever,
    [] Recurs,
    [] Spaced,
    [] Exponential,
    [] Fibonacci,
    [] Within,
    [F] FromFn<F>,
    [A, B] Union<A, B>,
    [A, B] Intersect<A, B>,
    [A, B] Then<A, B>,
    [S] Cap<S>,
    [S] Jitter<S>,
);
