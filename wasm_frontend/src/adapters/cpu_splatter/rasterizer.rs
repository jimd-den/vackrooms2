//! Persistent state and public facade for CPU voxel splatting.
//!
//! This file deliberately contains no frame algorithm. It owns allocations
//! and the stable [`RendererPort`] API; purpose-named child modules explain
//! what happens to a frame:
//!
//! * [`render_cpu_frame`] orchestrates one complete frame;
//! * [`traverse_voxel_scene`] projects and walks the SVO;
//! * [`resolve_hero_light_visibility`] amortizes secondary visibility rays;
//! * [`select_lights_for_chunk`] builds exact finite-support light clusters;
//! * [`draw_visible_flare_cores`] draws depth-tested dynamic-light cores;
//! * [`write_depth_tested_splats`] owns framebuffer and hierarchical Z data.
//!
//! Color remains in [`super::shading`], secondary rays in
//! [`super::raycast`], and per-frame camera math in [`super::camera`].

mod band_layout;
mod draw_visible_flare_cores;
mod frame_work_budget;
mod frame_work_plan;
mod project_aabb_footprint;
mod render_cpu_frame;
mod resolve_hero_light_visibility;
mod select_lights_for_chunk;
mod simd_row_ops;
mod traverse_voxel_scene;
mod write_depth_tested_splats;

use crate::application::ports::{ChunkDraw, Environment, FrameParams, LightSource, RendererPort};
use crate::application::rendering::encode_display_color;

use super::atlas::{MipNode, build_mips};
use super::camera::Camera;
use super::settings::CpuRenderSettings;
use super::shading::{self, FrameLighting, SplatSurface};
use super::surface_geometry::{ProjectedSurfaceDepth, SurfaceExposure};
use super::update_atlas_rows::update_prepared_atlas_rows;
use band_layout::{BandLayout, partition_bands};
use frame_work_budget::FrameWorkBudget;
use frame_work_plan::FrameWorkPlan;
use resolve_hero_light_visibility::HeroLightVisibilityCache;
use write_depth_tested_splats::{COARSE_TILE, DepthTestedSplatTarget, SquareSplat};

/// Per-frame counters for the HUD; tests also use them to prove termination
/// and safety-cap behavior.
#[derive(Debug, Clone, Copy, Default)]
pub struct SoftwareRasterizerTelemetry {
    pub visited_nodes: usize,
    pub node_soft_limit: usize,
    pub node_visit_limit: usize,
    pub node_budget_limited: bool,
    pub budget_exhausted: bool,
    pub max_virtual_depth: usize,
    pub splat_count: usize,
    pub pixel_writes: usize,
    pub pixel_write_limit: usize,
    pub pixel_budget_limited: bool,
}

/// Persistent state for the CPU renderer. The struct is intentionally the
/// allocation-owning facade; rendering algorithms live in focused modules.
pub struct SoftwareRasterizer {
    /// The band currently being rendered. Between bands its storage is
    /// swapped out to `band_targets`, so every operation below sees one
    /// ordinary target and needs no knowledge of the partitioning.
    target: DepthTestedSplatTarget,
    /// Parked storage for the bands that are not currently active. Empty
    /// when the frame is rendered unpartitioned.
    band_targets: Vec<DepthTestedSplatTarget>,
    bands: Vec<BandLayout>,
    /// Requested band count. One reproduces the unpartitioned renderer.
    band_count: usize,
    /// Remaining pixel writes for the current chunk, indexed by absolute
    /// coarse-tile row. Sized once per frame and refilled per chunk.
    zone_budgets: Vec<usize>,
    /// Zones owned by the band being rendered, as `first..first + count`.
    band_zones: (usize, usize),
    /// Which band is currently swapped into `target`.
    active_band: usize,
    /// When set, only this band is allocated and drawn; the others exist as
    /// geometry alone. That is how a render worker owns one slice of a frame
    /// it never holds in full — it still needs the whole partition, because
    /// zone allowances and clipping are defined against the whole frame.
    assigned_band: Option<usize>,
    /// Hero shadow rays this chunk may trace, and how many it has traced.
    /// Replaces the old frame-global pixel-write threshold, which made a
    /// splat's shading depend on how much had already been drawn.
    shadow_ray_budget: usize,
    shadow_rays_traced: usize,
    /// SVO atlas texels: four `u32`s per node.
    atlas: Vec<u32>,
    mips: Vec<MipNode>,
    /// Reused visitation bits for one streamed pool-slot MIP rebuild.
    mip_update_scratch: Vec<bool>,
    /// Reused finite-support fixture cluster; populated once per drawn chunk.
    chunk_light_scratch: Vec<LightSource>,

    // Public counters are retained for compatibility; `telemetry()` is the
    // stable snapshot API used by the browser driver and tests.
    pub visited_nodes: usize,
    pub budget_exhausted: bool,
    pub max_virtual_depth_reached: usize,
    pub splat_count: usize,
    pub pixel_writes: usize,
    /// The chunk currently being traversed. Narrowed from `frame_envelope`
    /// once per chunk, never from itself — deriving it from the previous
    /// chunk's value would ratchet the allowance down as a frame progressed
    /// and reintroduce exactly the order dependence bands must not have.
    frame_work_budget: FrameWorkBudget,
    /// The whole frame's envelope, fixed for the frame and reported to the HUD.
    frame_envelope: FrameWorkBudget,

    pub settings: CpuRenderSettings,
    hero_light_visibility: HeroLightVisibilityCache,
    /// Current level atmosphere, captured once at the start of `draw`.
    environment: Environment,
}

impl SoftwareRasterizer {
    pub fn new(width: usize, height: usize) -> Self {
        let settings = CpuRenderSettings::default();
        let frame_work_budget = FrameWorkBudget::for_target(&settings, width, height);
        let mut rasterizer = Self {
            target: DepthTestedSplatTarget::new(width, height),
            band_targets: Vec::new(),
            bands: Vec::new(),
            band_count: 1,
            zone_budgets: Vec::new(),
            band_zones: (0, 0),
            active_band: 0,
            assigned_band: None,
            shadow_ray_budget: 0,
            shadow_rays_traced: 0,
            atlas: Vec::new(),
            mips: Vec::new(),
            mip_update_scratch: Vec::new(),
            chunk_light_scratch: Vec::new(),
            visited_nodes: 0,
            budget_exhausted: false,
            max_virtual_depth_reached: 0,
            splat_count: 0,
            pixel_writes: 0,
            frame_work_budget,
            frame_envelope: frame_work_budget,
            settings,
            hero_light_visibility: HeroLightVisibilityCache::new(),
            environment: Environment::default(),
        };
        // The band layout is the single source of truth for the frame's
        // geometry, so build it here rather than leaving `new` and `resize`
        // to agree by coincidence.
        rasterizer.rebuild_bands(width.max(1), height.max(1));
        rasterizer
    }

    pub fn resize(&mut self, width: usize, height: usize) {
        self.rebuild_bands(width.max(1), height.max(1));
    }

    /// Splits subsequent frames across `band_count` horizontal bands.
    ///
    /// Bands are the unit of parallelism, but they are also meaningful with
    /// one thread: rendering N bands sequentially must produce exactly the
    /// frame that one band does, which is what the equivalence test pins.
    pub fn set_band_count(&mut self, band_count: usize) {
        let band_count = band_count.max(1);
        if band_count == self.band_count {
            return;
        }
        self.band_count = band_count;
        self.assigned_band = None;
        self.rebuild_bands(self.width(), self.total_height());
    }

    /// Renders only `band_index` of a `band_count`-way partition.
    ///
    /// For a render worker, which draws one slice of a frame that lives
    /// nowhere in its address space. The unassigned bands are still part of
    /// the partition — allowances and clipping are defined against the whole
    /// framebuffer — they just carry no pixels here.
    pub fn set_band_assignment(&mut self, band_count: usize, band_index: usize) {
        self.band_count = band_count.max(1);
        let bands = partition_bands(self.total_height(), self.band_count);
        self.assigned_band = Some(band_index.min(bands.len() - 1));
        self.rebuild_bands(self.width(), self.total_height());
    }

    /// The bands this rasterizer actually draws, in frame order.
    fn rendered_bands(&self) -> std::ops::Range<usize> {
        match self.assigned_band {
            Some(band) => band..band + 1,
            None => 0..self.bands.len(),
        }
    }

    fn rebuild_bands(&mut self, width: usize, total_height: usize) {
        self.bands = partition_bands(total_height, self.band_count);
        if let Some(assigned) = self.assigned_band {
            self.assigned_band = Some(assigned.min(self.bands.len() - 1));
        }
        // Unbounded at rest. `begin_chunk` installs the real allowance for
        // every chunk of a real frame; leaving zeros here would silently
        // reject writes from the direct splat seams the shading tests use.
        self.zone_budgets = vec![usize::MAX; total_height.div_ceil(COARSE_TILE)];
        // A band this rasterizer will never draw still needs a slot, so the
        // swap indices line up, but not the pixels — one row is enough to
        // keep it a valid target.
        let rows_for = |index: usize, band: BandLayout| match self.assigned_band {
            Some(assigned) if assigned != index => 1,
            _ => band.height,
        };
        // The first band lives in `target`; the rest are parked.
        self.target.resize_band(
            width,
            total_height,
            self.bands[0].row_offset,
            rows_for(0, self.bands[0]),
        );
        self.band_zones = (0, self.zone_budgets.len());
        self.active_band = 0;
        self.band_targets = self.bands[1..]
            .iter()
            .enumerate()
            .map(|(offset, band)| {
                let mut target = DepthTestedSplatTarget::new(width, total_height);
                target.resize_band(
                    width,
                    total_height,
                    band.row_offset,
                    rows_for(offset + 1, *band),
                );
                target
            })
            .collect();
    }

    pub fn width(&self) -> usize {
        self.target.width()
    }

    /// Height of the whole framebuffer, not of the active band.
    pub fn height(&self) -> usize {
        self.total_height()
    }

    fn total_height(&self) -> usize {
        self.bands
            .last()
            .map_or_else(|| self.target.height(), |band| band.row_end())
    }

    /// The finished frame as top-down RGBA8 rows.
    ///
    /// Contiguous when unpartitioned; otherwise the bands are concatenated
    /// into a reusable buffer, since they own separate allocations. Callers
    /// that can present per band should use [`Self::bands_rgba`] instead and
    /// skip the copy — that is what the worker present path does.
    pub fn framebuffer(&self) -> &[u8] {
        debug_assert_eq!(
            self.bands.len(),
            1,
            "a partitioned frame has no single contiguous buffer; use \
             compose_into or bands_rgba"
        );
        self.target.rgba()
    }

    /// Copies every band's rows into `out` in framebuffer order.
    pub fn compose_into(&self, out: &mut Vec<u8>) {
        debug_assert!(
            self.assigned_band.is_none(),
            "a worker rasterizer holds one band, not a frame; use bands_rgba"
        );
        let stride = self.width() * 4;
        out.clear();
        out.resize(stride * self.total_height(), 0);
        for (row_offset, rgba) in self.bands_rgba() {
            let start = row_offset * stride;
            out[start..start + rgba.len()].copy_from_slice(rgba);
        }
    }

    /// Each band's first framebuffer row paired with its own RGBA rows, in
    /// frame order. Presenting per band avoids the [`Self::compose_into`]
    /// copy entirely, which is what the worker present path wants.
    pub fn bands_rgba(&self) -> impl Iterator<Item = (usize, &[u8])> {
        self.rendered_bands().map(|index| {
            let rgba = if index == 0 {
                self.target.rgba()
            } else {
                self.band_targets[index - 1].rgba()
            };
            (self.bands[index].row_offset, rgba)
        })
    }

    pub fn telemetry(&self) -> SoftwareRasterizerTelemetry {
        let node_budget_limited = self.budget_exhausted
            && self.frame_envelope.outside_focus_reserve(self.visited_nodes);
        let pixel_budget_limited =
            self.budget_exhausted && self.frame_envelope.writes_exhausted(self.pixel_writes);
        SoftwareRasterizerTelemetry {
            visited_nodes: self.visited_nodes,
            node_soft_limit: self.frame_envelope.soft_node_visit_limit(),
            node_visit_limit: self.frame_envelope.node_visit_limit(),
            node_budget_limited,
            budget_exhausted: self.budget_exhausted,
            max_virtual_depth: self.max_virtual_depth_reached,
            splat_count: self.splat_count,
            pixel_writes: self.pixel_writes,
            pixel_write_limit: self.frame_envelope.pixel_write_limit(),
            pixel_budget_limited,
        }
    }

    /// Test seam and frame clear: atmosphere chooses the background; target
    /// storage owns color/depth/HZ reset mechanics.
    pub(super) fn clear(&mut self) {
        let background_radiance = if self.environment.outdoor {
            self.environment.sky_color
        } else {
            self.environment.fog_color
        };
        let background = encode_display_color(background_radiance)
            .map(|channel| (channel * 255.0).round().clamp(0.0, 255.0) as u8);
        self.target.clear(background);
        for target in &mut self.band_targets {
            target.clear(background);
        }
    }

    /// Frame-policy wrapper around the pixel-only target operation. It keeps
    /// the exact in-loop write cap and renderer telemetry out of `target`.
    fn splat(&mut self, cx: f32, cy: f32, half: f32, z: f32, rgb: [u8; 3]) {
        self.write_splat_request(SquareSplat::new(cx, cy, half, z, rgb));
    }

    /// Writes a shaded voxel surface using its exact ray/plane depth. The
    /// emissive flag affects only exact coplanar reconstruction ties.
    fn surface_splat(
        &mut self,
        cx: f32,
        cy: f32,
        half: f32,
        depth: ProjectedSurfaceDepth,
        rgb: [u8; 3],
        emissive: bool,
    ) {
        let mut splat = SquareSplat::new(cx, cy, half, depth.representative_depth(), rgb)
            .with_surface_depth(depth);
        if emissive {
            splat = splat.with_equal_depth_priority();
        }
        self.write_splat_request(splat);
    }

    fn write_splat_request(&mut self, splat: SquareSplat) {
        if self.band_zone_budgets_spent() {
            self.budget_exhausted = true;
            return;
        }
        self.splat_count += 1;

        let result = self.target.write_splat(
            splat,
            &mut self.zone_budgets,
            self.settings.toggles.hierarchical_z,
        );
        self.pixel_writes += result.pixel_writes;
        self.budget_exhausted |= result.budget_exhausted;
    }

    /// Swaps band `band_index` into `target` so the rest of the renderer can
    /// keep treating it as the one and only render target.
    ///
    /// Band 0 lives in `target` at rest, so activating it is a no-op and the
    /// unpartitioned case never touches `band_targets` at all.
    pub(super) fn activate_band(&mut self, band_index: usize) {
        // Restore first. The parked slots only line up with their bands when
        // nothing is checked out, so swapping a second band in on top of the
        // first would leave each one in the wrong slot.
        self.finish_bands();
        let band = self.bands[band_index];
        self.band_zones = (
            band.row_offset / COARSE_TILE,
            band.height.div_ceil(COARSE_TILE),
        );
        if band_index > 0 {
            std::mem::swap(&mut self.target, &mut self.band_targets[band_index - 1]);
        }
        self.active_band = band_index;
    }

    /// Returns the checked-out band to its slot, leaving band 0 in `target`
    /// and `band_targets[i - 1]` holding band `i` — the at-rest invariant
    /// every reader of the framebuffer depends on.
    pub(super) fn finish_bands(&mut self) {
        if self.active_band > 0 {
            std::mem::swap(&mut self.target, &mut self.band_targets[self.active_band - 1]);
        }
        self.active_band = 0;
        self.band_zones = (0, self.zone_budgets.len());
    }

    /// Loads one chunk's pre-computed allowances and zeroes the counters they
    /// are measured against, so traversal sees a per-chunk envelope.
    fn begin_chunk(&mut self, plan: &FrameWorkPlan, chunk_index: usize) {
        let (first_zone, zone_count) = self.band_zones;
        self.zone_budgets.fill(0);
        for zone in first_zone..(first_zone + zone_count).min(self.zone_budgets.len()) {
            self.zone_budgets[zone] = plan.zone_pixel_writes(chunk_index, zone);
        }
        self.frame_work_budget = self.frame_envelope.for_chunk(
            plan.chunk_node_visits(chunk_index),
            plan.band_pixel_writes(chunk_index, first_zone, zone_count),
        );
        self.shadow_ray_budget = plan.chunk_shadow_rays(chunk_index);
        self.shadow_rays_traced = 0;
        self.visited_nodes = 0;
        self.pixel_writes = 0;
    }

    /// True once every zone the active band owns has spent this chunk's
    /// write allowance, so no further splat from it can change a pixel.
    ///
    /// This is scoped to the band's own zones rather than the whole frame on
    /// purpose: a zone outside this band still having allowance says nothing
    /// about whether this band can write, and consulting it would make the
    /// early-out depend on how the frame happens to be partitioned.
    pub(super) fn band_zone_budgets_spent(&self) -> bool {
        let (first, count) = self.band_zones;
        self.zone_budgets
            .iter()
            .skip(first)
            .take(count)
            .all(|remaining| *remaining == 0)
    }

    /// Exact pre-shading fine-depth rejection. This deliberately lives at the
    /// renderer boundary: traversal can avoid radiance and visibility work,
    /// while the pixel target remains the single owner of clipping/depth math.
    fn surface_splat_may_contribute(
        &self,
        cx: f32,
        cy: f32,
        half: f32,
        depth: ProjectedSurfaceDepth,
        emissive: bool,
    ) -> bool {
        if self.band_zone_budgets_spent() {
            return false;
        }
        let mut splat = SquareSplat::new(cx, cy, half, depth.representative_depth(), [0; 3])
            .with_surface_depth(depth);
        if emissive {
            splat = splat.with_equal_depth_priority();
        }
        self.target.surface_splat_may_contribute(&splat)
    }

    /// Shades one exact box and writes its square splat. Retained as a narrow
    /// test seam; lighting itself remains in `shading`.
    #[cfg(test)]
    #[allow(clippy::too_many_arguments)]
    pub(super) fn shade_and_splat(
        &mut self,
        cam: &Camera,
        chunks: &[ChunkDraw],
        center: [f32; 3],
        world_size: f32,
        px: f32,
        py: f32,
        half_px: f32,
        dist: f32,
        base_color: [f32; 3],
        light_level: f32,
        is_emissive: bool,
        crowded_siblings: u32,
    ) {
        self.shade_and_splat_with_frame_lighting(
            cam,
            chunks,
            FrameLighting::unshadowed(&[]),
            center,
            world_size,
            px,
            py,
            half_px,
            dist,
            base_color,
            light_level,
            is_emissive,
            crowded_siblings,
        );
    }

    /// Production shading path with the exact per-chunk fixture cluster and
    /// optional id-matched hero visibility context.
    #[cfg(test)]
    #[allow(clippy::too_many_arguments)]
    pub(super) fn shade_and_splat_with_frame_lighting(
        &mut self,
        cam: &Camera,
        chunks: &[ChunkDraw],
        lighting: FrameLighting<'_>,
        center: [f32; 3],
        world_size: f32,
        px: f32,
        py: f32,
        half_px: f32,
        dist: f32,
        base_color: [f32; 3],
        light_level: f32,
        is_emissive: bool,
        crowded_siblings: u32,
    ) {
        let linear_albedo = super::decode_srgb_albedo::decode_albedo(base_color);
        self.shade_predecoded_and_splat_with_frame_lighting(
            cam,
            chunks,
            lighting,
            center,
            world_size,
            px,
            py,
            half_px,
            ProjectedSurfaceDepth::constant(dist),
            linear_albedo,
            [light_level; 3],
            is_emissive.then(|| shading::emission_strength(base_color)),
            1,
            SurfaceExposure::ALL_FACES,
            crowded_siblings,
        );
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn shade_predecoded_and_splat_with_frame_lighting(
        &mut self,
        cam: &Camera,
        chunks: &[ChunkDraw],
        lighting: FrameLighting<'_>,
        center: [f32; 3],
        world_size: f32,
        px: f32,
        py: f32,
        half_px: f32,
        surface_depth: ProjectedSurfaceDepth,
        linear_albedo: [f32; 3],
        baked_irradiance: [f32; 3],
        emission_strength: Option<f32>,
        voxel_type: u32,
        exposure: SurfaceExposure,
        crowded_siblings: u32,
    ) {
        let surface = SplatSurface {
            center,
            world_size,
            dist: surface_depth.representative_depth(),
            base_color: [0.0; 3],
            baked_irradiance,
            voxel_type,
            is_emissive: emission_strength.is_some(),
            exposure,
            crowded_siblings,
        };
        let rgb = shading::shade_predecoded_with_frame_lighting(
            cam,
            &self.environment,
            &self.settings,
            &self.atlas,
            chunks,
            lighting,
            &surface,
            linear_albedo,
            emission_strength,
        );
        self.surface_splat(
            px,
            py,
            half_px,
            surface_depth,
            rgb,
            emission_strength.is_some(),
        );
    }
}

impl RendererPort for SoftwareRasterizer {
    fn upload_atlas(&mut self, texels: &[u32]) {
        self.atlas = texels.to_vec();
        // MIP attributes are rebuilt on upload, never per frame.
        self.mips = build_mips(&self.atlas);
    }

    fn upload_atlas_rows(&mut self, first_row: u32, texels: &[u32]) -> bool {
        update_prepared_atlas_rows(
            &mut self.atlas,
            &mut self.mips,
            &mut self.mip_update_scratch,
            first_row,
            texels,
        )
    }

    fn draw(&mut self, frame: &FrameParams, chunks: &[ChunkDraw]) {
        self.render_cpu_frame(frame, chunks);
    }
}

#[cfg(test)]
mod atlas_row_upload_tests {
    use crate::application::atlas::ROW_TEXELS;
    use crate::application::ports::{ChunkDraw, FrameParams, RendererPort};

    use super::SoftwareRasterizer;

    const WORDS_PER_NODE: usize = 4;
    const AIR: [u32; WORDS_PER_NODE] = [1, 0, 0, 0];

    fn air_rows(rows: usize) -> Vec<u32> {
        AIR.repeat(ROW_TEXELS * rows)
    }

    #[test]
    fn renderer_port_row_upload_matches_a_full_reference_upload() {
        let initial = air_rows(2);
        let mut changed_row = air_rows(1);
        changed_row[1] = 1;
        changed_row[2] = 0x66aa44;
        changed_row[3] = 9 | (9 << 16) | (7 << 20) | (5 << 24);

        let mut incremental = SoftwareRasterizer::new(8, 8);
        incremental.upload_atlas(&initial);
        assert!(incremental.upload_atlas_rows(1, &changed_row));

        let mut expected_atlas = initial;
        let second_row_start = ROW_TEXELS * WORDS_PER_NODE;
        expected_atlas[second_row_start..].copy_from_slice(&changed_row);
        let mut reference = SoftwareRasterizer::new(8, 8);
        reference.upload_atlas(&expected_atlas);

        assert_eq!(incremental.atlas, reference.atlas);
        assert_eq!(incremental.mips, reference.mips);
        assert_eq!(incremental.mip_update_scratch.len(), ROW_TEXELS);

        let chunks = [ChunkDraw {
            origin: [0.0; 3],
            root_index: ROW_TEXELS as i32,
            world_size: 1.0,
            voxel_size: 1.0,
            svo_depth: 0,
        }];
        let frame = FrameParams {
            camera_pos: [0.5, 0.5, -2.0],
            yaw: std::f32::consts::PI,
            ..FrameParams::default()
        };
        incremental.draw(&frame, &chunks);
        reference.draw(&frame, &chunks);
        assert_eq!(incremental.framebuffer(), reference.framebuffer());
    }
}
