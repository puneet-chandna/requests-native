use std::fmt;
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{CryptoProvider, verify_tls12_signature, verify_tls13_signature};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{ClientConfig, DigitallySignedStruct, RootCertStore, SignatureScheme};
use tokio_rustls::TlsConnector;

use super::NativeRootLoader;
use crate::{CertificateSource, Error, Identity, Result, TlsConfig};

#[derive(Clone)]
pub(super) struct LoadedTls {
    config: Arc<ClientConfig>,
}

enum RootMode {
    Verified(RootCertStore),
    Disabled,
}

fn verified(roots: RootCertStore) -> RootMode {
    RootMode::Verified(roots)
}

pub(super) fn load(
    tls: &TlsConfig,
    native_root_loader: Option<NativeRootLoader>,
) -> Result<LoadedTls> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let builder = ClientConfig::builder_with_provider(Arc::clone(&provider))
        .with_safe_default_protocol_versions()
        .map_err(Error::tls)?;
    let roots = match &tls.roots {
        CertificateSource::Platform => load_platform_roots(native_root_loader).map(verified),
        CertificateSource::PemBundle(path) => load_root_bundle(path).map(verified),
        CertificateSource::PemDirectory(path) => load_root_directory(path).map(verified),
        CertificateSource::Disabled => Ok(RootMode::Disabled),
    }?;
    let builder = match roots {
        RootMode::Verified(roots) => builder.with_root_certificates(roots),
        RootMode::Disabled => builder
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(DisabledVerifier {
                provider: Arc::clone(&provider),
            })),
    };
    let mut config = match &tls.identity {
        Some(identity) => {
            let (certificates, private_key) = load_identity(identity)?;
            builder
                .with_client_auth_cert(certificates, private_key)
                .map_err(Error::tls)?
        }
        None => builder.with_no_client_auth(),
    };
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok(LoadedTls {
        config: Arc::new(config),
    })
}

fn load_platform_roots(native_root_loader: Option<NativeRootLoader>) -> Result<RootCertStore> {
    let native_root_loader = native_root_loader.unwrap_or_else(|| Arc::new(system_native_roots));
    let result = native_root_loader();
    if !result.errors.is_empty() {
        return Err(Error::tls(format!(
            "platform certificate store reported {} loading error(s)",
            result.errors.len()
        )));
    }
    root_store(result.certs, "platform certificate store")
}

fn system_native_roots() -> rustls_native_certs::CertificateResult {
    rustls_native_certs::load_native_certs()
}

fn load_root_bundle(path: &Path) -> Result<RootCertStore> {
    let certificates = load_certificates(path, "root certificate bundle")?;
    root_store(certificates, &format!("root certificate bundle {path:?}"))
}

fn load_root_directory(path: &Path) -> Result<RootCertStore> {
    let entries = std::fs::read_dir(path).map_err(|error| {
        Error::tls(PathError::new(
            "read root certificate directory",
            path,
            error,
        ))
    })?;
    let mut eligible = entries
        .map(|entry| {
            entry.map(|entry| entry.path()).map_err(|error| {
                Error::tls(PathError::new("read root directory entry", path, error))
            })
        })
        .collect::<Result<Vec<_>>>()?;
    eligible.retain(|path| {
        path.file_name()
            .and_then(|name| name.to_str())
            .is_some_and(is_capath_basename)
    });
    eligible.sort();

    let mut certificates = Vec::new();
    for entry in eligible {
        certificates.extend(load_certificates(
            &entry,
            "eligible root certificate entry",
        )?);
    }
    root_store(
        certificates,
        &format!("root certificate directory {path:?}"),
    )
}

fn is_capath_basename(name: &str) -> bool {
    let Some((hash, suffix)) = name.split_once('.') else {
        return false;
    };
    hash.len() == 8
        && hash.bytes().all(|byte| byte.is_ascii_hexdigit())
        && !suffix.is_empty()
        && suffix.bytes().all(|byte| byte.is_ascii_digit())
}

fn load_identity(
    identity: &Identity,
) -> Result<(
    Vec<CertificateDer<'static>>,
    rustls::pki_types::PrivateKeyDer<'static>,
)> {
    let certificate_bytes = read_nonempty(&identity.certificate_chain, "client certificate chain")?;
    let certificates = parse_certificates(
        &certificate_bytes,
        &identity.certificate_chain,
        "client certificate chain",
    )?;
    for (index, certificate) in certificates.iter().enumerate() {
        rustls::server::ParsedCertificate::try_from(certificate).map_err(|_| {
            Error::tls(format!(
                "invalid client certificate at index {index} in {:?}",
                identity.certificate_chain
            ))
        })?;
    }
    let key_path = identity
        .private_key
        .as_deref()
        .unwrap_or(&identity.certificate_chain);
    let key_bytes = if identity.private_key.is_some() {
        read_nonempty(key_path, "client private key")?
    } else {
        certificate_bytes
    };
    let private_key = rustls_pemfile::private_key(&mut Cursor::new(&key_bytes))
        .map_err(|error| Error::tls(PathError::new("parse client private key", key_path, error)))?
        .ok_or_else(|| Error::tls(format!("client private key is missing from {key_path:?}")))?;
    Ok((certificates, private_key))
}

fn load_certificates(path: &Path, role: &str) -> Result<Vec<CertificateDer<'static>>> {
    let bytes = read_nonempty(path, role)?;
    parse_certificates(&bytes, path, role)
}

fn read_nonempty(path: &Path, role: &str) -> Result<Vec<u8>> {
    let bytes = std::fs::read(path)
        .map_err(|error| Error::tls(PathError::new(&format!("read {role}"), path, error)))?;
    if bytes.is_empty() {
        return Err(Error::tls(format!("{role} is empty: {path:?}")));
    }
    Ok(bytes)
}

fn parse_certificates(
    bytes: &[u8],
    path: &Path,
    role: &str,
) -> Result<Vec<CertificateDer<'static>>> {
    let certificates = rustls_pemfile::certs(&mut Cursor::new(bytes))
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(|error| Error::tls(PathError::new(&format!("parse {role}"), path, error)))?;
    if certificates.is_empty() {
        return Err(Error::tls(format!(
            "{role} contains no certificates: {path:?}"
        )));
    }
    Ok(certificates)
}

fn root_store(certificates: Vec<CertificateDer<'static>>, source: &str) -> Result<RootCertStore> {
    if certificates.is_empty() {
        return Err(Error::tls(format!("{source} contains no certificates")));
    }
    let mut roots = RootCertStore::empty();
    for certificate in certificates {
        roots
            .add(certificate)
            .map_err(|error| Error::tls(format!("invalid certificate in {source}: {error}")))?;
    }
    Ok(roots)
}

pub(super) async fn handshake<S>(
    loaded: LoadedTls,
    host: &str,
    stream: S,
) -> Result<tokio_rustls::client::TlsStream<S>>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let server_name =
        ServerName::try_from(host.to_owned()).map_err(|error| Error::tls(error.to_string()))?;
    TlsConnector::from(loaded.config)
        .connect(server_name, stream)
        .await
        .map_err(Error::tls)
}

#[derive(Debug)]
struct DisabledVerifier {
    provider: Arc<CryptoProvider>,
}

impl ServerCertVerifier for DisabledVerifier {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> std::result::Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        certificate: &CertificateDer<'_>,
        signature: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls12_signature(
            message,
            certificate,
            signature,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        certificate: &CertificateDer<'_>,
        signature: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls13_signature(
            message,
            certificate,
            signature,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

struct PathError {
    action: String,
    path: PathBuf,
    source: std::io::Error,
}

impl PathError {
    fn new(action: &str, path: &Path, source: std::io::Error) -> Self {
        Self {
            action: action.to_owned(),
            path: path.to_path_buf(),
            source,
        }
    }
}

impl fmt::Display for PathError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{} {:?}: {}",
            self.action, self.path, self.source
        )
    }
}
