//! Hand every chunk its work allowance up front, before any of it is spent.
//!
//! The previous policy consumed one frame-global running counter: traversal
//! stopped mid-recursion when the counter hit the limit, and the chunk loop
//! broke outright. That makes a chunk's fate depend on how much earlier
//! chunks happened to spend, so the same scene renders differently depending
//! on draw order — and it cannot be split across threads at all, because
//! every worker would need to see every other worker's spending.
//!
//! Here the envelope is divided before the frame starts, from inputs that do
//! not depend on traversal: the camera, the chunk list, and the settings.
//!
//! # What makes bands equivalent
//!
//! Two quantities are partitioned differently, on purpose:
//!
//! - **Pixel writes** are split per *(chunk, zone)*, where a zone is one
//!   coarse-tile row of the framebuffer. Zones are a property of the
//!   framebuffer height alone, so they are identical whether the frame is
//!   rendered as one band or eight. A band enforces only the zones it owns,
//!   and each zone is capped the same in every configuration.
//! - **Node visits** are split per *chunk*, and every band receives that
//!   whole per-chunk allowance rather than a share of it. Traversal is
//!   duplicated across bands by design, so a band walks a chunk's tree
//!   exactly as the single-band renderer would and therefore stops at
//!   exactly the same node.
//!
//! Together those give a frame that is bit-identical across band counts. The
//! one thing that must *not* be added is per-node band culling: skipping a
//! subtree because its footprint misses the band would change the node count
//! and so change where a node-starved traversal stops. Culling a whole chunk
//! is fine, because a chunk's allowance is its own.

use crate::application::ports::ChunkDraw;

use super::super::camera::Camera;
use super::frame_work_budget::FrameWorkBudget;
use super::project_aabb_footprint::{
    NEAR_PLANE_DEPTH, aabb_camera_depth_interval, project_screen_footprint,
};
use super::write_depth_tested_splats::COARSE_TILE;

/// Smallest share any drawn chunk keeps, so a chunk that is on screen but
/// tiny still gets enough allowance to resolve its own geometry rather than
/// being rounded down into invisibility by a large neighbour.
const MINIMUM_CHUNK_NODE_VISITS: usize = 2_048;
const MINIMUM_ZONE_PIXEL_WRITES: usize = 64;

/// Per-frame work allowances, indexed by chunk and framebuffer zone.
pub(super) struct FrameWorkPlan {
    zones: usize,
    /// `chunk_index * zones + zone`.
    pixel_writes: Vec<usize>,
    node_visits: Vec<usize>,
    shadow_rays: Vec<usize>,
}

impl FrameWorkPlan {
    /// Splits `budget` across `chunks` by projected screen area.
    ///
    /// Areas are integer pixel counts and every division is integer, with
    /// remainders handed out in canonical chunk-then-zone order. No float
    /// accumulation, no sort, so the result depends only on the values —
    /// never on the order the slice happens to be in.
    pub(super) fn build(
        budget: FrameWorkBudget,
        shadow_ray_budget: usize,
        camera: &Camera,
        chunks: &[&ChunkDraw],
        width: usize,
        height: usize,
    ) -> Self {
        let zones = height.max(1).div_ceil(COARSE_TILE);
        let areas = chunk_zone_areas(camera, chunks, width, height, zones);

        let total_area: u128 = areas.iter().map(|area| *area as u128).sum();
        let chunk_areas: Vec<usize> = areas
            .chunks_exact(zones.max(1))
            .map(|zone_areas| zone_areas.iter().sum())
            .collect();

        Self {
            zones,
            pixel_writes: apportion(
                budget.pixel_write_limit(),
                &areas,
                total_area,
                MINIMUM_ZONE_PIXEL_WRITES,
            ),
            node_visits: apportion(
                budget.node_visit_limit(),
                &chunk_areas,
                total_area,
                MINIMUM_CHUNK_NODE_VISITS,
            ),
            shadow_rays: apportion(shadow_ray_budget, &chunk_areas, total_area, 0),
        }
    }

    /// Write allowance for one chunk within one framebuffer zone.
    pub(super) fn zone_pixel_writes(&self, chunk_index: usize, zone: usize) -> usize {
        self.pixel_writes
            .get(chunk_index * self.zones + zone)
            .copied()
            .unwrap_or(0)
    }

    /// Node allowance for one chunk. Every band receives all of it; see the
    /// module docs for why that is what keeps band counts equivalent.
    pub(super) fn chunk_node_visits(&self, chunk_index: usize) -> usize {
        self.node_visits.get(chunk_index).copied().unwrap_or(0)
    }

    pub(super) fn chunk_shadow_rays(&self, chunk_index: usize) -> usize {
        self.shadow_rays.get(chunk_index).copied().unwrap_or(0)
    }

    /// Total writes this chunk may make across the zones a band owns. Used
    /// only for the per-chunk traversal budget; the per-zone caps are what
    /// actually bound the fill.
    pub(super) fn band_pixel_writes(
        &self,
        chunk_index: usize,
        first_zone: usize,
        zone_count: usize,
    ) -> usize {
        (first_zone..first_zone + zone_count)
            .map(|zone| self.zone_pixel_writes(chunk_index, zone))
            .sum()
    }

    #[cfg(test)]
    pub(super) fn zones(&self) -> usize {
        self.zones
    }
}

/// Conservative projected screen area of each chunk within each zone.
///
/// An off-screen or behind-camera chunk contributes zero area everywhere and
/// so receives only the per-chunk minimum — it is culled before traversal
/// anyway, and giving it a proportional share would starve visible geometry.
fn chunk_zone_areas(
    camera: &Camera,
    chunks: &[&ChunkDraw],
    width: usize,
    height: usize,
    zones: usize,
) -> Vec<usize> {
    let mut areas = vec![0usize; chunks.len() * zones.max(1)];
    for (chunk_index, chunk) in chunks.iter().enumerate() {
        let half_size = chunk.world_size * 0.5;
        let relative_to_camera = [
            chunk.origin[0] + half_size - camera.pos[0],
            chunk.origin[1] + half_size - camera.pos[1],
            chunk.origin[2] + half_size - camera.pos[2],
        ];
        let depth_interval = aabb_camera_depth_interval(camera, relative_to_camera, half_size);
        if depth_interval[1] <= NEAR_PLANE_DEPTH {
            continue;
        }
        let Some(footprint) =
            project_screen_footprint(camera, chunk.origin, chunk.world_size, depth_interval[0])
        else {
            continue;
        };

        let x0 = (footprint.center[0] - footprint.half_extent[0]).max(0.0);
        let x1 = (footprint.center[0] + footprint.half_extent[0]).min(width as f32);
        if x1 <= x0 {
            continue;
        }
        let span_x = (x1 - x0).ceil() as usize;
        let y0 = (footprint.center[1] - footprint.half_extent[1]).max(0.0);
        let y1 = (footprint.center[1] + footprint.half_extent[1]).min(height as f32);

        for zone in 0..zones {
            let zone_top = (zone * COARSE_TILE) as f32;
            let zone_bottom = ((zone + 1) * COARSE_TILE).min(height) as f32;
            let overlap = y1.min(zone_bottom) - y0.max(zone_top);
            if overlap <= 0.0 {
                continue;
            }
            areas[chunk_index * zones + zone] = span_x.saturating_mul(overlap.ceil() as usize);
        }
    }
    areas
}

/// Splits `total` proportionally to `weights` using integer arithmetic.
///
/// Every entry with a non-zero weight first receives `minimum`, then the
/// remainder of the envelope is divided by weight.
///
/// The rounding remainder is deliberately **left unspent**. Handing it out
/// in index order would make an allowance depend on where a chunk sits in
/// the slice, and the slice gets re-sorted whenever the front-to-back toggle
/// is on — which is documented as a pure performance policy that must not
/// change output. Any weight-based tiebreak has the same problem for two
/// chunks of equal area. Leaving at most one unit per chunk unspent, out of
/// an envelope in the millions, costs nothing: these are safety valves, not
/// quality targets. What it buys is that an allowance is a function of the
/// weights alone.
fn apportion(total: usize, weights: &[usize], total_weight: u128, minimum: usize) -> Vec<usize> {
    if weights.is_empty() {
        return Vec::new();
    }
    // A frame with nothing on screen: split evenly rather than by zero area.
    if total_weight == 0 {
        return vec![total / weights.len(); weights.len()];
    }

    let drawn = weights.iter().filter(|weight| **weight > 0).count() as u128;
    let scaled_total = (total as u128).saturating_sub((minimum as u128).saturating_mul(drawn));
    weights
        .iter()
        .map(|weight| {
            if *weight == 0 {
                0
            } else {
                (minimum as u128 + scaled_total * *weight as u128 / total_weight) as usize
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::cpu_splatter::settings::CpuRenderSettings;
    use crate::application::ports::FrameParams;

    const WIDTH: usize = 64;
    const HEIGHT: usize = 64;

    fn test_camera(settings: &CpuRenderSettings) -> Camera {
        let frame = FrameParams {
            camera_pos: [0.0, 0.0, 0.0],
            ..FrameParams::default()
        };
        Camera::new(&frame, WIDTH, HEIGHT, settings)
    }

    fn chunk_at(origin: [f32; 3], world_size: f32) -> ChunkDraw {
        ChunkDraw {
            origin,
            root_index: 0,
            world_size,
            voxel_size: world_size / 4.0,
            svo_depth: 2,
        }
    }

    fn plan_for(chunks: &[ChunkDraw]) -> FrameWorkPlan {
        let settings = CpuRenderSettings::default();
        let budget = FrameWorkBudget::for_target(&settings, WIDTH, HEIGHT);
        let borrowed: Vec<&ChunkDraw> = chunks.iter().collect();
        FrameWorkPlan::build(
            budget,
            1_500_000,
            &test_camera(&settings),
            &borrowed,
            WIDTH,
            HEIGHT,
        )
    }

    #[test]
    fn allowances_never_exceed_the_frame_envelope() {
        let settings = CpuRenderSettings::default();
        let budget = FrameWorkBudget::for_target(&settings, WIDTH, HEIGHT);
        let chunks = [
            chunk_at([-4.0, -4.0, 4.0], 8.0),
            chunk_at([4.0, -4.0, 6.0], 8.0),
            chunk_at([-2.0, 0.0, 20.0], 4.0),
        ];
        let plan = plan_for(&chunks);

        let total_writes: usize = plan.pixel_writes.iter().sum();
        assert!(
            total_writes <= budget.pixel_write_limit(),
            "{total_writes} > {}",
            budget.pixel_write_limit()
        );
        let total_nodes: usize = plan.node_visits.iter().sum();
        assert!(
            total_nodes <= budget.node_visit_limit(),
            "{total_nodes} > {}",
            budget.node_visit_limit()
        );
        assert!(plan.shadow_rays.iter().sum::<usize>() <= 1_500_000);
    }

    #[test]
    fn a_chunks_allowance_does_not_depend_on_slice_order() {
        let a = chunk_at([-4.0, -4.0, 4.0], 8.0);
        let b = chunk_at([4.0, -4.0, 6.0], 8.0);
        let c = chunk_at([-2.0, 0.0, 20.0], 4.0);

        let forward = plan_for(&[a, b, c]);
        let shuffled = plan_for(&[c, b, a]);

        // `a` is index 0 forward and index 2 shuffled; `c` is the reverse.
        assert_eq!(
            forward.chunk_node_visits(0),
            shuffled.chunk_node_visits(2),
            "chunk a"
        );
        assert_eq!(
            forward.chunk_node_visits(2),
            shuffled.chunk_node_visits(0),
            "chunk c"
        );
        assert_eq!(
            forward.chunk_node_visits(1),
            shuffled.chunk_node_visits(1),
            "chunk b"
        );
        for zone in 0..forward.zones() {
            assert_eq!(
                forward.zone_pixel_writes(0, zone),
                shuffled.zone_pixel_writes(2, zone),
                "chunk a zone {zone}"
            );
        }
    }

    #[test]
    fn zone_allowances_are_independent_of_how_bands_group_them() {
        let chunks = [chunk_at([-4.0, -4.0, 4.0], 8.0), chunk_at([4.0, -4.0, 6.0], 8.0)];
        let plan = plan_for(&chunks);
        let zones = plan.zones();

        // However the zones are grouped into bands, the pieces sum to the
        // same total. This is the property band equivalence rests on.
        for chunk_index in 0..chunks.len() {
            let whole = plan.band_pixel_writes(chunk_index, 0, zones);
            for split in 1..zones {
                let lower = plan.band_pixel_writes(chunk_index, 0, split);
                let upper = plan.band_pixel_writes(chunk_index, split, zones - split);
                assert_eq!(lower + upper, whole, "chunk {chunk_index} split at {split}");
            }
        }
    }

    #[test]
    fn an_on_screen_chunk_keeps_a_usable_floor_beside_a_huge_neighbour() {
        let chunks = [
            chunk_at([-64.0, -64.0, 2.0], 128.0),
            chunk_at([-0.5, -0.5, 40.0], 1.0),
        ];
        let plan = plan_for(&chunks);
        assert!(
            plan.chunk_node_visits(1) >= MINIMUM_CHUNK_NODE_VISITS,
            "tiny chunk got {}",
            plan.chunk_node_visits(1)
        );
    }

    #[test]
    fn an_empty_scene_splits_evenly_rather_than_dividing_by_zero() {
        assert_eq!(apportion(10, &[0, 0, 0], 0, 5), vec![3, 3, 3]);
    }

    #[test]
    fn apportioning_is_a_function_of_the_weights_alone() {
        // The same multiset of weights in any arrangement must give each
        // weight the same share, or the front-to-back sort would silently
        // redistribute allowances and change the frame.
        let forward = apportion(1_000, &[7, 3, 90], 100, 0);
        let reversed = apportion(1_000, &[90, 3, 7], 100, 0);
        assert_eq!(forward[0], reversed[2]);
        assert_eq!(forward[1], reversed[1]);
        assert_eq!(forward[2], reversed[0]);
    }

    #[test]
    fn no_chunks_yields_no_allowances() {
        let plan = plan_for(&[]);
        assert_eq!(plan.chunk_node_visits(0), 0);
        assert_eq!(plan.zone_pixel_writes(0, 0), 0);
    }
}
