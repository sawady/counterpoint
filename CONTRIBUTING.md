# Contributing to counterpoint

Thanks for your interest! counterpoint is an early prototype, so bug reports, questions and
design discussion are as welcome as code.

## Issues

GitHub issues are welcome: bugs, surprising behaviour (especially around cancellation and
timing), a law that looks wrong or missing, or an API that is awkward to use. For a new
combinator or a change to an existing one, please open an issue first: every combinator comes
with laws, and it is easier to agree on them before writing code.

## Building and testing

You need Rust 1.88 or newer (the MSRV).

```sh
cargo test --all-features          # laws, behaviour tests and doctests
cargo test --no-default-features
cargo run --example dashboard
cargo fmt --all --check
cargo clippy --all-targets --all-features
cargo doc --no-deps --all-features
```

CI runs all of the above on stable and on 1.88, with warnings denied
(`RUSTFLAGS=-D warnings`).

## Style and rules

- Format with `rustfmt` and keep `clippy` quiet.
- Every combinator states its laws in its documentation and in the laws tables (crate docs
  and README), and
  `tests/laws.rs` or `tests/schedule_laws.rs` checks them with `proptest`.
- Tests that involve time run on tokio's paused clock (`tokio::time::pause`), never on the
  wall clock, so they are fast and deterministic.
- Behaviour that is not a law (cancellation, concurrency limits, hooks) gets a test in
  `tests/behaviour.rs`.
- No `Box<dyn>`, spawning or `'static` bounds in the combinators unless an issue discussed why.

## License

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion
in the work by you, as defined in the Apache-2.0 license, shall be dual licensed under
[MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE), without any additional terms or
conditions.
