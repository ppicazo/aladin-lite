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

// ---------------------------------------------------------------------------
// Camera-driven refinement
// ---------------------------------------------------------------------------

use al_core::VertexArrayObject;
use al_fits::reader::ImageReader as Reader;
use al_fits::tile::TileGrid;
use cgmath::Vector3;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use crate::camera::CameraViewPort;
use crate::ProjectionType as Proj;

/// A tile that has been read and decoded, waiting to become a texture.
struct TileReady {
    id: TileId,
    tile: Tile,
    size: (u32, u32),
}

/// Reads in flight at once.
///
/// Tile reads are latency-bound rather than bandwidth-bound, so several should
/// be outstanding; but a viewport can want dozens of tiles, and queueing all of
/// them makes the view lag behind the camera by however long the queue takes to
/// drain.
const MAX_TILES_IN_FLIGHT: usize = 8;

/// Textures kept before old ones are dropped.
///
/// Enough for several screenfuls at the current level plus the levels either
/// side of it, so zooming in and back out does not re-read what was just shown.
const MAX_CACHED_TILES: usize = 96;

/// The refinement drawn on top of an image's overview.
///
/// The overview covers the whole image at the top of the pyramid and is always
/// present, so this only ever has to fill in detail: any tile it is missing
/// simply is not drawn, and the coarser pixels underneath show through.
pub struct Refine {
    reader: Rc<Reader>,
    budget: u64,
    /// Level currently being requested, finest at 0.
    level: u32,
    tiles: HashMap<TileId, Texture2D>,
    pending: HashSet<TileId>,
    ready_send: async_channel::Sender<TileReady>,
    ready_recv: async_channel::Receiver<TileReady>,

    vao: VertexArrayObject,
    pos: Vec<f32>,
    uv: Vec<f32>,
    indices: Vec<u16>,
    num_indices: Vec<u32>,
    /// The tile each patch of the current mesh draws, in patch order.
    patch_tiles: Vec<TileId>,
}

/// The image-pixel rectangle the camera can currently see, as
/// `(x_min, y_min, x_max, y_max)`.
///
/// Found by sampling the viewport, deprojecting each sample to the sky and
/// projecting it back through the WCS. Sampling rather than solving because the
/// image's outline in screen space is whatever the sky projection makes of it,
/// and only some of the samples land on the image at all — those that miss are
/// dropped, and if none land the image is off screen.
fn visible_image_rect(
    camera: &CameraViewPort,
    projection: &Proj,
    wcs: &fitsrs::WCS,
) -> Option<(f64, f64, f64, f64)> {
    const SAMPLES: usize = 12;

    let size = camera.get_screen_size();
    let dim = wcs.img_dimensions();
    let (width, height) = (dim[0] as f64, dim[1] as f64);

    let mut found = false;
    let (mut x0, mut y0) = (f64::MAX, f64::MAX);
    let (mut x1, mut y1) = (f64::MIN, f64::MIN);

    for j in 0..=SAMPLES {
        for i in 0..=SAMPLES {
            let screen = crate::math::projection::coo_space::XYScreen::new(
                size.x as f64 * (i as f64) / (SAMPLES as f64),
                size.y as f64 * (j as f64) / (SAMPLES as f64),
            );

            let Some(model) = projection.screen_to_model_space(&screen, camera) else {
                continue;
            };

            // Back to ICRS, then undo the axis permutation the mesh applies
            // when it goes the other way.
            let icrs = crate::coosys::apply_coo_system(
                camera.get_coo_system(),
                CooSystem::ICRS,
                &Vector3::new(model.x, model.y, model.z),
            );

            let Some(xy) = wcs.proj_xyz(&(icrs.z, icrs.x, icrs.y)) else {
                continue;
            };

            let (x, y) = (xy.x(), xy.y());
            if !x.is_finite() || !y.is_finite() {
                continue;
            }

            found = true;
            x0 = x0.min(x);
            y0 = y0.min(y);
            x1 = x1.max(x);
            y1 = y1.max(y);
        }
    }

    if !found {
        return None;
    }

    // A sample grid can miss the edges of what is really visible, so widen by
    // one sample spacing before clamping to the image.
    let pad_x = ((x1 - x0) / SAMPLES as f64).abs();
    let pad_y = ((y1 - y0) / SAMPLES as f64).abs();

    let x0 = (x0 - pad_x).max(0.0);
    let y0 = (y0 - pad_y).max(0.0);
    let x1 = (x1 + pad_x).min(width);
    let y1 = (y1 + pad_y).min(height);

    if x1 <= x0 || y1 <= y0 {
        return None;
    }

    Some((x0, y0, x1, y1))
}

/// The level at which one sample is about one screen pixel.
fn level_for(grid: &TileGrid, camera: &CameraViewPort, rect: (f64, f64, f64, f64)) -> u32 {
    let (x0, _, x1, _) = rect;
    let screen_px = camera.get_screen_size().x.max(1.0) as f64;
    grid.level_for_scale((x1 - x0) / screen_px)
}

impl Refine {
    pub fn new(gl: &WebGlContext, reader: Rc<Reader>, budget: u64) -> Result<Self, JsValue> {
        let (ready_send, ready_recv) = async_channel::unbounded();

        let pos: Vec<f32> = vec![];
        let uv: Vec<f32> = vec![];
        let indices: Vec<u16> = vec![];

        let mut vao = VertexArrayObject::new(gl);
        vao.bind_for_update()
            .add_array_buffer_single(
                2,
                "ndc_pos",
                web_sys::WebGl2RenderingContext::DYNAMIC_DRAW,
                al_core::VecData::<f32>(&pos),
            )
            .add_array_buffer_single(
                2,
                "uv",
                web_sys::WebGl2RenderingContext::DYNAMIC_DRAW,
                al_core::VecData::<f32>(&uv),
            )
            .add_element_buffer(
                web_sys::WebGl2RenderingContext::DYNAMIC_DRAW,
                al_core::VecData::<u16>(&indices),
            )
            .unbind();

        Ok(Self {
            reader,
            budget,
            level: u32::MAX,
            tiles: HashMap::new(),
            pending: HashSet::new(),
            ready_send,
            ready_recv,
            vao,
            pos,
            uv,
            indices,
            num_indices: vec![],
            patch_tiles: vec![],
        })
    }

    /// Turn decoded tiles into textures. Returns whether anything arrived.
    fn take_ready(&mut self, gl: &WebGlContext) -> Result<bool, JsValue> {
        let mut arrived = false;

        while let Ok(TileReady { id, tile, size }) = self.ready_recv.try_recv() {
            self.pending.remove(&id);

            let texture = match tile.kind {
                PixelKind::U8 => texture_of::<R8U>(gl, &tile, size)?,
                PixelKind::I16 => texture_of::<R16I>(gl, &tile, size)?,
                PixelKind::I32 => texture_of::<R32I>(gl, &tile, size)?,
                PixelKind::F32 => texture_of::<R32F>(gl, &tile, size)?,
            };
            texture.generate_mipmap();

            self.tiles.insert(id, texture);
            arrived = true;
        }

        Ok(arrived)
    }

    /// Start reading a tile, if there is room in flight for it.
    fn request(&mut self, id: TileId) {
        if self.tiles.contains_key(&id)
            || self.pending.contains(&id)
            || self.pending.len() >= MAX_TILES_IN_FLIGHT
        {
            return;
        }

        let Some(sampling) = self.reader.sampling(id, self.budget) else {
            return;
        };
        let size = texture_size(self.reader.grid.tile_size, &sampling);

        self.pending.insert(id);

        let reader = self.reader.clone();
        let send = self.ready_send.clone();
        let budget = self.budget;

        wasm_bindgen_futures::spawn_local(async move {
            match reader.read_tile(id, budget).await {
                Ok((tile, _)) => {
                    let _ = send.send(TileReady { id, tile, size }).await;
                }
                Err(e) => {
                    // A failed tile leaves the coarser pixels showing rather
                    // than a hole, so this is worth a line in the console and
                    // nothing more. It stays out of `tiles`, so a later frame
                    // will ask for it again.
                    web_sys::console::warn_1(
                        &format!("tile {:?} could not be read: {}", id, e).into(),
                    );
                }
            }
        });
    }

    /// Forget tiles that are no longer worth keeping.
    fn evict(&mut self, keep: &HashSet<TileId>) {
        if self.tiles.len() <= MAX_CACHED_TILES {
            return;
        }

        let level = self.level;
        // Keep what is on screen, and the immediately coarser level that backs
        // it while finer tiles are still arriving.
        self.tiles
            .retain(|id, _| keep.contains(id) || id.level > level);

        if self.tiles.len() > MAX_CACHED_TILES {
            self.tiles.retain(|id, _| keep.contains(id));
        }
    }
}

impl Refine {
    /// Bring the refinement up to date with the camera.
    ///
    /// Returns whether the view should be redrawn.
    pub fn update(
        &mut self,
        gl: &WebGlContext,
        camera: &CameraViewPort,
        projection: &Proj,
        wcs: &fitsrs::WCS,
    ) -> Result<bool, JsValue> {
        let mut redraw = self.take_ready(gl)?;

        let Some(rect) = visible_image_rect(camera, projection, wcs) else {
            // Off screen: keep the textures, drop the mesh.
            self.num_indices.clear();
            self.patch_tiles.clear();
            return Ok(redraw);
        };

        let grid = self.reader.grid.clone();
        let level = level_for(&grid, camera, rect);
        let step = grid.step_at(level);
        let patch = step * grid.tile_size as u64;

        // Nothing to refine: the overview already draws this level.
        if level >= grid.level_count() - 1 {
            self.num_indices.clear();
            self.patch_tiles.clear();
            return Ok(redraw);
        }

        let (x0, y0, x1, y1) = rect;
        let (tx0, ty0) = ((x0 as u64) / patch, (y0 as u64) / patch);
        let (tx1, ty1) = (
            ((x1.ceil() as u64).saturating_sub(1)) / patch,
            ((y1.ceil() as u64).saturating_sub(1)) / patch,
        );

        let (nx, ny) = ((tx1 - tx0 + 1) as usize, (ty1 - ty0 + 1) as usize);

        let mut wanted = HashSet::with_capacity(nx * ny);
        let mut patch_tiles = Vec::with_capacity(nx * ny);
        for ty in ty0..=ty1 {
            for tx in tx0..=tx1 {
                let id = TileId::new(level, tx as u32, ty as u32);
                wanted.insert(id);
                patch_tiles.push(id);
            }
        }

        if level != self.level {
            self.level = level;
            redraw = true;
        }

        for id in &patch_tiles {
            self.request(*id);
        }
        self.evict(&wanted);

        // The mesh spans whole patches so that its patch grid lines up with the
        // tile grid: `grid::vertices` breaks its patches on multiples of the
        // patch size, and the textures are addressed by that same division.
        let mesh_x0 = (tx0 * patch) as f64;
        let mesh_y0 = (ty0 * patch) as f64;
        let dim = wcs.img_dimensions();
        let mesh_x1 = (((tx1 + 1) * patch) as f64).min(dim[0] as f64);
        let mesh_y1 = (((ty1 + 1) * patch) as f64).min(dim[1] as f64);

        let num_vertices = ((camera.get_aperture().to_degrees() / 180.0) * 15.0).ceil() as u64;

        let (pos, uv, indices, num_indices) = super::grid::vertices(
            &(mesh_x0, mesh_y0),
            &(mesh_x1, mesh_y1),
            patch,
            patch,
            num_vertices.max(1),
            camera,
            wcs,
            projection,
            false,
        );

        // If the mesh did not come back with one patch per tile, the two
        // disagree about the grid and binding textures by position would draw
        // the wrong pixels. Drawing nothing is the safe answer; the overview
        // still shows.
        if num_indices.len() != patch_tiles.len() {
            self.num_indices.clear();
            self.patch_tiles.clear();
            return Ok(redraw);
        }

        self.pos = pos;
        self.uv = uv;
        self.indices = indices;
        self.num_indices = num_indices;
        self.patch_tiles = patch_tiles;

        self.vao
            .bind_for_update()
            .update_array(
                "ndc_pos",
                web_sys::WebGl2RenderingContext::DYNAMIC_DRAW,
                al_core::VecData(&self.pos),
            )
            .update_array(
                "uv",
                web_sys::WebGl2RenderingContext::DYNAMIC_DRAW,
                al_core::VecData(&self.uv),
            )
            .update_element_array(
                web_sys::WebGl2RenderingContext::DYNAMIC_DRAW,
                al_core::VecData::<u16>(&self.indices),
            );

        Ok(redraw)
    }

    /// Whether there is anything to draw on top of the overview.
    pub fn has_mesh(&self) -> bool {
        !self.num_indices.is_empty()
            && self.patch_tiles.iter().any(|id| self.tiles.contains_key(id))
    }

    /// Draw the patches whose tiles have arrived.
    ///
    /// Patches still missing their tile are skipped, leaving the overview
    /// underneath visible rather than a hole.
    pub fn draw(
        &self,
        gl: &WebGlContext,
        shader: &al_core::shader::ShaderBound<'_>,
    ) -> Result<(), JsValue> {
        let mut off_indices = 0u32;

        for (patch, id) in self.patch_tiles.iter().enumerate() {
            let num_indices = self.num_indices[patch];

            if let Some(texture) = self.tiles.get(id) {
                shader
                    .attach_uniform("tex", texture)
                    .bind_vertex_array_object_ref(&self.vao)
                    .draw_elements_with_i32(
                        web_sys::WebGl2RenderingContext::TRIANGLES,
                        Some(num_indices as i32),
                        web_sys::WebGl2RenderingContext::UNSIGNED_SHORT,
                        (off_indices as usize * std::mem::size_of::<u16>()) as i32,
                    );
            }

            // Advance whether or not the patch was drawn: the offset addresses
            // the shared index buffer, not the patches actually rendered.
            off_indices += num_indices;
        }

        let _ = gl;
        Ok(())
    }
}
