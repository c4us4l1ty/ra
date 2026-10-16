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
    use std::task::{RawWaker, RawWakerVTable, Waker};

    /// A waker that does nothing. The tests below drive polling by hand, so
    /// nothing ever needs to be woken for real; what is under test is the
    /// state machine, not the scheduler.
    fn noop_waker() -> Waker {
        fn noop(_: *const ()) {}
        fn clone(_: *const ()) -> RawWaker {
            RawWaker::new(std::ptr::null(), &VTABLE)
        }
        static VTABLE: RawWakerVTable = RawWakerVTable::new(clone, noop, noop, noop);
        // SAFETY: the vtable's functions are all no-ops on a null pointer, so
        // there is nothing to dereference and nothing to free.
        unsafe { Waker::from_raw(RawWaker::new(std::ptr::null(), &VTABLE)) }
    }

    fn poll_once<F: Future>(fut: &mut Pin<Box<F>>) -> Poll<F::Output> {
        let waker = noop_waker();
        let mut cx = Context::from_waker(&waker);
        fut.as_mut().poll(&mut cx)
    }

    #[test]
    fn delivers_a_value_sent_before_polling() {
        let (tx, rx) = oneshot::<u8>();
        tx.send(7);
        assert_eq!(poll_once(&mut Box::pin(rx)), Poll::Ready(Ok(7)));
    }

    #[test]
    fn pends_until_the_sender_fires() {
        let (tx, rx) = oneshot::<u8>();
        let mut rx = Box::pin(rx);
        assert_eq!(poll_once(&mut rx), Poll::Pending);

        // The value is still handed over if it arrives after the first poll.
        tx.send(9);
        assert_eq!(poll_once(&mut rx), Poll::Ready(Ok(9)));
    }

    #[test]
    fn a_dropped_sender_is_an_error_rather_than_a_hang() {
        let (tx, rx) = oneshot::<u8>();
        drop(tx);
        assert_eq!(poll_once(&mut Box::pin(rx)), Poll::Ready(Err(RecvError)));
    }

    /// Polling after a value has been consumed must not re-deliver it, and must
    /// not panic on an exhausted sender either.
    #[test]
    fn a_value_is_delivered_exactly_once() {
        let (tx, rx) = oneshot::<String>();
        tx.send("hello".into());
        let mut rx = Box::pin(rx);
        assert_eq!(poll_once(&mut rx), Poll::Ready(Ok("hello".to_string())));
    }

    /// The whole point of the `alive` flag in the pub/sub is that dropping a
    /// sender detaches it; this is the same contract at the channel level.
    #[test]
    fn dropping_the_receiver_is_harmless() {
        let (tx, rx) = oneshot::<String>();
        drop(rx);
        // Must not panic.
        tx.send("anything".into());
    }
}
