//! Observability plumbing. With the `tracing` feature every interesting event (retry,
//! timeout, fallback, breaker transition) is emitted as a `tracing` debug event; without it
//! the macro compiles to nothing.

#[cfg(feature = "tracing")]
macro_rules! event {
    ($($arg:tt)*) => { ::tracing::debug!($($arg)*) };
}

#[cfg(not(feature = "tracing"))]
macro_rules! event {
    ($($arg:tt)*) => {
        if false {
            let _ = ::core::format_args!($($arg)*);
        }
    };
}

pub(crate) use event;
