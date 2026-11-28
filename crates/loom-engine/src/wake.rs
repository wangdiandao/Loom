//! Zero-polling event-driven snapshot and command-event wakeup signaling.
//!
//! [`SnapshotWakeSignal`] allows UI stores (such as GPUI foreground tasks) or
//! headless consumers to sleep with zero timer polling and wake up immediately
//! whenever [`crate::EngineCoordinator`] publishes a new [`loom_types::AppSnapshot`]
//! or emits a [`crate::CommandEvent`].

use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll, Waker};

use crossbeam_channel::{Receiver, Sender, TrySendError};
use parking_lot::Mutex;

type WakeCallback = Arc<dyn Fn() + Send + Sync>;

#[derive(Default)]
struct WakeState {
    pending: bool,
    closed: bool,
    wakers: Vec<Waker>,
    channels: Vec<Sender<()>>,
    callbacks: Vec<WakeCallback>,
}

/// Lock-light wakeup signal fired whenever [`crate::EngineCoordinator`] publishes
/// a new snapshot or emits a [`crate::CommandEvent`].
///
/// Supports three zero-polling consumption styles:
/// 1. Async `Future` via [`Self::wait`] (mirrors `HotkeyWakeSignal`/`TrayEventSignal` in `loom-app`).
/// 2. Bounded(1) [`crossbeam_channel::Receiver<()>`] via [`Self::subscribe_channel`].
/// 3. Direct non-blocking callback hook via [`Self::add_callback`].
///
/// Calling [`Self::notify`] is strictly non-blocking and invokes wakers/callbacks
/// outside the internal mutex so the fast-lane sampling thread never stalls on UI.
#[derive(Clone, Default)]
pub struct SnapshotWakeSignal {
    inner: Arc<Mutex<WakeState>>,
}

impl fmt::Debug for SnapshotWakeSignal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let state = self.inner.lock();
        f.debug_struct("SnapshotWakeSignal")
            .field("pending", &state.pending)
            .field("closed", &state.closed)
            .field("wakers_len", &state.wakers.len())
            .field("channels_len", &state.channels.len())
            .field("callbacks_len", &state.callbacks.len())
            .finish()
    }
}

impl SnapshotWakeSignal {
    /// Creates a new idle [`SnapshotWakeSignal`].
    pub fn new() -> Self {
        Self::default()
    }

    /// Signals all waiting async tasks, channel subscribers, and callbacks non-blockingly.
    pub fn notify(&self) {
        let callbacks = {
            let mut state = self.inner.lock();
            if state.closed {
                return;
            }
            state.pending = true;
            state.callbacks.clone()
        };

        // Invoke callbacks before unblocking channel/async waiters so any observer
        // woken via `subscribe_channel` or `wait` sees callback side effects immediately.
        for cb in callbacks {
            cb();
        }

        let wakers = {
            let mut state = self.inner.lock();
            if state.closed {
                return;
            }
            state.channels.retain(|tx| match tx.try_send(()) {
                Ok(()) | Err(TrySendError::Full(())) => true,
                Err(TrySendError::Disconnected(())) => false,
            });
            std::mem::take(&mut state.wakers)
        };

        for waker in wakers {
            waker.wake();
        }
    }

    /// Closes this signal on coordinator shutdown so awaiting tasks and channels exit cleanly.
    pub fn close(&self) {
        let wakers = {
            let mut state = self.inner.lock();
            state.closed = true;
            state.channels.clear();
            state.callbacks.clear();
            std::mem::take(&mut state.wakers)
        };
        for waker in wakers {
            waker.wake();
        }
    }

    /// Returns a [`Future`] that resolves to `true` on the next wakeup and `false` once closed.
    pub fn wait(&self) -> NextSnapshotWake {
        NextSnapshotWake {
            inner: Arc::clone(&self.inner),
        }
    }

    /// Subscribes a coalescing `bounded(1)` channel that receives `()` on each wakeup.
    ///
    /// Multiple rapid updates coalesce into a single queued `()` token without blocking
    /// the fast lane. When the coordinator shuts down, the sender is dropped and the
    /// receiver disconnects cleanly.
    pub fn subscribe_channel(&self) -> Receiver<()> {
        let (tx, rx) = crossbeam_channel::bounded(1);
        let mut state = self.inner.lock();
        if !state.closed {
            if state.pending {
                let _ = tx.try_send(());
            }
            state.channels.push(tx);
        }
        rx
    }

    /// Registers a non-blocking callback invoked whenever a snapshot or event is published.
    pub fn add_callback(&self, callback: WakeCallback) {
        let mut state = self.inner.lock();
        if !state.closed {
            state.callbacks.push(callback);
        }
    }
}

/// [`Future`] returned by [`SnapshotWakeSignal::wait`].
///
/// Resolves to `true` when new snapshot/event data is ready, or `false` once closed.
pub struct NextSnapshotWake {
    inner: Arc<Mutex<WakeState>>,
}

impl Future for NextSnapshotWake {
    type Output = bool;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut state = self.inner.lock();
        if state.pending {
            state.pending = false;
            return Poll::Ready(true);
        }
        if state.closed {
            return Poll::Ready(false);
        }
        state.wakers.push(cx.waker().clone());
        Poll::Pending
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::task::{RawWaker, RawWakerVTable};

    use super::*;

    fn counting_waker(counter: Arc<AtomicUsize>) -> Waker {
        fn clone_fn(ptr: *const ()) -> RawWaker {
            let arc = unsafe { Arc::from_raw(ptr.cast::<AtomicUsize>()) };
            let cloned = Arc::clone(&arc);
            let _ = Arc::into_raw(arc);
            RawWaker::new(Arc::into_raw(cloned).cast(), &VTABLE)
        }
        fn wake_fn(ptr: *const ()) {
            let arc = unsafe { Arc::from_raw(ptr.cast::<AtomicUsize>()) };
            arc.fetch_add(1, Ordering::AcqRel);
        }
        fn wake_by_ref_fn(ptr: *const ()) {
            let arc = unsafe { Arc::from_raw(ptr.cast::<AtomicUsize>()) };
            arc.fetch_add(1, Ordering::AcqRel);
            let _ = Arc::into_raw(arc);
        }
        fn drop_fn(ptr: *const ()) {
            let _ = unsafe { Arc::from_raw(ptr.cast::<AtomicUsize>()) };
        }
        static VTABLE: RawWakerVTable =
            RawWakerVTable::new(clone_fn, wake_fn, wake_by_ref_fn, drop_fn);
        let raw = RawWaker::new(Arc::into_raw(counter).cast(), &VTABLE);
        unsafe { Waker::from_raw(raw) }
    }

    #[test]
    fn wake_signal_notifies_futures_channels_and_callbacks_and_closes_cleanly() {
        let signal = SnapshotWakeSignal::new();
        let rx = signal.subscribe_channel();
        let cb_hits = Arc::new(AtomicUsize::new(0));
        let cb_counter = Arc::clone(&cb_hits);
        signal.add_callback(Arc::new(move || {
            cb_counter.fetch_add(1, Ordering::AcqRel);
        }));

        let wake_hits = Arc::new(AtomicUsize::new(0));
        let waker = counting_waker(Arc::clone(&wake_hits));
        let mut cx = Context::from_waker(&waker);

        let mut fut = signal.wait();
        assert_eq!(Pin::new(&mut fut).poll(&mut cx), Poll::Pending);

        // Multiple rapid notifications coalesce on bounded(1) channel and wake the future.
        signal.notify();
        signal.notify();
        assert_eq!(cb_hits.load(Ordering::Acquire), 2);
        assert_eq!(wake_hits.load(Ordering::Acquire), 1);
        assert_eq!(Pin::new(&mut fut).poll(&mut cx), Poll::Ready(true));
        assert!(rx.try_recv().is_ok());
        assert!(rx.try_recv().is_err(), "bounded(1) must coalesce wakeups");

        // Closing wakes pending future with `false` and disconnects channel.
        let mut fut2 = signal.wait();
        assert_eq!(Pin::new(&mut fut2).poll(&mut cx), Poll::Pending);
        signal.close();
        assert_eq!(Pin::new(&mut fut2).poll(&mut cx), Poll::Ready(false));
        assert!(rx.recv().is_err());
    }
}
