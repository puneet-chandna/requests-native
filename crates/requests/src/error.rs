use std::fmt;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum ErrorKind {
    InvalidUrl,
}

#[derive(Debug)]
pub struct Error {
    kind: ErrorKind,
    message: String,
}

impl Error {
    pub fn kind(&self) -> ErrorKind {
        self.kind
    }

    pub(crate) fn invalid_url(url: &str) -> Self {
        Self {
            kind: ErrorKind::InvalidUrl,
            message: format!("invalid URL: {url}"),
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for Error {}

pub type Result<T> = std::result::Result<T, Error>;
