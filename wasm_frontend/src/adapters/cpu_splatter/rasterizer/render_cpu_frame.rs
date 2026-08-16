//! Orchestrate one complete CPU-splat frame.
//!
//! The sequence is intentionally visible in [`SoftwareRasterizer::render_cpu_frame`]:
//! capture atmosphere, clear reusable buffers, build the camera, choose chunk
//! order, reject non-contributing chunks, traverse, then draw flare cores.
//! Individual rendering algorithms live behind those named operations.

use crate::application::ports::{ChunkDraw, FrameParams};

use super::super::camera::{Camera, dot};
use super::SoftwareRasterizer;
use super::frame_work_budget::FrameWorkBudget;
use super::frame_work_plan::FrameWorkPlan;
use super::project_aabb_footprint::{
    NEAR_PLANE_DEPTH, aabb_camera_depth_interval, project_screen_footprint,
};
use super::resolve_hero_light_visibility::SHADOW_PIXEL_BUDGET;
use super::select_lights_for_chunk::select_lights_for_chunk;

impl SoftwareRasterizer {
    pub(super) fn render_cpu_frame(&mut self, frame: &FrameParams, chunks: &[ChunkDraw]) {
        self.environment = frame.environment;
        self.frame_envelope =
            FrameWorkBudget::for_target(&self.settings, self.width(), self.height());
        self.frame_work_budget = self.frame_envelope;
        self.clear();
        self.reset_frame_telemetry();
        if self.atlas.is_empty() {
            return;
        }

        let camera = Camera::new(frame, self.width(), self.height(), &self.settings);
        let ordered_chunks = chunks_in_draw_order(
            chunks,
            frame.camera_pos,
            self.settings.toggles.front_to_back,
        );
        // Allowances are fixed before anything is drawn, from the camera and
        // chunk list alone. Nothing below can change what another chunk or
        // another band is allowed to spend.
        let plan = FrameWorkPlan::build(
            self.frame_envelope,
            SHADOW_PIXEL_BUDGET,
            &camera,
            &ordered_chunks,
            self.width(),
            self.height(),
        );
        // Reused across chunks: one world-fixture scan per chunk, no
        // per-splat allocation and no truncation of overlapping lights.
        let mut chunk_scene_lights = std::mem::take(&mut self.chunk_light_scratch);
        let global_hero_id = frame.active_scene_lights().first().map(|light| light.id);
        let mut visited_nodes = 0;
        let mut pixel_writes = 0;
        for band_index in self.rendered_bands() {
            self.activate_band(band_index);
            for (chunk_index, chunk) in ordered_chunks.iter().enumerate() {
                // Culling a whole chunk is safe because a chunk's allowance
                // is its own. Culling individual *nodes* against the band
                // would not be: it would change the node count and so move
                // where a node-starved traversal stops.
                if self.settings.toggles.distance_cull
                    && !chunk_can_contribute(
                        chunk,
                        &camera,
                        self.width(),
                        self.height(),
                        self.settings.max_draw_distance,
                    )
                {
                    continue;
                }
                self.begin_chunk(&plan, chunk_index);
                select_lights_for_chunk(
                    frame.active_scene_lights(),
                    chunk,
                    &mut chunk_scene_lights,
                );
                self.traverse_voxel_chunk(
                    &camera,
                    chunks,
                    chunk,
                    &chunk_scene_lights,
                    global_hero_id,
                );
                visited_nodes += self.visited_nodes;
                pixel_writes += self.pixel_writes;
            }
            self.draw_visible_flare_cores(frame, &camera);
        }
        self.finish_bands();

        self.frame_work_budget = self.frame_envelope;
        self.visited_nodes = visited_nodes;
        self.pixel_writes = pixel_writes;
        self.chunk_light_scratch = chunk_scene_lights;
        self.hero_light_visibility.advance_frame();
    }

    fn reset_frame_telemetry(&mut self) {
        self.visited_nodes = 0;
        self.budget_exhausted = false;
        self.max_virtual_depth_reached = 0;
        self.splat_count = 0;
        self.pixel_writes = 0;
    }
}

/// Front-to-back ordering is an optional performance policy; fine depth
/// testing makes the original chunk order the output-equivalent reference.
fn chunks_in_draw_order(
    chunks: &[ChunkDraw],
    camera_position: [f32; 3],
    front_to_back: bool,
) -> Vec<&ChunkDraw> {
    let mut ordered: Vec<_> = chunks.iter().collect();
    if front_to_back {
        ordered.sort_by(|a, b| {
            chunk_distance_sq(a, camera_position).total_cmp(&chunk_distance_sq(b, camera_position))
        });
    }
    ordered
}

/// Conservative chunk-level distance and screen-bounds rejection.
fn chunk_can_contribute(
    chunk: &ChunkDraw,
    camera: &Camera,
    width: usize,
    height: usize,
    max_draw_distance: f32,
) -> bool {
    let half_size = chunk.world_size * 0.5;
    let center = [
        chunk.origin[0] + half_size,
        chunk.origin[1] + half_size,
        chunk.origin[2] + half_size,
    ];
    let radius = chunk.world_size * 0.5 * 3.0_f32.sqrt();
    let relative_to_camera = [
        center[0] - camera.pos[0],
        center[1] - camera.pos[1],
        center[2] - camera.pos[2],
    ];

    if dot(relative_to_camera, relative_to_camera).sqrt() - radius > max_draw_distance {
        return false;
    }
    let depth_interval = aabb_camera_depth_interval(camera, relative_to_camera, half_size);
    if depth_interval[1] <= NEAR_PLANE_DEPTH {
        return false;
    }

    project_screen_footprint(camera, chunk.origin, chunk.world_size, depth_interval[0])
        .is_some_and(|footprint| !footprint.outside_framebuffer(width, height))
}

fn chunk_distance_sq(chunk: &ChunkDraw, position: [f32; 3]) -> f32 {
    let half = chunk.world_size * 0.5;
    let dx = chunk.origin[0] + half - position[0];
    let dz = chunk.origin[2] + half - position[2];
    dx * dx + dz * dz
}
