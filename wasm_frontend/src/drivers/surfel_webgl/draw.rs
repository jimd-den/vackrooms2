//! Per-frame surfel pipeline.
//!
//! The sequence mirrors the splat path, minus the shadow stage:
//!
//! 1. clear and establish fixed-function state;
//! 2. select visible chunks near-to-far;
//! 3. select the bounded set of most important lights;
//! 4. bind frame uniforms once;
//! 5. draw one instanced call per chunk.

use web_sys::WebGl2RenderingContext as Gl;

use crate::application::ports::{
    ChunkDraw, FrameParams, RenderArtifactNeeds, RendererPort, SurfaceChunk, SurfaceChunkKey,
};
use crate::application::render_settings::RenderToggles;
use crate::drivers::gl::lights::{
    SelectedLights, dynamic_light_sources, flare_cores, select_lights,
};
use crate::drivers::gl::math::camera_matrices;
use crate::drivers::gl::visibility::{
    MAX_DRAW_DISTANCE, chunk_bounding_sphere, frustum_side_planes_visible, sphere_visible,
};

use super::resources::SurfelChunk;
use super::{FrameStats, SurfelRenderer};

impl RendererPort for SurfelRenderer {
    fn artifact_needs(&self) -> RenderArtifactNeeds {
        // Discs only. Unlike the splat driver this asks for no indexed mesh:
        // there is no shadow pass to rasterize one, and a surfel cloud is
        // already a complete description of the visible surface. Requesting
        // the mesh as well would pay for a second encoding nothing reads.
        RenderArtifactNeeds::SURFEL
    }

    fn uses_surface_meshes(&self) -> bool {
        true
    }

    fn upload_surfaces(&mut self, chunks: &[SurfaceChunk<'_>]) {
        for &chunk in chunks {
            self.upload_surface(chunk);
        }
    }

    fn remove_surfaces(&mut self, keys: &[SurfaceChunkKey]) {
        for key in keys {
            if let Some(chunk) = self.chunks.remove(key) {
                self.destroy_chunk(chunk);
            }
        }
    }

    fn clear_surfaces(&mut self) {
        let chunks = std::mem::take(&mut self.chunks);
        for (_, chunk) in chunks {
            self.destroy_chunk(chunk);
        }
    }

    fn cpu_telemetry_string(&self) -> Option<String> {
        let stats = self.stats;
        let gpu_ms = self.timer.last_ms().unwrap_or(0.0);
        Some(format!(
            "surfel {} discs / {} calls / {}/{} chunks / {} dropped / GPU {:.2}ms",
            stats.surfels_drawn,
            stats.draw_calls,
            stats.chunks_drawn,
            self.chunks.len(),
            stats.surfels_dropped,
            gpu_ms,
        ))
    }

    fn upload_atlas(&mut self, _texels: &[u32]) {}

    fn draw(&mut self, frame: &FrameParams, _chunks: &[ChunkDraw]) {
        let toggles = crate::get_render_toggles();

        // OPTIMIZATION (rt_timer): asynchronous GPU timing. Polling never
        // blocks; disabling it prevents new timer queries from being issued.
        self.timer.poll(&self.gl);
        let timer_started = self.timer.begin(&self.gl, false);

        self.stats = render_frame(self, frame, toggles);
        self.timer.end(&self.gl, timer_started);
    }
}

fn render_frame(
    renderer: &SurfelRenderer,
    frame: &FrameParams,
    toggles: RenderToggles,
) -> FrameStats {
    begin_main_frame(renderer, frame);

    let resident: Vec<&SurfelChunk> = renderer.chunks.values().collect();
    let aspect = renderer.width as f32 / (renderer.height.max(1)) as f32;
    let fov_tan = crate::drivers::webgl::fov_tan();
    let visible = collect_visible_chunks(&resident, frame, toggles.distance_cull, fov_tan, aspect);
    let lights = select_frame_lights(frame);

    bind_frame_uniforms(renderer, frame, &lights, toggles.dither);
    draw_visible_chunks(renderer, &visible, toggles.face_budget)
}

fn begin_main_frame(renderer: &SurfelRenderer, frame: &FrameParams) {
    let gl = &renderer.gl;
    gl.viewport(0, 0, renderer.width, renderer.height);

    let environment = frame.environment;
    if environment.outdoor {
        gl.clear_color(
            environment.sky_color[0],
            environment.sky_color[1],
            environment.sky_color[2],
            1.0,
        );
    } else {
        // Indoor fog reaches black before the draw limit, so unloaded space
        // and fogged geometry meet without a colored chunk boundary.
        gl.clear_color(0.0, 0.0, 0.0, 1.0);
    }
    gl.clear_depth(1.0);
    gl.clear(Gl::COLOR_BUFFER_BIT | Gl::DEPTH_BUFFER_BIT);
    gl.enable(Gl::DEPTH_TEST);
    gl.depth_func(Gl::LEQUAL);
    gl.depth_mask(true);
    // The strip corner order is counter-clockwise seen from the front, so
    // back-face culling removes the inside of every wall for free. Without
    // it the far side of a wall draws over the near side wherever the
    // normal lift puts it closer.
    gl.enable(Gl::CULL_FACE);
    gl.cull_face(Gl::BACK);
}

/// OPTIMIZATION (rt_cull): conservative chunk sphere rejection. The disabled
/// reference path submits every resident chunk. Both paths retain
/// near-to-far ordering because the disc budget drops whole far chunks
/// before nearby geometry.
fn collect_visible_chunks<'a>(
    resident: &[&'a SurfelChunk],
    frame: &FrameParams,
    culling_enabled: bool,
    fov_tan: f32,
    aspect: f32,
) -> Vec<&'a SurfelChunk> {
    let mut ranked: Vec<(&SurfelChunk, f32)> = resident
        .iter()
        .copied()
        .filter_map(|chunk| {
            let (center, radius) = chunk_bounding_sphere(chunk.origin, chunk.bounds_max);
            let distance2 = if culling_enabled {
                let distance2 = sphere_visible(center, radius, frame, MAX_DRAW_DISTANCE)?;
                if !frustum_side_planes_visible(center, radius, frame, fov_tan, aspect) {
                    return None;
                }
                distance2
            } else {
                squared_distance(center, frame.camera_pos)
            };
            Some((chunk, distance2))
        })
        .collect();

    ranked.sort_by(|a, b| a.1.total_cmp(&b.1));
    ranked.into_iter().map(|(chunk, _)| chunk).collect()
}

fn squared_distance(a: [f32; 3], b: [f32; 3]) -> f32 {
    let dx = a[0] - b[0];
    let dy = a[1] - b[1];
    let dz = a[2] - b[2];
    dx * dx + dy * dy + dz * dz
}

fn select_frame_lights(frame: &FrameParams) -> SelectedLights {
    let dynamic = dynamic_light_sources(frame);
    select_lights(frame.active_scene_lights().iter(), &dynamic, frame)
}

fn bind_frame_uniforms(
    renderer: &SurfelRenderer,
    frame: &FrameParams,
    lights: &SelectedLights,
    dither_enabled: bool,
) {
    let gl = &renderer.gl;
    let uniforms = &renderer.uniforms;
    gl.use_program(Some(&renderer.program));

    let (projection, view) =
        camera_matrices(frame, renderer.width, renderer.height, MAX_DRAW_DISTANCE);
    gl.uniform_matrix4fv_with_f32_array(uniforms.projection.as_ref(), false, &projection);
    gl.uniform_matrix4fv_with_f32_array(uniforms.view.as_ref(), false, &view);
    gl.uniform3f(
        uniforms.camera_position.as_ref(),
        frame.camera_pos[0],
        frame.camera_pos[1],
        frame.camera_pos[2],
    );

    // The cone direction must use the same yaw/pitch basis as the view
    // matrix. Omitting this uniform collapses the spotlight around (0,0,0).
    let (sp, cp) = frame.pitch.sin_cos();
    let (sy, cy) = frame.yaw.sin_cos();
    gl.uniform3f(uniforms.cam_forward.as_ref(), -cp * sy, sp, -cp * cy);
    gl.uniform1i(
        uniforms.flashlight.as_ref(),
        if frame.flashlight { 1 } else { 0 },
    );
    gl.uniform1i(uniforms.dither.as_ref(), if dither_enabled { 1 } else { 0 });

    let environment = frame.environment;
    gl.uniform1i(
        uniforms.outdoor.as_ref(),
        if environment.outdoor { 1 } else { 0 },
    );
    gl.uniform3f(
        uniforms.fog_color.as_ref(),
        environment.fog_color[0],
        environment.fog_color[1],
        environment.fog_color[2],
    );
    gl.uniform1f(uniforms.ambient_scale.as_ref(), environment.ambient_scale);

    bind_light_uniforms(renderer, frame, lights);
}

fn bind_light_uniforms(renderer: &SurfelRenderer, frame: &FrameParams, lights: &SelectedLights) {
    let gl = &renderer.gl;
    let uniforms = &renderer.uniforms;
    gl.uniform1i(uniforms.light_count.as_ref(), lights.count as i32);
    if lights.count > 0 {
        gl.uniform3fv_with_f32_array(uniforms.light_positions.as_ref(), &lights.positions);
        gl.uniform3fv_with_f32_array(uniforms.light_colors.as_ref(), &lights.colors);
        gl.uniform4fv_with_f32_array(uniforms.light_params.as_ref(), &lights.params);
    }

    let cores = flare_cores(frame);
    gl.uniform1i(uniforms.core_count.as_ref(), cores.count);
    gl.uniform4fv_with_f32_array(uniforms.cores.as_ref(), &cores.pos_intensity);
    gl.uniform3fv_with_f32_array(uniforms.core_colors.as_ref(), &cores.colors);
}

fn draw_visible_chunks(
    renderer: &SurfelRenderer,
    visible: &[&SurfelChunk],
    budget_enabled: bool,
) -> FrameStats {
    let gl = &renderer.gl;
    let mut stats = FrameStats::default();

    for chunk in visible {
        let total = chunk.surfel_count.max(0) as u32;
        // OPTIMIZATION (rt_budget): whole-chunk drops once the frame's disc
        // budget is spent. Chunks arrive near-to-far, so what gets dropped
        // is always the farthest thing still standing.
        if budget_enabled && stats.surfels_drawn >= renderer.profile.surfel_budget {
            stats.surfels_dropped = stats.surfels_dropped.saturating_add(total);
            continue;
        }
        if total == 0 {
            continue;
        }

        bind_chunk(renderer, chunk);
        gl.draw_arrays_instanced(Gl::TRIANGLE_STRIP, 0, 4, total.min(i32::MAX as u32) as i32);
        stats.surfels_drawn = stats.surfels_drawn.saturating_add(total);
        stats.draw_calls += 1;
        stats.chunks_drawn += 1;
    }

    gl.bind_vertex_array(None);
    gl.bind_buffer(Gl::ARRAY_BUFFER, None);
    stats
}

fn bind_chunk(renderer: &SurfelRenderer, chunk: &SurfelChunk) {
    let gl = &renderer.gl;
    let uniforms = &renderer.uniforms;
    gl.uniform3f(
        uniforms.chunk_origin.as_ref(),
        chunk.origin[0],
        chunk.origin[1],
        chunk.origin[2],
    );
    gl.uniform3f(
        uniforms.chunk_size.as_ref(),
        chunk.bounds_max[0],
        chunk.bounds_max[1],
        chunk.bounds_max[2],
    );
    gl.uniform1f(uniforms.voxel_scale.as_ref(), chunk.voxel_scale);

    gl.active_texture(Gl::TEXTURE0);
    gl.bind_texture(Gl::TEXTURE_3D, Some(&chunk.light_texture));
    gl.uniform1i(uniforms.light_volume.as_ref(), 0);
    gl.bind_vertex_array(Some(&chunk.vao));
}
