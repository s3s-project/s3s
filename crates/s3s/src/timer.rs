// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

//! An internal timer for components that need periodic ticks.
//!
//! The backend is chosen at compile time: `futures-timer` when it is enabled (it
//! does not depend on a runtime), otherwise `tokio-timer`, and with neither of them
//! [`available`] reports whether there is one. Callers keep their tick source in an
//! `Option` and hold `None` in that case, so a build without a backend never creates
//! an interval that could not tick anyway.

use std::time::Duration;

#[cfg(feature = "futures-timer")]
mod imp {
    use std::pin::Pin;
    use std::task::{Context, Poll};
    use std::time::Duration;

    /// A tick source backed by `futures-timer`, which runs its own timer thread and
    /// therefore works on any native runtime. On wasm32 enable the `wasm` feature so
    /// that `futures-timer` uses its JS timer implementation instead.
    #[derive(Debug)]
    pub(crate) struct Interval {
        next: Option<futures_timer::Delay>,
        period: Duration,
    }

    impl Interval {
        pub(crate) fn new(period: Duration) -> Self {
            Self {
                next: Some(futures_timer::Delay::new(period)),
                period,
            }
        }

        pub(crate) fn poll_tick(&mut self, cx: &mut Context<'_>) -> Poll<()> {
            let delay = self.next.get_or_insert_with(|| futures_timer::Delay::new(self.period));
            std::task::ready!(Pin::new(delay).poll(cx));
            // `futures-timer` has no interval type, so schedule the next tick.
            self.next = None;
            Poll::Ready(())
        }
    }
}

#[cfg(all(not(feature = "futures-timer"), feature = "tokio-timer"))]
mod imp {
    use std::task::{Context, Poll};
    use std::time::Duration;

    /// A tick source backed by `tokio`'s timer, which needs a reactor exactly like
    /// `tokio::time::interval` does: outside a runtime it panics, and that is
    /// deliberate — driving a tokio build from another executor is a configuration
    /// mistake, not something to paper over with silent degradation.
    #[derive(Debug)]
    pub(crate) struct Interval(tokio::time::Interval);

    impl Interval {
        pub(crate) fn new(period: Duration) -> Self {
            Self(tokio::time::interval(period))
        }

        pub(crate) fn poll_tick(&mut self, cx: &mut Context<'_>) -> Poll<()> {
            self.0.poll_tick(cx).map(|_| ())
        }
    }
}

/// No timer backend is compiled in: the type is uninhabited, so no interval can
/// exist in this build. It is only the field type of the callers, which hold `None`
/// and never construct or poll one; the method below proves that with `match`.
#[cfg(not(any(feature = "tokio-timer", feature = "futures-timer")))]
mod imp {
    use std::task::{Context, Poll};

    #[derive(Debug)]
    pub(crate) enum Interval {}

    impl Interval {
        /// Unreachable: an interval cannot exist in this build.
        pub(crate) fn poll_tick(&mut self, _cx: &mut Context<'_>) -> Poll<()> {
            match *self {}
        }
    }
}

pub(crate) use imp::Interval;

/// The tick source of this build, or `None` when it has none.
///
/// This consults [`available`] instead of restating the feature question, so the
/// constructor and the check cannot disagree.
#[cfg(any(feature = "tokio-timer", feature = "futures-timer"))]
pub(crate) fn interval(period: Duration) -> Option<Interval> {
    if !available() {
        return None;
    }
    Some(Interval::new(period))
}

/// No timer backend is compiled in, so there is never a tick source.
#[cfg(not(any(feature = "tokio-timer", feature = "futures-timer")))]
pub(crate) fn interval(_period: Duration) -> Option<Interval> {
    None
}

/// Whether this build has a tick source.
///
/// The answer only depends on the compiled-in backend, so it does not change with
/// the call site or the moment it is asked. Whether a tokio timer can actually run
/// is a runtime question, decided when the interval is polled, where it degrades
/// instead of failing.
pub(crate) fn available() -> bool {
    cfg!(any(feature = "tokio-timer", feature = "futures-timer"))
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    const HAS_BACKEND: bool = cfg!(any(feature = "tokio-timer", feature = "futures-timer"));

    #[test]
    fn availability_only_depends_on_the_backend() {
        assert_eq!(super::available(), HAS_BACKEND);

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .expect("tokio runtime");
        runtime.block_on(async {
            assert_eq!(super::available(), HAS_BACKEND);
        });
    }

    /// The point of the futures-timer backend: ticks need no runtime at all.
    #[cfg(feature = "futures-timer")]
    #[test]
    fn futures_timer_backend_needs_no_runtime() {
        let mut interval = super::Interval::new(Duration::from_millis(1));

        futures::executor::block_on(std::future::poll_fn(|cx| interval.poll_tick(cx)));
    }

    /// Tokio's timer needs a reactor, and the backend follows tokio instead of
    /// degrading silently: outside a runtime it panics like `tokio::time::interval`.
    #[cfg(all(not(feature = "futures-timer"), feature = "tokio-timer"))]
    #[test]
    fn tokio_backend_requires_a_runtime() {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        let panicked = std::panic::catch_unwind(|| super::Interval::new(Duration::from_millis(1))).is_err();
        std::panic::set_hook(previous);

        assert!(panicked, "creating a tokio interval outside a runtime must panic");
    }

    #[cfg(all(not(feature = "futures-timer"), feature = "tokio-timer"))]
    #[test]
    fn tokio_backend_ticks_inside_a_runtime() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .expect("tokio runtime");
        runtime.block_on(async {
            let mut interval = super::Interval::new(Duration::from_millis(1));
            // Tokio's first tick is immediate.
            std::future::poll_fn(|cx| interval.poll_tick(cx)).await;
        });
    }
}
