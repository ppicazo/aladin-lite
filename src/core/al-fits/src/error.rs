use std::fmt;

#[derive(Debug)]
pub enum Error {
    /// The transport failed: network error, CORS, a 4xx/5xx, a dead Blob.
    Io(String),
    /// The bytes came back, but they are not a FITS file we can follow.
    Format(String),
    /// A read asked for bytes past the end of the source.
    OutOfBounds { offset: u64, len: u64, size: u64 },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io(m) => write!(f, "I/O error: {}", m),
            Error::Format(m) => write!(f, "malformed FITS: {}", m),
            Error::OutOfBounds { offset, len, size } => write!(
                f,
                "read of {} bytes at offset {} is past the end of the {}-byte source",
                len, offset, size
            ),
        }
    }
}

impl std::error::Error for Error {}

impl From<Error> for wasm_bindgen::JsValue {
    fn from(e: Error) -> Self {
        wasm_bindgen::JsValue::from_str(&e.to_string())
    }
}

pub type Result<T> = std::result::Result<T, Error>;
