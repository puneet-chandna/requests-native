use std::fmt;
use std::sync::mpsc;
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::time::Duration;

use tokio::sync::oneshot;

/// Marker for payloads whose ownership may safely move to or be destroyed on
/// a runtime worker. Implementations are an explicit crate-local inventory;
/// Python handles and Python-controlled destructors must never implement it.
pub(crate) trait WorkerPayload: Send + 'static {}

/// Cross-thread wake signal shared by a pump loop, its action sender and the
/// completion path of the submitted future.
///
/// Notifications are counted rather than flagged, so a notification that
/// arrives while nobody is waiting stays observable for the next wait. That
/// keeps the pump loop free of polling: it sleeps until work actually exists.
pub(crate) struct WakeSignal {
    notifications: Mutex<u64>,
    waiter: Condvar,
}

impl WakeSignal {
    fn new() -> Self {
        Self {
            notifications: Mutex::new(0),
            waiter: Condvar::new(),
        }
    }

    pub(crate) fn notify(&self) {
        let mut notifications = self
            .notifications
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        *notifications = notifications.saturating_add(1);
        self.waiter.notify_one();
    }

    /// Wait for a notification or the timeout, returning whether one was
    /// consumed.
    pub(crate) fn wait_timeout(&self, timeout: Duration) -> bool {
        let mut notifications = self
            .notifications
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if *notifications > 0 {
            *notifications -= 1;
            return true;
        }
        let (mut notifications, _) = self
            .waiter
            .wait_timeout(notifications, timeout)
            .unwrap_or_else(PoisonError::into_inner);
        if *notifications > 0 {
            *notifications -= 1;
            return true;
        }
        false
    }
}

pub(crate) fn action_channel<A, R>() -> (ActionSender<A, R>, ActionReceiver<A, R>)
where
    A: WorkerPayload,
    R: WorkerPayload,
{
    let (sender, receiver) = mpsc::channel();
    let wake = Arc::new(WakeSignal::new());
    (
        ActionSender {
            sender,
            wake: Arc::clone(&wake),
        },
        ActionReceiver { receiver, wake },
    )
}

pub(crate) struct ActionSender<A, R> {
    sender: mpsc::Sender<ActionRequest<A, R>>,
    wake: Arc<WakeSignal>,
}

impl<A, R> Clone for ActionSender<A, R> {
    fn clone(&self) -> Self {
        Self {
            sender: self.sender.clone(),
            wake: Arc::clone(&self.wake),
        }
    }
}

impl<A, R> ActionSender<A, R>
where
    A: WorkerPayload,
    R: WorkerPayload,
{
    pub(crate) fn enqueue(&self, action: A) -> Result<oneshot::Receiver<R>, BridgeClosed> {
        let (reply, receive_reply) = oneshot::channel();
        self.sender
            .send(ActionRequest { action, reply })
            .map_err(|_| BridgeClosed::ActionReceiver)?;
        // Wake the pump loop after the request is visible, so a woken loop
        // always observes it.
        self.wake.notify();
        Ok(receive_reply)
    }

    pub(crate) async fn request(&self, action: A) -> Result<R, BridgeClosed> {
        let receive_reply = self.enqueue(action)?;
        receive_reply.await.map_err(|_| BridgeClosed::ReplySender)
    }
}

pub(crate) struct ActionReceiver<A, R> {
    receiver: mpsc::Receiver<ActionRequest<A, R>>,
    wake: Arc<WakeSignal>,
}

impl<A, R> ActionReceiver<A, R> {
    /// The wake signal notified by this channel's sender and by the completion
    /// path of an associated submission.
    pub(crate) fn wake(&self) -> Arc<WakeSignal> {
        Arc::clone(&self.wake)
    }

    pub(crate) fn try_recv(&self) -> Result<ActionRequest<A, R>, mpsc::TryRecvError> {
        self.receiver.try_recv()
    }

    pub(crate) fn recv_timeout(
        &mut self,
        timeout: Duration,
    ) -> Result<ActionRequest<A, R>, mpsc::RecvTimeoutError> {
        self.receiver.recv_timeout(timeout)
    }
}

pub(crate) struct ActionRequest<A, R> {
    action: A,
    reply: oneshot::Sender<R>,
}

impl<A, R> ActionRequest<A, R> {
    pub(crate) fn into_parts(self) -> (A, oneshot::Sender<R>) {
        (self.action, self.reply)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum BridgeClosed {
    ActionReceiver,
    ReplySender,
}

impl fmt::Display for BridgeClosed {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ActionReceiver => formatter.write_str("Python action receiver closed"),
            Self::ReplySender => formatter.write_str("Python action reply sender closed"),
        }
    }
}

impl std::error::Error for BridgeClosed {}

#[cfg(test)]
mod tests {
    use super::WorkerPayload;

    struct Unlisted;

    trait AmbiguousIfPayload<A> {
        fn marker() {}
    }

    impl<T: ?Sized> AmbiguousIfPayload<()> for T {}
    impl<T: ?Sized + WorkerPayload> AmbiguousIfPayload<u8> for T {}

    #[test]
    fn worker_payload_has_no_blanket_impl() {
        let _ = <Unlisted as AmbiguousIfPayload<_>>::marker;
    }
}
