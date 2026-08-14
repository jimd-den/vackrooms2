//! RGBA32UI SVO node-atlas lifecycle, and the RG32UI dense brick-voxel
//! arena that sits alongside it once a chunk is delivered bricked.
//!
//! The serializer pads every atlas row to [`ATLAS_WIDTH`], which makes both a
//! full replacement and an incremental row overwrite exact typed-array
//! copies for the node atlas. The brick arena has no incremental path (see
//! [`BrickVoxelTexture::replace`]) and only ever fully reallocates.

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
        gl.tex_parameteri(Gl::TEXTURE_2D, Gl::TEXTURE_MIN_FILTER, Gl::NEAREST as i32);
        gl.tex_parameteri(Gl::TEXTURE_2D, Gl::TEXTURE_MAG_FILTER, Gl::NEAREST as i32);
        gl.tex_parameteri(Gl::TEXTURE_2D, Gl::TEXTURE_WRAP_S, Gl::CLAMP_TO_EDGE as i32);
        gl.tex_parameteri(Gl::TEXTURE_2D, Gl::TEXTURE_WRAP_T, Gl::CLAMP_TO_EDGE as i32);
        gl.bind_texture(Gl::TEXTURE_2D, None);

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

/// RG32UI dense brick-voxel arena. Empty (no texture bound) whenever the
/// resident chunks hold the plain SVO encoding instead of bricks -- the
/// decoder never reaches a brick node then, so nothing samples it.
pub(super) struct BrickVoxelTexture {
    texture: Option<WebGlTexture>,
}

impl BrickVoxelTexture {
    pub(super) fn new() -> Self {
        Self { texture: None }
    }

    /// Replaces the whole arena. `RendererPort` has no incremental brick
    /// upload (WebGPU's own pipeline replaces its whole buffer too, per
    /// `raymarch.rs::upload_brick_voxels`), so unlike the node atlas this
    /// texture only ever fully reallocates. An empty stream intentionally
    /// releases the previous texture so level transitions cannot sample
    /// stale voxels.
    pub(super) fn replace(&mut self, gl: &Gl, words: &[u32]) {
        if let Some(old) = self.texture.take() {
            gl.delete_texture(Some(&old));
        }
        if words.is_empty() {
            return;
        }

        debug_assert_eq!(words.len() % 2, 0, "brick voxels are two u32 words each");
        let texel_count = (words.len() / 2) as i32;
        let rows = (texel_count + BRICK_ATLAS_WIDTH - 1) / BRICK_ATLAS_WIDTH;
        let texture = gl.create_texture().expect("create brick voxel texture");

        gl.bind_texture(Gl::TEXTURE_2D, Some(&texture));
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
        gl.tex_parameteri(Gl::TEXTURE_2D, Gl::TEXTURE_MIN_FILTER, Gl::NEAREST as i32);
        gl.tex_parameteri(Gl::TEXTURE_2D, Gl::TEXTURE_MAG_FILTER, Gl::NEAREST as i32);
        gl.tex_parameteri(Gl::TEXTURE_2D, Gl::TEXTURE_WRAP_S, Gl::CLAMP_TO_EDGE as i32);
        gl.tex_parameteri(Gl::TEXTURE_2D, Gl::TEXTURE_WRAP_T, Gl::CLAMP_TO_EDGE as i32);
        gl.bind_texture(Gl::TEXTURE_2D, None);

        self.texture = Some(texture);
    }

    pub(super) fn bind(&self, gl: &Gl) {
        gl.bind_texture(Gl::TEXTURE_2D, self.texture.as_ref());
    }
}
