//! Tiling a FITS image, and working out which bytes a tile needs.
//!
//! # Why tiles cost what they cost
//!
//! FITS image data is row-major with the first axis varying fastest. A
//! rectangle of the image is therefore not a span of bytes but one span per
//! row, and those spans sit `NAXIS1 * bytes_per_pixel` apart.
//!
//! That geometry decides everything here. Zoomed in, a tile's rows are adjacent
//! and coalesce into one or two reads of a few hundred kilobytes — the case
//! this whole pipeline exists to make cheap. Zoomed out, the rows a tile needs
//! are scattered across the entire file, and every one of them must be touched;
//! no amount of cleverness turns a row-major layout into a cheap overview. That
//! is what a byte budget is for: the sampling coarsens until the read fits, and
//! the caller is told how coarse it had to get.

use crate::index::HduEntry;
use std::ops::Range;

/// Side length of a tile, in samples.
///
/// 512 keeps a float32 tile at 1 MiB — comfortably inside `MAX_TEXTURE_SIZE`
/// everywhere, and large enough that a zoomed-in view needs only a handful.
pub const TILE_SIZE: u32 = 512;

/// A tile: a level of detail, and a position in that level's grid.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TileId {
    /// 0 is full resolution; each level after that halves it.
    pub level: u32,
    pub x: u32,
    pub y: u32,
}

impl TileId {
    pub fn new(level: u32, x: u32, y: u32) -> Self {
        Self { level, x, y }
    }
}

/// Which source pixels a tile is made of.
///
/// Sampling is nearest-neighbour by construction: take every `step`-th pixel
/// starting at `origin`. `step` is per-axis because the byte budget can force
/// the rows to be thinned without thinning the columns — reading fewer rows
/// costs proportionally less, while reading fewer columns costs nothing at all,
/// since a row span is fetched whole either way.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sampling {
    pub origin_x: u64,
    pub origin_y: u64,
    pub step_x: u64,
    pub step_y: u64,
    /// Samples across. Fewer than [`TILE_SIZE`] at the image's right edge.
    pub cols: u32,
    /// Samples down.
    pub rows: u32,
}

impl Sampling {
    /// Source pixels covered, as `(width, height)`.
    pub fn covered(&self) -> (u64, u64) {
        (
            self.cols as u64 * self.step_x,
            self.rows as u64 * self.step_y,
        )
    }

    /// How much coarser the rows ended up than the columns.
    ///
    /// 1 when the tile is sampled squarely. Higher means the byte budget forced
    /// the rows to be thinned, and the tile is softer vertically than
    /// horizontally.
    pub fn row_thinning(&self) -> u64 {
        self.step_y / self.step_x
    }
}

/// The tile pyramid over one image HDU.
#[derive(Debug, Clone)]
pub struct TileGrid {
    pub width: u64,
    pub height: u64,
    pub bytes_per_pixel: u64,
    /// Samples across a tile.
    pub tile_w: u32,
    /// Samples down a tile.
    ///
    /// This is what a tile costs: a tile is read as one span per row, so its
    /// height *is* its request count. Short wide tiles buy the same pixels for
    /// a fraction of the requests, which on a wide image is the difference
    /// between a view arriving and not.
    pub tile_h: u32,
}

impl TileGrid {
    pub fn new(width: u64, height: u64, bytes_per_pixel: u64) -> Self {
        Self {
            width,
            height,
            bytes_per_pixel,
            tile_w: TILE_SIZE,
            tile_h: TILE_SIZE,
        }
    }

    /// The same image tiled differently.
    ///
    /// One image can be addressed by several grids at once — a square one for
    /// the overview, whose depth and top-level resolution depend on the tile
    /// being square, and a wide short one for refinement, where request count
    /// is what matters.
    pub fn with_tile_shape(&self, tile_w: u32, tile_h: u32) -> Self {
        Self {
            tile_w: tile_w.max(1),
            tile_h: tile_h.max(1),
            ..self.clone()
        }
    }

    pub fn from_hdu(hdu: &HduEntry) -> Option<Self> {
        let (width, height, _) = hdu.image_dimensions()?;
        Some(Self::new(width, height, hdu.bytes_per_pixel()))
    }

    /// Pixels per sample at `level`.
    pub fn step_at(&self, level: u32) -> u64 {
        1u64 << level.min(63)
    }

    /// Number of levels, the last of which holds the whole image in one tile.
    pub fn level_count(&self) -> u32 {
        let mut levels = 1;
        let (mut covered_x, mut covered_y) = (self.tile_w as u64, self.tile_h as u64);
        while covered_x < self.width.max(1) || covered_y < self.height.max(1) {
            covered_x = covered_x.saturating_mul(2);
            covered_y = covered_y.saturating_mul(2);
            levels += 1;
        }
        levels
    }

    /// Image pixels one tile spans at `level`, as `(width, height)`.
    pub fn span_at(&self, level: u32) -> (u64, u64) {
        let step = self.step_at(level);
        (step * self.tile_w as u64, step * self.tile_h as u64)
    }

    /// Grid dimensions at `level`, as `(columns, rows)`.
    pub fn tiles_at(&self, level: u32) -> (u32, u32) {
        let (span_x, span_y) = self.span_at(level);
        (
            self.width.div_ceil(span_x).max(1) as u32,
            self.height.div_ceil(span_y).max(1) as u32,
        )
    }

    /// The level at which one sample covers roughly `pixels_per_sample` source
    /// pixels — i.e. the coarsest level that still resolves what is on screen.
    pub fn level_for_scale(&self, pixels_per_sample: f64) -> u32 {
        if !pixels_per_sample.is_finite() || pixels_per_sample <= 1.0 {
            return 0;
        }
        let level = pixels_per_sample.log2().floor() as u32;
        level.min(self.level_count().saturating_sub(1))
    }

    /// Bytes one row span of `cols` samples at `step_x` costs.
    ///
    /// The whole span is fetched and then decimated, because the samples inside
    /// it are `step_x * bytes_per_pixel` apart and asking for them individually
    /// would mean hundreds of range requests per row.
    fn row_span_bytes(&self, origin_x: u64, cols: u32, step_x: u64) -> u64 {
        let last = origin_x + (cols.saturating_sub(1)) as u64 * step_x;
        let end = (last + 1).min(self.width);
        (end - origin_x) * self.bytes_per_pixel
    }

    /// How the given tile should be sampled, respecting a byte budget.
    ///
    /// Returns `None` when the tile lies outside the image at that level.
    ///
    /// When the natural sampling would read more than `max_bytes`, the row step
    /// is doubled until it fits. Callers that do not want that can pass
    /// `u64::MAX`; [`Sampling::row_thinning`] reports what happened.
    pub fn sampling(&self, tile: TileId, max_bytes: u64) -> Option<Sampling> {
        let step = self.step_at(tile.level);
        let (span_x, span_y) = self.span_at(tile.level);

        let origin_x = tile.x as u64 * span_x;
        let origin_y = tile.y as u64 * span_y;
        if origin_x >= self.width || origin_y >= self.height {
            return None;
        }

        let cols = (self.width - origin_x).div_ceil(step).min(self.tile_w as u64) as u32;

        let mut step_y = step;
        loop {
            // The row count is capped by the tile's height in *samples*, and
            // the rows a thinned tile keeps still span the whole tile.
            let rows = (self.height - origin_y)
                .div_ceil(step_y)
                .min((span_y / step_y).max(1)) as u32;

            let bytes = rows as u64 * self.row_span_bytes(origin_x, cols, step);

            // Stop thinning once it fits, or once there is nothing left to
            // thin: a single row must be read whatever it costs.
            if bytes <= max_bytes || rows <= 1 {
                return Some(Sampling {
                    origin_x,
                    origin_y,
                    step_x: step,
                    step_y,
                    cols,
                    rows,
                });
            }

            step_y = step_y.saturating_mul(2);
        }
    }
}

/// A contiguous read, and the sampled rows it covers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadSpan {
    /// Absolute byte range in the file.
    pub range: Range<u64>,
}

impl ReadSpan {
    pub fn len(&self) -> u64 {
        self.range.end - self.range.start
    }

    pub fn is_empty(&self) -> bool {
        self.range.is_empty()
    }
}

/// The reads needed to build one tile.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadPlan {
    pub spans: Vec<ReadSpan>,
    /// Byte range of each sampled row, in file coordinates and row order.
    pub rows: Vec<Range<u64>>,
}

impl ReadPlan {
    /// Bytes that will actually be transferred, including anything pulled in by
    /// coalescing that the tile does not use.
    pub fn bytes(&self) -> u64 {
        self.spans.iter().map(|s| s.len()).sum()
    }

    /// Bytes the tile needs. The gap to [`bytes`](Self::bytes) is coalescing overhead.
    pub fn useful_bytes(&self) -> u64 {
        self.rows.iter().map(|r| r.end - r.start).sum()
    }

    pub fn requests(&self) -> usize {
        self.spans.len()
    }
}

/// Bridge gaps up to this wide, fetching bytes the tile will not use in order
/// to save a request.
///
/// The trade is bytes against round trips, and there is no middle ground to
/// find: merging any run of a tile's rows costs about `row_stride / row_span`
/// times the useful bytes, whether the run is two rows or five hundred. So the
/// threshold is the whole decision, and it falls out of the arithmetic —
/// bridging is worth it while
///
/// ```text
/// gap / bandwidth  <  latency / concurrent_requests
/// ```
///
/// At 10 MB/s, 50 ms round trips and 8 requests in flight, that is 62 KiB.
///
/// It is worth being concrete about what this buys, because the byte counts
/// look alarming on their own. A 512 MB image has rows 46 KiB apart, so a tile
/// merges into one 23 MiB read to use 1 MiB — measured at 35 ms, against about
/// 360 ms for the same tile as 512 separate requests. A 4 GB image has rows
/// 256 KiB apart, past the threshold, so its tiles stay unmerged and fetch
/// exactly the megabyte they need. Both are the faster choice for their shape.
pub const COALESCE_GAP: u64 = 64 * 1024;

/// Work out which byte ranges a sampling needs, and merge the ones that are
/// close enough together to be worth fetching as one.
pub fn plan_reads(hdu: &HduEntry, grid: &TileGrid, sampling: &Sampling) -> ReadPlan {
    let row_bytes = grid.width * grid.bytes_per_pixel;
    let span_bytes = grid.row_span_bytes(sampling.origin_x, sampling.cols, sampling.step_x);

    let mut rows = Vec::with_capacity(sampling.rows as usize);
    for j in 0..sampling.rows as u64 {
        let y = sampling.origin_y + j * sampling.step_y;
        if y >= grid.height {
            break;
        }
        let start = hdu.data.start + y * row_bytes + sampling.origin_x * grid.bytes_per_pixel;
        rows.push(start..start + span_bytes);
    }

    let mut spans: Vec<ReadSpan> = Vec::new();
    for row in &rows {
        match spans.last_mut() {
            Some(last) if row.start.saturating_sub(last.range.end) <= COALESCE_GAP => {
                last.range.end = last.range.end.max(row.end);
            }
            _ => spans.push(ReadSpan {
                range: row.clone(),
            }),
        }
    }

    ReadPlan { spans, rows }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::HduKind;
    use serde_json::Map;

    fn hdu(width: u64, height: u64, bitpix: i64, data_start: u64) -> HduEntry {
        let bpp = bitpix.unsigned_abs() / 8;
        HduEntry {
            index: 0,
            kind: HduKind::Primary,
            header: 0..data_start,
            data: data_start..data_start + width * height * bpp,
            bitpix,
            axes: vec![width, height],
            keywords: Map::new(),
        }
    }

    #[test]
    fn level_count_covers_the_image_in_one_top_tile() {
        assert_eq!(TileGrid::new(512, 512, 4).level_count(), 1);
        assert_eq!(TileGrid::new(1024, 512, 4).level_count(), 2);
        assert_eq!(TileGrid::new(4096, 4096, 4).level_count(), 4);
        // 65536 = 512 * 2^7
        assert_eq!(TileGrid::new(65536, 65536, 4).level_count(), 8);
    }

    #[test]
    fn the_top_level_is_a_single_tile() {
        let grid = TileGrid::new(4096, 4096, 4);
        let top = grid.level_count() - 1;
        assert_eq!(grid.tiles_at(top), (1, 1));
        assert_eq!(grid.tiles_at(0), (8, 8));
    }

    #[test]
    fn level_follows_the_zoom() {
        let grid = TileGrid::new(65536, 65536, 4);
        // One screen pixel per source pixel: full resolution.
        assert_eq!(grid.level_for_scale(1.0), 0);
        assert_eq!(grid.level_for_scale(2.0), 1);
        assert_eq!(grid.level_for_scale(3.9), 1);
        assert_eq!(grid.level_for_scale(4.0), 2);
        // Zoomed out past the top of the pyramid, stay at the top.
        assert_eq!(grid.level_for_scale(1e9), grid.level_count() - 1);
    }

    #[test]
    fn tiles_outside_the_image_have_no_sampling() {
        let grid = TileGrid::new(1024, 1024, 4);
        assert!(grid.sampling(TileId::new(0, 0, 0), u64::MAX).is_some());
        assert!(grid.sampling(TileId::new(0, 2, 0), u64::MAX).is_none());
        assert!(grid.sampling(TileId::new(0, 0, 2), u64::MAX).is_none());
    }

    #[test]
    fn edge_tiles_are_short() {
        // 600 wide is one full tile plus 88 columns.
        let grid = TileGrid::new(600, 700, 4);
        let edge = grid.sampling(TileId::new(0, 1, 1), u64::MAX).unwrap();
        assert_eq!(edge.cols, 88);
        assert_eq!(edge.rows, 188);
    }

    #[test]
    fn a_zoomed_in_tile_coalesces_into_one_read() {
        // Adjacent rows of a level-0 tile are a row stride apart, well under
        // the coalescing gap for a 4096-wide image.
        let hdu = hdu(4096, 4096, -32, 2880);
        let grid = TileGrid::from_hdu(&hdu).unwrap();
        let sampling = grid.sampling(TileId::new(0, 1, 1), u64::MAX).unwrap();
        let plan = plan_reads(&hdu, &grid, &sampling);

        assert_eq!(plan.requests(), 1);
        assert_eq!(plan.rows.len(), 512);
        // 512 rows x 512 samples x 4 bytes of useful data.
        assert_eq!(plan.useful_bytes(), 512 * 512 * 4);
    }

    #[test]
    fn scattered_rows_stay_separate_reads() {
        // At the top level of a large image the sampled rows are megabytes
        // apart, so nothing coalesces and every row is its own request.
        let hdu = hdu(65536, 65536, -32, 2880);
        let grid = TileGrid::from_hdu(&hdu).unwrap();
        let top = grid.level_count() - 1;
        let sampling = grid.sampling(TileId::new(top, 0, 0), u64::MAX).unwrap();
        let plan = plan_reads(&hdu, &grid, &sampling);

        assert_eq!(sampling.step_x, 128);
        assert_eq!(plan.requests(), 512);
        // Each read spans a whole row bar the tail past the last sample: the
        // 512 samples in it are 512 bytes apart, and fetching them individually
        // would mean 512 range requests per row instead of one.
        let span = (511 * 128 + 1) * 4;
        assert_eq!(plan.bytes(), 512 * span);
        assert_eq!(plan.useful_bytes(), plan.bytes());
    }

    #[test]
    fn coalescing_is_all_or_nothing_either_side_of_the_threshold() {
        // 11585 wide: rows are 46 KiB apart, inside the gap threshold, so a
        // tile becomes one read that pulls in far more than it uses — and is
        // an order of magnitude faster for it.
        let narrow = hdu(11585, 11585, -32, 2880);
        let grid = TileGrid::from_hdu(&narrow).unwrap();
        let sampling = grid.sampling(TileId::new(0, 11, 11), u64::MAX).unwrap();
        let plan = plan_reads(&narrow, &grid, &sampling);

        assert_eq!(plan.requests(), 1);
        assert_eq!(plan.useful_bytes(), 512 * 512 * 4);
        assert!(plan.bytes() > plan.useful_bytes() * 20);

        // 65536 wide: rows are 256 KiB apart, past the threshold, so nothing
        // merges and the tile fetches exactly what it needs.
        let wide = hdu(65536, 65536, -32, 2880);
        let grid = TileGrid::from_hdu(&wide).unwrap();
        let sampling = grid.sampling(TileId::new(0, 32, 32), u64::MAX).unwrap();
        let plan = plan_reads(&wide, &grid, &sampling);

        assert_eq!(plan.requests(), 512);
        assert_eq!(plan.bytes(), plan.useful_bytes());
    }

    #[test]
    fn a_wide_short_tile_costs_far_fewer_requests() {
        // The whole point of non-square tiles: a tile is one request per row,
        // so the same pixels cost proportionally fewer requests when the tile
        // is short. 65536 wide means nothing coalesces either way.
        let hdu = hdu(65536, 65536, -32, 2880);

        let square = TileGrid::from_hdu(&hdu).unwrap();
        let sampling = square.sampling(TileId::new(0, 0, 0), u64::MAX).unwrap();
        let square_plan = plan_reads(&hdu, &square, &sampling);

        let wide = square.with_tile_shape(2048, 128);
        let sampling = wide.sampling(TileId::new(0, 0, 0), u64::MAX).unwrap();
        let wide_plan = plan_reads(&hdu, &wide, &sampling);

        assert_eq!(square_plan.requests(), 512);
        assert_eq!(wide_plan.requests(), 128);
        // And for that it buys the same number of samples.
        assert_eq!(square_plan.useful_bytes(), 512 * 512 * 4);
        assert_eq!(wide_plan.useful_bytes(), 2048 * 128 * 4);
    }

    #[test]
    fn a_wide_grid_still_covers_every_pixel() {
        let grid = TileGrid::new(5000, 3000, 4).with_tile_shape(2048, 128);
        let (nx, ny) = grid.tiles_at(0);

        let mut samples = 0u64;
        for ty in 0..ny {
            for tx in 0..nx {
                let s = grid.sampling(TileId::new(0, tx, ty), u64::MAX).unwrap();
                samples += s.cols as u64 * s.rows as u64;
            }
        }
        assert_eq!(samples, 5000 * 3000);
    }

    #[test]
    fn a_non_square_pyramid_still_ends_in_one_tile() {
        let grid = TileGrid::new(32768, 32768, 4).with_tile_shape(2048, 128);
        let top = grid.level_count() - 1;
        assert_eq!(grid.tiles_at(top), (1, 1));

        // The shorter axis is what sets the depth, so this pyramid is deeper
        // than the square one over the same image.
        assert!(grid.level_count() > TileGrid::new(32768, 32768, 4).level_count());
    }

    #[test]
    fn the_square_default_is_unchanged() {
        let grid = TileGrid::new(4096, 4096, 4);
        assert_eq!((grid.tile_w, grid.tile_h), (TILE_SIZE, TILE_SIZE));
        assert_eq!(grid.level_count(), 4);
        assert_eq!(grid.tiles_at(0), (8, 8));
    }

    #[test]
    fn a_byte_budget_thins_the_rows() {
        let hdu = hdu(65536, 65536, -32, 2880);
        let grid = TileGrid::from_hdu(&hdu).unwrap();
        let top = grid.level_count() - 1;

        let budget = 16 * 1024 * 1024;
        let sampling = grid.sampling(TileId::new(top, 0, 0), budget).unwrap();
        let plan = plan_reads(&hdu, &grid, &sampling);

        assert!(
            plan.bytes() <= budget,
            "plan reads {} bytes, budget was {}",
            plan.bytes(),
            budget
        );
        // Columns are untouched; only the rows were thinned.
        assert_eq!(sampling.cols, 512);
        assert!(sampling.rows < 512);
        assert!(sampling.row_thinning() > 1);
    }

    #[test]
    fn a_budget_never_thins_below_one_row() {
        // A budget smaller than a single row must still produce a usable plan
        // rather than looping forever or returning nothing.
        let hdu = hdu(65536, 65536, -32, 2880);
        let grid = TileGrid::from_hdu(&hdu).unwrap();
        let top = grid.level_count() - 1;

        let sampling = grid.sampling(TileId::new(top, 0, 0), 1).unwrap();
        assert_eq!(sampling.rows, 1);
    }

    #[test]
    fn planned_rows_stay_inside_the_data_unit() {
        let hdu = hdu(600, 700, 16, 2880);
        let grid = TileGrid::from_hdu(&hdu).unwrap();

        for level in 0..grid.level_count() {
            let (nx, ny) = grid.tiles_at(level);
            for ty in 0..ny {
                for tx in 0..nx {
                    let sampling = grid.sampling(TileId::new(level, tx, ty), u64::MAX).unwrap();
                    let plan = plan_reads(&hdu, &grid, &sampling);
                    for row in &plan.rows {
                        assert!(
                            row.start >= hdu.data.start && row.end <= hdu.data.end,
                            "level {} tile {},{} reads {:?}, data is {:?}",
                            level,
                            tx,
                            ty,
                            row,
                            hdu.data
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn level_zero_tiles_cover_every_pixel_exactly_once() {
        let hdu = hdu(600, 700, 16, 2880);
        let grid = TileGrid::from_hdu(&hdu).unwrap();
        let (nx, ny) = grid.tiles_at(0);

        let mut samples = 0u64;
        for ty in 0..ny {
            for tx in 0..nx {
                let s = grid.sampling(TileId::new(0, tx, ty), u64::MAX).unwrap();
                samples += s.cols as u64 * s.rows as u64;
            }
        }
        assert_eq!(samples, 600 * 700);
    }
}
