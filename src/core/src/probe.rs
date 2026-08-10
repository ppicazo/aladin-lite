//! Look at a FITS file without loading it.
//!
//! `probeFITS(url)` answers what the file contains — how many image HDUs, how
//! large they are, where they sit on the sky — by reading headers only. On a
//! server that honours `Range` this costs one request and a few kilobytes,
//! whatever the file's size, which is what makes it possible to decide how to
//! display a 4 GB image before committing to downloading any of it.

use al_fits::index::HduKind;
use al_fits::{ByteSource, HduEntry};
use serde::Serialize;
use wasm_bindgen::prelude::*;

/// Where an image HDU sits on the sky, if it says.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SkyPosition {
    /// Right ascension of the image centre, in degrees.
    pub ra: f64,
    /// Declination of the image centre, in degrees.
    pub dec: f64,
    /// Angular width of the image, in degrees.
    pub fov: f64,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HduSummary {
    pub index: usize,
    pub kind: HduKind,
    pub bitpix: i64,
    pub axes: Vec<u64>,
    /// Byte offset of the data unit within the file.
    pub data_offset: u64,
    /// Length of the data unit in bytes.
    pub data_length: u64,
    /// Present only for HDUs with at least two axes and a usable WCS.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub position: Option<SkyPosition>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProbeResult {
    pub url: String,
    /// How the bytes were reached: `http-range`, `blob` or `memory`.
    pub source: &'static str,
    /// Size of the whole file in bytes.
    pub size: u64,
    /// Bytes actually read to produce this answer.
    pub bytes_read: u64,
    pub hdus: Vec<HduSummary>,
}

/// Centre and angular extent of an image HDU, from its WCS.
fn sky_position(hdu: &HduEntry) -> Option<SkyPosition> {
    use al_fits::wcs::ImgXY;

    let (width, height, _) = hdu.image_dimensions()?;
    let wcs = hdu.wcs()?;

    let centre = wcs.unproj_lonlat(&ImgXY::new(width as f64 / 2.0, height as f64 / 2.0))?;
    let left = wcs.unproj_lonlat(&ImgXY::new(0.5, height as f64 / 2.0))?;
    let right = wcs.unproj_lonlat(&ImgXY::new(width as f64 - 0.5, height as f64 / 2.0))?;

    // Angular distance between the two edge midpoints, via the dot product of
    // their unit vectors — correct near the poles and across the 0h meridian,
    // where differencing longitudes is not.
    let (a, b) = (left.to_xyz(), right.to_xyz());
    let dot = (a.x() * b.x() + a.y() * b.y() + a.z() * b.z()).clamp(-1.0, 1.0);

    Some(SkyPosition {
        ra: centre.lon().to_degrees(),
        dec: centre.lat().to_degrees(),
        fov: dot.acos().to_degrees(),
    })
}

fn summarise(hdu: &HduEntry) -> HduSummary {
    HduSummary {
        index: hdu.index,
        kind: hdu.kind,
        bitpix: hdu.bitpix,
        axes: hdu.axes.clone(),
        data_offset: hdu.data.start,
        data_length: hdu.data_len(),
        position: sky_position(hdu),
    }
}

/// Describe the FITS file at `url` without downloading its pixels.
#[wasm_bindgen(js_name = probeFITS)]
pub async fn probe_fits(url: String) -> Result<JsValue, JsValue> {
    let source = al_fits::open_url(&url).await?;
    let index = al_fits::index(&source).await?;

    let result = ProbeResult {
        url,
        source: source.kind(),
        size: source.size(),
        bytes_read: index.bytes_read,
        hdus: index.hdus.iter().map(summarise).collect(),
    };

    serde_wasm_bindgen::to_value(&result).map_err(|e| JsValue::from_str(&e.to_string()))
}

/// What one tile read cost, and what it produced.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TileReport {
    pub level: u32,
    pub x: u32,
    pub y: u32,
    /// Samples in the decoded tile.
    pub cols: u32,
    pub rows: u32,
    /// Source pixels one sample covers, per axis.
    pub step_x: u64,
    pub step_y: u64,
    /// Bytes transferred, including anything coalescing pulled in unused.
    pub bytes_fetched: u64,
    /// Bytes the tile actually needed.
    pub bytes_used: u64,
    pub requests: usize,
    /// >1 when the byte budget forced the rows to be sampled more coarsely
    /// than the columns.
    pub row_thinning: u64,
    /// Wall-clock time for the fetch and decode together.
    pub total_ms: f64,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TileReadResult {
    pub url: String,
    pub source: &'static str,
    pub size: u64,
    /// Bytes spent indexing the file's structure.
    pub index_bytes: u64,
    pub hdu: usize,
    pub width: u64,
    pub height: u64,
    pub levels: u32,
    pub tiles: Vec<TileReport>,
    /// Fraction of the file transferred to produce all of the above.
    pub fraction_of_file: f64,
}

fn now_ms() -> f64 {
    js_sys::Date::now()
}

/// Read tiles from a FITS image over range requests, and report what it cost.
///
/// `tiles` is a flat `[level, x, y, ...]` list. This is the measurement entry
/// point for the tiled path — the renderer will drive the same `ImageReader`,
/// but this makes the cost visible without a camera in the loop.
#[wasm_bindgen(js_name = readFITSTiles)]
pub async fn read_fits_tiles(
    url: String,
    hdu: Option<usize>,
    tiles: Vec<u32>,
    budget_bytes: Option<f64>,
) -> Result<JsValue, JsValue> {
    use al_fits::reader::{ImageReader, DEFAULT_TILE_BUDGET};
    use al_fits::tile::TileId;

    if tiles.len() % 3 != 0 {
        return Err(JsValue::from_str(
            "tiles must be a flat list of level, x, y triples",
        ));
    }

    let budget = budget_bytes.map_or(DEFAULT_TILE_BUDGET, |b| b as u64);

    let source = al_fits::open_url(&url).await?;
    let reader = ImageReader::open(source, hdu).await?;

    let (width, height, _) = reader
        .entry()
        .image_dimensions()
        .ok_or_else(|| JsValue::from_str("HDU has no image dimensions"))?;

    let mut reports = Vec::with_capacity(tiles.len() / 3);
    let mut fetched = 0u64;

    for triple in tiles.chunks_exact(3) {
        let id = TileId::new(triple[0], triple[1], triple[2]);

        let started = now_ms();
        let (tile, stats) = reader.read_tile(id, budget).await?;
        let fetched_at = now_ms();

        let sampling = reader
            .sampling(id, budget)
            .ok_or_else(|| JsValue::from_str("tile is outside the image"))?;

        fetched += stats.bytes_fetched;

        reports.push(TileReport {
            level: id.level,
            x: id.x,
            y: id.y,
            cols: tile.cols,
            rows: tile.rows,
            step_x: sampling.step_x,
            step_y: sampling.step_y,
            bytes_fetched: stats.bytes_fetched,
            bytes_used: stats.bytes_used,
            requests: stats.requests,
            row_thinning: stats.row_thinning,
            total_ms: fetched_at - started,
        });
    }

    let size = reader.index.size;
    let result = TileReadResult {
        url,
        source: reader.source_kind(),
        size,
        index_bytes: reader.index.bytes_read,
        hdu: reader.hdu,
        width,
        height,
        levels: reader.grid.level_count(),
        tiles: reports,
        fraction_of_file: if size == 0 {
            0.0
        } else {
            (fetched + reader.index.bytes_read) as f64 / size as f64
        },
    };

    serde_wasm_bindgen::to_value(&result).map_err(|e| JsValue::from_str(&e.to_string()))
}

/// Describe a local `File` or `Blob` without reading its pixels.
#[wasm_bindgen(js_name = probeFITSBlob)]
pub async fn probe_fits_blob(blob: web_sys::Blob) -> Result<JsValue, JsValue> {
    let source = al_fits::open_blob(blob);
    let index = al_fits::index(&source).await?;

    let result = ProbeResult {
        url: String::new(),
        source: source.kind(),
        size: source.size(),
        bytes_read: index.bytes_read,
        hdus: index.hdus.iter().map(summarise).collect(),
    };

    serde_wasm_bindgen::to_value(&result).map_err(|e| JsValue::from_str(&e.to_string()))
}
