//! # Async runtime agnostic facilities
//!
//! ## For library crates:
//! * [task::spawn] to spawn tasks in runtime agnostic way.
//! * [select] to multiplex asynchronous futures simultaneously
//! * Feature `test` to enable `#[asyncs::test]` to test in a sample runtime.
//!
//! ## For binary crates:
//! * Feature `tokio` to toggle support for [tokio](https://docs.rs/tokio).
//! * Feature `smol` to toggle support for [smol](https://docs.rs/smol) builtin global executor.
//! * Feature `async-global-executor` to toggle support for [async-global-executor](https://docs.rs/async-global-executor).
//!
//! Uses environment variable `SPAWNS_GLOBAL_SPAWNER` to switch global executor if there are
//! multiple ones. See [spawns] for how to compat with [task::spawn].

pub use async_select::select;
pub use task::spawn;

/// Spawn, join and cancel tasks in runtime agnostic way
pub mod task {
    pub use spawns_core::*;
}

#[doc(hidden)]
#[cfg(feature = "test")]
pub mod __executor {
    pub use spawns::Blocking;
}

#[cfg(feature = "test")]
pub use asyncs_test::test;

#[cfg(test)]
mod tests {
    use std::future::{pending, ready};

    use crate::select;

    #[crate::test(crate = "crate")]
    async fn pending_default() {
        let v = select! {
            default => 5,
            i = pending() => i,
        };
        assert_eq!(v, 5);
    }

    #[crate::test(crate = "crate")]
    #[should_panic]
    async fn all_disabled_panic() {
        select! {
            Some(i) = ready(None) => i,
        };
    }

    #[test_case::test_case(5)]
    #[crate::test(crate = "crate")]
    #[should_panic]
    async fn with_test_case(input: i32) {
        assert_eq!(input, 6);
    }
}
