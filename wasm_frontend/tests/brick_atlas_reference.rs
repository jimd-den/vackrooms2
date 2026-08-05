//! Native reference for the brick-aware node fetch in
//! `webgpu/shaders/raymarch.wgsl::lookup_leaf`, run over a real pooled
//! multi-chunk atlas produced by `LocalChunkSource` and `AtlasPool`.
//!
//! Bricking changes how a voxel is *reached*, never which voxel is found.
//! That is the property worth testing and it is testable without a GPU: the
//! same chunk is generated twice, once as a plain SVO and once bricked, and
//! the two fetches must agree at every sample point. A disagreement is a
//! decoding or rebasing bug; the traversal above `lookup_leaf` is unchanged
//! and stays covered by `raymarch_reference.rs`.

use vackrooms::frameworks_drivers::simple_noise::SimpleNoiseProvider;
use vackrooms::use_cases::generate_chunk::GeneratorConfig;

use wasm_frontend::adapters::local_chunk_source::LocalChunkSource;
use wasm_frontend::application::atlas::{AtlasPool, payload_brick_rows, payload_rows};
use wasm_frontend::application::ports::{ChunkPayload, ChunkSourcePort, RenderArtifactNeeds};
use wasm_frontend::application::streaming::chunk_key;

use vackrooms::domain::entities::anomaly::RealitySnapshot;

const NODE_KIND_LEAF: u32 = 1;
const NODE_KIND_BRICK: u32 = 2;
const BRICK_EDGE: u32 = 4;

/// What `lookup_leaf` returns: the voxel plus the box the traversal above
/// will skip across if it is empty.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Leaf {
    material: u32,
    color: u32,
    light_word: u32,
    bounds_min: [f32; 3],
    bounds_max: [f32; 3],
}

/// 1:1 port of `raymarch.wgsl::lookup_leaf`, including the brick case.
fn lookup_leaf(
    nodes: &[u32],
    bricks: &[u32],
    root: usize,
    depth: u32,
    world_size: f32,
    point: [f32; 3],
) -> Leaf {
    let mut node_index = root;
    let mut bounds_min = [0.0f32; 3];
    let mut bounds_max = [world_size; 3];

    for level in 0..=10u32 {
        let word = node_index * 4;
        let node_type = nodes[word];
        let payload = nodes[word + 1];
        let color_or_mask = nodes[word + 2];
        let light_word = nodes[word + 3];

        if node_type == NODE_KIND_LEAF || level >= depth {
            return Leaf {
                material: payload,
                color: color_or_mask,
                light_word,
                bounds_min,
                bounds_max,
            };
        }
        if node_type == NODE_KIND_BRICK {
            return read_brick_voxel(bricks, payload, point, bounds_min, bounds_max);
        }

        let mut center = [0.0f32; 3];
        for a in 0..3 {
            center[a] = (bounds_min[a] + bounds_max[a]) * 0.5;
        }
        let cx = (point[0] >= center[0]) as u32;
        let cy = (point[1] >= center[1]) as u32;
        let cz = (point[2] >= center[2]) as u32;
        let child = (cz << 2) | (cy << 1) | cx;

        let mut child_min = bounds_min;
        let mut child_max = bounds_max;
        for a in 0..3 {
            if point[a] >= center[a] {
                child_min[a] = center[a];
            } else {
                child_max[a] = center[a];
            }
        }

        if color_or_mask & (1 << child) == 0 {
            return Leaf {
                material: 0,
                color: 0,
                light_word: 0,
                bounds_min: child_min,
                bounds_max: child_max,
            };
        }
        node_index = (payload + child) as usize;
        bounds_min = child_min;
        bounds_max = child_max;
    }

    Leaf {
        material: 0,
        color: 0,
        light_word: 0,
        bounds_min,
        bounds_max,
    }
}

fn read_brick_voxel(
    bricks: &[u32],
    word_base: u32,
    point: [f32; 3],
    bounds_min: [f32; 3],
    bounds_max: [f32; 3],
) -> Leaf {
    let limit = BRICK_EDGE as f32 - 1.0;
    let mut extent = [0.0f32; 3];
    let mut local = [0.0f32; 3];
    for a in 0..3 {
        extent[a] = (bounds_max[a] - bounds_min[a]) / BRICK_EDGE as f32;
        local[a] = ((point[a] - bounds_min[a]) / extent[a])
            .floor()
            .clamp(0.0, limit);
    }
    let offset =
        local[2] as u32 * BRICK_EDGE * BRICK_EDGE + local[1] as u32 * BRICK_EDGE + local[0] as u32;
    let word = (word_base + offset * 2) as usize;
    let packed_voxel = bricks[word];
    let packed_light = bricks[word + 1];

    let red = (packed_light >> 8) & 0xF;
    let green = (packed_light >> 12) & 0xF;
    let blue = (packed_light >> 16) & 0xF;
    let light_word = red.max(green).max(blue)
        | ((packed_light & 0xFF) << 8)
        | (red << 16)
        | (green << 20)
        | (blue << 24);

    let mut voxel_min = [0.0f32; 3];
    let mut voxel_max = [0.0f32; 3];
    for a in 0..3 {
        voxel_min[a] = bounds_min[a] + local[a] * extent[a];
        voxel_max[a] = voxel_min[a] + extent[a];
    }
    Leaf {
        material: packed_voxel >> 24,
        color: packed_voxel & 0x00FF_FFFF,
        light_word,
        bounds_min: voxel_min,
        bounds_max: voxel_max,
    }
}

struct Atlas {
    nodes: Vec<u32>,
    bricks: Vec<u32>,
    /// Per chunk: pooled root node index.
    roots: Vec<usize>,
}

/// Merges payloads through the engine's real pool, so pointer rebasing is
/// exercised rather than assumed.
fn build_atlas(payloads: &[ChunkPayload], origins: &[(f32, f32)]) -> Atlas {
    let mut pool = AtlasPool::new();
    let rows = payloads.iter().map(payload_rows).max().unwrap_or(1).max(1);
    let brick_rows = payloads.iter().map(payload_brick_rows).max().unwrap_or(0);
    pool.ensure_layout_with_bricks(payloads.len(), rows, brick_rows);

    let mut roots = Vec::new();
    for (payload, origin) in payloads.iter().zip(origins) {
        let key = chunk_key(origin.0, origin.1);
        pool.assign(key).expect("pool sized for all chunks");
        roots.push(pool.node_offset_of(key).unwrap() + payload.root as usize);
    }
    let find = |k| {
        origins
            .iter()
            .position(|o| chunk_key(o.0, o.1) == k)
            .map(|i| &payloads[i])
    };
    Atlas {
        nodes: pool.full_texels(find),
        bricks: pool.full_brick_words(find),
        roots,
    }
}

fn sample_points(config: &GeneratorConfig) -> Vec<[f32; 3]> {
    let scale = config.voxel_scale;
    let extent = 1u32 << config.svo_depth();
    let mut points = Vec::new();
    // Voxel centres, strided to keep the sweep to a few tens of thousands
    // of samples while still landing in every brick.
    let stride = 3;
    for x in (0..extent).step_by(stride) {
        for y in (0..extent.min(32)).step_by(stride) {
            for z in (0..extent).step_by(stride) {
                points.push([
                    (x as f32 + 0.5) * scale,
                    (y as f32 + 0.5) * scale,
                    (z as f32 + 0.5) * scale,
                ]);
            }
        }
    }
    points
}

/// The load-bearing test: bricked and plain fetches must agree everywhere.
#[test]
fn a_bricked_atlas_returns_the_same_voxel_as_the_plain_one() {
    let seed = 42;
    let config = GeneratorConfig::low_spec();
    let source = LocalChunkSource::new(SimpleNoiseProvider::new(), seed, config);
    let reality = RealitySnapshot::empty();

    // Several chunks so at least one lands in a non-zero slot and proves
    // that both node and brick pointers were rebased.
    let origins: Vec<(f32, f32)> = (-1..=1)
        .flat_map(|dz| (-1..=1).map(move |dx| (dx as f32 * 10.0, dz as f32 * 10.0)))
        .collect();

    let bricked: Vec<_> = origins
        .iter()
        .map(|o| source.load_with_artifacts(o.0, o.1, 0, 0, &reality, RenderArtifactNeeds::BRICKS))
        .collect();
    let plain: Vec<_> = origins
        .iter()
        .map(|o| {
            source.load_with_artifacts(o.0, o.1, 0, 0, &reality, RenderArtifactNeeds::RAYMARCH)
        })
        .collect();

    assert!(
        bricked.iter().all(|p| !p.brick_voxels.is_empty()),
        "the brick artifact must actually carry a voxel arena"
    );
    assert!(
        plain.iter().all(|p| p.brick_voxels.is_empty()),
        "the SVO artifact must not pay for an arena it cannot address"
    );

    let brick_atlas = build_atlas(&bricked, &origins);
    let plain_atlas = build_atlas(&plain, &origins);
    let points = sample_points(&config);
    let world_size = config.svo_world_size();

    let mut solid_samples = 0usize;
    for (chunk, payload) in bricked.iter().enumerate() {
        for point in &points {
            let from_bricks = lookup_leaf(
                &brick_atlas.nodes,
                &brick_atlas.bricks,
                brick_atlas.roots[chunk],
                payload.svo_depth as u32,
                world_size,
                *point,
            );
            let from_svo = lookup_leaf(
                &plain_atlas.nodes,
                &plain_atlas.bricks,
                plain_atlas.roots[chunk],
                plain[chunk].svo_depth as u32,
                world_size,
                *point,
            );
            if from_svo.material != 0 {
                solid_samples += 1;
            }
            assert_eq!(
                (
                    from_bricks.material,
                    from_bricks.color,
                    from_bricks.light_word
                ),
                (from_svo.material, from_svo.color, from_svo.light_word),
                "chunk {chunk} at {point:?}"
            );
        }
    }

    assert!(
        solid_samples > 1000,
        "sanity: the sweep must land inside real geometry, got {solid_samples}"
    );
}

/// A brick node must hand back the individual voxel's box, never the
/// brick's. Empty-space skipping advances a ray to the far side of whatever
/// bounds it receives, so returning the brick would step over the solid
/// voxels sharing it -- geometry would vanish in 4-voxel bites.
#[test]
fn a_brick_reports_voxel_sized_bounds() {
    let config = GeneratorConfig::low_spec();
    let source = LocalChunkSource::new(SimpleNoiseProvider::new(), 42, config);
    let payload = source.load_with_artifacts(
        0.0,
        0.0,
        0,
        0,
        &RealitySnapshot::empty(),
        RenderArtifactNeeds::BRICKS,
    );
    let atlas = build_atlas(std::slice::from_ref(&payload), &[(0.0, 0.0)]);

    let scale = config.voxel_scale;
    let mut brick_leaves = 0usize;
    for point in sample_points(&config) {
        let leaf = lookup_leaf(
            &atlas.nodes,
            &atlas.bricks,
            atlas.roots[0],
            payload.svo_depth as u32,
            config.svo_world_size(),
            point,
        );
        let size = leaf.bounds_max[0] - leaf.bounds_min[0];
        // Only voxel-sized boxes can have come from a brick; uniform nodes
        // legitimately report larger ones.
        if (size - scale).abs() < scale * 1e-3 {
            brick_leaves += 1;
            for a in 0..3 {
                assert!(
                    point[a] >= leaf.bounds_min[a] - 1e-4 && point[a] <= leaf.bounds_max[a] + 1e-4,
                    "sample {point:?} fell outside the box it was resolved to: {leaf:?}"
                );
            }
        }
    }
    assert!(
        brick_leaves > 100,
        "sanity: the sweep must reach bricks, got {brick_leaves}"
    );
}

/// Records what bricking actually buys against the path it replaces, which
/// is not what the brick pool's own doc table suggests.
///
/// That table compared bricks to a *raw* SVO and found them smaller. The
/// raymarch path never shipped a raw SVO: it ships an SVDAG, where every
/// identical subtree in a repetitive world collapses to one shared block --
/// and Level 0 is nothing but repeated wall, floor and ceiling. Measured on
/// the spawn chunk (seed 42, low_spec):
///
/// | encoding | hierarchy | arena  | total  |
/// |----------|-----------|--------|--------|
/// | SVDAG    |    20480w |      0 | 20480w |
/// | bricked  |     6144w | 57344w | 63488w |
///
/// So bricking cuts the pointer structure by 3.3x and costs 3.1x the total
/// memory. That is the honest trade: fewer dependent fetches per ray at the
/// bottom of the tree, paid for in bandwidth, because a dense brick stores
/// its air explicitly and cannot be deduplicated the way a DAG node can.
/// This test pins both halves so neither can regress unnoticed.
#[test]
fn bricking_trades_memory_for_a_shallower_walk() {
    let config = GeneratorConfig::low_spec();
    let source = LocalChunkSource::new(SimpleNoiseProvider::new(), 42, config);
    let reality = RealitySnapshot::empty();

    let bricked = source.load_with_artifacts(0.0, 0.0, 0, 0, &reality, RenderArtifactNeeds::BRICKS);
    let plain = source.load_with_artifacts(0.0, 0.0, 0, 0, &reality, RenderArtifactNeeds::RAYMARCH);

    assert!(
        bricked.nodes.len() * 3 < plain.nodes.len(),
        "the hierarchy must shrink by at least 3x: {} words against {}",
        bricked.nodes.len(),
        plain.nodes.len()
    );

    let bricked_total = bricked.nodes.len() + bricked.brick_voxels.len();
    assert!(
        bricked_total < plain.nodes.len() * 4,
        "the arena must not run away: {bricked_total} words ({} hierarchy \
         + {} arena) against the SVDAG's {}",
        bricked.nodes.len(),
        bricked.brick_voxels.len(),
        plain.nodes.len()
    );
}
