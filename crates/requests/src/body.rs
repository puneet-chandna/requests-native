use bytes::Bytes;

#[derive(Debug, Default)]
#[non_exhaustive]
pub enum BodySource {
    #[default]
    Empty,
    Bytes(Bytes),
}

impl From<Bytes> for BodySource {
    fn from(body: Bytes) -> Self {
        Self::Bytes(body)
    }
}

impl From<Vec<u8>> for BodySource {
    fn from(body: Vec<u8>) -> Self {
        Self::Bytes(body.into())
    }
}
