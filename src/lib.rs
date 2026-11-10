//! # counterpoint
//!
//! Lawful combinators for orchestrating async Rust on tokio.
//!
//! In musical counterpoint, independent voices are combined under strict rules. This crate
//! does the same for async computations: you describe *what* should happen — in sequence, in
//! parallel, as a race, with timeouts, retries, fallbacks and guards — with a small set of
//! combinators whose behaviour is pinned down by **algebraic laws**, and those laws are
//! checked with property-based tests under tokio's virtual clock.
//!
//! The central type is the [`Op`] trait: a *re-runnable* description of a fallible async
//! computation (a one-shot `Future` cannot be retried). Ops are plain generic structs —
//! no `Box<dyn>`, no spawning, no `'static` bound — and dropping a running op cancels
//! everything inside it.
//!
//! ```
//! use counterpoint::prelude::*;
//! use std::time::Duration;
//!
//! #[derive(Debug, Clone, PartialEq)]
//! enum Error { Timeout, Unavailable }
//! impl From<Elapsed> for Error { fn from(_: Elapsed) -> Self { Error::Timeout } }
//!
//! async fn primary() -> Result<String, Error> { Err(Error::Unavailable) }
//! async fn replica() -> Result<String, Error> { Ok("from replica".into()) }
//!
//! # #[tokio::main(flavor = "current_thread")] async fn main() {
//! let ms = Duration::from_millis;
//! let mut read = from_fn(primary)
//!     .timeout(ms(100))
//!     .retry(schedule::exponential(ms(10)).full_jitter() & schedule::recurs(2))
//!     .or(from_fn(replica).timeout(ms(100)));
//!
//! assert_eq!(read.run().await, Ok("from replica".to_string()));
//! # }
//! ```
//!
//! ## Laws
//!
//! `≡` means *observationally equivalent*: same result **and** same (virtual) running time.
//! "Except on ties" means the law holds whenever the operands do not finish at the exact same
//! instant.
//!
//! | Combinator | Laws |
//! |---|---|
//! | [`Op::and_then`] | `succeed(a).and_then(f) ≡ f(a)`; `m.and_then(succeed) ≡ m`; associativity; duration adds |
//! | [`Op::map`] | `m.map(id) ≡ m`; `m.map(f).map(g) ≡ m.map(g ∘ f)` |
//! | [`Op::zip`] | associative and commutative up to tuple isomorphism; unit `succeed(())`; duration is `max` |
//! | [`zip3`]…[`zip8`] | each is the previous arity zipped with one more op and flattened |
//! | [`Op::race`] | associative; identity [`never()`](fn@crate::never); commutative except on ties; duration is `min` |
//! | [`Op::or`] | associative; left identity `fail(e)`; left zero `succeed(a)` |
//! | [`Op::ensure`] | `ensure(true) ≡ id`; `ensure(p).ensure(q) ≡ ensure(p ∧ q)` |
//! | [`Op::timeout`] | `timeout(∞) ≡ id`; `timeout(a).timeout(b) ≡ timeout(min(a, b))` except on ties |
//! | [`Op::retry`] | `retry(stop) ≡ id`; `succeed(a).retry(s) ≡ succeed(a)`; `retry(recurs(n))` runs at most `n + 1` times |
//! | [`Op::repeat`] | `repeat(stop) ≡ id`; `fail(e).repeat(s) ≡ fail(e)`; `repeat` is `retry` with success and failure swapped |
//! | [`Op::hedge`] | `hedge(stop) ≡ id`; `succeed(a).hedge(s) ≡ succeed(a)`; a success is never slower than the primary's; with `.when(|_| false)` the first attempt to finish wins |
//! | [`Op::with_breaker`] | `with_schedule(n, spaced(d)) ≡ new(n, d)`; `.when(|_| true) ≡` no filter; errors that `.when` does not count are invisible to the breaker |
//! | [`Schedule`] | `\|`/`&` form a distributive lattice (min/max of delays) with `stop`/`forever` as bounds; `then` is a monoid with unit `stop` |
//!
//! See `tests/laws.rs` and `tests/schedule_laws.rs` for the executable versions.
//!
//! ## Feature flags
//!
//! - `tracing`: emit `tracing` debug events for retries, timeouts, fallbacks and breaker
//!   transitions.

#![warn(missing_docs)]

pub mod breaker;
mod obs;
mod op;
pub mod par;
pub mod retry;
pub mod schedule;
pub mod seq;
pub mod time;

pub use breaker::{BreakerState, CircuitBreaker, CircuitOpen};
pub use op::{Fail, FromFn, Never, Op, Succeed, fail, from_fn, never, succeed};
pub use par::{all, race_all, zip3, zip4, zip5, zip6, zip7, zip8};
pub use schedule::{Schedule, ScheduleFor};
pub use time::Elapsed;

/// Everything needed to write orchestration code: `use counterpoint::prelude::*;`.
pub mod prelude {
    pub use crate::breaker::{BreakerState, CircuitBreaker, CircuitOpen};
    pub use crate::op::{Op, fail, from_fn, never, succeed};
    pub use crate::par::{all, race_all, zip3, zip4, zip5, zip6, zip7, zip8};
    pub use crate::schedule::{self, Schedule, ScheduleFor};
    pub use crate::time::Elapsed;
}
