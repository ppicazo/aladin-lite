//! Turning fetched byte spans into a tile the GPU can hold.
//!
//! # Byte order
//!
//! FITS stores pixels big-endian. Aladin Lite does *not* swap them: a float32
//! image is uploaded as an RGBA8 texture and a 16-bit integer image as RG8, and
//! the fragment shader reassembles the value from those bytes (see
//! `src/glsl/webgl2/decode.glsl`). So the decoder's job is to *gather* the
//! sampled pixels, not to convert them — the bytes go through untouched, in
//! their original order.
//!
//! The exceptions are the 64-bit types, which have no place to live on the GPU
//! and are narrowed here to 32-bit, exactly as the existing loader does.

use crate::error::{Error, Result};
use crate::tile::{ReadPlan, Sampling};
use std::ops::Range;

/// The GPU-side pixel type a `BITPIX` maps to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PixelKind {
    U8,
    I16,
    I32,
    F32,
}

impl PixelKind {
    /// `None` for a `BITPIX` that is not a pixel type we can display.
    pub fn from_bitpix(bitpix: i64) -> Option<Self> {
        match bitpix {
            8 => Some(PixelKind::U8),
            16 => Some(PixelKind::I16),
            // 64-bit values are narrowed to 32 bits on the way to the GPU.
            32 | 64 => Some(PixelKind::I32),
            -32 | -64 => Some(PixelKind::F32),
            _ => None,
        }
    }

    /// Bytes per pixel once on the GPU.
    pub fn size(&self) -> usize {
        match self {
            PixelKind::U8 => 1,
            PixelKind::I16 => 2,
            PixelKind::I32 | PixelKind::F32 => 4,
        }
    }
}

/// A decoded tile, ready to upload.
#[derive(Debug, Clone)]
pub struct Tile {
    pub kind: PixelKind,
    pub cols: u32,
    pub rows: u32,
    /// `cols * rows` pixels, big-endian, row-major.
    pub bytes: Vec<u8>,
    /// Finite, non-blank sample values, for computing cuts. Sampled sparsely:
    /// percentiles do not need every pixel, and sorting a million of them per
    /// tile would cost more than the decode.
    pub samples: Vec<f32>,
}

/// One pixel's bytes read big-endian, as an f32, or `None` if it is blank or
/// not finite.
fn value_of(kind: PixelKind, px: &[u8], blank: Option<f32>) -> Option<f32> {
    let value = match kind {
        PixelKind::U8 => px[0] as f32,
        PixelKind::I16 => i16::from_be_bytes([px[0], px[1]]) as f32,
        PixelKind::I32 => i32::from_be_bytes([px[0], px[1], px[2], px[3]]) as f32,
        PixelKind::F32 => f32::from_be_bytes([px[0], px[1], px[2], px[3]]),
    };

    if !value.is_finite() {
        return None;
    }
    if blank == Some(value) {
        return None;
    }
    Some(value)
}

/// Copy one source pixel into the tile, narrowing 64-bit values on the way.
fn write_pixel(bitpix: i64, src: &[u8], dst: &mut [u8]) {
    match bitpix {
        64 => {
            let v = i64::from_be_bytes([
                src[0], src[1], src[2], src[3], src[4], src[5], src[6], src[7],
            ]);
            dst.copy_from_slice(&(v as i32).to_be_bytes());
        }
        -64 => {
            let v = f64::from_be_bytes([
                src[0], src[1], src[2], src[3], src[4], src[5], src[6], src[7],
            ]);
            dst.copy_from_slice(&(v as f32).to_be_bytes());
        }
        _ => dst.copy_from_slice(src),
    }
}

/// Take roughly this many samples per tile for the statistics.
///
/// Percentiles off ~4k samples are within a fraction of a percent of the true
/// value for image data, and cost nothing next to the gather.
const STAT_SAMPLES: usize = 4096;

/// Assemble a tile from the bytes fetched for its [`ReadPlan`].
///
/// `spans` must be the bytes of `plan.spans`, in order — one buffer per span,
/// each exactly as long as its span.
pub fn decode_tile(
    bitpix: i64,
    sampling: &Sampling,
    plan: &ReadPlan,
    spans: &[Vec<u8>],
    blank: Option<f32>,
) -> Result<Tile> {
    let kind = PixelKind::from_bitpix(bitpix)
        .ok_or_else(|| Error::Format(format!("BITPIX {} is not a pixel type", bitpix)))?;

    if spans.len() != plan.spans.len() {
        return Err(Error::Format(format!(
            "got {} buffers for a plan with {} spans",
            spans.len(),
            plan.spans.len()
        )));
    }
    for (buffer, span) in spans.iter().zip(&plan.spans) {
        if buffer.len() as u64 != span.len() {
            return Err(Error::Format(format!(
                "span {:?} is {} bytes, got {}",
                span.range,
                span.len(),
                buffer.len()
            )));
        }
    }

    let src_size = bitpix.unsigned_abs() as usize / 8;
    let dst_size = kind.size();
    let cols = sampling.cols as usize;
    let rows = plan.rows.len();

    let mut bytes = vec![0u8; cols * rows * dst_size];
    let mut samples = Vec::new();

    // Sample every nth pixel for the statistics, spread over the whole tile.
    let total = (cols * rows).max(1);
    let stat_step = total.div_ceil(STAT_SAMPLES).max(1);
    let mut seen = 0usize;

    /// Find the fetched buffer holding a row, and the row's offset within it.
    fn locate<'a>(
        spans: &[crate::tile::ReadSpan],
        buffers: &'a [Vec<u8>],
        row: &Range<u64>,
    ) -> Option<&'a [u8]> {
        let (i, span) = spans
            .iter()
            .enumerate()
            .find(|(_, s)| s.range.start <= row.start && row.end <= s.range.end)?;
        let offset = (row.start - span.range.start) as usize;
        buffers
            .get(i)?
            .get(offset..offset + (row.end - row.start) as usize)
    }

    for (j, row) in plan.rows.iter().enumerate() {
        let row_bytes = locate(&plan.spans, spans, row).ok_or_else(|| {
            Error::Format(format!("row {:?} is not covered by any fetched span", row))
        })?;

        for i in 0..cols {
            let src_offset = i * sampling.step_x as usize * src_size;
            let Some(px) = row_bytes.get(src_offset..src_offset + src_size) else {
                // The last row span stops at the last sample, so a short read
                // here means the plan and the sampling disagree.
                return Err(Error::Format(format!(
                    "sample {} of row {} falls outside its {}-byte span",
                    i,
                    j,
                    row_bytes.len()
                )));
            };

            let dst_offset = (j * cols + i) * dst_size;
            write_pixel(bitpix, px, &mut bytes[dst_offset..dst_offset + dst_size]);

            if seen % stat_step == 0 {
                if let Some(v) = value_of(kind, px, blank) {
                    samples.push(v);
                }
            }
            seen += 1;
        }
    }

    Ok(Tile {
        kind,
        cols: cols as u32,
        rows: rows as u32,
        bytes,
        samples,
    })
}

/// The `[first, last]` percentile range of `values`, the same 1%–99% cut the
/// existing loader applies.
///
/// Consumes the slice's order: selection is done in place.
pub fn percentile_cuts(values: &mut [f32], first: f32, last: f32) -> Option<Range<f32>> {
    if values.is_empty() {
        return None;
    }

    let pick = |values: &mut [f32], percent: f32| -> f32 {
        let n = values.len();
        // In f64: 1% of 1000 lands on 9.99999977 in f32, which truncates to the
        // wrong element.
        let i = (((percent as f64 * 0.01) * n as f64) as usize).min(n - 1);
        let (_, value, _) = values.select_nth_unstable_by(i, |a, b| a.total_cmp(b));
        *value
    };

    let low = pick(values, first.min(last));
    let high = pick(values, first.max(last));
    Some(low..high)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::{HduEntry, HduKind};
    use crate::tile::{plan_reads, TileGrid, TileId};
    use serde_json::Map;

    const DATA_START: u64 = 2880;

    fn hdu(width: u64, height: u64, bitpix: i64) -> HduEntry {
        let bpp = bitpix.unsigned_abs() / 8;
        HduEntry {
            index: 0,
            kind: HduKind::Primary,
            header: 0..DATA_START,
            data: DATA_START..DATA_START + width * height * bpp,
            bitpix,
            axes: vec![width, height],
            keywords: Map::new(),
        }
    }

    /// A file whose pixel at (x, y) has the value `y * width + x`.
    fn ramp_file(width: u64, height: u64, bitpix: i64) -> Vec<u8> {
        let mut bytes = vec![0u8; DATA_START as usize];
        for y in 0..height {
            for x in 0..width {
                let v = (y * width + x) as f32;
                match bitpix {
                    8 => bytes.push(v as u8),
                    16 => bytes.extend((v as i16).to_be_bytes()),
                    32 => bytes.extend((v as i32).to_be_bytes()),
                    -32 => bytes.extend(v.to_be_bytes()),
                    -64 => bytes.extend((v as f64).to_be_bytes()),
                    64 => bytes.extend((v as i64).to_be_bytes()),
                    _ => unreachable!(),
                }
            }
        }
        bytes
    }

    /// Serve a plan's spans out of an in-memory file.
    fn fetch(file: &[u8], plan: &ReadPlan) -> Vec<Vec<u8>> {
        plan.spans
            .iter()
            .map(|s| file[s.range.start as usize..s.range.end as usize].to_vec())
            .collect()
    }

    fn decode_at(width: u64, height: u64, bitpix: i64, tile: TileId) -> Tile {
        let hdu = hdu(width, height, bitpix);
        let file = ramp_file(width, height, bitpix);
        let grid = TileGrid::from_hdu(&hdu).unwrap();
        let sampling = grid.sampling(tile, u64::MAX).unwrap();
        let plan = plan_reads(&hdu, &grid, &sampling);
        decode_tile(bitpix, &sampling, &plan, &fetch(&file, &plan), None).unwrap()
    }

    fn f32_at(tile: &Tile, x: usize, y: usize) -> f32 {
        let o = (y * tile.cols as usize + x) * 4;
        f32::from_be_bytes([
            tile.bytes[o],
            tile.bytes[o + 1],
            tile.bytes[o + 2],
            tile.bytes[o + 3],
        ])
    }

    #[test]
    fn level_zero_reproduces_the_pixels_exactly() {
        let tile = decode_at(600, 40, -32, TileId::new(0, 0, 0));
        assert_eq!((tile.cols, tile.rows), (512, 40));

        assert_eq!(f32_at(&tile, 0, 0), 0.0);
        assert_eq!(f32_at(&tile, 511, 0), 511.0);
        assert_eq!(f32_at(&tile, 0, 1), 600.0);
        assert_eq!(f32_at(&tile, 7, 3), (3 * 600 + 7) as f32);
    }

    #[test]
    fn the_second_tile_starts_where_the_first_ends() {
        let tile = decode_at(600, 40, -32, TileId::new(0, 1, 0));
        assert_eq!(tile.cols, 88);
        assert_eq!(f32_at(&tile, 0, 0), 512.0);
        assert_eq!(f32_at(&tile, 87, 0), 599.0);
    }

    #[test]
    fn coarse_levels_take_every_nth_pixel() {
        // Level 1 samples every second pixel on both axes.
        let tile = decode_at(600, 40, -32, TileId::new(1, 0, 0));
        assert_eq!((tile.cols, tile.rows), (300, 20));
        assert_eq!(f32_at(&tile, 0, 0), 0.0);
        assert_eq!(f32_at(&tile, 1, 0), 2.0);
        assert_eq!(f32_at(&tile, 0, 1), (2 * 600) as f32);
        assert_eq!(f32_at(&tile, 5, 3), (6 * 600 + 10) as f32);
    }

    #[test]
    fn bytes_keep_their_big_endian_order() {
        // The shader reads the value back out of the texture's bytes, so the
        // decoder must not helpfully swap them.
        let tile = decode_at(8, 2, 16, TileId::new(0, 0, 0));
        assert_eq!(tile.kind, PixelKind::I16);
        assert_eq!(&tile.bytes[0..2], &0i16.to_be_bytes());
        assert_eq!(&tile.bytes[2..4], &1i16.to_be_bytes());
        assert_eq!(&tile.bytes[16..18], &8i16.to_be_bytes());
    }

    #[test]
    fn sixty_four_bit_pixels_are_narrowed() {
        let tile = decode_at(8, 2, -64, TileId::new(0, 0, 0));
        assert_eq!(tile.kind, PixelKind::F32);
        assert_eq!(tile.bytes.len(), 8 * 2 * 4);
        assert_eq!(f32_at(&tile, 3, 1), 11.0);

        let tile = decode_at(8, 2, 64, TileId::new(0, 0, 0));
        assert_eq!(tile.kind, PixelKind::I32);
        assert_eq!(tile.bytes.len(), 8 * 2 * 4);
    }

    #[test]
    fn statistics_ignore_blanks_and_non_finite_values() {
        let hdu = hdu(4, 2, -32);
        let mut file = vec![0u8; DATA_START as usize];
        for v in [1.0f32, f32::NAN, -999.0, 3.0, f32::INFINITY, 2.0, 4.0, 5.0] {
            file.extend(v.to_be_bytes());
        }

        let grid = TileGrid::from_hdu(&hdu).unwrap();
        let sampling = grid.sampling(TileId::new(0, 0, 0), u64::MAX).unwrap();
        let plan = plan_reads(&hdu, &grid, &sampling);
        let tile = decode_tile(-32, &sampling, &plan, &fetch(&file, &plan), Some(-999.0)).unwrap();

        let mut samples = tile.samples.clone();
        samples.sort_by(f32::total_cmp);
        assert_eq!(samples, vec![1.0, 2.0, 3.0, 4.0, 5.0]);
    }

    #[test]
    fn a_mismatched_buffer_is_rejected_rather_than_misread() {
        let hdu = hdu(16, 4, -32);
        let file = ramp_file(16, 4, -32);
        let grid = TileGrid::from_hdu(&hdu).unwrap();
        let sampling = grid.sampling(TileId::new(0, 0, 0), u64::MAX).unwrap();
        let plan = plan_reads(&hdu, &grid, &sampling);

        let mut spans = fetch(&file, &plan);
        let shortened = spans[0].len() - 1;
        spans[0].truncate(shortened);
        assert!(decode_tile(-32, &sampling, &plan, &spans, None).is_err());

        assert!(decode_tile(-32, &sampling, &plan, &[], None).is_err());
    }

    #[test]
    fn percentiles_bracket_the_data() {
        let mut values: Vec<f32> = (0..1000).map(|i| i as f32).collect();
        let cuts = percentile_cuts(&mut values, 1.0, 99.0).unwrap();
        assert_eq!(cuts.start, 10.0);
        assert_eq!(cuts.end, 990.0);

        // Reversed bounds must not produce an inverted range.
        let mut values: Vec<f32> = (0..1000).map(|i| i as f32).collect();
        let cuts = percentile_cuts(&mut values, 99.0, 1.0).unwrap();
        assert!(cuts.start <= cuts.end);

        assert!(percentile_cuts(&mut [], 1.0, 99.0).is_none());
    }

    #[test]
    fn an_unsupported_bitpix_is_an_error_not_a_panic() {
        let hdu = hdu(4, 2, -32);
        let grid = TileGrid::from_hdu(&hdu).unwrap();
        let sampling = grid.sampling(TileId::new(0, 0, 0), u64::MAX).unwrap();
        let plan = plan_reads(&hdu, &grid, &sampling);
        assert!(decode_tile(7, &sampling, &plan, &[vec![0; 32]], None).is_err());
    }
}
