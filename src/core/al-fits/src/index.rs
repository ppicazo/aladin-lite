//! Walking a FITS file's structure without reading its pixels.
//!
//! Headers are small and data units are enormous, and the size of a data unit
//! is computable from its header. So the whole file can be indexed by reading
//! only headers and *arithmetically* stepping over everything else — the bytes
//! in between are never requested. Indexing a 4 GB image costs a few kilobytes.

use crate::error::{Error, Result};
use crate::header::{
    axes, data_unit_len, padded_to_block, parse_block, Keywords, KeywordsExt, BLOCK_LEN,
};
use crate::source::ByteSource;
use serde::Serialize;
use std::ops::Range;

/// What kind of HDU this is, as declared by `XTENSION` (or its absence).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum HduKind {
    Primary,
    Image,
    BinTable,
    AsciiTable,
    Unknown,
}

impl HduKind {
    fn from_xtension(xtension: Option<&str>) -> Self {
        match xtension.map(str::trim) {
            None => HduKind::Primary,
            Some("IMAGE") => HduKind::Image,
            Some("BINTABLE") | Some("A3DTABLE") => HduKind::BinTable,
            Some("TABLE") => HduKind::AsciiTable,
            Some(_) => HduKind::Unknown,
        }
    }
}

/// One HDU, located but not read.
#[derive(Debug, Clone, Serialize)]
pub struct HduEntry {
    /// Position in the file, 0 for the primary HDU.
    pub index: usize,
    pub kind: HduKind,
    /// Byte range of the header, including its padding to a block boundary.
    pub header: Range<u64>,
    /// Byte range of the data unit, excluding padding. Empty when there is none.
    pub data: Range<u64>,
    pub bitpix: i64,
    /// Axis lengths, `NAXIS1` first.
    pub axes: Vec<u64>,
    /// Every keyword in the header, upper-cased.
    pub keywords: Keywords,
}

impl HduEntry {
    /// Width, height and depth for an HDU that can be displayed as an image.
    ///
    /// `None` when the HDU has fewer than two axes, which rules out tables and
    /// keyword-only primary HDUs.
    pub fn image_dimensions(&self) -> Option<(u64, u64, u64)> {
        if self.axes.len() < 2 {
            return None;
        }
        let depth = if self.axes.len() >= 3 { self.axes[2] } else { 1 };
        Some((self.axes[0], self.axes[1], depth))
    }

    pub fn data_len(&self) -> u64 {
        self.data.end - self.data.start
    }

    /// Bytes per pixel implied by `BITPIX`.
    pub fn bytes_per_pixel(&self) -> u64 {
        self.bitpix.unsigned_abs() / 8
    }

    /// Byte range of one row of the image, in the data unit.
    ///
    /// FITS image data is row-major with the first axis varying fastest, which
    /// is what makes a tile a set of strided row ranges rather than one span.
    pub fn row_range(&self, y: u64) -> Option<Range<u64>> {
        let (width, height, _) = self.image_dimensions()?;
        if y >= height {
            return None;
        }
        let row_len = width.checked_mul(self.bytes_per_pixel())?;
        let start = self.data.start.checked_add(y.checked_mul(row_len)?)?;
        Some(start..start + row_len)
    }
}

/// The structure of a FITS file: every HDU, located, with no data read.
#[derive(Debug, Clone, Serialize)]
pub struct FitsIndex {
    pub size: u64,
    pub hdus: Vec<HduEntry>,
    /// Bytes read to build this index. The point of the exercise.
    pub bytes_read: u64,
}

impl FitsIndex {
    /// The HDUs that can be drawn as images, in file order.
    pub fn image_hdus(&self) -> impl Iterator<Item = &HduEntry> {
        self.hdus
            .iter()
            .filter(|h| matches!(h.kind, HduKind::Primary | HduKind::Image))
            .filter(|h| h.image_dimensions().is_some())
    }
}

/// How many header blocks to ask for at a time.
///
/// One request covering 8 blocks reads a 288-card header in a single round
/// trip. Headers longer than that are rare; when one turns up, the walk just
/// asks again.
const HEADER_READ_BLOCKS: u64 = 8;

/// Read and parse one HDU's header starting at `offset`.
///
/// Returns the keywords and the offset just past the header's padding.
async fn read_header<S: ByteSource>(
    source: &S,
    offset: u64,
    bytes_read: &mut u64,
) -> Result<(Keywords, u64)> {
    let mut keywords = Keywords::new();
    let mut cursor = offset;

    loop {
        let remaining = source.size().saturating_sub(cursor);
        if remaining < BLOCK_LEN as u64 {
            return Err(Error::Format(format!(
                "header at byte {} runs past the end of the file",
                offset
            )));
        }

        let want = (HEADER_READ_BLOCKS * BLOCK_LEN as u64).min(remaining);
        let chunk = source.read_range(cursor, want).await?;
        *bytes_read += chunk.len() as u64;

        for block in chunk.chunks_exact(BLOCK_LEN) {
            let parsed = parse_block(block)?;
            keywords.extend(parsed.keywords);
            cursor += BLOCK_LEN as u64;

            if parsed.end {
                return Ok((keywords, cursor));
            }
        }
    }
}

/// Index every HDU in `source`, reading headers only.
pub async fn index<S: ByteSource>(source: &S) -> Result<FitsIndex> {
    let size = source.size();
    let mut hdus = Vec::new();
    let mut bytes_read = 0;
    let mut offset = 0u64;

    while offset + (BLOCK_LEN as u64) <= size {
        let header_start = offset;
        let (keywords, header_end) = read_header(source, offset, &mut bytes_read).await?;

        if hdus.is_empty() && keywords.get("SIMPLE").is_none() {
            return Err(Error::Format(
                "the first header has no SIMPLE keyword, so this is not a FITS file".into(),
            ));
        }

        let data_len = data_unit_len(&keywords);
        let data_start = header_end;
        let data_end = data_start.saturating_add(data_len);

        if data_end > size {
            return Err(Error::Format(format!(
                "HDU {} declares {} bytes of data at offset {}, past the end of the {}-byte file",
                hdus.len(),
                data_len,
                data_start,
                size
            )));
        }

        hdus.push(HduEntry {
            index: hdus.len(),
            kind: HduKind::from_xtension(keywords.text("XTENSION")),
            header: header_start..header_end,
            data: data_start..data_end,
            bitpix: keywords.int("BITPIX").unwrap_or(0),
            axes: axes(&keywords),
            keywords,
        });

        // The step over the data unit is arithmetic: those bytes are never read.
        offset = data_start + padded_to_block(data_len);
    }

    if hdus.is_empty() {
        return Err(Error::Format("no HDU found".into()));
    }

    Ok(FitsIndex {
        size,
        hdus,
        bytes_read,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::header::CARD_LEN;
    use crate::source::MemorySource;

    fn block_on<F: std::future::Future>(f: F) -> F::Output {
        futures::executor::block_on(f)
    }

    fn card(text: &str) -> Vec<u8> {
        let mut c = text.as_bytes().to_vec();
        c.resize(CARD_LEN, b' ');
        c
    }

    fn header(cards: &[&str]) -> Vec<u8> {
        let mut out = Vec::new();
        for c in cards {
            out.extend(card(c));
        }
        out.extend(card("END"));
        out.resize(padded_to_block(out.len() as u64) as usize, b' ');
        out
    }

    /// A primary HDU of `w` x `h` float32 pixels, plus its data unit.
    fn image_fits(w: u64, h: u64) -> Vec<u8> {
        let mut out = header(&[
            "SIMPLE  =                    T",
            "BITPIX  =                  -32",
            "NAXIS   =                    2",
            &format!("NAXIS1  = {:>20}", w),
            &format!("NAXIS2  = {:>20}", h),
            "CTYPE1  = 'RA---TAN'",
            "CTYPE2  = 'DEC--TAN'",
        ]);
        let data_len = w * h * 4;
        out.resize(out.len() + padded_to_block(data_len) as usize, 0u8);
        out
    }

    #[test]
    fn indexes_a_single_image_hdu() {
        let src = MemorySource::new(image_fits(16, 8));
        let idx = block_on(index(&src)).unwrap();

        assert_eq!(idx.hdus.len(), 1);
        let hdu = &idx.hdus[0];
        assert_eq!(hdu.kind, HduKind::Primary);
        assert_eq!(hdu.bitpix, -32);
        assert_eq!(hdu.axes, vec![16, 8]);
        assert_eq!(hdu.image_dimensions(), Some((16, 8, 1)));
        assert_eq!(hdu.header, 0..2880);
        assert_eq!(hdu.data, 2880..2880 + 16 * 8 * 4);
    }

    #[test]
    fn indexing_reads_headers_only() {
        // 4096 x 4096 float32 is 64 MiB of data behind a 2880-byte header.
        let src = MemorySource::new(image_fits(4096, 4096));
        let idx = block_on(index(&src)).unwrap();

        assert_eq!(idx.size, 2880 + padded_to_block(4096 * 4096 * 4));
        assert_eq!(idx.hdus[0].data_len(), 4096 * 4096 * 4);
        // One read of at most 8 blocks, and nothing else.
        assert!(
            idx.bytes_read <= 8 * BLOCK_LEN as u64,
            "read {} bytes to index the file",
            idx.bytes_read
        );
    }

    #[test]
    fn steps_over_data_units_to_find_later_hdus() {
        let mut bytes = image_fits(16, 8);
        bytes.extend(header(&[
            "XTENSION= 'IMAGE   '",
            "BITPIX  =                   16",
            "NAXIS   =                    2",
            "NAXIS1  =                   32",
            "NAXIS2  =                    4",
        ]));
        let second_data = 32 * 4 * 2;
        bytes.resize(bytes.len() + padded_to_block(second_data) as usize, 0u8);

        let src = MemorySource::new(bytes);
        let idx = block_on(index(&src)).unwrap();

        assert_eq!(idx.hdus.len(), 2);
        assert_eq!(idx.hdus[1].kind, HduKind::Image);
        assert_eq!(idx.hdus[1].axes, vec![32, 4]);
        assert_eq!(idx.image_hdus().count(), 2);
    }

    #[test]
    fn skips_a_keyword_only_primary_hdu() {
        let mut bytes = header(&[
            "SIMPLE  =                    T",
            "BITPIX  =                    8",
            "NAXIS   =                    0",
        ]);
        bytes.extend(header(&[
            "XTENSION= 'IMAGE   '",
            "BITPIX  =                  -32",
            "NAXIS   =                    2",
            "NAXIS1  =                    4",
            "NAXIS2  =                    4",
        ]));
        bytes.resize(bytes.len() + 2880, 0u8);

        let src = MemorySource::new(bytes);
        let idx = block_on(index(&src)).unwrap();

        assert_eq!(idx.hdus.len(), 2);
        assert!(idx.hdus[0].data.is_empty());
        // The keyword-only primary is not offered as something to draw.
        assert_eq!(idx.image_hdus().count(), 1);
        assert_eq!(idx.image_hdus().next().unwrap().index, 1);
    }

    #[test]
    fn row_ranges_land_inside_the_data_unit() {
        let src = MemorySource::new(image_fits(16, 8));
        let idx = block_on(index(&src)).unwrap();
        let hdu = &idx.hdus[0];

        assert_eq!(hdu.row_range(0), Some(2880..2880 + 64));
        assert_eq!(hdu.row_range(7), Some(2880 + 7 * 64..2880 + 8 * 64));
        assert_eq!(hdu.row_range(8), None);
    }

    #[test]
    fn rejects_something_that_is_not_fits() {
        let src = MemorySource::new(vec![b'x'; 4 * BLOCK_LEN]);
        assert!(block_on(index(&src)).is_err());
    }

    #[test]
    fn rejects_a_truncated_data_unit() {
        let mut bytes = image_fits(64, 64);
        bytes.truncate(bytes.len() - BLOCK_LEN);
        let src = MemorySource::new(bytes);

        let err = block_on(index(&src)).unwrap_err();
        assert!(
            matches!(err, Error::Format(_)),
            "expected a format error, got {:?}",
            err
        );
    }
}
