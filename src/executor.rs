//! Executors
//!
//! This module contains tools for managing the raw execution of futures,
//! which is needed when building *executors* (places where futures can run).
//!
//! More information about executors can be [found online at tokio.rs][online].
//!
//! [online]: https://tokio.rs/docs/going-deeper-futures/tasks/

#[allow(deprecated)]

pub use task_impl::{Spawn, spawn, Notify, with_notify};
pub use task_impl::{UnsafeNotify, NotifyHandle};

#[cfg(feature = "use_std")]
pub use self::std_support::*;

#[cfg(feature = "use_std")]
mod std_support {
    use std::cell::Cell;

    #[allow(deprecated)]
    pub use task_impl::{Unpark, Executor, Run};

    thread_local!(static ENTERED: Cell<bool> = Cell::new(false));

    /// A guard that prevents the current thread from entering multiple Executors.
    ///
    /// Created by the `enter` function.
    #[derive(Debug)]
    pub struct Guard {
        _priv: (),
    }

    /// Instantiates a guard that prevents the current thread from
    /// entering any other executor.
    pub fn enter() -> Guard {
        ENTERED.with(|c| {
            if c.get() {
                panic!("cannot reenter an executor context");
            }
            c.set(true);
        });

        Guard { _priv: () }
    }

    impl Drop for Guard {
        fn drop(&mut self) {
            ENTERED.with(|c| c.set(false));
        }
    }
}
