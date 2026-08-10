//! Reading tiles out of a FITS image.
//!
//! Ties the three halves together: a [`Source`] to fetch byte ranges, a
//! [`TileGrid`] to decide which ranges a tile needs, and the decoder to turn
//! them into pixels.

use crate::decode::{decode_tile, percentile_cuts, Tile};
use crate::error::{Error, Result};
use crate::header::KeywordsExt;
use crate::index::{index, FitsIndex, HduEntry};
use crate::source::{ByteSource, Source};
use crate::tile::{plan_reads, ReadPlan, Sampling, TileGrid, TileId};
use std::ops::Range;

/// How many range requests to have in flight at once.
///
/// A zoomed-out tile needs one read per sampled row and those rows are far
/// apart, so the reads cannot be merged and latency, not bandwidth, decides how
/// long the tile takes. Eight is about where browsers stop opening new
/// connections per origin anyway.
const MAX_CONCURRENT_READS: usize = 8;

/// Bytes a single tile read may transfer before the sampling is coarsened.
///
/// Only binds when zoomed out far enough that the sampled rows are scattered:
/// a zoomed-in tile is a megabyte or two whatever this is set to.
pub const DEFAULT_TILE_BUDGET: u64 = 16 * 1024 * 1024;

/// What a tile read actually cost.
#[derive(Debug, Clone, Copy)]
pub struct ReadStats {
    pub bytes_fetched: u64,
    pub bytes_used: u64,
    pub requests: usize,
    /// How much coarser the rows are than the columns; 1 when unthinned.
    pub row_thinning: u64,
}

/// An image HDU, open and ready to serve tiles.
pub struct ImageReader {
    source: Source,
    pub index: FitsIndex,
    pub hdu: usize,
    pub grid: TileGrid,
    pub blank: Option<f32>,
    pub bscale: f32,
    pub bzero: f32,
}

impl ImageReader {
    /// Open a source and pick an image HDU.
    ///
    /// `hdu` selects by file position; `None` takes the first HDU that can be
    /// displayed, which is what skips a keyword-only primary header.
    pub async fn open(source: Source, hdu: Option<usize>) -> Result<Self> {
        let index = index(&source).await?;

        let entry = match hdu {
            Some(i) => index
                .hdus
                .get(i)
                .filter(|h| h.image_dimensions().is_some())
                .ok_or_else(|| Error::Format(format!("HDU {} is not a displayable image", i)))?,
            None => index
                .image_hdus()
                .next()
                .ok_or_else(|| Error::Format("no image HDU in this file".into()))?,
        };

        let grid = TileGrid::from_hdu(entry)
            .ok_or_else(|| Error::Format("image HDU has no usable dimensions".into()))?;

        Ok(Self {
            hdu: entry.index,
            grid,
            blank: entry.keywords.float("BLANK").map(|v| v as f32),
            bscale: entry.keywords.float("BSCALE").unwrap_or(1.0) as f32,
            bzero: entry.keywords.float("BZERO").unwrap_or(0.0) as f32,
            index,
            source,
        })
    }

    pub fn entry(&self) -> &HduEntry {
        &self.index.hdus[self.hdu]
    }

    pub fn source_kind(&self) -> &'static str {
        self.source.kind()
    }

    /// Physical value for a raw sample, applying `BSCALE`/`BZERO`.
    pub fn scale(&self, raw: f32) -> f32 {
        raw * self.bscale + self.bzero
    }

    /// Fetch the spans of a plan, a few at a time.
    async fn fetch(&self, plan: &ReadPlan) -> Result<Vec<Vec<u8>>> {
        let mut buffers = Vec::with_capacity(plan.spans.len());

        for group in plan.spans.chunks(MAX_CONCURRENT_READS) {
            let reads = group
                .iter()
                .map(|span| self.source.read_range(span.range.start, span.len()));
            for buffer in futures::future::join_all(reads).await {
                buffers.push(buffer?);
            }
        }

        Ok(buffers)
    }

    /// Read one tile from this reader's own grid.
    pub async fn read_tile(&self, id: TileId, budget: u64) -> Result<(Tile, ReadStats)> {
        let grid = self.grid.clone();
        self.read_tile_on(&grid, id, budget).await
    }

    /// Read one tile addressed by some other tiling of the same image.
    ///
    /// The overview and the refinement tile the image differently — square for
    /// the one, wide and short for the other — so the grid is a parameter
    /// rather than a property of the reader.
    pub async fn read_tile_on(
        &self,
        grid: &TileGrid,
        id: TileId,
        budget: u64,
    ) -> Result<(Tile, ReadStats)> {
        let entry = self.entry();
        let sampling = grid
            .sampling(id, budget)
            .ok_or_else(|| Error::Format(format!("tile {:?} is outside the image", id)))?;

        let plan = plan_reads(entry, grid, &sampling);
        let buffers = self.fetch(&plan).await?;
        let tile = decode_tile(entry.bitpix, &sampling, &plan, &buffers, self.blank)?;

        Ok((
            tile,
            ReadStats {
                bytes_fetched: plan.bytes(),
                bytes_used: plan.useful_bytes(),
                requests: plan.requests(),
                row_thinning: sampling.row_thinning(),
            },
        ))
    }

    /// The sampling a tile would use, without reading anything.
    pub fn sampling(&self, id: TileId, budget: u64) -> Option<Sampling> {
        self.grid.sampling(id, budget)
    }

    /// A grid over this image with a different tile shape.
    pub fn grid_with_tile_shape(&self, tile_w: u32, tile_h: u32) -> TileGrid {
        self.grid.with_tile_shape(tile_w, tile_h)
    }

    /// Initial display cuts, taken from the top of the pyramid.
    ///
    /// The coarsest level is one tile covering the whole image, so this is a
    /// whole-image estimate that costs one tile read instead of a full decode.
    pub async fn initial_cuts(&self, budget: u64) -> Result<Range<f32>> {
        let top = self.grid.level_count() - 1;
        let (tile, _) = self.read_tile(TileId::new(top, 0, 0), budget).await?;

        let mut samples = tile.samples;
        let cuts = percentile_cuts(&mut samples, 1.0, 99.0)
            .ok_or_else(|| Error::Format("image has no finite pixel values".into()))?;

        Ok(self.scale(cuts.start)..self.scale(cuts.end))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::header::padded_to_block;
    use crate::source::MemorySource;

    fn block_on<F: std::future::Future>(f: F) -> F::Output {
        futures::executor::block_on(f)
    }

    /// A single-HDU float32 file whose pixel at (x, y) is `y * width + x`.
    fn ramp_fits(width: u64, height: u64) -> Vec<u8> {
        let cards = [
            "SIMPLE  =                    T".to_string(),
            "BITPIX  =                  -32".to_string(),
            "NAXIS   =                    2".to_string(),
            format!("NAXIS1  = {:>20}", width),
            format!("NAXIS2  = {:>20}", height),
            "END".to_string(),
        ];

        let mut bytes = Vec::new();
        for c in &cards {
            let mut card = c.clone().into_bytes();
            card.resize(80, b' ');
            bytes.extend(card);
        }
        bytes.resize(2880, b' ');

        for y in 0..height {
            for x in 0..width {
                bytes.extend(((y * width + x) as f32).to_be_bytes());
            }
        }
        bytes.resize(padded_to_block(bytes.len() as u64) as usize, 0);
        bytes
    }

    fn reader(width: u64, height: u64) -> ImageReader {
        let source = Source::Memory(MemorySource::new(ramp_fits(width, height)));
        block_on(ImageReader::open(source, None)).unwrap()
    }

    #[test]
    fn reads_a_tile_end_to_end() {
        let reader = reader(600, 40);
        let (tile, stats) = block_on(reader.read_tile(TileId::new(0, 0, 0), u64::MAX)).unwrap();

        assert_eq!((tile.cols, tile.rows), (512, 40));
        assert_eq!(stats.requests, 1);
        assert_eq!(stats.row_thinning, 1);

        let first = f32::from_be_bytes([tile.bytes[0], tile.bytes[1], tile.bytes[2], tile.bytes[3]]);
        assert_eq!(first, 0.0);
    }

    #[test]
    fn a_tile_outside_the_image_is_an_error() {
        let reader = reader(600, 40);
        assert!(block_on(reader.read_tile(TileId::new(0, 9, 9), u64::MAX)).is_err());
    }

    #[test]
    fn cuts_come_from_the_top_of_the_pyramid() {
        // Values run 0..(64*64), so the 1%-99% cuts sit near either end.
        let reader = reader(64, 64);
        let cuts = block_on(reader.initial_cuts(u64::MAX)).unwrap();

        assert!(cuts.start >= 0.0 && cuts.start < 200.0, "{:?}", cuts);
        assert!(cuts.end > 3800.0 && cuts.end <= 4095.0, "{:?}", cuts);
    }

    #[test]
    fn every_tile_of_every_level_reads_cleanly() {
        let reader = reader(600, 700);

        for level in 0..reader.grid.level_count() {
            let (nx, ny) = reader.grid.tiles_at(level);
            for ty in 0..ny {
                for tx in 0..nx {
                    let id = TileId::new(level, tx, ty);
                    let (tile, _) = block_on(reader.read_tile(id, u64::MAX))
                        .unwrap_or_else(|e| panic!("tile {:?} failed: {}", id, e));
                    assert_eq!(
                        tile.bytes.len(),
                        tile.cols as usize * tile.rows as usize * 4
                    );
                }
            }
        }
    }
}
