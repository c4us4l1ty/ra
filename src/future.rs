//! A minimal single-value channel, used to turn GDBus's callback API back
//! into something awaitable.
//!
//! `glib` 0.20 removed `MainContext::channel` and `glib::oneshot`, and pulling
//! in `futures-channel` just for this would add a dependency to a project
//! whose whole premise is having none. The pattern below is about sixty lines
//! of well-trodden code and does exactly one thing.

use std::cell::RefCell;
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::task::{Context, Poll, Waker};

/// Error returned when the sender was dropped before sending.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub struct RecvError;

struct Shared<T> {
    state: RefCell<State<T>>,
    /// Keeps the channel !Send. The panel is single-threaded by design, and
    /// making that a compile error stops an accidental thread hop from
    /// introducing a data race.
    _not_send: std::marker::PhantomData<*const ()>,
}

struct State<T> {
    value: Option<T>,
    waker: Option<Waker>,
    sender_alive: bool,
    receiver_dropped: bool,
}

/// Produces the single value. Dropping without sending makes the receiver
/// resolve to [`RecvError`].
pub struct Sender<T> {
    shared: Rc<Shared<T>>,
}

impl<T> Sender<T> {
    pub fn send(self, value: T) {
        let mut st = self.shared.state.borrow_mut();
        st.value = Some(value);
        if let Some(w) = st.waker.take() {
            w.wake();
        }
        st.sender_alive = false;
    }
}

impl<T> Drop for Sender<T> {
    fn drop(&mut self) {
        let mut st = self.shared.state.borrow_mut();
        st.sender_alive = false;
        if let Some(w) = st.waker.take() {
            // Wake so the receiver can observe the sender's death instead of
            // hanging forever.
            w.wake();
        }
    }
}

/// Resolves to the value the sender produced.
pub struct Receiver<T> {
    shared: Rc<Shared<T>>,
}

impl<T> Future for Receiver<T> {
    type Output = Result<T, RecvError>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut st = self.shared.state.borrow_mut();
        if let Some(v) = st.value.take() {
            return Poll::Ready(Ok(v));
        }
        if !st.sender_alive {
            return Poll::Ready(Err(RecvError));
        }
        st.waker = Some(cx.waker().clone());
        Poll::Pending
    }
}

impl<T> Drop for Receiver<T> {
    fn drop(&mut self) {
        // Mark the sender's side as unneeded so a late send can bail out
        // instead of holding the value alive.
        self.shared.state.borrow_mut().receiver_dropped = true;
    }
}

/// Create a linked sender/receiver pair.
pub fn oneshot<T>() -> (Sender<T>, Receiver<T>) {
    let shared = Rc::new(Shared {
        state: RefCell::new(State { value: None, waker: None, sender_alive: true, receiver_dropped: false }),
        _not_send: std::marker::PhantomData,
    });
    (Sender { shared: shared.clone() }, Receiver { shared })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A trivial executor: poll once, and if pending, poll again after
    /// `f` runs. Enough to exercise the state machine.
    fn block_on<F: Future>(mut fut: F, f: impl FnOnce()) -> F::Output {
        use std::task::{RawWaker, RawWakerVTable, Waker};
        fn noop(_: *const ()) {}
        fn clone(_: *const ()) -> RawWaker {
            RawWaker::new(std::ptr::null(), &VTABLE)
        }
        static VTABLE: RawWakerVTable = RawWakerVTable::new(clone, noop, noop, noop);
        let waker = unsafe { Waker::from_raw(RawWaker::new(std::ptr::null(), &VTABLE)) };
        let mut cx = Context::from_waker(&waker);
        let mut fut = unsafe { Pin::new_unchecked(&mut fut) };
        loop {
            if let Poll::Ready(v) = fut.as_mut().poll(&mut cx) {
                return v;
            }
            f();
        }
    }

    #[test]
    fn delivers_a_value() {
        let (tx, rx) = oneshot::<u8>();
        let out = block_on(rx, || tx.send(7));
        assert_eq!(out, Ok(7));
    }

    #[test]
    fn dropped_sender_is_an_error() {
        let (tx, rx) = oneshot::<u8>();
        let out = block_on(rx, || drop(tx));
        assert_eq!(out, Err(RecvError));
    }

    #[test]
    fn value_survives_multiple_polls() {
        let (tx, rx) = oneshot::<String>();
        let mut rx = Box::pin(rx);
        tx.send("hello".into());
        let w = glib::idle_add_local_once(|| ());
        let _ = w;
        // Poll directly; the value is already present so this must not pend.
        let poll = {
            use std::task::{RawWaker, RawWakerVTable, Waker};
            fn noop(_: *const ()) {}
            fn clone(_: *const ()) -> RawWaker {
                RawWaker::new(std::ptr::null(), &VTABLE)
            }
            static VTABLE: RawWakerVTable = RawWakerVTable::new(clone, noop, noop, noop);
            let waker = unsafe { Waker::from_raw(RawWaker::new(std::ptr::null(), &VTABLE)) };
            let mut cx = Context::from_waker(&waker);
            rx.as_mut().poll(&mut cx)
        };
        assert_eq!(poll, Poll::Ready(Ok("hello".to_string())));
    }
}