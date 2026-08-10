//! Lazy FITS access for the browser.
//!
//! The stock path loads a FITS file by downloading all of it, copying all of it
//! into the WASM heap, and decoding all of it before anything appears. That
//! stops working somewhere below 1 GB (see `BASELINE.md`), and it does the same
//! work whether the image covers the screen or a single pixel of it.
//!
//! This crate is the other half of that: address a FITS file as a
//! [`source::ByteSource`], index its structure from headers alone
//! ([`index::index`]), and read only the bytes some particular view actually
//! needs.

pub mod error;
pub mod header;
pub mod index;
pub mod source;
pub mod wcs;

pub use error::{Error, Result};
pub use index::{index, FitsIndex, HduEntry, HduKind};
pub use source::{ByteSource, MemorySource, Source};
pub use wcs::{wcs_from_keywords, WCS};

#[cfg(target_arch = "wasm32")]
pub use source::{BlobSource, HttpRangeSource};

/// Open a URL as a [`Source`], streaming when the server allows it.
///
/// One request decides both questions: whether ranges are supported, and how
/// large the file is. A server that ignores `Range` hands back the whole file,
/// which is then used as-is — there is no second request and no worse outcome
/// than the current behaviour.
#[cfg(target_arch = "wasm32")]
pub async fn open_url(url: &str) -> Result<Source> {
    match HttpRangeSource::open(url).await? {
        source::Opened::Ranged(s) => Ok(Source::Http(s)),
        source::Opened::Whole(bytes) => Ok(Source::Memory(MemorySource::new(bytes))),
    }
}

/// Open a `File` or `Blob` chosen by the user.
#[cfg(target_arch = "wasm32")]
pub fn open_blob(blob: web_sys::Blob) -> Source {
    Source::Blob(BlobSource::new(blob))
}
