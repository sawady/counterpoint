# counterpoint

**Lawful combinators for orchestrating async Rust.**

In musical counterpoint, independent voices are combined under strict rules. `counterpoint`
does the same for async computations on [tokio](https://tokio.rs): you describe what should
happen — in sequence, in parallel, as a race, with timeouts, retries, fallbacks and guards —
with a small set of combinators whose behaviour is pinned down by **algebraic laws**, and every
law is checked by property-based tests on tokio's virtual clock.

> Status: early prototype, not published on crates.io. The API will change.

```rust
use counterpoint::prelude::*;
use std::time::Duration;

let ms = Duration::from_millis;

let profile = from_fn(|| fetch_profile(user))           // a re-runnable async computation
    .timeout(ms(100))                                     // each attempt: at most 100ms
    .retry(                                               // a retry *schedule* is an algebra:
        schedule::exponential(ms(10)).full_jitter().cap(ms(200))
            & schedule::recurs(5)                         //   & = both must agree (max delay)
            & schedule::within(ms(400)),                  //   | = either may continue (min delay)
    )
    .or(from_fn(|| profile_cache(user)))                  // fallback
    .ensure(|p| !p.name.is_empty(), |_| Error::Invalid);  // guard

let config = from_fn(|| mirror_eu()).race(from_fn(|| mirror_us()));  // hedged request

let recs = all(items.map(|i| from_fn(move || recommend(i))))
    .limit(4)                                             // bounded concurrency
    .settled();                                           // keep partial results

let mut dashboard = profile.zip(config).zip(recs);         // run all three in parallel
let ((profile, config), recs) = dashboard.run().await?;
```

A complete, runnable version lives in [`examples/dashboard.rs`](examples/dashboard.rs)
(`cargo run --example dashboard`).

## Why another one?

Rust already has excellent pieces: `futures` and `futures-concurrency` for joins and races,
`tokio::time` for timeouts, `backon`/`tokio-retry`/`relentless` for retries, `tower` for
service middleware, `failsafe` for circuit breakers. `counterpoint` aims at the gap between
them:

- **One vocabulary** for sequencing, parallelism, racing, timeouts, retries, fallbacks, guards,
  bounded concurrency and circuit breaking, composing freely with each other.
- **Laws, written down and tested.** Each combinator states the equations it satisfies
  (below), and `tests/laws.rs` / `tests/schedule_laws.rs` check them with `proptest` on
  tokio's paused clock — including *timing* laws such as "`zip` takes the max, `race` takes
  the min, `and_then` adds".
- **Retry schedules as an algebra**, in the spirit of ZIO's `Schedule`: `|` and `&` are the
  pointwise `min`/`max` of delays, so schedules form a distributive lattice with `stop()` and
  `forever()` as bounds, and `then` sequences them as a monoid.
- **Zero-cost and cancel-safe.** Ops are plain generic structs: no `Box<dyn>`, no spawning, no
  `'static` bound. Dropping a running op cancels everything inside it (losing race branches,
  sibling branches after a failure, in-flight attempts, backoff sleeps, breaker probes).
- **Typed errors.** Timeouts and an open breaker surface as `Elapsed` / `CircuitOpen`, which
  you convert into your own error type with `From`.

## The model

The central abstraction is the `Op` trait: a *re-runnable description* of a fallible async
computation (a Rust `Future` is one-shot, so it cannot be retried):

```rust
pub trait Op {
    type Ok;
    type Err;
    fn run(&mut self) -> impl Future<Output = Result<Self::Ok, Self::Err>>;
    // ... plus combinators as provided methods
}
```

Build ops with `from_fn(|| async { .. })` (any async closure), `succeed(v)`, `fail(e)` and
`never()`, then compose. Nothing runs until you `.run().await`.

## Laws

`≡` means *observationally equivalent*: same result **and** same virtual running time.
"Except on ties" means: unless both sides finish at the exact same instant.

| Combinator | Laws |
|---|---|
| `and_then` | `succeed(a).and_then(f) ≡ f(a)` · `m.and_then(succeed) ≡ m` · associative · durations add |
| `map` | `m.map(id) ≡ m` · `m.map(f).map(g) ≡ m.map(g ∘ f)` |
| `zip` | associative and commutative up to tuple isomorphism · unit `succeed(())` · duration is the max · fails fast |
| `join` | never fails · duration is the max |
| `race` | associative · identity `never()` · commutative except on ties · duration is the min |
| `or` | associative · left identity `fail(e)` · left zero `succeed(a)` · *no* right identity |
| `ensure` | `ensure(true) ≡ id` · `ensure(p).ensure(q) ≡ ensure(p ∧ q)` |
| `timeout` | `timeout(∞) ≡ id` · `timeout(t) ≡ id` when the op is faster · `timeout(a).timeout(b) ≡ timeout(min(a, b))` except on ties |
| `retry` | `retry(stop()) ≡ retry(recurs(0)) ≡ id` · `succeed(a).retry(s) ≡ succeed(a)` · `retry(recurs(n))` runs at most `n + 1` times · nested retries multiply |
| `all` | equals its model (all values in order, or the earliest error) · with `limit(k)` the makespan is greedy list scheduling on `k` slots |
| schedules | `\|`, `&`: associative, commutative, idempotent, absorptive, distributive · `stop()` / `forever()` are the bounds · `then`: associative with unit `stop()` · `cap(a).cap(b) ≡ cap(min(a, b))` · `jitter(0) ≡ id` |

## Related work

- [`futures`](https://crates.io/crates/futures) and
  [`futures-concurrency`](https://crates.io/crates/futures-concurrency): joins and races over
  futures, runtime-agnostic.
- [`backon`](https://crates.io/crates/backon), [`tokio-retry`](https://crates.io/crates/tokio-retry),
  [`relentless`](https://crates.io/crates/relentless): retries; `relentless` composes wait and stop
  strategies with operators.
- [`tower`](https://crates.io/crates/tower): retry/timeout/limit middleware for `Service`s.
- [`failsafe`](https://crates.io/crates/failsafe): circuit breaker.
- [`asupersync`](https://crates.io/crates/asupersync): a cancel-correct runtime whose combinators
  also come with stated laws.
- ZIO's `Schedule` and cats-effect's `IO` (Scala): the inspiration for re-runnable descriptions and
  for retry policies as an algebra.

## Feature flags

- `tracing` — emit `tracing` debug events for retries, timeouts, fallbacks and breaker
  transitions.

## Minimum supported Rust version

1.88 (async closures and let-chains, edition 2024).

## License

MIT — see [LICENSE](LICENSE).
