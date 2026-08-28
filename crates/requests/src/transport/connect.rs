use tokio::net::TcpStream;

use crate::session_runtime::{SessionCheckpoint, SessionRuntimeHarness};
use crate::{Error, Result};

pub(super) struct ConnectDialEntered {
    checkpoint: SessionCheckpoint,
}

pub(super) struct SessionInjectedConnectorGate {
    harness: SessionRuntimeHarness,
    entered: ConnectDialEntered,
}

impl SessionInjectedConnectorGate {
    pub(super) fn new(harness: SessionRuntimeHarness, checkpoint: SessionCheckpoint) -> Self {
        Self {
            harness,
            entered: ConnectDialEntered { checkpoint },
        }
    }

    async fn wait(self) {
        self.harness.wait(self.entered.checkpoint).await;
    }
}

pub(super) async fn connect_with_gate(
    host: &str,
    port: u16,
    target: &str,
    gate: Option<SessionInjectedConnectorGate>,
) -> Result<TcpStream> {
    if let Some(gate) = gate {
        gate.wait().await;
    }
    connect(host, port, target).await
}

pub(super) async fn connect(host: &str, port: u16, target: &str) -> Result<TcpStream> {
    let addresses = tokio::net::lookup_host((host, port))
        .await
        .map_err(|error| Error::dns_io(target, error))?;
    let mut attempted = false;
    let mut last_error = None;

    for address in addresses {
        attempted = true;
        match TcpStream::connect(address).await {
            Ok(stream) => return Ok(stream),
            Err(error) => last_error = Some(error),
        }
    }

    match (attempted, last_error) {
        (true, Some(error)) => Err(Error::connect_io(target, error)),
        _ => Err(Error::no_addresses(target)),
    }
}
