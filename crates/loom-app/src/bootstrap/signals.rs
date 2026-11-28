//! Zero-polling waker-backed wakeup signals bridging OS callbacks onto the GPUI loop.

use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context as TaskContext, Poll, Waker};

use parking_lot::Mutex;

#[derive(Default)]
struct HotkeySignalState {
    pending: bool,
    waker: Option<Waker>,
    closed: bool,
}

/// Zero-polling waker-backed wakeup signal delivering global hotkey presses onto the GPUI loop.
#[derive(Clone, Default)]
pub(super) struct HotkeyWakeSignal {
    inner: Arc<Mutex<HotkeySignalState>>,
}

impl HotkeyWakeSignal {
    pub(super) fn trigger(&self) {
        let waker = {
            let mut state = self.inner.lock();
            if state.closed {
                return;
            }
            state.pending = true;
            state.waker.take()
        };
        if let Some(waker) = waker {
            waker.wake();
        }
    }

    pub(super) fn close(&self) {
        let waker = {
            let mut state = self.inner.lock();
            state.closed = true;
            state.waker.take()
        };
        if let Some(waker) = waker {
            waker.wake();
        }
    }

    pub(super) fn next(&self) -> NextHotkeyWake {
        NextHotkeyWake {
            inner: Arc::clone(&self.inner),
        }
    }
}

pub(super) struct NextHotkeyWake {
    inner: Arc<Mutex<HotkeySignalState>>,
}

impl Future for NextHotkeyWake {
    type Output = bool;

    fn poll(self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<Self::Output> {
        let mut state = self.inner.lock();
        if state.pending {
            state.pending = false;
            return Poll::Ready(true);
        }
        if state.closed {
            return Poll::Ready(false);
        }
        state.waker = Some(cx.waker().clone());
        Poll::Pending
    }
}

#[derive(Default)]
struct ActivationSignalState {
    pending: VecDeque<Vec<String>>,
    waker: Option<Waker>,
    closed: bool,
}

/// Zero-polling waker-backed signal delivering single-instance activation payloads onto the GPUI loop.
#[derive(Clone, Default)]
pub(super) struct ActivationWakeSignal {
    inner: Arc<Mutex<ActivationSignalState>>,
}

impl ActivationWakeSignal {
    pub(super) fn push(&self, args: Vec<String>) {
        let waker = {
            let mut state = self.inner.lock();
            if state.closed {
                return;
            }
            if state.pending.len() < 16 {
                state.pending.push_back(args);
            }
            state.waker.take()
        };
        if let Some(waker) = waker {
            waker.wake();
        }
    }

    pub(super) fn close(&self) {
        let waker = {
            let mut state = self.inner.lock();
            state.closed = true;
            state.waker.take()
        };
        if let Some(waker) = waker {
            waker.wake();
        }
    }

    pub(super) fn next(&self) -> NextActivationWake {
        NextActivationWake {
            inner: Arc::clone(&self.inner),
        }
    }
}

pub(super) struct NextActivationWake {
    inner: Arc<Mutex<ActivationSignalState>>,
}

impl Future for NextActivationWake {
    type Output = Option<Vec<String>>;

    fn poll(self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<Self::Output> {
        let mut state = self.inner.lock();
        if let Some(args) = state.pending.pop_front() {
            return Poll::Ready(Some(args));
        }
        if state.closed {
            return Poll::Ready(None);
        }
        state.waker = Some(cx.waker().clone());
        Poll::Pending
    }
}
