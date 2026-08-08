//! Python-free Basic and Digest authentication primitives.

use std::fmt::Write;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use md5::Md5;
use sha1::Sha1;
use sha2::{Digest, Sha256, Sha512};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BasicCredentials {
    username: Vec<u8>,
    password: Vec<u8>,
}

impl BasicCredentials {
    #[must_use]
    pub fn new(username: Vec<u8>, password: Vec<u8>) -> Self {
        Self { username, password }
    }

    #[must_use]
    pub fn authorization(&self) -> String {
        let mut joined = Vec::with_capacity(self.username.len() + self.password.len() + 1);
        joined.extend_from_slice(&self.username);
        joined.push(b':');
        joined.extend_from_slice(&self.password);
        format!("Basic {}", STANDARD.encode(joined))
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DigestChallenge {
    pub realm: String,
    pub nonce: String,
    pub qop: Option<String>,
    pub algorithm: Option<String>,
    pub opaque: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DigestState {
    pub last_nonce: String,
    pub nonce_count: u32,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DigestRequest {
    pub username: String,
    pub password: String,
    pub method: String,
    pub url: String,
    pub challenge: DigestChallenge,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DigestAlgorithm {
    Md5,
    Md5Sess,
    Sha1,
    Sha256,
    Sha512,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DigestPlan {
    request: DigestRequest,
    algorithm: DigestAlgorithm,
    path: String,
    state: DigestState,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DigestPreparation {
    Unsupported(DigestState),
    Ready(Box<DigestPlan>),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DigestOutput {
    pub header: Option<String>,
    pub state: DigestState,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Digest401Step {
    Seek,
    Challenge,
    Increment,
    Parse,
    Consume,
    Close,
    Copy,
    ExtractCookies,
    PrepareCookies,
    PreparedParts,
    UpdateNonceCount,
    Ctime,
    Random,
    UpdateLastNonce,
    SetHeader,
    Send,
    AppendHistory,
    ReplaceRequest,
}

#[derive(Debug, Default)]
pub struct Digest401Machine {
    index: usize,
}

impl Digest401Machine {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

impl Iterator for Digest401Machine {
    type Item = Digest401Step;

    fn next(&mut self) -> Option<Self::Item> {
        const STEPS: [Digest401Step; 18] = [
            Digest401Step::Seek,
            Digest401Step::Challenge,
            Digest401Step::Increment,
            Digest401Step::Parse,
            Digest401Step::Consume,
            Digest401Step::Close,
            Digest401Step::Copy,
            Digest401Step::ExtractCookies,
            Digest401Step::PrepareCookies,
            Digest401Step::PreparedParts,
            Digest401Step::UpdateNonceCount,
            Digest401Step::Ctime,
            Digest401Step::Random,
            Digest401Step::UpdateLastNonce,
            Digest401Step::SetHeader,
            Digest401Step::Send,
            Digest401Step::AppendHistory,
            Digest401Step::ReplaceRequest,
        ];
        let step = STEPS.get(self.index).copied();
        self.index += usize::from(step.is_some());
        step
    }
}

impl DigestPlan {
    #[must_use]
    pub fn state(&self) -> &DigestState {
        &self.state
    }

    #[must_use]
    pub fn finish(mut self, ctime: &[u8], random: &[u8]) -> DigestOutput {
        let challenge = &self.request.challenge;
        let ha1_seed = format!(
            "{}:{}:{}",
            self.request.username, challenge.realm, self.request.password
        );
        let ha2_seed = format!("{}:{}", self.request.method, self.path);
        let mut ha1 = digest_hex(self.algorithm, ha1_seed.as_bytes());
        let ha2 = digest_hex(self.algorithm, ha2_seed.as_bytes());
        let ncvalue = format!("{:08x}", self.state.nonce_count);

        let mut cnonce_seed = self.state.nonce_count.to_string().into_bytes();
        cnonce_seed.extend_from_slice(challenge.nonce.as_bytes());
        cnonce_seed.extend_from_slice(ctime);
        cnonce_seed.extend_from_slice(random);
        let cnonce = hex_digest::<Sha1>(&cnonce_seed)[..16].to_owned();

        if self.algorithm == DigestAlgorithm::Md5Sess {
            ha1 = digest_hex(
                self.algorithm,
                format!("{ha1}:{}:{cnonce}", challenge.nonce).as_bytes(),
            );
        }

        let response = match challenge.qop.as_deref() {
            None | Some("") => digest_hex(
                self.algorithm,
                format!("{ha1}:{}:{ha2}", challenge.nonce).as_bytes(),
            ),
            Some(qop) if qop == "auth" || qop.split(',').any(|value| value == "auth") => {
                let noncebit = format!("{}:{ncvalue}:{cnonce}:auth:{ha2}", challenge.nonce);
                digest_hex(self.algorithm, format!("{ha1}:{noncebit}").as_bytes())
            }
            Some(_) => {
                return DigestOutput {
                    header: None,
                    state: self.state,
                };
            }
        };

        self.state.last_nonce.clone_from(&challenge.nonce);
        let mut fields = format!(
            "username=\"{}\", realm=\"{}\", nonce=\"{}\", uri=\"{}\", response=\"{}\"",
            self.request.username, challenge.realm, challenge.nonce, self.path, response
        );
        if let Some(opaque) = challenge
            .opaque
            .as_deref()
            .filter(|value| !value.is_empty())
        {
            fields.push_str(&format!(", opaque=\"{opaque}\""));
        }
        if let Some(algorithm) = challenge
            .algorithm
            .as_deref()
            .filter(|value| !value.is_empty())
        {
            fields.push_str(&format!(", algorithm=\"{algorithm}\""));
        }
        if challenge
            .qop
            .as_deref()
            .is_some_and(|value| !value.is_empty())
        {
            fields.push_str(&format!(
                ", qop=\"auth\", nc={ncvalue}, cnonce=\"{cnonce}\""
            ));
        }
        DigestOutput {
            header: Some(format!("Digest {fields}")),
            state: self.state,
        }
    }
}

#[must_use]
pub fn digest_request_target(url: &str) -> Option<String> {
    let remainder = url
        .strip_prefix("http://")
        .or_else(|| url.strip_prefix("https://"))?;
    let boundary = remainder.find(['/', '?', '#']).unwrap_or(remainder.len());
    let (authority, suffix) = remainder.split_at(boundary);
    if authority.is_empty() {
        return None;
    }
    let lexical_suffix = suffix.split('#').next()?;
    let lexical_target = if lexical_suffix.is_empty() {
        "/".to_owned()
    } else if lexical_suffix.starts_with('?') {
        format!("/{lexical_suffix}")
    } else {
        lexical_suffix.to_owned()
    };
    let parsed = url::Url::parse(url).ok()?;
    if !matches!(parsed.scheme(), "http" | "https") || parsed.host().is_none() {
        return None;
    }
    let mut path = parsed.path().to_owned();
    if path.is_empty() {
        path.push('/');
    }
    if let Some(query) = parsed.query() {
        path.push('?');
        path.push_str(query);
    }
    if path != lexical_target {
        return None;
    }

    let (path, query) = lexical_target
        .split_once('?')
        .map_or((lexical_target.as_str(), None), |(path, query)| {
            (path, Some(query))
        });
    let path = if path.is_empty() { "/" } else { path };
    let final_segment = path.rsplit_once('/').map_or(path, |(_, segment)| segment);
    let path = final_segment.find(';').map_or_else(
        || path.to_owned(),
        |params| path[..path.len() - final_segment.len() + params].to_owned(),
    );
    if query.is_none_or(str::is_empty) {
        Some(path)
    } else {
        Some(format!("{path}?{}", query.expect("nonempty query")))
    }
}

#[must_use]
pub fn prepare_digest(request: DigestRequest, mut state: DigestState) -> DigestPreparation {
    let algorithm_name = request.challenge.algorithm.as_deref().unwrap_or("MD5");
    let algorithm = match algorithm_name.to_uppercase().as_str() {
        "MD5" => DigestAlgorithm::Md5,
        "MD5-SESS" => DigestAlgorithm::Md5Sess,
        "SHA" => DigestAlgorithm::Sha1,
        "SHA-256" => DigestAlgorithm::Sha256,
        "SHA-512" => DigestAlgorithm::Sha512,
        _ => return DigestPreparation::Unsupported(state),
    };
    let Some(path) = digest_request_target(&request.url) else {
        return DigestPreparation::Unsupported(state);
    };

    if request.challenge.nonce == state.last_nonce {
        state.nonce_count += 1;
    } else {
        state.nonce_count = 1;
    }
    DigestPreparation::Ready(Box::new(DigestPlan {
        request,
        algorithm,
        path,
        state,
    }))
}

fn digest_hex(algorithm: DigestAlgorithm, value: &[u8]) -> String {
    match algorithm {
        DigestAlgorithm::Md5 | DigestAlgorithm::Md5Sess => hex_digest::<Md5>(value),
        DigestAlgorithm::Sha1 => hex_digest::<Sha1>(value),
        DigestAlgorithm::Sha256 => hex_digest::<Sha256>(value),
        DigestAlgorithm::Sha512 => hex_digest::<Sha512>(value),
    }
}

fn hex_digest<D>(value: &[u8]) -> String
where
    D: Digest + Default,
{
    let digest = D::digest(value);
    let mut encoded = String::with_capacity(digest.len() * 2);
    for byte in digest {
        write!(encoded, "{byte:02x}").expect("writing to a String cannot fail");
    }
    encoded
}

#[cfg(test)]
mod tests {
    use super::{
        BasicCredentials, DigestChallenge, DigestRequest, DigestState, digest_request_target,
        prepare_digest,
    };

    #[test]
    fn basic_header_matches_the_rfc_example() {
        let credentials = BasicCredentials::new(b"Aladdin".to_vec(), b"open sesame".to_vec());
        assert_eq!(
            credentials.authorization(),
            "Basic QWxhZGRpbjpvcGVuIHNlc2FtZQ=="
        );
    }

    #[test]
    fn digest_target_matches_python_urlparse_path_params_and_query() {
        assert_eq!(
            digest_request_target("http://example.test/path?"),
            Some("/path".to_owned())
        );
        assert_eq!(
            digest_request_target("http://example.test?"),
            Some("/".to_owned())
        );
        assert_eq!(
            digest_request_target("http://[::1]/a;b?x=1#f"),
            Some("/a?x=1".to_owned())
        );
        assert_eq!(
            digest_request_target("http://example.test/a;b/c;d?x=1"),
            Some("/a;b/c?x=1".to_owned())
        );
    }

    #[test]
    fn digest_nonce_count_changes_before_unsupported_qop_finishes() {
        let request = DigestRequest {
            username: "user".to_owned(),
            password: "password".to_owned(),
            method: "POST".to_owned(),
            url: "https://example.test/".to_owned(),
            challenge: DigestChallenge {
                realm: "realm".to_owned(),
                nonce: "new".to_owned(),
                qop: Some("auth-int".to_owned()),
                algorithm: Some("MD5".to_owned()),
                opaque: None,
            },
        };
        let plan = prepare_digest(
            request,
            DigestState {
                last_nonce: "old".to_owned(),
                nonce_count: 4,
            },
        );
        let super::DigestPreparation::Ready(plan) = plan else {
            panic!("MD5 must be supported");
        };
        assert_eq!(plan.state().nonce_count, 1);
        let output = (*plan).finish(b"fixed-time", b"01234567");
        assert_eq!(output.header, None);
        assert_eq!(output.state.last_nonce, "old");
    }

    #[test]
    fn digest_401_machine_has_the_oracle_resend_order() {
        use super::Digest401Step::{
            AppendHistory, Challenge, Close, Consume, Copy, ExtractCookies, Increment,
            PrepareCookies, PreparedParts, ReplaceRequest, Seek, Send, SetHeader, UpdateLastNonce,
            UpdateNonceCount,
        };

        assert_eq!(
            super::Digest401Machine::new().collect::<Vec<_>>(),
            vec![
                Seek,
                Challenge,
                Increment,
                super::Digest401Step::Parse,
                Consume,
                Close,
                Copy,
                ExtractCookies,
                PrepareCookies,
                PreparedParts,
                UpdateNonceCount,
                super::Digest401Step::Ctime,
                super::Digest401Step::Random,
                UpdateLastNonce,
                SetHeader,
                Send,
                AppendHistory,
                ReplaceRequest,
            ]
        );
    }
}
