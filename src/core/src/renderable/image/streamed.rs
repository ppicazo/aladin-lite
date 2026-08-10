//! Building an [`Image`](super::Image) out of pyramid tiles.
//!
//! The existing loader turns a whole FITS file into a dense grid of texture
//! patches and lets the mesh in [`grid`](super::grid) map image pixels onto the
//! sky. Tiles from `al-fits` are the same shape — a dense, uniform, row-major
//! grid — so the entire drawing path is reused as-is. The only difference is
//! what a patch means: at pyramid level *L* one patch still covers
//! `TILE_SIZE` texels, but those texels span `TILE_SIZE * 2^L` image pixels.
//!
//! That is what lets a 4 GB image be displayed at all. A level is chosen whose
//! whole grid is a handful of tiles, and only those tiles are read.

use al_core::texture::format::{TextureFormat, R16I, R32F, R32I, R8U};
use al_core::{Texture2D, WebGlContext};
use al_fits::decode::{PixelKind, Tile};
use al_fits::reader::ImageReader;
use al_fits::tile::{Sampling, TileId};
use wasm_bindgen::JsValue;

use super::{Image, ImagePatches, TEX_PARAMS};
use al_api::coo_system::CooSystem;
use al_core::texture::format::PixelType;

/// How many samples a *full* patch yields at this sampling.
///
/// Not always the tile size. The mesh maps a patch's UVs across the whole
/// texture, so the texture has to represent the whole patch — and when the byte
/// budget thins the rows, a full patch is `tile_size / thinning` rows tall
/// rather than `tile_size`. Sizing the texture at `tile_size` regardless would
/// leave the samples occupying a fraction of it, and the image would render as
/// a band with the rest blank.
fn texture_size(tile_size: u32, sampling: &Sampling) -> (u32, u32) {
    let patch_pixels = tile_size as u64 * sampling.step_x;
    let rows = (patch_pixels / sampling.step_y).max(1) as u32;
    (tile_size, rows)
}

/// Place a tile's samples at the start of a zeroed texture buffer.
///
/// Tiles at the right and bottom edges of the image are short, but the mesh
/// addresses every patch as if it were whole and simply stops its UVs early.
/// Putting the samples at the start is what makes those UVs point at them.
fn padded<F: TextureFormat>(tile: &Tile, size: (u32, u32)) -> Vec<u8> {
    let (width, height) = size;
    let stride = width as usize * F::NUM_CHANNELS;
    let mut buffer = vec![0u8; stride * height as usize];

    let row_bytes = tile.cols as usize * F::NUM_CHANNELS;
    for row in 0..(tile.rows as usize).min(height as usize) {
        let src = row * row_bytes;
        let dst = row * stride;
        buffer[dst..dst + row_bytes].copy_from_slice(&tile.bytes[src..src + row_bytes]);
    }

    buffer
}

fn texture_of<F: TextureFormat>(
    gl: &WebGlContext,
    tile: &Tile,
    size: (u32, u32),
) -> Result<Texture2D, JsValue> {
    Texture2D::create_from_raw_bytes::<F>(
        gl,
        size.0 as i32,
        size.1 as i32,
        TEX_PARAMS,
        &padded::<F>(tile, size),
    )
}

fn pixel_type_of(kind: PixelKind) -> PixelType {
    match kind {
        PixelKind::U8 => PixelType::R8U,
        PixelKind::I16 => PixelType::R16I,
        PixelKind::I32 => PixelType::R32I,
        PixelKind::F32 => PixelType::R32F,
    }
}

/// The coarsest level: the whole image in a single tile.
///
/// Counting tiles is the wrong way to budget this. What a read costs is the
/// number of *rows* it touches, and every level's tile has the same 512 of
/// them — so a level with sixteen tiles costs sixteen times a level with one,
/// for detail that a first view cannot show anyway. On a 4 GB image that is
/// 8192 range requests against 128.
///
/// So the initial view is always the top of the pyramid. Finer levels belong
/// to camera-driven refinement, which asks only for the tiles actually on
/// screen.
fn initial_level(reader: &ImageReader) -> u32 {
    reader.grid.level_count() - 1
}

/// Read a whole pyramid level and build a displayable image from it.
///
/// Returns the image and the level it settled on.
pub async fn image_from_level(
    gl: &WebGlContext,
    reader: &ImageReader,
    tile_budget: u64,
    coo_sys: CooSystem,
) -> Result<(Image, u32), JsValue> {
    let level = initial_level(reader);
    let (nx, ny) = reader.grid.tiles_at(level);
    let tile_size = reader.grid.tile_size;

    let entry = reader.entry();
    let wcs = entry
        .wcs()
        .ok_or_else(|| JsValue::from_str("image HDU has no WCS, so it cannot be placed on the sky"))?;

    let mut textures = Vec::with_capacity((nx * ny) as usize);
    let mut samples = Vec::new();
    let mut pixel_type = None;

    // Row-major, every tile present: the mesh indexes patches by position and
    // has no way to express a hole.
    for ty in 0..ny {
        for tx in 0..nx {
            let id = TileId::new(level, tx, ty);
            let sampling = reader
                .sampling(id, tile_budget)
                .ok_or_else(|| JsValue::from_str("tile lies outside the image"))?;
            let (tile, _) = reader.read_tile(id, tile_budget).await?;

            let size = texture_size(tile_size, &sampling);
            let texture = match tile.kind {
                PixelKind::U8 => texture_of::<R8U>(gl, &tile, size)?,
                PixelKind::I16 => texture_of::<R16I>(gl, &tile, size)?,
                PixelKind::I32 => texture_of::<R32I>(gl, &tile, size)?,
                PixelKind::F32 => texture_of::<R32F>(gl, &tile, size)?,
            };

            pixel_type.get_or_insert(pixel_type_of(tile.kind));
            samples.extend(tile.samples);
            textures.push(texture);
        }
    }

    let pixel_type =
        pixel_type.ok_or_else(|| JsValue::from_str("image has no tiles to display"))?;

    for texture in &textures {
        texture.generate_mipmap();
    }

    // Cuts come from the samples the tiles collected on their way through the
    // decoder, so they cost nothing extra and already describe the whole image.
    let cuts = al_fits::decode::percentile_cuts(&mut samples, 1.0, 99.0).unwrap_or(0.0..1.0);

    // A patch spans TILE_SIZE texels, which at this level is TILE_SIZE * 2^L
    // image pixels — and the mesh works in image pixels.
    let patch_pixels = (tile_size as u64 * reader.grid.step_at(level)) as usize;

    let image = Image::from_patches(
        gl.clone(),
        ImagePatches::new(pixel_type, textures, cuts, patch_pixels, patch_pixels),
        wcs,
        reader.bscale,
        reader.bzero,
        reader.blank,
        coo_sys,
        // al-fits parses headers into its own map; the fitsrs ValueMap the
        // layer hands back to JS cannot be built from outside that crate, so
        // the streamed path reports no header dictionary for now.
        None,
    )?;

    Ok((image, level))
}

/// Where the image sits on the sky and how big it is, for the JS caller.
pub fn describe(reader: &ImageReader) -> Result<(f64, f64, f64), JsValue> {
    use al_fits::wcs::ImgXY;

    let entry = reader.entry();
    let (width, height, _) = entry
        .image_dimensions()
        .ok_or_else(|| JsValue::from_str("HDU has no image dimensions"))?;
    let wcs = entry
        .wcs()
        .ok_or_else(|| JsValue::from_str("HDU has no WCS"))?;

    let centre = wcs
        .unproj_lonlat(&ImgXY::new(width as f64 / 2.0, height as f64 / 2.0))
        .ok_or_else(|| JsValue::from_str("image centre does not lie on the sky"))?;
    let left = wcs.unproj_lonlat(&ImgXY::new(0.5, height as f64 / 2.0));
    let right = wcs.unproj_lonlat(&ImgXY::new(width as f64 - 0.5, height as f64 / 2.0));

    let fov = match (left, right) {
        (Some(a), Some(b)) => {
            let (a, b) = (a.to_xyz(), b.to_xyz());
            let dot = (a.x() * b.x() + a.y() * b.y() + a.z() * b.z()).clamp(-1.0, 1.0);
            dot.acos().to_degrees()
        }
        _ => 180.0,
    };

    Ok((
        centre.lon().to_degrees(),
        centre.lat().to_degrees(),
        fov,
    ))
}
