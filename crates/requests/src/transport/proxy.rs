use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::pin::Pin;
use std::task::{Context, Poll};

use http::HeaderValue;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;

use super::tls::LoadedTls;
use crate::{Error, Proxy, Result};

const MAX_RESPONSE_HEAD: usize = 16 * 1024;

pub(super) enum ProxyStream {
    Plain(TcpStream),
    Tls(Box<tokio_rustls::client::TlsStream<TcpStream>>),
}

impl AsyncRead for ProxyStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffer: &mut tokio::io::ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        match &mut *self {
            Self::Plain(stream) => Pin::new(stream).poll_read(context, buffer),
            Self::Tls(stream) => Pin::new(stream).poll_read(context, buffer),
        }
    }
}

impl AsyncWrite for ProxyStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffer: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        match &mut *self {
            Self::Plain(stream) => Pin::new(stream).poll_write(context, buffer),
            Self::Tls(stream) => Pin::new(stream).poll_write(context, buffer),
        }
    }

    fn poll_flush(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
    ) -> Poll<std::io::Result<()>> {
        match &mut *self {
            Self::Plain(stream) => Pin::new(stream).poll_flush(context),
            Self::Tls(stream) => Pin::new(stream).poll_flush(context),
        }
    }

    fn poll_shutdown(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
    ) -> Poll<std::io::Result<()>> {
        match &mut *self {
            Self::Plain(stream) => Pin::new(stream).poll_shutdown(context),
            Self::Tls(stream) => Pin::new(stream).poll_shutdown(context),
        }
    }
}

pub(super) struct Endpoint {
    pub(super) host: String,
    pub(super) port: u16,
}

pub(super) fn endpoint(proxy: &Proxy) -> Result<Endpoint> {
    let parsed = parse(proxy)?;
    Ok(Endpoint {
        host: parsed
            .host_str()
            .ok_or_else(|| Error::invalid_proxy("proxy URL has no host"))?
            .to_owned(),
        port: parsed.port_or_known_default().unwrap_or(1080),
    })
}

pub(super) fn needs_tls(proxy: &Proxy) -> bool {
    matches!(proxy, Proxy::Https(_))
}

pub(super) fn uses_absolute_form(proxy: Option<&Proxy>, target_is_https: bool) -> bool {
    !target_is_https && matches!(proxy, Some(Proxy::Http(_) | Proxy::Https(_)))
}

pub(super) fn authorization(proxy: &Proxy) -> Result<Option<HeaderValue>> {
    if !matches!(proxy, Proxy::Http(_) | Proxy::Https(_)) {
        return Ok(None);
    }
    let parsed = parse(proxy)?;
    if parsed.username().is_empty() {
        return Ok(None);
    }
    let username = percent_decode(parsed.username())?;
    let password = percent_decode(parsed.password().unwrap_or_default())?;
    let encoded = base64(format!("{username}:{password}").as_bytes());
    HeaderValue::from_str(&format!("Basic {encoded}"))
        .map(Some)
        .map_err(Error::invalid_proxy)
}

pub(super) async fn establish(
    proxy: &Proxy,
    stream: TcpStream,
    proxy_tls: Option<LoadedTls>,
    target_host: &str,
    target_port: u16,
    target_is_https: bool,
) -> Result<ProxyStream> {
    let mut stream = match proxy {
        Proxy::Https(_) => {
            let endpoint = endpoint(proxy)?;
            let loaded =
                proxy_tls.ok_or_else(|| Error::proxy("HTTPS proxy TLS was not configured"))?;
            ProxyStream::Tls(Box::new(
                super::tls::handshake(loaded, &endpoint.host, stream).await?,
            ))
        }
        _ => ProxyStream::Plain(stream),
    };

    match proxy {
        Proxy::Http(_) | Proxy::Https(_) if target_is_https => {
            http_connect(&mut stream, proxy, target_host, target_port).await?
        }
        Proxy::Http(_) | Proxy::Https(_) => {}
        Proxy::Socks4(uri) => {
            let remote_dns = uri
                .scheme_str()
                .is_some_and(|scheme| scheme.eq_ignore_ascii_case("socks4a"));
            socks4_connect(&mut stream, proxy, target_host, target_port, remote_dns).await?;
        }
        Proxy::Socks5 { remote_dns, .. } => {
            socks5_connect(&mut stream, proxy, target_host, target_port, *remote_dns).await?;
        }
    }
    Ok(stream)
}

fn parse(proxy: &Proxy) -> Result<url::Url> {
    url::Url::parse(&proxy.uri().to_string()).map_err(Error::invalid_proxy)
}

async fn http_connect<S>(stream: &mut S, proxy: &Proxy, host: &str, port: u16) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let authority = authority(host, port);
    let mut request = format!("CONNECT {authority} HTTP/1.1\r\nHost: {authority}\r\n");
    if let Some(authorization) = authorization(proxy)? {
        let authorization = authorization.to_str().map_err(Error::invalid_proxy)?;
        request.push_str(&format!("Proxy-Authorization: {authorization}\r\n"));
    }
    request.push_str("\r\n");
    stream
        .write_all(request.as_bytes())
        .await
        .map_err(Error::proxy)?;
    stream.flush().await.map_err(Error::proxy)?;
    let response = read_head(stream).await?;
    let status = response
        .split(|byte| *byte == b' ')
        .nth(1)
        .and_then(|value| std::str::from_utf8(value).ok())
        .and_then(|value| value.parse::<u16>().ok())
        .ok_or_else(|| Error::proxy("CONNECT response has no valid status"))?;
    if !(200..300).contains(&status) {
        return Err(Error::proxy(format!(
            "CONNECT proxy returned status {status}"
        )));
    }
    Ok(())
}

async fn read_head<S>(stream: &mut S) -> Result<Vec<u8>>
where
    S: AsyncRead + Unpin,
{
    let mut response = Vec::new();
    while !response.ends_with(b"\r\n\r\n") {
        if response.len() == MAX_RESPONSE_HEAD {
            return Err(Error::proxy("proxy response head exceeded 16 KiB"));
        }
        let mut byte = [0_u8; 1];
        let read = stream.read(&mut byte).await.map_err(Error::proxy)?;
        if read == 0 {
            return Err(Error::proxy(
                "proxy closed before completing the response head",
            ));
        }
        response.push(byte[0]);
    }
    Ok(response)
}

async fn socks4_connect<S>(
    stream: &mut S,
    proxy: &Proxy,
    host: &str,
    port: u16,
    remote_dns: bool,
) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let parsed = parse(proxy)?;
    let user = percent_decode(parsed.username())?;
    if user.as_bytes().contains(&0) || host.as_bytes().contains(&0) {
        return Err(Error::proxy("SOCKS4 credentials or host contain NUL"));
    }
    let address = if remote_dns {
        Ipv4Addr::new(0, 0, 0, 1)
    } else {
        resolve(host, port)
            .await?
            .into_iter()
            .find_map(|address| match address.ip() {
                IpAddr::V4(address) => Some(address),
                IpAddr::V6(_) => None,
            })
            .ok_or_else(|| Error::proxy("SOCKS4 local DNS returned no IPv4 address"))?
    };
    let mut request = vec![4, 1];
    request.extend_from_slice(&port.to_be_bytes());
    request.extend_from_slice(&address.octets());
    request.extend_from_slice(user.as_bytes());
    request.push(0);
    if remote_dns {
        request.extend_from_slice(host.as_bytes());
        request.push(0);
    }
    stream.write_all(&request).await.map_err(Error::proxy)?;
    let mut response = [0_u8; 8];
    stream
        .read_exact(&mut response)
        .await
        .map_err(Error::proxy)?;
    if response[0] != 0 || response[1] != 90 {
        return Err(Error::proxy(format!(
            "SOCKS4 proxy rejected connection with code {}",
            response[1]
        )));
    }
    Ok(())
}

async fn socks5_connect<S>(
    stream: &mut S,
    proxy: &Proxy,
    host: &str,
    port: u16,
    remote_dns: bool,
) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let parsed = parse(proxy)?;
    let username = percent_decode(parsed.username())?;
    let password = percent_decode(parsed.password().unwrap_or_default())?;
    let authenticated = !username.is_empty() && !password.is_empty();
    stream
        .write_all(if authenticated {
            &[5, 2, 0, 2]
        } else {
            &[5, 1, 0]
        })
        .await
        .map_err(Error::proxy)?;
    let mut method = [0_u8; 2];
    stream.read_exact(&mut method).await.map_err(Error::proxy)?;
    if method[0] != 5 || (method[1] != 0 && (!authenticated || method[1] != 2)) {
        return Err(Error::proxy(format!(
            "SOCKS5 proxy selected unsupported authentication method {}",
            method[1]
        )));
    }
    if method[1] == 2 {
        let username = length_prefixed(username.as_bytes(), "SOCKS5 username")?;
        let password = length_prefixed(password.as_bytes(), "SOCKS5 password")?;
        let mut credentials = vec![1];
        credentials.extend(username);
        credentials.extend(password);
        stream.write_all(&credentials).await.map_err(Error::proxy)?;
        let mut response = [0_u8; 2];
        stream
            .read_exact(&mut response)
            .await
            .map_err(Error::proxy)?;
        if response != [1, 0] {
            return Err(Error::proxy(
                "SOCKS5 username/password authentication failed",
            ));
        }
    }

    let mut request = vec![5, 1, 0];
    if remote_dns && host.parse::<IpAddr>().is_err() {
        request.push(3);
        request.extend(length_prefixed(host.as_bytes(), "SOCKS5 hostname")?);
    } else {
        let address = if let Ok(address) = host.parse::<IpAddr>() {
            address
        } else {
            resolve(host, port)
                .await?
                .into_iter()
                .next()
                .ok_or_else(|| Error::proxy("SOCKS5 local DNS returned no address"))?
                .ip()
        };
        match address {
            IpAddr::V4(address) => {
                request.push(1);
                request.extend_from_slice(&address.octets());
            }
            IpAddr::V6(address) => {
                request.push(4);
                request.extend_from_slice(&address.octets());
            }
        }
    }
    request.extend_from_slice(&port.to_be_bytes());
    stream.write_all(&request).await.map_err(Error::proxy)?;

    let mut response = [0_u8; 4];
    stream
        .read_exact(&mut response)
        .await
        .map_err(Error::proxy)?;
    if response[0] != 5 || response[1] != 0 {
        return Err(Error::proxy(format!(
            "SOCKS5 proxy rejected connection with code {}",
            response[1]
        )));
    }
    let address_length = match response[3] {
        1 => 4,
        4 => 16,
        3 => {
            let mut length = [0_u8; 1];
            stream.read_exact(&mut length).await.map_err(Error::proxy)?;
            usize::from(length[0])
        }
        address_type => {
            return Err(Error::proxy(format!(
                "SOCKS5 proxy returned unknown address type {address_type}"
            )));
        }
    };
    let mut ignored = vec![0_u8; address_length + 2];
    stream
        .read_exact(&mut ignored)
        .await
        .map_err(Error::proxy)?;
    Ok(())
}

async fn resolve(host: &str, port: u16) -> Result<Vec<SocketAddr>> {
    tokio::net::lookup_host((host, port))
        .await
        .map(|addresses| addresses.collect())
        .map_err(|error| Error::proxy(format!("local DNS resolution failed: {error}")))
}

fn length_prefixed(value: &[u8], role: &str) -> Result<Vec<u8>> {
    let length =
        u8::try_from(value.len()).map_err(|_| Error::proxy(format!("{role} exceeds 255 bytes")))?;
    let mut encoded = Vec::with_capacity(value.len() + 1);
    encoded.push(length);
    encoded.extend_from_slice(value);
    Ok(encoded)
}

fn authority(host: &str, port: u16) -> String {
    if host.contains(':') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    }
}

fn percent_decode(value: &str) -> Result<String> {
    let mut decoded = Vec::with_capacity(value.len());
    let bytes = value.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let high = bytes
                .get(index + 1)
                .and_then(|byte| hex(*byte))
                .ok_or_else(|| Error::invalid_proxy("invalid percent escape in credentials"))?;
            let low = bytes
                .get(index + 2)
                .and_then(|byte| hex(*byte))
                .ok_or_else(|| Error::invalid_proxy("invalid percent escape in credentials"))?;
            decoded.push(high << 4 | low);
            index += 3;
        } else {
            decoded.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(decoded)
        .map_err(|_| Error::invalid_proxy("proxy credentials are not valid UTF-8"))
}

fn hex(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn base64(value: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut encoded = String::with_capacity(value.len().div_ceil(3) * 4);
    for chunk in value.chunks(3) {
        let first = chunk[0];
        let second = chunk.get(1).copied().unwrap_or_default();
        let third = chunk.get(2).copied().unwrap_or_default();
        encoded.push(ALPHABET[(first >> 2) as usize] as char);
        encoded.push(ALPHABET[(((first & 0x03) << 4) | (second >> 4)) as usize] as char);
        encoded.push(if chunk.len() > 1 {
            ALPHABET[(((second & 0x0f) << 2) | (third >> 6)) as usize] as char
        } else {
            '='
        });
        encoded.push(if chunk.len() > 2 {
            ALPHABET[(third & 0x3f) as usize] as char
        } else {
            '='
        });
    }
    encoded
}
