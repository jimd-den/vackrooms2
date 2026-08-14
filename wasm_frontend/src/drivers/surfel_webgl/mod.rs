//! Instanced surfel renderer — the WebGL2 disc splatter.
//!
//! One instance is one oriented disc (`PackedSurfel`). The vertex shader
//! builds a square in the disc's own tangent plane from `gl_VertexID` and the
//! fragment shader discards everything outside the inscribed circle.
//!
//! This is the splat driver with the entire shadow subsystem removed — no
//! caster program, no framebuffer, no retained mesh, no second VAO. That is
//! not a simplification for its own sake: a surfel cloud has no mesh to cast
//! with, and building one purely to cast would let the caster and the visible
//! surface disagree about the silhouette.
//!
//! The other structural difference is that a chunk draws in one call. The
//! splat path buckets its face page into culling cells and re-points
//! attributes per range because WebGL2 has no `baseInstance`; a surfel cloud
//! is a flat array, so there is no range to point at.
//!
//! The module is split like its sibling:
//!
//! * this file owns composition, profile settings, the program and uniforms;
//! * [`resources`] owns per-chunk GPU buffers and textures;
//! * [`draw`] owns the frame pipeline and renderer-port implementation.
//!
//! The byte layout the instance buffer uses lives in
//! [`crate::drivers::surfel_layout`], which compiles natively so it can be
//! tested; everything in this module needs a live WebGL2 context.

mod draw;
mod resources;

use std::collections::HashMap;

use wasm_bindgen::JsValue;
use web_sys::{
    HtmlCanvasElement, WebGl2RenderingContext as Gl, WebGlProgram, WebGlUniformLocation,
};

use crate::application::ports::SurfaceChunkKey;
use crate::drivers::gl::program::{ContextOptions, create_context, link_program};
use crate::drivers::gl::timer::GpuFrameTimer;
use crate::drivers::shaders::surfel;

use resources::{SurfelAttribs, SurfelChunk};

/// Hardware-profile knobs chosen once by the browser composition root.
#[derive(Debug, Clone, Copy)]
pub struct SurfelProfile {
    /// Per-frame disc budget. Once exhausted, farther chunks are dropped
    /// whole so nearby surfaces stay spatially coherent — the same policy
    /// the splat path applies to faces, for the same reason.
    pub surfel_budget: u32,
}

impl SurfelProfile {
    pub fn low() -> Self {
        Self {
            surfel_budget: 400_000,
        }
    }

    pub fn high() -> Self {
        Self {
            surfel_budget: 1_500_000,
        }
    }
}

/// Program uniforms, resolved once after linking.
///
/// This is the splat set minus `uShadowMap`, `uLightViewProjection`,
/// `uShadowedLightIndex` and `uShadowTaps`.
pub(crate) struct Uniforms {
    pub projection: Option<WebGlUniformLocation>,
    pub view: Option<WebGlUniformLocation>,
    pub chunk_origin: Option<WebGlUniformLocation>,
    pub chunk_size: Option<WebGlUniformLocation>,
    pub voxel_scale: Option<WebGlUniformLocation>,
    pub camera_position: Option<WebGlUniformLocation>,
    pub cam_forward: Option<WebGlUniformLocation>,
    pub flashlight: Option<WebGlUniformLocation>,
    pub dither: Option<WebGlUniformLocation>,
    pub light_volume: Option<WebGlUniformLocation>,
    pub light_count: Option<WebGlUniformLocation>,
    pub light_positions: Option<WebGlUniformLocation>,
    pub light_colors: Option<WebGlUniformLocation>,
    pub light_params: Option<WebGlUniformLocation>,
    pub core_count: Option<WebGlUniformLocation>,
    pub cores: Option<WebGlUniformLocation>,
    pub core_colors: Option<WebGlUniformLocation>,
    pub outdoor: Option<WebGlUniformLocation>,
    pub fog_color: Option<WebGlUniformLocation>,
    pub ambient_scale: Option<WebGlUniformLocation>,
}

#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct FrameStats {
    pub surfels_drawn: u32,
    pub surfels_dropped: u32,
    pub draw_calls: u32,
    pub chunks_drawn: u32,
}

/// WebGL2 surfel renderer with one immutable instance page per resident
/// chunk. Mutable per-frame state is limited to the asynchronous timer and
/// telemetry counters.
pub struct SurfelRenderer {
    pub(crate) gl: Gl,
    pub(crate) program: WebGlProgram,
    pub(crate) uniforms: Uniforms,
    pub(crate) attribs: SurfelAttribs,
    pub(crate) profile: SurfelProfile,
    pub(crate) chunks: HashMap<SurfaceChunkKey, SurfelChunk>,
    pub(crate) width: i32,
    pub(crate) height: i32,
    pub(crate) timer: GpuFrameTimer,
    pub(crate) stats: FrameStats,
}

impl SurfelRenderer {
    pub fn new(canvas: &HtmlCanvasElement, profile: SurfelProfile) -> Result<Self, JsValue> {
        let gl = create_context(canvas, ContextOptions { depth: true })?;

        let vertex_source = surfel::vertex_source();
        let fragment_source = surfel::fragment_source();
        let program = link_program(&gl, &vertex_source, &fragment_source)?;
        let uniforms = Uniforms {
            projection: gl.get_uniform_location(&program, "uProjection"),
            view: gl.get_uniform_location(&program, "uView"),
            chunk_origin: gl.get_uniform_location(&program, "uChunkOrigin"),
            chunk_size: gl.get_uniform_location(&program, "uChunkSize"),
            voxel_scale: gl.get_uniform_location(&program, "uVoxelScale"),
            camera_position: gl.get_uniform_location(&program, "uCameraPosition"),
            cam_forward: gl.get_uniform_location(&program, "uCamForward"),
            flashlight: gl.get_uniform_location(&program, "uFlashlightEnabled"),
            dither: gl.get_uniform_location(&program, "uDitherEnabled"),
            light_volume: gl.get_uniform_location(&program, "uLightVolume"),
            light_count: gl.get_uniform_location(&program, "uLightCount"),
            light_positions: gl.get_uniform_location(&program, "uLightPositions"),
            light_colors: gl.get_uniform_location(&program, "uLightColors"),
            light_params: gl.get_uniform_location(&program, "uLightParams"),
            core_count: gl.get_uniform_location(&program, "uCoreCount"),
            cores: gl.get_uniform_location(&program, "uCores"),
            core_colors: gl.get_uniform_location(&program, "uCoreColors"),
            outdoor: gl.get_uniform_location(&program, "uOutdoor"),
            fog_color: gl.get_uniform_location(&program, "uFogColor"),
            ambient_scale: gl.get_uniform_location(&program, "uAmbientScale"),
        };
        let attribs = SurfelAttribs::resolve(&gl, &program)?;
        let timer = GpuFrameTimer::new(&gl);

        Ok(Self {
            gl,
            program,
            uniforms,
            attribs,
            profile,
            chunks: HashMap::new(),
            width: canvas.width() as i32,
            height: canvas.height() as i32,
            timer,
            stats: FrameStats::default(),
        })
    }

    /// Updates the backing-store viewport dimensions after adaptive scaling
    /// or a browser resize. GPU resources are resolution-independent.
    pub fn resize(&mut self, width: u32, height: u32) {
        self.width = width as i32;
        self.height = height as i32;
    }
}
