use tokio::net::TcpStream;

use crate::{Error, Result};

pub(super) async fn connect(host: &str, port: u16, target: &str) -> Result<TcpStream> {
    let addresses = tokio::net::lookup_host((host, port))
        .await
        .map_err(|error| Error::dns(target, error))?;
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
        (true, Some(error)) => Err(Error::connect(target, error)),
        _ => Err(Error::no_addresses(target)),
    }
}
