use std::fmt;
use std::sync::mpsc;
use std::time::Duration;

use tokio::sync::oneshot;

pub(crate) fn action_channel<A, R>() -> (ActionSender<A, R>, ActionReceiver<A, R>) {
    let (sender, receiver) = mpsc::channel();
    (ActionSender { sender }, ActionReceiver { receiver })
}

pub(crate) struct ActionSender<A, R> {
    sender: mpsc::Sender<ActionRequest<A, R>>,
}

impl<A, R> ActionSender<A, R> {
    pub(crate) async fn request(&self, action: A) -> Result<R, BridgeClosed> {
        let (reply, receive_reply) = oneshot::channel();
        self.sender
            .send(ActionRequest { action, reply })
            .map_err(|_| BridgeClosed::ActionReceiver)?;
        receive_reply.await.map_err(|_| BridgeClosed::ReplySender)
    }
}

pub(crate) struct ActionReceiver<A, R> {
    receiver: mpsc::Receiver<ActionRequest<A, R>>,
}

impl<A, R> ActionReceiver<A, R> {
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
