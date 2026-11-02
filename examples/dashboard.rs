//! Loading a user dashboard from several unreliable backends.
//!
//! - the profile comes from a flaky primary (timeout + jittered exponential retries, bounded
//!   by a time budget), falling back to a stale cache;
//! - the config is fetched from two mirrors at once and the fastest answer wins (hedging);
//! - 12 recommendation items are fetched with at most 4 requests in flight, keeping partial
//!   results;
//! - the payments service sits behind a circuit breaker.
//!
//! Profile, config and recommendations are loaded in parallel. Run it with
//! `cargo run --example dashboard`.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use counterpoint::prelude::*;

#[derive(Debug, Clone, PartialEq)]
enum Error {
    Timeout,
    Unavailable(&'static str),
    CircuitOpen,
    Invalid(String),
}

impl From<Elapsed> for Error {
    fn from(_: Elapsed) -> Self {
        Error::Timeout
    }
}

impl From<CircuitOpen> for Error {
    fn from(_: CircuitOpen) -> Self {
        Error::CircuitOpen
    }
}

fn ms(n: u64) -> Duration {
    Duration::from_millis(n)
}

// ----- simulated backends ---------------------------------------------------------------------

/// Fails the first two calls, and the third one hangs for a while.
async fn profile_primary(calls: Arc<AtomicU32>, user: u32) -> Result<String, Error> {
    match calls.fetch_add(1, Ordering::SeqCst) {
        0 | 1 => {
            tokio::time::sleep(ms(20)).await;
            Err(Error::Unavailable("profile-db"))
        }
        2 => {
            tokio::time::sleep(ms(500)).await; // will hit the timeout
            Ok(format!("user-{user}"))
        }
        _ => {
            tokio::time::sleep(ms(30)).await;
            Ok(format!("user-{user}"))
        }
    }
}

async fn profile_cache(user: u32) -> Result<String, Error> {
    tokio::time::sleep(ms(2)).await;
    Ok(format!("user-{user} (stale)"))
}

async fn config_mirror(name: &'static str, latency: u64) -> Result<String, Error> {
    tokio::time::sleep(ms(latency)).await;
    Ok(format!("config from {name}"))
}

async fn recommendation(item: u32) -> Result<String, Error> {
    tokio::time::sleep(ms(25 + u64::from(item % 3) * 10)).await;
    if item % 5 == 4 {
        Err(Error::Unavailable("recs"))
    } else {
        Ok(format!("item-{item}"))
    }
}

async fn charge(amount: u32) -> Result<u32, Error> {
    tokio::time::sleep(ms(5)).await;
    if amount > 100 {
        Err(Error::Unavailable("payments"))
    } else {
        Ok(amount)
    }
}

// ----- orchestration --------------------------------------------------------------------------

#[tokio::main]
async fn main() {
    let start = Instant::now();
    let user = 7;

    let calls = Arc::new(AtomicU32::new(0));
    let profile = from_fn({
        let calls = calls.clone();
        move || profile_primary(calls.clone(), user)
    })
    .timeout(ms(100))
    .retry(
        schedule::exponential(ms(10)).full_jitter().cap(ms(200))
            & schedule::recurs(5)
            & schedule::within(ms(400)),
    )
    .on_retry(|e: &Error, n, d| println!("  profile: retry #{n} in {d:?} after {e:?}"))
    .or(from_fn(move || profile_cache(user)))
    .ensure(
        |name| !name.is_empty(),
        |_| Error::Invalid("empty profile".into()),
    );

    let config = from_fn(|| config_mirror("eu-mirror", 40))
        .race(from_fn(|| config_mirror("us-mirror", 15)))
        .timeout(ms(200));

    let recommendations = all((0..12).map(|i| from_fn(move || recommendation(i))))
        .limit(4)
        .settled()
        .map(|results| {
            results
                .into_iter()
                .filter_map(Result::ok)
                .collect::<Vec<_>>()
        })
        .map_err(|never| match never {});

    let mut dashboard = profile.zip(config).zip(recommendations);

    match dashboard.run().await {
        Ok(((profile, config), recs)) => {
            println!("profile: {profile}");
            println!("config:  {config}");
            println!("recs:    {} of 12 -> {recs:?}", recs.len());
        }
        Err(e) => println!("dashboard failed: {e:?}"),
    }
    println!("primary was called {} times", calls.load(Ordering::SeqCst));
    println!("dashboard loaded in {:?}\n", start.elapsed());

    // Payments behind a circuit breaker: two failures open it, then calls fail fast.
    let breaker = CircuitBreaker::new(2, ms(50));
    for amount in [10, 500, 600, 20, 30] {
        let mut pay = from_fn(move || charge(amount)).with_breaker(breaker.clone());
        println!(
            "charge {amount:>3}: {:?} (breaker now {:?})",
            pay.run().await,
            breaker.state()
        );
    }
    tokio::time::sleep(ms(50)).await;
    let mut pay = from_fn(|| charge(40)).with_breaker(breaker.clone());
    println!(
        "after cool-down, charge 40: {:?} (breaker now {:?})",
        pay.run().await,
        breaker.state()
    );
}
