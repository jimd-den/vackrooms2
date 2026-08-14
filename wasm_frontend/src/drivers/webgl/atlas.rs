//! RGBA32UI SVO node-atlas lifecycle, and the RG32UI dense brick-voxel
//! arena that sits alongside it once a chunk is delivered bricked.
//!
//! The serializers pad every row -- the node atlas to [`ATLAS_WIDTH`]
//! texels, the brick arena to `BRICK_ROW_WORDS` words -- so both a full
//! replacement and an incremental row overwrite are exact typed-array
//! copies on either texture, and one pool slot maps to a whole number of
//! rows in both.
//!
//! Both uploads verify with `gl.get_error()` rather than trusting the call:
//! WebGL reports a rejected allocation by leaving the texture incomplete,
//! which samples as zero, so the failure arrives as a world made of air
//! rather than as an error.

use web_sys::{WebGl2RenderingContext as Gl, WebGlTexture};

/// Must match the raymarch shader's node-index decoder and the core
/// `OctreeGpuSerializer` row padding, and `application::atlas::ROW_TEXELS`.
const ATLAS_WIDTH: i32 = 1024;

/// Brick-voxel texel width. Two words per voxel (RG32UI), and
/// `application::atlas::BRICK_ROW_WORDS` pads every slot row to 1024 words,
/// so 512 texels/row keeps this texture's rows aligned with the pool's.
const BRICK_ATLAS_WIDTH: i32 = 512;

pub(super) struct AtlasTexture {
    texture: Option<WebGlTexture>,
    /// Allocated atlas height, used to reject out-of-bounds row patches.
    rows: i32,
}

impl AtlasTexture {
    pub(super) fn new() -> Self {
        Self {
            texture: None,
            rows: 0,
        }
    }

    /// Replaces the whole atlas. An empty stream intentionally releases the
    /// previous texture so level transitions cannot sample stale nodes.
    pub(super) fn replace(&mut self, gl: &Gl, texels: &[u32]) {
        if let Some(old) = self.texture.take() {
            gl.delete_texture(Some(&old));
        }
        self.rows = 0;
        if texels.is_empty() {
            return;
        }

        debug_assert_eq!(texels.len() % 4, 0, "atlas texels are RGBA u32 values");
        let texel_count = (texels.len() / 4) as i32;
        let rows = (texel_count + ATLAS_WIDTH - 1) / ATLAS_WIDTH;
        let texture = gl.create_texture().expect("create SVO atlas texture");

        gl.bind_texture(Gl::TEXTURE_2D, Some(&texture));
        gl.pixel_storei(Gl::UNPACK_ALIGNMENT, 4);
        let view = js_sys::Uint32Array::from(texels);
        gl.tex_image_2d_with_i32_and_i32_and_i32_and_format_and_type_and_opt_array_buffer_view(
            Gl::TEXTURE_2D,
            0,
            Gl::RGBA32UI as i32,
            ATLAS_WIDTH,
            rows,
            0,
            Gl::RGBA_INTEGER,
            Gl::UNSIGNED_INT,
            Some(&view),
        )
        .expect("SVO atlas texture upload failed");
        set_nearest_clamped(gl);
        gl.bind_texture(Gl::TEXTURE_2D, None);

        // See `BrickVoxelTexture::replace`: a rejected allocation is silent
        // and samples as zero, which here means every node reads as air.
        let error = gl.get_error();
        if error != Gl::NO_ERROR {
            web_sys::console::warn_1(
                &format!(
                    "node atlas upload rejected: {ATLAS_WIDTH}x{rows} RGBA32UI \
                     ({} texels), gl error 0x{error:x}",
                    texels.len() / 4
                )
                .into(),
            );
            gl.delete_texture(Some(&texture));
            return;
        }

        self.texture = Some(texture);
        self.rows = rows;
    }

    /// Patches complete atlas rows without reallocating the texture.
    pub(super) fn update_rows(&mut self, gl: &Gl, first_row: u32, texels: &[u32]) -> bool {
        let row_stride = 4 * ATLAS_WIDTH as usize;
        let patch_rows = texels.len() / row_stride;
        if self.texture.is_none()
            || patch_rows == 0
            || texels.len() % row_stride != 0
            || first_row as i32 + patch_rows as i32 > self.rows
        {
            return false;
        }

        gl.bind_texture(Gl::TEXTURE_2D, self.texture.as_ref());
        let view = js_sys::Uint32Array::from(texels);
        let uploaded = gl
            .tex_sub_image_2d_with_i32_and_i32_and_u32_and_type_and_opt_array_buffer_view(
                Gl::TEXTURE_2D,
                0,
                0,
                first_row as i32,
                ATLAS_WIDTH,
                patch_rows as i32,
                Gl::RGBA_INTEGER,
                Gl::UNSIGNED_INT,
                Some(&view),
            )
            .is_ok();
        gl.bind_texture(Gl::TEXTURE_2D, None);
        uploaded
    }

    pub(super) fn bind(&self, gl: &Gl) {
        gl.bind_texture(Gl::TEXTURE_2D, self.texture.as_ref());
    }
}

/// RG32UI dense brick-voxel arena.
///
/// Always has a complete texture bound -- a 1x1 zero-filled placeholder
/// whenever the resident chunks hold zero bricks -- mirroring the WebGPU
/// pipeline's own `create_brick_buffer(device, &[0, 0])` placeholder
/// (`drivers/webgpu/pipelines/raymarch.rs`). A genuinely unbound
/// `usampler2D` reads back undefined per the WebGL spec; leaving this `None`
/// between construction and the first upload, or on any frame where every
/// resident chunk happens to hold zero bricks, showed up as real frame
/// corruption rather than harmless (unreached) sampling.
pub(super) struct BrickVoxelTexture {
    texture: WebGlTexture,
    /// Allocated arena height in `BRICK_ATLAS_WIDTH`-wide rows, or `0` while
    /// the placeholder is active (never a valid row-patch target: the
    /// placeholder is 1x1, not `BRICK_ATLAS_WIDTH` wide).
    rows: i32,
}

impl BrickVoxelTexture {
    pub(super) fn new(gl: &Gl) -> Self {
        let texture = gl.create_texture().expect("create brick voxel texture");
        upload_placeholder(gl, &texture);
        Self { texture, rows: 0 }
    }

    /// Replaces the whole arena. An empty stream falls back to the
    /// placeholder rather than unbinding (see the struct doc).
    pub(super) fn replace(&mut self, gl: &Gl, words: &[u32]) {
        if words.is_empty() {
            upload_placeholder(gl, &self.texture);
            self.rows = 0;
            return;
        }

        debug_assert_eq!(words.len() % 2, 0, "brick voxels are two u32 words each");
        let texel_count = (words.len() / 2) as i32;
        let rows = (texel_count + BRICK_ATLAS_WIDTH - 1) / BRICK_ATLAS_WIDTH;

        gl.bind_texture(Gl::TEXTURE_2D, Some(&self.texture));
        gl.pixel_storei(Gl::UNPACK_ALIGNMENT, 4);
        let view = js_sys::Uint32Array::from(words);
        gl.tex_image_2d_with_i32_and_i32_and_i32_and_format_and_type_and_opt_array_buffer_view(
            Gl::TEXTURE_2D,
            0,
            Gl::RG32UI as i32,
            BRICK_ATLAS_WIDTH,
            rows,
            0,
            Gl::RG_INTEGER,
            Gl::UNSIGNED_INT,
            Some(&view),
        )
        .expect("brick voxel texture upload failed");
        set_nearest_clamped(gl);
        gl.bind_texture(Gl::TEXTURE_2D, None);
        // A rejected allocation is silent: `tex_image_2d` only returns `Err`
        // for a JS exception, and an over-large or out-of-memory texture
        // instead leaves the object incomplete, which samples as zero --
        // every brick reads as air and the world renders as the handful of
        // uniform nodes above the brick level. Ask GL directly rather than
        // trust the call, and refuse to treat the arena as patchable when it
        // did not land.
        let error = gl.get_error();
        if error != Gl::NO_ERROR {
            web_sys::console::warn_1(
                &format!(
                    "brick arena upload rejected: {BRICK_ATLAS_WIDTH}x{rows} RG32UI \
                     ({} words), gl error 0x{error:x}",
                    words.len()
                )
                .into(),
            );
            self.rows = 0;
            return;
        }
        if rows != self.rows {
            // Shape changes are rare (a relayout), so this is a handful of
            // lines per session and says which half of the pipeline to
            // suspect when bricks render as air: a zero arena is a
            // generation or pooling fault, a populated one that still
            // renders empty is a sampling fault.
            let occupied = words.iter().filter(|&&w| w != 0).count();
            web_sys::console::log_1(
                &format!(
                    "brick arena {BRICK_ATLAS_WIDTH}x{rows} RG32UI, {} words, \
                     {occupied} non-zero",
                    words.len()
                )
                .into(),
            );
        }
        self.rows = rows;
    }

    /// Patches complete arena rows without reallocating the texture. Mirrors
    /// `AtlasTexture::update_rows`; unlike `replace`, this is what lets a
    /// single streamed-in chunk's bricks land without a full-pool reupload
    /// -- see `application::engine::Engine::stream_chunks`'s incremental
    /// path, which patches this alongside the matching node-atlas rows so
    /// the two never disagree for a frame.
    pub(super) fn update_rows(&mut self, gl: &Gl, first_row: u32, words: &[u32]) -> bool {
        let row_stride = 2 * BRICK_ATLAS_WIDTH as usize;
        let patch_rows = words.len() / row_stride;
        if patch_rows == 0
            || words.len() % row_stride != 0
            || first_row as i32 + patch_rows as i32 > self.rows
        {
            return false;
        }

        gl.bind_texture(Gl::TEXTURE_2D, Some(&self.texture));
        let view = js_sys::Uint32Array::from(words);
        let uploaded = gl
            .tex_sub_image_2d_with_i32_and_i32_and_u32_and_type_and_opt_array_buffer_view(
                Gl::TEXTURE_2D,
                0,
                0,
                first_row as i32,
                BRICK_ATLAS_WIDTH,
                patch_rows as i32,
                Gl::RG_INTEGER,
                Gl::UNSIGNED_INT,
                Some(&view),
            )
            .is_ok();
        gl.bind_texture(Gl::TEXTURE_2D, None);
        uploaded
    }

    pub(super) fn bind(&self, gl: &Gl) {
        gl.bind_texture(Gl::TEXTURE_2D, Some(&self.texture));
    }
}

/// Uploads a 1x1 zero texel. Deliberately not expressed as `replace(gl, &[0,
/// 0])`: that would ask for a `BRICK_ATLAS_WIDTH`-wide row (matching real,
/// row-padded uploads from `AtlasPool::full_brick_words`) while supplying
/// only one texel of data, an undersized-buffer mismatch WebGL rejects.
fn upload_placeholder(gl: &Gl, texture: &WebGlTexture) {
    gl.bind_texture(Gl::TEXTURE_2D, Some(texture));
    gl.pixel_storei(Gl::UNPACK_ALIGNMENT, 4);
    let view = js_sys::Uint32Array::from([0u32, 0u32].as_slice());
    gl.tex_image_2d_with_i32_and_i32_and_i32_and_format_and_type_and_opt_array_buffer_view(
        Gl::TEXTURE_2D,
        0,
        Gl::RG32UI as i32,
        1,
        1,
        0,
        Gl::RG_INTEGER,
        Gl::UNSIGNED_INT,
        Some(&view),
    )
    .expect("brick placeholder texture upload failed");
    set_nearest_clamped(gl);
    gl.bind_texture(Gl::TEXTURE_2D, None);
}

fn set_nearest_clamped(gl: &Gl) {
    gl.tex_parameteri(Gl::TEXTURE_2D, Gl::TEXTURE_MIN_FILTER, Gl::NEAREST as i32);
    gl.tex_parameteri(Gl::TEXTURE_2D, Gl::TEXTURE_MAG_FILTER, Gl::NEAREST as i32);
    gl.tex_parameteri(Gl::TEXTURE_2D, Gl::TEXTURE_WRAP_S, Gl::CLAMP_TO_EDGE as i32);
    gl.tex_parameteri(Gl::TEXTURE_2D, Gl::TEXTURE_WRAP_T, Gl::CLAMP_TO_EDGE as i32);
}
