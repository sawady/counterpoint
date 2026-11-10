//! Deterministic costs: size of the future and number of heap allocations per run. Unlike the
//! timing benchmarks these do not depend on the machine.
//!
//! `cargo bench --bench footprint`

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::future::ready;
use std::hint::black_box;
use std::mem::size_of_val;
use std::pin::pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use backon::{ConstantBuilder, Retryable};
use counterpoint::prelude::*;
use futures::future::{self, Either};
use futures::stream::{self, StreamExt};
use tokio::runtime::{Builder, Runtime};

struct Counting;

static ALLOCS: AtomicU64 = AtomicU64::new(0);

// SAFETY: forwards everything to `System`.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        ALLOCS.fetch_add(1, Ordering::Relaxed);
        unsafe { System.alloc(l) }
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        unsafe { System.dealloc(p, l) }
    }
    unsafe fn realloc(&self, p: *mut u8, l: Layout, n: usize) -> *mut u8 {
        ALLOCS.fetch_add(1, Ordering::Relaxed);
        unsafe { System.realloc(p, l, n) }
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

#[derive(Debug, Clone, Copy)]
struct E;

impl From<Elapsed> for E {
    fn from(_: Elapsed) -> Self {
        E
    }
}

const FAR: Duration = Duration::from_secs(3600);

async fn leaf(x: u64) -> Result<u64, E> {
    Ok(black_box(x))
}

async fn yielding(x: u64) -> Result<u64, E> {
    tokio::task::yield_now().await;
    Ok(black_box(x))
}

/// Heap allocations of `block_on` for an empty future, to subtract from every row.
fn baseline(rt: &Runtime) -> u64 {
    rt.block_on(async {});
    let before = ALLOCS.load(Ordering::Relaxed);
    rt.block_on(async {});
    ALLOCS.load(Ordering::Relaxed) - before
}

/// Print the size of the future and the allocations of a later (warmed-up) run.
macro_rules! row {
    ($rt:expr, $name:literal, $fut:expr) => {{
        let rt: &Runtime = $rt;
        let size = size_of_val(&$fut);
        rt.block_on($fut).expect("the scenario succeeds");
        let before = ALLOCS.load(Ordering::Relaxed);
        rt.block_on($fut).expect("the scenario succeeds");
        let allocs = ALLOCS.load(Ordering::Relaxed) - before;
        println!(
            "| {} | {size} | {} |",
            $name,
            allocs.saturating_sub(baseline(rt))
        );
    }};
}

fn main() {
    let rt = &Builder::new_current_thread().enable_time().build().unwrap();
    let backoff = ConstantBuilder::default()
        .with_delay(Duration::ZERO)
        .with_max_times(3);

    println!("| scenario | future size (bytes) | heap allocations per run |");
    println!("|---|---|---|");

    row!(rt, "leaf / direct", leaf(1));
    let mut op = from_fn(|| leaf(1));
    row!(rt, "leaf / from_fn", op.run());

    row!(
        rt,
        "zip / futures::try_join",
        future::try_join(yielding(1), yielding(2))
    );
    let mut op = from_fn(|| yielding(1)).zip(from_fn(|| yielding(2)));
    row!(rt, "zip / counterpoint", op.run());

    row!(rt, "race / futures::select", async {
        let a = pin!(yielding(1));
        let b = pin!(yielding(2));
        match future::select(a, b).await {
            Either::Left((r, _)) | Either::Right((r, _)) => r,
        }
    });
    let mut op = from_fn(|| yielding(1)).race(from_fn(|| yielding(2)));
    row!(rt, "race / counterpoint", op.run());

    row!(
        rt,
        "all(16) / futures::try_join_all",
        future::try_join_all((0..16).map(leaf))
    );
    let mut op = all((0..16).map(|i| from_fn(move || leaf(i))));
    row!(rt, "all(16) / counterpoint", op.run());

    row!(rt, "all(16).limit(4) / futures::buffered", async {
        let out: Vec<_> = stream::iter((0..16).map(leaf)).buffered(4).collect().await;
        out.into_iter().collect::<Result<Vec<_>, E>>()
    });
    let mut op = all((0..16).map(|i| from_fn(move || leaf(i)))).limit(4);
    row!(rt, "all(16).limit(4) / counterpoint", op.run());

    row!(rt, "timeout / tokio", async {
        tokio::time::timeout(FAR, leaf(1)).await.map_err(|_| E)?
    });
    let mut op = from_fn(|| leaf(1)).timeout(FAR);
    row!(rt, "timeout / counterpoint", op.run());

    row!(
        rt,
        "retry, no failure / backon",
        (|| leaf(1)).retry(backoff)
    );
    let mut op = from_fn(|| leaf(1)).retry(schedule::recurs(3));
    row!(rt, "retry, no failure / counterpoint", op.run());

    // Attempt counts are checked here so the timing benchmarks compare the same work.
    let calls = Cell::new(0u32);
    let flaky = || {
        let n = calls.get();
        calls.set(n + 1);
        async move { if n < 3 { Err(E) } else { Ok(u64::from(n)) } }
    };
    row!(rt, "retry, 3 failures / backon (no-op sleeper)", {
        calls.set(0);
        flaky.retry(backoff).sleep(|_| ready(()))
    });
    assert_eq!(calls.get(), 4, "backon must make 1 + 3 attempts");
    let mut op = from_fn(flaky).retry(schedule::recurs(3));
    row!(rt, "retry, 3 failures / counterpoint", {
        calls.set(0);
        op.run()
    });
    assert_eq!(calls.get(), 4, "counterpoint must make 1 + 3 attempts");
}
