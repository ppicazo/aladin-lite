//! Where FITS bytes come from, and how few of them we can get away with reading.
//!
//! Everything above this module addresses a FITS file as an offset and a
//! length. A [`Source`] turns that into whatever the origin actually supports —
//! an HTTP range request, a `Blob` slice, or a plain memory read — so that no
//! part of the pipeline needs to know, or needs the whole file to exist
//! anywhere at once.

use crate::error::{Error, Result};

/// A source of bytes that can be read at arbitrary offsets.
///
/// Reads are asynchronous because the interesting implementation is a network
/// round trip. Sources are cheap to clone-by-reference and are expected to be
/// shared: several tile requests read from the same one concurrently.
pub trait ByteSource {
    /// Total size in bytes.
    fn size(&self) -> u64;

    /// Read exactly `len` bytes starting at `offset`.
    ///
    /// Reading past the end is an error rather than a short read: callers
    /// compute their ranges from the FITS structure, so a short read means we
    /// misparsed something and should say so.
    fn read_range(
        &self,
        offset: u64,
        len: u64,
    ) -> impl std::future::Future<Output = Result<Vec<u8>>>;
}

fn check_bounds(offset: u64, len: u64, size: u64) -> Result<()> {
    if offset.saturating_add(len) > size {
        Err(Error::OutOfBounds { offset, len, size })
    } else {
        Ok(())
    }
}

/// Bytes already in memory.
///
/// The fallback for origins that cannot do better — a server without
/// `Accept-Ranges`, or a small file that is not worth streaming — and the
/// source used by native unit tests.
#[derive(Debug, Clone)]
pub struct MemorySource {
    bytes: std::rc::Rc<[u8]>,
}

impl MemorySource {
    pub fn new(bytes: impl Into<std::rc::Rc<[u8]>>) -> Self {
        Self {
            bytes: bytes.into(),
        }
    }
}

impl ByteSource for MemorySource {
    fn size(&self) -> u64 {
        self.bytes.len() as u64
    }

    async fn read_range(&self, offset: u64, len: u64) -> Result<Vec<u8>> {
        check_bounds(offset, len, self.size())?;
        let start = offset as usize;
        Ok(self.bytes[start..start + len as usize].to_vec())
    }
}

#[cfg(target_arch = "wasm32")]
pub use web::{BlobSource, HttpRangeSource, Opened};

#[cfg(target_arch = "wasm32")]
mod web {
    use super::{check_bounds, ByteSource};
    use crate::error::{Error, Result};
    use js_sys::Uint8Array;
    use wasm_bindgen::{JsCast, JsValue};
    use wasm_bindgen_futures::JsFuture;
    use web_sys::{Request, RequestInit, Response};

    fn js_err(what: &str, e: JsValue) -> Error {
        Error::Io(format!(
            "{}: {}",
            what,
            e.as_string()
                .or_else(|| e.dyn_ref::<js_sys::Error>().map(|e| String::from(e.message())))
                .unwrap_or_else(|| format!("{:?}", e))
        ))
    }

    /// `fetch`, from either a window or a worker.
    ///
    /// Decoding runs in workers from Phase 3 onward, and `web_sys::window()` is
    /// `None` there, so the global is resolved dynamically instead.
    fn fetch(request: &Request) -> Result<js_sys::Promise> {
        let global = js_sys::global();

        if let Some(window) = global.dyn_ref::<web_sys::Window>() {
            return Ok(window.fetch_with_request(request));
        }
        if let Some(scope) = global.dyn_ref::<web_sys::WorkerGlobalScope>() {
            return Ok(scope.fetch_with_request(request));
        }
        Err(Error::Io("no fetch available in this global scope".into()))
    }

    async fn fetch_range(url: &str, range: Option<(u64, u64)>) -> Result<Response> {
        let mut opts = RequestInit::new();
        opts.method("GET");

        let request = Request::new_with_str_and_init(url, &opts)
            .map_err(|e| js_err("building request", e))?;

        if let Some((start, end_inclusive)) = range {
            request
                .headers()
                .set("Range", &format!("bytes={}-{}", start, end_inclusive))
                .map_err(|e| js_err("setting Range header", e))?;
        }

        let response: Response = JsFuture::from(fetch(&request)?)
            .await
            .map_err(|e| js_err(&format!("fetching {}", url), e))?
            .dyn_into()
            .map_err(|e| js_err("response was not a Response", e))?;

        if !response.ok() {
            return Err(Error::Io(format!(
                "{} returned HTTP {}",
                url,
                response.status()
            )));
        }

        Ok(response)
    }

    async fn response_bytes(response: &Response) -> Result<Vec<u8>> {
        let buffer = JsFuture::from(
            response
                .array_buffer()
                .map_err(|e| js_err("reading response body", e))?,
        )
        .await
        .map_err(|e| js_err("reading response body", e))?;

        Ok(Uint8Array::new(&buffer).to_vec())
    }

    /// Parse the total size out of a `Content-Range: bytes 0-1023/4294972800`.
    fn total_from_content_range(header: &str) -> Option<u64> {
        header.rsplit('/').next()?.trim().parse().ok()
    }

    /// A remote FITS read through HTTP range requests.
    ///
    /// Opening one costs a single request: it asks for the first
    /// [`PROBE_LEN`](HttpRangeSource::PROBE_LEN) bytes, and the `Content-Range`
    /// of the reply carries the total size, so the file's length and its first
    /// headers arrive together. A server that ignores the range and sends the
    /// whole file is detected here — [`HttpRangeSource::open`] reports it, and
    /// the caller falls back to [`super::MemorySource`] rather than requesting
    /// the file a second time.
    #[derive(Debug)]
    pub struct HttpRangeSource {
        url: String,
        size: u64,
        /// The bytes from the opening probe, reused for the first header reads.
        head: Vec<u8>,
    }

    /// What [`HttpRangeSource::open`] found at the far end.
    pub enum Opened {
        /// Ranges work. Reads will fetch only what they ask for.
        Ranged(HttpRangeSource),
        /// The server ignored `Range` and sent everything, so we already have
        /// the whole file and there is nothing to stream.
        Whole(Vec<u8>),
    }

    impl HttpRangeSource {
        /// Enough to cover the primary header of essentially any real file
        /// (36 cards per 2880-byte block), so opening a file and listing its
        /// first HDU usually costs exactly one request.
        pub const PROBE_LEN: u64 = 64 * 1024;

        pub async fn open(url: &str) -> Result<Opened> {
            let response = fetch_range(url, Some((0, Self::PROBE_LEN - 1))).await?;

            // 206 means the range was honoured. Anything else (200, typically)
            // means the body is the entire file.
            if response.status() != 206 {
                return Ok(Opened::Whole(response_bytes(&response).await?));
            }

            let content_range = response
                .headers()
                .get("Content-Range")
                .map_err(|e| js_err("reading Content-Range", e))?
                .ok_or_else(|| {
                    Error::Io(format!(
                        "{} answered 206 without a Content-Range header; if it is \
                         cross-origin, the server must expose that header via \
                         Access-Control-Expose-Headers",
                        url
                    ))
                })?;

            let size = total_from_content_range(&content_range).ok_or_else(|| {
                Error::Io(format!("could not parse Content-Range: {}", content_range))
            })?;

            let head = response_bytes(&response).await?;

            Ok(Opened::Ranged(Self {
                url: url.to_string(),
                size,
                head,
            }))
        }

        pub fn url(&self) -> &str {
            &self.url
        }
    }

    impl ByteSource for HttpRangeSource {
        fn size(&self) -> u64 {
            self.size
        }

        async fn read_range(&self, offset: u64, len: u64) -> Result<Vec<u8>> {
            check_bounds(offset, len, self.size)?;
            if len == 0 {
                return Ok(Vec::new());
            }

            // Serve from the opening probe when the read lies inside it. The
            // header walk reads the first few kilobytes repeatedly, and this
            // keeps all of that to the one request already spent.
            let end = offset + len;
            if end <= self.head.len() as u64 {
                return Ok(self.head[offset as usize..end as usize].to_vec());
            }

            let response = fetch_range(&self.url, Some((offset, end - 1))).await?;
            let bytes = response_bytes(&response).await?;

            if bytes.len() as u64 != len {
                return Err(Error::Io(format!(
                    "{} returned {} bytes for a {}-byte range request",
                    self.url,
                    bytes.len(),
                    len
                )));
            }

            Ok(bytes)
        }
    }

    /// A local file chosen by the user.
    ///
    /// `Blob::slice` is already lazy — the browser reads from disk on demand —
    /// so a local file streams as well as a ranged remote one, with no server
    /// and no CORS involved.
    #[derive(Debug)]
    pub struct BlobSource {
        blob: web_sys::Blob,
        size: u64,
    }

    impl BlobSource {
        pub fn new(blob: web_sys::Blob) -> Self {
            let size = blob.size() as u64;
            Self { blob, size }
        }
    }

    impl ByteSource for BlobSource {
        fn size(&self) -> u64 {
            self.size
        }

        async fn read_range(&self, offset: u64, len: u64) -> Result<Vec<u8>> {
            check_bounds(offset, len, self.size)?;
            if len == 0 {
                return Ok(Vec::new());
            }

            // f64 rather than i32: a 2 GB offset does not fit in the i32 form
            // of Blob::slice, which is exactly the case this exists for.
            let slice = self
                .blob
                .slice_with_f64_and_f64(offset as f64, (offset + len) as f64)
                .map_err(|e| js_err("slicing blob", e))?;

            let buffer = JsFuture::from(slice.array_buffer())
                .await
                .map_err(|e| js_err("reading blob slice", e))?;

            Ok(Uint8Array::new(&buffer).to_vec())
        }
    }

    #[cfg(test)]
    mod tests {
        use super::total_from_content_range;

        #[test]
        fn parses_total_size_from_content_range() {
            assert_eq!(
                total_from_content_range("bytes 0-16383/4294972800"),
                Some(4294972800)
            );
            assert_eq!(total_from_content_range("bytes 0-0/1"), Some(1));
            // A server that does not know the total says so with a star.
            assert_eq!(total_from_content_range("bytes 0-16383/*"), None);
        }
    }
}

/// A [`ByteSource`] chosen at runtime.
///
/// Dispatch has to be dynamic — the origin is only known once the URL has been
/// probed — but the trait has async methods and so is not object safe. An enum
/// keeps the dispatch static and avoids boxing every read.
pub enum Source {
    Memory(MemorySource),
    #[cfg(target_arch = "wasm32")]
    Http(HttpRangeSource),
    #[cfg(target_arch = "wasm32")]
    Blob(BlobSource),
}

impl ByteSource for Source {
    fn size(&self) -> u64 {
        match self {
            Source::Memory(s) => s.size(),
            #[cfg(target_arch = "wasm32")]
            Source::Http(s) => s.size(),
            #[cfg(target_arch = "wasm32")]
            Source::Blob(s) => s.size(),
        }
    }

    async fn read_range(&self, offset: u64, len: u64) -> Result<Vec<u8>> {
        match self {
            Source::Memory(s) => s.read_range(offset, len).await,
            #[cfg(target_arch = "wasm32")]
            Source::Http(s) => s.read_range(offset, len).await,
            #[cfg(target_arch = "wasm32")]
            Source::Blob(s) => s.read_range(offset, len).await,
        }
    }
}

impl Source {
    /// How the bytes are reaching us, for diagnostics and benchmarking.
    pub fn kind(&self) -> &'static str {
        match self {
            Source::Memory(_) => "memory",
            #[cfg(target_arch = "wasm32")]
            Source::Http(_) => "http-range",
            #[cfg(target_arch = "wasm32")]
            Source::Blob(_) => "blob",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn block_on<F: std::future::Future>(f: F) -> F::Output {
        futures::executor::block_on(f)
    }

    #[test]
    fn memory_source_reads_ranges() {
        let src = MemorySource::new(vec![0u8, 1, 2, 3, 4, 5, 6, 7]);
        assert_eq!(src.size(), 8);
        assert_eq!(block_on(src.read_range(2, 3)).unwrap(), vec![2, 3, 4]);
        assert_eq!(block_on(src.read_range(0, 0)).unwrap(), Vec::<u8>::new());
        assert_eq!(block_on(src.read_range(8, 0)).unwrap(), Vec::<u8>::new());
    }

    #[test]
    fn reading_past_the_end_is_an_error() {
        let src = MemorySource::new(vec![0u8; 4]);
        assert!(block_on(src.read_range(2, 3)).is_err());
        assert!(block_on(src.read_range(5, 1)).is_err());
        // Overflow must not wrap into a spurious success.
        assert!(block_on(src.read_range(u64::MAX, 1)).is_err());
    }
}
