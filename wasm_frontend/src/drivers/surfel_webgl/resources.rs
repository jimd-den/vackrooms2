//! Per-chunk GPU resources for the surfel renderer.
//!
//! A chunk owns one packed instance page and one baked light volume. There is
//! no shadow caster and no index buffer: the quad comes from `gl_VertexID`.
//!
//! Unlike the splat driver there is no `point_at`. That exists there only
//! because a face page is bucketed into culling cells and WebGL2 has no
//! `baseInstance` to start a draw partway through. A surfel cloud is a flat
//! array, so the pointers are recorded once at upload and never moved.

use js_sys::Uint8Array;
use wasm_bindgen::JsValue;
use web_sys::{WebGl2RenderingContext as Gl, WebGlBuffer, WebGlProgram, WebGlVertexArrayObject};

use crate::application::ports::SurfaceChunk;
use crate::drivers::gl::light_volume::upload_light_volume;
use crate::drivers::surfel_layout::{
    ATTRIB_FLAGS, ATTRIB_META, ATTRIB_POS, OFFSET_FLAGS, OFFSET_META, OFFSET_POS, SURFEL_STRIDE,
    pack_surfels,
};

use super::SurfelRenderer;

/// Attribute locations are resolved once because every chunk uses the same
/// packed surfel layout.
pub(crate) struct SurfelAttribs {
    pos: u32,
    meta: u32,
    flags: u32,
}

impl SurfelAttribs {
    pub(crate) fn resolve(gl: &Gl, program: &WebGlProgram) -> Result<Self, JsValue> {
        let lookup = |name: &str| -> Result<u32, JsValue> {
            let location = gl.get_attrib_location(program, name);
            if location < 0 {
                return Err(JsValue::from_str(&format!("surfel shader lost {name}")));
            }
            Ok(location as u32)
        };

        Ok(Self {
            pos: lookup(ATTRIB_POS)?,
            meta: lookup(ATTRIB_META)?,
            flags: lookup(ATTRIB_FLAGS)?,
        })
    }

    /// Enables the three attributes and records their pointers. The caller
    /// must have the chunk VAO and its instance buffer bound.
    ///
    /// Every pointer is unnormalized: the shader decodes position by
    /// `1/1024` and radius by `RADIUS_QUANT`, and the remaining bytes are
    /// indices that a 0..1 remap would destroy.
    fn bind(&self, gl: &Gl) {
        for location in [self.pos, self.meta, self.flags] {
            gl.enable_vertex_attrib_array(location);
            gl.vertex_attrib_divisor(location, 1);
        }
        gl.vertex_attrib_pointer_with_i32(
            self.pos,
            3,
            Gl::UNSIGNED_SHORT,
            false,
            SURFEL_STRIDE,
            OFFSET_POS,
        );
        gl.vertex_attrib_pointer_with_i32(
            self.meta,
            4,
            Gl::UNSIGNED_BYTE,
            false,
            SURFEL_STRIDE,
            OFFSET_META,
        );
        gl.vertex_attrib_pointer_with_i32(
            self.flags,
            2,
            Gl::UNSIGNED_BYTE,
            false,
            SURFEL_STRIDE,
            OFFSET_FLAGS,
        );
    }
}

/// All GPU data owned by one resident chunk.
pub(crate) struct SurfelChunk {
    pub vao: WebGlVertexArrayObject,
    pub buffer: WebGlBuffer,
    pub surfel_count: i32,
    pub voxel_scale: f32,
    pub origin: [f32; 3],
    pub bounds_max: [f32; 3],
    pub light_texture: web_sys::WebGlTexture,
}

impl SurfelRenderer {
    pub(crate) fn destroy_chunk(&self, chunk: SurfelChunk) {
        let gl = &self.gl;
        gl.delete_vertex_array(Some(&chunk.vao));
        gl.delete_buffer(Some(&chunk.buffer));
        gl.delete_texture(Some(&chunk.light_texture));
    }

    pub(crate) fn upload_surface(&mut self, chunk: SurfaceChunk<'_>) {
        if let Some(old) = self.chunks.remove(&chunk.key) {
            self.destroy_chunk(old);
        }
        // An empty cloud means the chunk was generated without
        // `RenderArtifactNeeds::SURFEL`. Nothing to draw, and nothing to
        // allocate for.
        if chunk.mesh.surfels.is_empty() {
            return;
        }

        let (vao, buffer) = self.upload_instance_page(chunk);
        let light_texture =
            upload_light_volume(&self.gl, chunk.mesh).expect("upload surfel light volume");

        self.chunks.insert(
            chunk.key,
            SurfelChunk {
                vao,
                buffer,
                surfel_count: chunk.mesh.surfels.surfels.len().min(i32::MAX as usize) as i32,
                voxel_scale: chunk.mesh.voxel_scale,
                origin: chunk.origin,
                bounds_max: chunk.mesh.bounds.max,
                light_texture,
            },
        );
    }

    fn upload_instance_page(
        &self,
        chunk: SurfaceChunk<'_>,
    ) -> (WebGlVertexArrayObject, WebGlBuffer) {
        let gl = &self.gl;
        let vao = gl.create_vertex_array().expect("create surfel VAO");
        let buffer = gl.create_buffer().expect("create surfel instance buffer");
        let packed = pack_surfels(&chunk.mesh.surfels.surfels);

        gl.bind_vertex_array(Some(&vao));
        gl.bind_buffer(Gl::ARRAY_BUFFER, Some(&buffer));
        let bytes = Uint8Array::from(packed.as_slice());
        gl.buffer_data_with_array_buffer_view(Gl::ARRAY_BUFFER, &bytes, Gl::STATIC_DRAW);
        self.attribs.bind(gl);
        gl.bind_vertex_array(None);
        gl.bind_buffer(Gl::ARRAY_BUFFER, None);

        (vao, buffer)
    }
}
