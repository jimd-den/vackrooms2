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

/// The traversal the WebGL2 raymarcher actually runs, ported from
/// `drivers/shaders/trace_voxel_scene/intersect_voxel_scene.rs`.
///
/// `lookup_leaf` agreeing point-for-point is necessary but not sufficient:
/// the empty-space walk consumes the *bounds* a leaf reports, and bricking
/// changes those even where the material is identical -- a brick hands back
/// one voxel where the plain SVO hands back a whole collapsed octant. That
/// is the difference a step budget can turn into missing geometry, and it is
/// invisible to a per-point fetch comparison.
mod trace {
    use super::{Leaf, lookup_leaf};

    const TRACE_MIN_TIE_EPSILON: f32 = 1e-7;
    /// The shader's own fixed per-chunk step budget, in both walks.
    pub const MAX_STEPS: usize = 768;

    pub struct Chunk<'a> {
        pub nodes: &'a [u32],
        pub bricks: &'a [u32],
        pub root: usize,
        pub depth: u32,
        pub world_size: f32,
        pub voxel_size: f32,
    }

    impl Chunk<'_> {
        fn leaf(&self, point: [f32; 3]) -> Leaf {
            lookup_leaf(
                self.nodes,
                self.bricks,
                self.root,
                self.depth,
                self.world_size,
                point,
            )
        }
    }

    #[derive(Debug, Clone, Copy, PartialEq)]
    pub struct Hit {
        pub distance: f32,
        pub material: u32,
        /// Steps the walk consumed before it hit, missed, or ran out.
        pub steps: usize,
        /// True when the walk exhausted [`MAX_STEPS`] rather than finishing.
        pub exhausted: bool,
    }

    fn sample_bias(voxel_size: f32) -> f32 {
        (voxel_size * 1e-5).max(TRACE_MIN_TIE_EPSILON)
    }

    fn tie_tolerance(distance: f32) -> f32 {
        (distance.abs() * 1e-6).max(TRACE_MIN_TIE_EPSILON)
    }

    fn at(ro: [f32; 3], rd: [f32; 3], t: f32) -> [f32; 3] {
        [ro[0] + rd[0] * t, ro[1] + rd[1] * t, ro[2] + rd[2] * t]
    }

    fn exit_distance(ro: [f32; 3], rd: [f32; 3], min: [f32; 3], max: [f32; 3]) -> f32 {
        let mut nearest = f32::INFINITY;
        for a in 0..3 {
            if rd[a].abs() >= 1e-20 {
                let plane = if rd[a] > 0.0 { max[a] } else { min[a] };
                nearest = nearest.min((plane - ro[a]) / rd[a]);
            }
        }
        nearest
    }

    /// `traceChunkDda`: exact finest-cell stepping, the canonical resolver.
    pub fn dda(
        chunk: &Chunk<'_>,
        ro: [f32; 3],
        rd: [f32; 3],
        entry: f32,
        exit: f32,
    ) -> Option<Hit> {
        let bias = sample_bias(chunk.voxel_size);
        let mut distance = entry;
        let sample = at(ro, rd, (distance + bias).min(exit));
        let mut cell = [
            (sample[0] / chunk.voxel_size).floor(),
            (sample[1] / chunk.voxel_size).floor(),
            (sample[2] / chunk.voxel_size).floor(),
        ];
        let step = [rd[0].signum(), rd[1].signum(), rd[2].signum()];

        for taken in 0..MAX_STEPS {
            if distance > exit {
                return None;
            }
            let sample = at(ro, rd, (distance + bias).min(exit));
            let leaf = chunk.leaf(sample);
            if leaf.material != 0 {
                return Some(Hit {
                    distance,
                    material: leaf.material,
                    steps: taken,
                    exhausted: false,
                });
            }

            let mut next_times = [f32::INFINITY; 3];
            for a in 0..3 {
                if rd[a].abs() >= 1e-20 {
                    let boundary = (cell[a] + step[a].max(0.0)) * chunk.voxel_size;
                    next_times[a] = (boundary - ro[a]) / rd[a];
                }
            }
            let next = next_times[0].min(next_times[1]).min(next_times[2]);
            if next > exit {
                return None;
            }
            let epsilon = tie_tolerance(next);
            for a in 0..3 {
                if (next_times[a] - next).abs() <= epsilon {
                    cell[a] += step[a];
                }
            }
            distance = next;
        }
        Some(Hit {
            distance,
            material: 0,
            steps: MAX_STEPS,
            exhausted: true,
        })
    }

    /// `traceChunkSkippingEmptyLeaves`: the coarse walk, which proves
    /// emptiness only and hands any hit to [`dda`] to resolve exactly.
    pub fn skipping(
        chunk: &Chunk<'_>,
        ro: [f32; 3],
        rd: [f32; 3],
        entry: f32,
        exit: f32,
    ) -> Option<Hit> {
        let bias = sample_bias(chunk.voxel_size);
        let mut distance = entry;
        let mut refinement_entry = entry;

        for taken in 0..MAX_STEPS {
            if distance > exit {
                return None;
            }
            let sample = at(ro, rd, (distance + bias).min(exit));
            let leaf = chunk.leaf(sample);
            if leaf.material != 0 {
                return dda(chunk, ro, rd, refinement_entry, exit).map(|hit| Hit {
                    steps: taken + hit.steps,
                    ..hit
                });
            }

            let mut next = exit_distance(ro, rd, leaf.bounds_min, leaf.bounds_max);
            if next <= distance + bias * 0.25 {
                next = distance + bias;
            }
            refinement_entry = distance.max(next - chunk.voxel_size);
            distance = next;
        }
        Some(Hit {
            distance,
            material: 0,
            steps: MAX_STEPS,
            exhausted: true,
        })
    }
}

/// Rays fanned across the chunk from an interior eye, in chunk-local space.
fn fan(world_size: f32) -> Vec<([f32; 3], [f32; 3])> {
    let eye = [world_size * 0.5, world_size * 0.25, world_size * 0.5];
    let mut rays = Vec::new();
    for yaw_step in 0..48 {
        let yaw = yaw_step as f32 / 48.0 * std::f32::consts::TAU;
        for pitch_step in -6..=6 {
            let pitch = pitch_step as f32 / 6.0 * 0.9;
            let direction = [
                yaw.cos() * pitch.cos(),
                pitch.sin(),
                yaw.sin() * pitch.cos(),
            ];
            rays.push((eye, direction));
        }
    }
    rays
}

/// The load-bearing traversal test: a ray must find the same surface at the
/// same distance whichever encoding it walks, and must not need more of the
/// shader's fixed step budget to do it.
///
/// Exhausting that budget is the failure mode worth naming: the walk then
/// reports *no hit at all*, so a wall silently becomes sky. That reads on
/// screen as a fast renderer drawing almost nothing, which is exactly the
/// shape of a bricking regression.
#[test]
fn a_ray_finds_the_same_surface_through_bricks_as_through_the_plain_svo() {
    let config = GeneratorConfig::low_spec();
    let source = LocalChunkSource::new(SimpleNoiseProvider::new(), 42, config);
    let reality = RealitySnapshot::empty();
    let origins = [(0.0f32, 0.0f32)];

    let bricked =
        source.load_with_artifacts(0.0, 0.0, 0, 0, &reality, RenderArtifactNeeds::BRICKS);
    let plain = source.load_with_artifacts(0.0, 0.0, 0, 0, &reality, RenderArtifactNeeds::RAYMARCH);
    let brick_atlas = build_atlas(std::slice::from_ref(&bricked), &origins);
    let plain_atlas = build_atlas(std::slice::from_ref(&plain), &origins);
    let world_size = config.svo_world_size();

    let brick_chunk = trace::Chunk {
        nodes: &brick_atlas.nodes,
        bricks: &brick_atlas.bricks,
        root: brick_atlas.roots[0],
        depth: bricked.svo_depth as u32,
        world_size,
        voxel_size: config.voxel_scale,
    };
    let plain_chunk = trace::Chunk {
        nodes: &plain_atlas.nodes,
        bricks: &plain_atlas.bricks,
        root: plain_atlas.roots[0],
        depth: plain.svo_depth as u32,
        world_size,
        voxel_size: config.voxel_scale,
    };

    let mut hits = 0usize;
    let mut brick_exhausted = 0usize;
    let mut plain_exhausted = 0usize;
    let mut worst_step_ratio = 0.0f32;
    let mut disagreements = Vec::new();

    for (ro, rd) in fan(world_size) {
        // `traceChunk` is only ever handed the ray's span *inside* the chunk
        // box, so the walk terminates at the far wall rather than marching
        // through unbounded air outside the atlas.
        let mut exit = f32::INFINITY;
        for a in 0..3 {
            if rd[a].abs() >= 1e-20 {
                let plane = if rd[a] > 0.0 { world_size } else { 0.0 };
                exit = exit.min((plane - ro[a]) / rd[a]);
            }
        }
        let from_bricks = trace::skipping(&brick_chunk, ro, rd, 0.0, exit);
        let from_svo = trace::skipping(&plain_chunk, ro, rd, 0.0, exit);

        if from_bricks.is_some_and(|h| h.exhausted) {
            brick_exhausted += 1;
        }
        if from_svo.is_some_and(|h| h.exhausted) {
            plain_exhausted += 1;
        }
        if let (Some(b), Some(p)) = (from_bricks, from_svo) {
            if !b.exhausted && !p.exhausted {
                hits += 1;
                if b.material != p.material || (b.distance - p.distance).abs() > config.voxel_scale
                {
                    disagreements.push((ro, rd, b, p));
                }
                worst_step_ratio = worst_step_ratio.max(b.steps as f32 / p.steps.max(1) as f32);
            }
        } else if from_bricks.is_some() != from_svo.is_some() {
            disagreements.push((
                ro,
                rd,
                from_bricks.unwrap_or(trace::Hit {
                    distance: 0.0,
                    material: 0,
                    steps: 0,
                    exhausted: false,
                }),
                from_svo.unwrap_or(trace::Hit {
                    distance: 0.0,
                    material: 0,
                    steps: 0,
                    exhausted: false,
                }),
            ));
        }
    }

    assert!(
        hits > 100,
        "sanity: the fan must actually strike geometry, got {hits}"
    );
    assert!(
        disagreements.is_empty(),
        "{} of the fan's rays resolved differently through bricks; first: {:?}",
        disagreements.len(),
        disagreements.first()
    );
    // Some rays exhaust the budget in *both* encodings -- grazing, nearly
    // axis-parallel ones that inch along a wall. That is a pre-existing
    // property of the walk, not a bricking regression, so what is pinned
    // here is that bricking does not make it worse.
    assert!(
        brick_exhausted <= plain_exhausted,
        "bricking exhausted the {}-step budget on more rays than the plain SVO \
         ({brick_exhausted} vs {plain_exhausted}): geometry those rays cross \
         renders as sky",
        trace::MAX_STEPS
    );
    assert!(
        worst_step_ratio <= 4.0,
        "bricking cost {worst_step_ratio:.1}x the steps on some ray; the walk \
         has a fixed budget, so a large multiplier turns into missing geometry"
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
