//! In-wasm chunk source: implements the application's [`ChunkSourcePort`] by
//! driving the core engine use cases directly. There is no server round-trip;
//! the entire pipeline — procedural generation, renderer-selected artifacts,
//! SVO build, and collision extraction — runs inside the wasm module.
//!
//! Pipeline per chunk:
//!   GenerateChunkArchitectureUseCase  (haloed VoxelGrid + lighting)
//!     -> BuildOctreeUseCase           (always: collision authority)
//!        -> collision walk            (solid leaves -> world-space AABBs)
//!        -> compress_svdag + OctreeGpuSerializer
//!                                     (only when SVO upload words are requested)
//!     -> surface/face extraction      (only when the renderer requests it)
//!
//! `build_octree_direct` (see `benches/generation.rs`,
//! `octree_direct_vs_dense`) is NOT used here: its `dyn VoxelSampler`
//! dispatch costs ~2x `BuildOctreeUseCase` when the sampler is just a
//! `GridSampler` wrapping an already-materialized grid, since its
//! `uniform_hint` only proves out-of-grid cubes uniform and gets no pruning
//! benefit for interior geometry. It only pays off with a sampler that can
//! prove large uniform regions cheaply without touching a dense array —
//! this pipeline's grid is already dense by the time it reaches here.

use std::cell::RefCell;

use vackrooms::adapters::brick_pool_gpu_serializer::BrickPoolGpuSerializer;
use vackrooms::adapters::material_palette::DEFAULT_MATERIAL_PALETTE;
use vackrooms::adapters::octree_gpu_serializer::OctreeGpuSerializer;
use vackrooms::adapters::voxel_mapper::VoxelMapper;
use vackrooms::domain::entities::anomaly::RealitySnapshot;
use vackrooms::domain::entities::position::Position;
use vackrooms::domain::entities::sparse_voxel_octree::{SparseVoxelOctree, SvoNode};
use vackrooms::domain::entities::voxel_grid::{
    FACE_OCCLUDED_NEGATIVE_X, FACE_OCCLUDED_NEGATIVE_Y, FACE_OCCLUDED_NEGATIVE_Z,
    FACE_OCCLUDED_POSITIVE_X, FACE_OCCLUDED_POSITIVE_Y, FACE_OCCLUDED_POSITIVE_Z, VOXEL_AIR,
    VoxelGrid,
};
use vackrooms::use_cases::build_brick_pool::build_brick_pool;
use vackrooms::use_cases::build_octree::BuildOctreeUseCase;
use vackrooms::use_cases::compress_svdag::compress_svdag;
use vackrooms::use_cases::generate_chunk::{GenerateChunkArchitectureUseCase, GeneratorConfig};
use vackrooms::use_cases::generated_chunk::GeneratedChunk;
use vackrooms::use_cases::ports::{NULL_TELEMETRY, NoiseProvider, TelemetryPort};
use vackrooms::use_cases::world_block::BlockCoord;

use crate::adapters::block_cache::{BlockCache, DEFAULT_BLOCK_CAPACITY};

use crate::adapters::collect_emissive_lights::collect_emissive_lights;
use crate::adapters::surface_mesh::build_surface_artifacts;
use crate::adapters::surfel_cloud::{SurfelCloud, build_surfel_cloud};
use crate::application::collision::Aabb;
use crate::application::ports::{
    ChunkPayload, ChunkSourcePort, RenderArtifactNeeds, SurfaceMeshPayload,
};

/// Voxel types that block the player. FLOOR/CEILING/LIGHT/GRASS/WATER and
/// the carpet/fluid classes are visual-only: including them would make the
/// player collide with the floor they stand on. Aliased rather than
/// re-declared with its own length: restating the arity here meant every
/// new solid material broke this file, which is exactly the coupling the
/// "shared with the core" note was trying to avoid.
use vackrooms::domain::entities::voxel_grid::SOLID_MATERIALS as SOLID_TYPES;

pub struct LocalChunkSource<N: NoiseProvider> {
    noise: N,
    telemetry: &'static dyn TelemetryPort,
    seed: u32,
    config: GeneratorConfig,
    /// Planned blocks feeding the voxel path. `RefCell` because
    /// [`ChunkSourcePort`] loads through `&self` — the source is logically
    /// immutable and this is a memo, not state the caller can observe.
    blocks: RefCell<BlockCache>,
}

impl<N: NoiseProvider> LocalChunkSource<N> {
    pub fn new(noise: N, seed: u32, config: GeneratorConfig) -> Self {
        Self::with_telemetry(noise, seed, config, &NULL_TELEMETRY)
    }

    pub fn with_telemetry(
        noise: N,
        seed: u32,
        config: GeneratorConfig,
        telemetry: &'static dyn TelemetryPort,
    ) -> Self {
        Self {
            noise,
            telemetry,
            seed,
            config,
            blocks: RefCell::new(BlockCache::new(seed, DEFAULT_BLOCK_CAPACITY)),
        }
    }

    /// How many blocks this source has planned. Diagnostics only — a number
    /// that keeps climbing while the player stays put means the cache is
    /// thrashing.
    pub fn blocks_planned(&self) -> u64 {
        self.blocks.borrow().planned_count()
    }

    /// Blocks that should be planned now, given where the player is.
    ///
    /// Empty in the interior of a block, which is almost always. The caller
    /// decides whether it has somewhere to do the work — this only answers
    /// what the work would be.
    pub fn preload_targets(&self, world: Position, level: u32) -> Vec<BlockCoord> {
        let coord = BlockCoord::of(world);
        let mut cache = self.blocks.borrow_mut();
        if !cache.holds(level, coord) {
            // The block underfoot is not planned yet. It outranks every
            // neighbour: planning a neighbour first would spend most of a
            // second on somewhere the player is not standing.
            return vec![coord];
        }
        let targets = cache
            .get_or_plan(
                level,
                coord,
                &self.config.with_level(level),
                &self.noise,
                &mut |_, _| {},
            )
            .preload_targets(world);
        targets
            .into_iter()
            .filter(|target| !cache.holds(level, *target))
            .collect()
    }

    /// Plans one block if it is not already held, reporting progress. This is
    /// the call a loading screen drives.
    pub fn ensure_block(
        &self,
        level: u32,
        coord: BlockCoord,
        progress: &mut dyn FnMut(usize, usize),
    ) {
        self.blocks.borrow_mut().get_or_plan(
            level,
            coord,
            &self.config.with_level(level),
            &self.noise,
            progress,
        );
    }

    /// The chunk's voxels plus a one-voxel X/Z halo, and the world corner
    /// that halo starts at.
    ///
    /// Extracted so every representation derived from a chunk is derived
    /// from the *same* grid. The halo rules here are subtle — a one-voxel
    /// skirt so boundary faces are decided against real neighbours, plans
    /// taken from the block containing the chunk's centre — and a second
    /// caller reimplementing them would not fail loudly. It would produce
    /// geometry that disagreed with the mesh only at chunk seams.
    fn halo_grid(
        &self,
        origin_x: f32,
        origin_z: f32,
        config: &GeneratorConfig,
        level: u32,
        reality: &RealitySnapshot,
    ) -> (GeneratedChunk, [f32; 3]) {
        let generator =
            GenerateChunkArchitectureUseCase::with_telemetry(&self.noise, self.telemetry);

        // Generate one extra voxel around the X/Z perimeter, then crop the
        // authoritative interior for SVO/collision. The greedy mesher reads
        // the halo while deciding boundary faces, so it never invents a face
        // merely because the adjacent streamed chunk has not uploaded yet.
        let halo_config = GeneratorConfig {
            chunk_size: config.chunk_size + config.voxel_scale * 2.0,
            ..*config
        };
        let halo_world_origin = [
            origin_x - config.voxel_scale,
            0.0,
            origin_z - config.voxel_scale,
        ];
        // Voxelize from the plans of the block this chunk sits in. The block
        // carries a one-region halo, which comfortably covers this chunk's
        // one-voxel halo even for a chunk flush against a block edge, so the
        // sampler never asks about a region the block cannot answer for.
        //
        // `BlockCoord::of` is given the chunk's *centre*, not its origin: a
        // chunk whose origin lands exactly on a block boundary would
        // otherwise be attributed to the block behind it.
        let halo_origin = Position::new(halo_world_origin[0], halo_world_origin[2]);
        let centre = Position::new(
            origin_x + config.chunk_size * 0.5,
            origin_z + config.chunk_size * 0.5,
        );
        let mut blocks = self.blocks.borrow_mut();
        let block = blocks.get_or_plan(
            level,
            BlockCoord::of(centre),
            config,
            &self.noise,
            &mut |_, _| {},
        );
        let grid = generator.execute_from_plans(
            halo_origin,
            self.seed,
            halo_config,
            reality,
            Some(block.plans()),
        );
        (grid, halo_world_origin)
    }

    /// The chunk's visible surface as a cloud of oriented discs, sampled at
    /// `spacing` world units.
    ///
    /// Built from the same halo grid and the same greedy quads the mesh and
    /// the face splats come from, so the three representations can only ever
    /// disagree about how a surface is *drawn*, never about where it is.
    pub fn load_surfel_cloud(
        &self,
        origin_x: f32,
        origin_z: f32,
        level: u32,
        lod: u8,
        spacing: f32,
    ) -> SurfelCloud {
        let config = self.config.with_level(level).at_lod(lod);
        let (halo_grid, _) = self.halo_grid(
            origin_x,
            origin_z,
            &config,
            level,
            &RealitySnapshot::default(),
        );
        let mapper = VoxelMapper::new(config.voxel_scale, &DEFAULT_MATERIAL_PALETTE);
        let quads = mapper.map_voxel_grid_with_padding(&halo_grid, 1);
        build_surfel_cloud(&quads, config.voxel_scale, spacing)
    }

    fn generate_payload(
        &self,
        origin_x: f32,
        origin_z: f32,
        level: u32,
        lod: u8,
        reality: &RealitySnapshot,
        artifacts: RenderArtifactNeeds,
    ) -> ChunkPayload {
        let config = self.config.with_level(level).at_lod(lod);
        let (halo_grid, halo_world_origin) =
            self.halo_grid(origin_x, origin_z, &config, level, reality);
        let grid = crop_lateral_halo(&halo_grid, 1);
        let lights = collect_emissive_lights(&halo_grid, config.voxel_scale, halo_world_origin, 1);
        let surface = if artifacts.needs_surface_extraction() {
            build_surface_artifacts(&halo_grid, config.voxel_scale, lod, 1, artifacts)
        } else {
            let mut empty = SurfaceMeshPayload::empty(lod);
            empty.voxel_scale = config.voxel_scale;
            empty
        };

        let svo_depth = config.svo_depth();
        let svo = BuildOctreeUseCase::new(&DEFAULT_MATERIAL_PALETTE).execute(
            &grid,
            svo_depth,
            config.svo_world_size(),
        );

        let (root, nodes, brick_voxels) = if artifacts.bricks() {
            // Bricking folds the bottom two levels into dense blocks, so
            // the ray stops pointer-chasing where the nodes actually are.
            // Deliberately built from the tree, not the SVDAG: a brick is
            // addressed by a word offset the parent hands out, and shared
            // subtrees would have to agree on one, which is the DAG's whole
            // point undone. Bricking already removes ~93% of the nodes.
            let pool = build_brick_pool(&svo);
            let gpu = BrickPoolGpuSerializer::serialize_to_gpu_data(&pool);
            (gpu.root, gpu.node_data, gpu.voxel_data)
        } else if artifacts.svo_nodes() {
            // SVDAG upload: identical subtrees collapse to one shared block.
            // Traversal-only consumers (the raymarch shader) are agnostic;
            // the collision walk below deliberately keeps the tree because
            // it visits arena nodes positionally.
            let (dag, _stats) = compress_svdag(&svo);
            (
                dag.root as u32,
                OctreeGpuSerializer::serialize_to_gpu_data(&dag).texel_data,
                Vec::new(),
            )
        } else {
            (0, Vec::new(), Vec::new())
        };
        let collision = extract_collision_boxes(&svo, origin_x, origin_z, config.voxel_scale);

        ChunkPayload {
            root,
            nodes,
            brick_voxels,
            world_size: config.svo_world_size(),
            voxel_size: config.voxel_scale,
            svo_depth: svo_depth as u8,
            surface,
            lights,
            collision,
            traversal_gates: halo_grid.entities.traversal_gates.clone(),
            pit_hazards: halo_grid.entities.pit_hazards.clone(),
            supply_items: halo_grid.entities.supply_items.clone(),
            level_exits: halo_grid.entities.level_exits.clone(),
        }
    }
}

impl<N: NoiseProvider> ChunkSourcePort for LocalChunkSource<N> {
    fn load(&self, origin_x: f32, origin_z: f32, level: u32, lod: u8) -> ChunkPayload {
        self.generate_payload(
            origin_x,
            origin_z,
            level,
            lod,
            &RealitySnapshot::default(),
            RenderArtifactNeeds::ALL,
        )
    }

    fn load_with_reality(
        &self,
        origin_x: f32,
        origin_z: f32,
        level: u32,
        lod: u8,
        reality: &RealitySnapshot,
    ) -> ChunkPayload {
        self.generate_payload(
            origin_x,
            origin_z,
            level,
            lod,
            reality,
            RenderArtifactNeeds::ALL,
        )
    }

    fn load_with_artifacts(
        &self,
        origin_x: f32,
        origin_z: f32,
        level: u32,
        lod: u8,
        reality: &RealitySnapshot,
        artifacts: RenderArtifactNeeds,
    ) -> ChunkPayload {
        self.generate_payload(origin_x, origin_z, level, lod, reality, artifacts)
    }
}

/// Copies the actual chunk interior out of a generated X/Z halo while
/// retaining voxel type and colored baked lighting, then deriving exact
/// six-direction neighbor occupancy while the halo is still available.
fn crop_lateral_halo(halo: &VoxelGrid, padding: usize) -> VoxelGrid {
    assert!(halo.width() > padding * 2 && halo.depth() > padding * 2);
    let mut inner = VoxelGrid::new(
        halo.width() - padding * 2,
        halo.height(),
        halo.depth() - padding * 2,
    );
    for z in 0..inner.depth() {
        for y in 0..inner.height() {
            for x in 0..inner.width() {
                let hx = x + padding;
                let hz = z + padding;
                inner.set(x, y, z, halo.get(hx, y, hz));
                inner.set_light_rgb(x, y, z, halo.get_light_rgb(hx, y, hz));
                inner.set_face_occlusion(x, y, z, neighbor_occlusion_mask(halo, hx, y, hz));
            }
        }
    }
    inner
}

/// Exact face closure for one generated voxel, including the lateral halo.
///
/// The returned bits travel unchanged through BuildOctree and the GPU atlas.
/// Computing them before the crop is what makes a streamed chunk boundary
/// authoritative even while its adjacent chunk is unloaded or represented by
/// an overlapping power-of-two padded SVO root.
fn neighbor_occlusion_mask(grid: &VoxelGrid, x: usize, y: usize, z: usize) -> u8 {
    if grid.get(x, y, z) == VOXEL_AIR {
        return 0;
    }

    let occupied = |x: isize, y: isize, z: isize| {
        x >= 0
            && y >= 0
            && z >= 0
            && (x as usize) < grid.width()
            && (y as usize) < grid.height()
            && (z as usize) < grid.depth()
            && grid.get(x as usize, y as usize, z as usize) != VOXEL_AIR
    };
    let x = x as isize;
    let y = y as isize;
    let z = z as isize;
    let mut mask = 0;
    if occupied(x + 1, y, z) {
        mask |= FACE_OCCLUDED_POSITIVE_X;
    }
    if occupied(x - 1, y, z) {
        mask |= FACE_OCCLUDED_NEGATIVE_X;
    }
    if occupied(x, y + 1, z) {
        mask |= FACE_OCCLUDED_POSITIVE_Y;
    }
    if occupied(x, y - 1, z) {
        mask |= FACE_OCCLUDED_NEGATIVE_Y;
    }
    if occupied(x, y, z + 1) {
        mask |= FACE_OCCLUDED_POSITIVE_Z;
    }
    if occupied(x, y, z - 1) {
        mask |= FACE_OCCLUDED_NEGATIVE_Z;
    }
    mask
}

/// Walks the SVO and emits one world-space AABB per solid leaf region.
/// Uniform subtrees collapsed by the SVO become single large boxes for free,
/// which keeps the collision set small.
pub fn extract_collision_boxes(
    svo: &SparseVoxelOctree,
    origin_x: f32,
    origin_z: f32,
    voxel_scale: f32,
) -> Vec<Aabb> {
    let mut boxes = Vec::new();
    let size = 1u32 << svo.depth;
    walk(
        svo,
        svo.root,
        0,
        0,
        0,
        size,
        origin_x,
        origin_z,
        voxel_scale,
        &mut boxes,
    );
    boxes
}

#[allow(clippy::too_many_arguments)]
fn walk(
    svo: &SparseVoxelOctree,
    node_idx: usize,
    x: u32,
    y: u32,
    z: u32,
    size: u32,
    origin_x: f32,
    origin_z: f32,
    voxel_scale: f32,
    out: &mut Vec<Aabb>,
) {
    match svo.nodes[node_idx] {
        SvoNode::Leaf { voxel_type, .. } => {
            if SOLID_TYPES.contains(&voxel_type) {
                let min = [
                    origin_x + x as f32 * voxel_scale,
                    y as f32 * voxel_scale,
                    origin_z + z as f32 * voxel_scale,
                ];
                let extent = size as f32 * voxel_scale;
                out.push(Aabb::new(
                    min,
                    [min[0] + extent, min[1] + extent, min[2] + extent],
                ));
            }
        }
        SvoNode::Internal {
            child_base_index,
            child_mask,
        } => {
            let half = size / 2;
            for child in 0..8u32 {
                if child_mask & (1 << child) != 0 {
                    walk(
                        svo,
                        child_base_index as usize + child as usize,
                        x + (child & 1) * half,
                        y + ((child >> 1) & 1) * half,
                        z + ((child >> 2) & 1) * half,
                        half,
                        origin_x,
                        origin_z,
                        voxel_scale,
                        out,
                    );
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vackrooms::domain::entities::voxel_grid::{
        FACE_OCCLUDED_NEGATIVE_X, FACE_OCCLUDED_NEGATIVE_Y, FACE_OCCLUDED_POSITIVE_X,
        FACE_OCCLUDED_POSITIVE_Y, VOXEL_FLOOR, VOXEL_WALL, VoxelGrid,
    };
    use vackrooms::frameworks_drivers::simple_noise::SimpleNoiseProvider;
    use vackrooms::use_cases::world_block::BLOCK_SIZE;

    /// Regenerates a chunk the pre-block way — plans derived for this chunk
    /// alone — and reduces it to the same collision set the payload carries.
    /// Any difference in the voxels shows up here as a different box set.
    fn streamed_collision(seed: u32, origin_x: f32, origin_z: f32, lod: u8) -> Vec<Aabb> {
        let noise = SimpleNoiseProvider::new();
        let config = GeneratorConfig::low_spec().with_level(0).at_lod(lod);
        let halo_config = GeneratorConfig {
            chunk_size: config.chunk_size + config.voxel_scale * 2.0,
            ..config
        };
        let halo = GenerateChunkArchitectureUseCase::new(&noise).execute_with_reality(
            Position::new(origin_x - config.voxel_scale, origin_z - config.voxel_scale),
            seed,
            halo_config,
            &RealitySnapshot::default(),
        );
        let grid = crop_lateral_halo(&halo, 1);
        let svo = BuildOctreeUseCase::new(&DEFAULT_MATERIAL_PALETTE).execute(
            &grid,
            config.svo_depth(),
            config.svo_world_size(),
        );
        extract_collision_boxes(&svo, origin_x, origin_z, config.voxel_scale)
    }

    #[test]
    fn a_block_backed_chunk_is_identical_to_the_streamed_one() {
        // The load-bearing property of bulk loading: planning a whole block
        // up front is allowed to change *when* the world was decided and
        // nothing whatsoever about *what it is*. If this fails, every
        // screenshot, save file and shared seed predating the change is void.
        let source =
            LocalChunkSource::new(SimpleNoiseProvider::new(), 42, GeneratorConfig::low_spec());
        for (ox, oz) in [(0.0, 30.0), (10.0, 10.0), (-40.0, 80.0)] {
            let from_block = source.load(ox, oz, 0, 0).collision;
            let streamed = streamed_collision(42, ox, oz, 0);
            assert_eq!(
                from_block, streamed,
                "chunk ({ox}, {oz}) differs when cut from block plans"
            );
        }
    }

    #[test]
    fn a_chunk_flush_against_a_block_edge_still_matches() {
        // The boundary is where this design dies if the block's halo ring is
        // too thin: the sampler asks about the region across the seam and
        // gets a different answer than the streaming path would have given.
        let source =
            LocalChunkSource::new(SimpleNoiseProvider::new(), 7, GeneratorConfig::low_spec());
        let chunk = GeneratorConfig::low_spec().chunk_size;
        for origin in [0.0, BLOCK_SIZE - chunk, BLOCK_SIZE, -chunk] {
            assert_eq!(
                source.load(origin, origin, 0, 0).collision,
                streamed_collision(7, origin, origin, 0),
                "chunk at the block seam ({origin}) differs"
            );
        }
    }

    #[test]
    fn a_block_serves_every_lod_of_the_same_chunk() {
        // Why the cache is not keyed by LOD. A region plan never reads
        // `voxel_scale`, so one block is the right answer for a coarse
        // distant chunk and the fine chunk that later replaces it — and they
        // must agree, or geometry would pop into a different building.
        let source =
            LocalChunkSource::new(SimpleNoiseProvider::new(), 42, GeneratorConfig::low_spec());
        for lod in [0u8, 1, 2] {
            assert_eq!(
                source.load(0.0, 30.0, 0, lod).collision,
                streamed_collision(42, 0.0, 30.0, lod),
                "lod {lod} differs when cut from block plans"
            );
        }
    }

    #[test]
    fn streaming_a_neighbourhood_plans_one_block_not_one_per_chunk() {
        // The economy the whole path exists for. Before this, every chunk
        // re-derived its own region plans.
        let source =
            LocalChunkSource::new(SimpleNoiseProvider::new(), 42, GeneratorConfig::low_spec());
        let chunk = GeneratorConfig::low_spec().chunk_size;
        for cz in 0..4 {
            for cx in 0..4 {
                source.load(cx as f32 * chunk, cz as f32 * chunk, 0, 0);
            }
        }
        assert_eq!(
            source.blocks_planned(),
            1,
            "16 chunks of one block should have planned it once"
        );
    }

    #[test]
    fn the_block_underfoot_is_asked_for_before_any_neighbour() {
        // A cold source standing anywhere must name exactly the block it is
        // standing in. Preloading a neighbour first would spend most of a
        // second planning somewhere the player is not.
        let source =
            LocalChunkSource::new(SimpleNoiseProvider::new(), 42, GeneratorConfig::low_spec());
        let corner = Position::new(BLOCK_SIZE - 4.0, BLOCK_SIZE - 4.0);
        assert_eq!(
            source.preload_targets(corner, 0),
            vec![BlockCoord::of(corner)]
        );

        source.ensure_block(0, BlockCoord::of(corner), &mut |_, _| {});
        let targets = source.preload_targets(corner, 0);
        assert!(
            !targets.contains(&BlockCoord::of(corner)),
            "a planned block was asked for again"
        );
        assert!(
            targets.contains(&BlockCoord { x: 1, z: 1 }),
            "the diagonal a corner-bound player arrives in was not requested: {targets:?}"
        );
    }

    #[test]
    fn the_interior_of_a_planned_block_asks_for_no_background_work() {
        let source =
            LocalChunkSource::new(SimpleNoiseProvider::new(), 42, GeneratorConfig::low_spec());
        let middle = Position::new(BLOCK_SIZE * 0.5, BLOCK_SIZE * 0.5);
        source.ensure_block(0, BlockCoord::of(middle), &mut |_, _| {});
        assert!(source.preload_targets(middle, 0).is_empty());
    }

    #[test]
    fn wall_leaf_becomes_world_space_box_and_floor_does_not() {
        let mut grid = VoxelGrid::new(4, 4, 4);
        grid.set(1, 0, 2, VOXEL_WALL);
        grid.set(0, 0, 0, VOXEL_FLOOR);
        let svo = BuildOctreeUseCase::new(&DEFAULT_MATERIAL_PALETTE).execute(&grid, 2, 2.0);

        let boxes = extract_collision_boxes(&svo, 10.0, 20.0, 0.5);
        assert_eq!(boxes.len(), 1);
        let b = &boxes[0];
        assert_eq!(b.min, [10.5, 0.0, 21.0]);
        assert_eq!(b.max, [11.0, 0.5, 21.5]);
    }

    #[test]
    fn generated_chunk_produces_row_padded_nodes_and_some_collision() {
        let source =
            LocalChunkSource::new(SimpleNoiseProvider::new(), 42, GeneratorConfig::low_spec());
        let payload = source.load(10.0, 10.0, 0, 0);
        // Row padding: node stream is a whole number of 1024-texel rows.
        assert_eq!(payload.nodes.len() % (1024 * 4), 0);
        assert!(payload.world_size > 0.0);
        assert!(
            !payload.collision.is_empty(),
            "a maze chunk must have walls"
        );
    }

    #[test]
    fn generated_ceiling_panels_reach_the_runtime_light_contract() {
        let source =
            LocalChunkSource::new(SimpleNoiseProvider::new(), 42, GeneratorConfig::low_spec());
        // Seed 42's main-spine chunk contains the fixtures visible from the
        // deterministic browser spawn used by the GPU regression tests.
        let payload = source.load(0.0, 30.0, 0, 0);

        assert!(
            !payload.lights.is_empty(),
            "voxelized emissive panels must not disappear before frame lighting"
        );
        for light in &payload.lights {
            assert!(light.enabled);
            assert!(light.position[1] > payload.voxel_size);
            assert!(light.half_size.into_iter().all(|extent| extent > 0.0));
            assert!(
                light.intensity > 1.0,
                "panel radiance was normalized to darkness"
            );
            assert_ne!(light.kind, crate::application::ports::LightKind::Point);
        }
    }

    #[test]
    fn renderer_artifact_selection_omits_unconsumed_outputs() {
        let source =
            LocalChunkSource::new(SimpleNoiseProvider::new(), 42, GeneratorConfig::low_spec());
        let reality = RealitySnapshot::default();

        let surface =
            source.load_with_artifacts(0.0, 30.0, 0, 1, &reality, RenderArtifactNeeds::SURFACE);
        assert_eq!(surface.root, 0);
        assert!(surface.nodes.is_empty());
        assert!(!surface.surface.vertices.is_empty());
        assert!(!surface.surface.indices.is_empty());
        assert!(surface.surface.faces.instances.is_empty());
        assert!(!surface.lights.is_empty());
        assert!(!surface.collision.is_empty());

        let splat =
            source.load_with_artifacts(0.0, 30.0, 0, 1, &reality, RenderArtifactNeeds::SPLAT);
        assert_eq!(splat.root, 0);
        assert!(splat.nodes.is_empty());
        assert!(splat.surface.vertices.is_empty());
        assert!(splat.surface.indices.is_empty());
        assert!(!splat.surface.faces.instances.is_empty());
        assert!(!splat.lights.is_empty());
        assert!(!splat.collision.is_empty());

        let raymarch =
            source.load_with_artifacts(0.0, 30.0, 0, 1, &reality, RenderArtifactNeeds::RAYMARCH);
        assert!(!raymarch.nodes.is_empty());
        assert!(raymarch.surface.vertices.is_empty());
        assert!(raymarch.surface.indices.is_empty());
        assert!(raymarch.surface.faces.instances.is_empty());
        assert!(!raymarch.lights.is_empty());
        assert!(!raymarch.collision.is_empty());
    }

    #[test]
    fn coarse_lod_is_smaller_but_covers_the_same_world_cube() {
        let source =
            LocalChunkSource::new(SimpleNoiseProvider::new(), 42, GeneratorConfig::low_spec());
        let fine = source.load(10.0, 10.0, 0, 0);
        let coarse = source.load(10.0, 10.0, 0, 1);
        assert_eq!(
            coarse.world_size, fine.world_size,
            "LODs must be interchangeable to the renderer"
        );
        assert!(
            coarse.nodes.len() < fine.nodes.len(),
            "coarse SVO must be smaller: {} vs {}",
            coarse.nodes.len(),
            fine.nodes.len()
        );
        assert!(
            !coarse.collision.is_empty(),
            "coarse chunks still collide (they cover the pre-refinement window)"
        );
    }

    #[test]
    fn halo_crop_matches_direct_chunk_generation() {
        let noise = SimpleNoiseProvider::new();
        let config = GeneratorConfig::low_spec();
        let generator = GenerateChunkArchitectureUseCase::new(&noise);
        let direct = generator.execute(Position::new(10.0, -10.0), 42, config);
        let halo = generator.execute(
            Position::new(10.0 - config.voxel_scale, -10.0 - config.voxel_scale),
            42,
            GeneratorConfig {
                chunk_size: config.chunk_size + config.voxel_scale * 2.0,
                ..config
            },
        );
        let cropped = crop_lateral_halo(&halo, 1);
        assert_eq!(direct.width(), cropped.width());
        assert_eq!(direct.height(), cropped.height());
        assert_eq!(direct.depth(), cropped.depth());
        let mut matches = 0usize;
        let total = direct.width() * direct.height() * direct.depth();
        for z in 0..direct.depth() {
            for y in 0..direct.height() {
                for x in 0..direct.width() {
                    if direct.get(x, y, z) == cropped.get(x, y, z) {
                        matches += 1;
                    }
                    assert_eq!(
                        cropped.get_face_occlusion(x, y, z),
                        neighbor_occlusion_mask(&halo, x + 1, y, z + 1),
                    );
                }
            }
        }
        let match_ratio = matches as f32 / total as f32;
        assert!(
            match_ratio >= 0.999,
            "voxel geometry match ratio must be at least 99.9% (got {match_ratio})"
        );
    }

    #[test]
    fn halo_closes_chunk_boundary_faces_without_hiding_the_floor_top() {
        let mut halo = VoxelGrid::new(5, 3, 5);
        // Logical voxel (0, 1, 1), plus a real neighbor just beyond the
        // negative-X chunk boundary and a supporting voxel below it.
        halo.set(1, 1, 2, VOXEL_FLOOR);
        halo.set(0, 1, 2, VOXEL_WALL);
        halo.set(1, 0, 2, VOXEL_WALL);

        // Mirror the boundary condition at the other side. These halo cells
        // represent adjacent deterministic chunks even though their padded
        // SVO root cubes would overlap the current root in world space.
        halo.set(3, 1, 2, VOXEL_FLOOR);
        halo.set(4, 1, 2, VOXEL_WALL);

        let cropped = crop_lateral_halo(&halo, 1);
        let left = cropped.get_face_occlusion(0, 1, 1);
        assert_ne!(left & FACE_OCCLUDED_NEGATIVE_X, 0);
        assert_ne!(left & FACE_OCCLUDED_NEGATIVE_Y, 0);
        assert_eq!(left & FACE_OCCLUDED_POSITIVE_Y, 0);

        let right = cropped.get_face_occlusion(2, 1, 1);
        assert_ne!(right & FACE_OCCLUDED_POSITIVE_X, 0);
        assert_eq!(right & FACE_OCCLUDED_POSITIVE_Y, 0);
    }
}
