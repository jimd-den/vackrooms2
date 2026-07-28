//! Size-parameterized benchmarks for the core-crate generation stages
//! flagged as complexity risks: octree construction and region planning.
//!
//! `cargo bench` (from the workspace root) or `cargo bench --bench generation`.
//!
//! - `octree_build/*`: `BuildOctreeUseCase::execute` on synthetic voxel grids
//!   of increasing edge length, in two patterns that bracket its complexity —
//!   `hollow_room` (four walls + floor/ceiling, uniform air interior: the
//!   octree collapses large uniform regions into single nodes) and
//!   `checkerboard` (alternating solid/air every voxel: no region is ever
//!   uniform, so every possible node must be visited and emitted — the
//!   adversarial worst case). Comparing their growth curves separates "cost
//!   scales with occupied/boundary voxels" from "cost scales with the full
//!   8^depth node space".
//! - `octree_direct_vs_dense/*`: the same two grid patterns, but comparing
//!   `BuildOctreeUseCase` (always fully descends, then collapses uniform
//!   blocks after the fact) against `build_octree_direct` + `GridSampler`
//!   (checks `uniform_hint` before descending). `GridSampler`'s hint only
//!   proves *out-of-grid* cubes uniform, so for grids with no halo margin
//!   the two should land at parity — this benchmark is what tells us
//!   whether that expectation holds instead of assuming it.
//! - `svdag_compress/*`: `compress_svdag` on the same grids, reporting the
//!   cost of dedup plus (via the printed node counts) the GPU upload size
//!   win — the number that actually matters for the Pi target.
//! - `region_plan/*`: `generate_region_plan` directly, sweeping the pillar
//!   density tuning knob — a proxy for assembly/spine count, since
//!   `place_suite`'s candidate search is quadratic in assemblies-per-region.

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use vackrooms::domain::entities::voxel_grid::{VOXEL_AIR, VOXEL_WALL, VoxelGrid};
use vackrooms::adapters::material_palette::DEFAULT_MATERIAL_PALETTE;
use vackrooms::use_cases::build_octree::BuildOctreeUseCase;
use vackrooms::use_cases::build_octree_direct::{GridSampler, build_octree_direct};
use vackrooms::use_cases::compress_svdag::compress_svdag;
use vackrooms::domain::entities::position::Position;
use vackrooms::frameworks_drivers::simple_noise::SimpleNoiseProvider;
use vackrooms::use_cases::generate_chunk::{GenerateChunkArchitectureUseCase, GeneratorConfig};
use vackrooms::use_cases::region_plan::{REGION_SIZE, generate_region_plan};

const VOXEL_SCALE: f32 = 0.2;

fn hollow_room(edge: usize) -> VoxelGrid {
    let mut grid = VoxelGrid::new(edge, edge, edge);
    for z in 0..edge {
        for y in 0..edge {
            for x in 0..edge {
                let on_boundary =
                    x == 0 || x == edge - 1 || y == 0 || y == edge - 1 || z == 0 || z == edge - 1;
                grid.set(x, y, z, if on_boundary { VOXEL_WALL } else { VOXEL_AIR });
            }
        }
    }
    grid
}

fn checkerboard(edge: usize) -> VoxelGrid {
    let mut grid = VoxelGrid::new(edge, edge, edge);
    for z in 0..edge {
        for y in 0..edge {
            for x in 0..edge {
                let solid = (x + y + z) % 2 == 0;
                grid.set(x, y, z, if solid { VOXEL_WALL } else { VOXEL_AIR });
            }
        }
    }
    grid
}

fn octree_build(c: &mut Criterion) {
    let mut group = c.benchmark_group("octree_build");
    for edge in [16usize, 32, 48, 64, 96] {
        let depth = (edge as u32).next_power_of_two().trailing_zeros();
        let world_size = (1u32 << depth) as f32 * VOXEL_SCALE;

        let room = hollow_room(edge);
        group.bench_with_input(BenchmarkId::new("hollow_room", edge), &room, |b, grid| {
            b.iter(|| {
                BuildOctreeUseCase::new(&DEFAULT_MATERIAL_PALETTE)
                    .execute(grid, depth, world_size)
            });
        });

        let checker = checkerboard(edge);
        group.bench_with_input(
            BenchmarkId::new("checkerboard", edge),
            &checker,
            |b, grid| {
                b.iter(|| {
                    BuildOctreeUseCase::new(&DEFAULT_MATERIAL_PALETTE)
                        .execute(grid, depth, world_size)
                });
            },
        );
    }
    group.finish();
}

/// A real generated Level 0 chunk, matching the scenario the SVDAG
/// regression test (`a_real_level_zero_chunk_compresses`) pins.
fn real_level_zero_chunk() -> VoxelGrid {
    let noise = SimpleNoiseProvider::new();
    GenerateChunkArchitectureUseCase::new(&noise)
        .execute(
            Position::new(20.0, 20.0),
            42,
            GeneratorConfig::low_spec(),
        )
        .grid
}

fn octree_direct_vs_dense(c: &mut Criterion) {
    let mut group = c.benchmark_group("octree_direct_vs_dense");
    for edge in [16usize, 32, 48, 64, 96] {
        let depth = (edge as u32).next_power_of_two().trailing_zeros();
        let world_size = (1u32 << depth) as f32 * VOXEL_SCALE;

        for (label, grid) in [("hollow_room", hollow_room(edge)), ("checkerboard", checkerboard(edge))] {
            group.bench_with_input(BenchmarkId::new(format!("{label}/dense"), edge), &grid, |b, grid| {
                b.iter(|| {
                    BuildOctreeUseCase::new(&DEFAULT_MATERIAL_PALETTE)
                        .execute(grid, depth, world_size)
                });
            });
            group.bench_with_input(BenchmarkId::new(format!("{label}/direct"), edge), &grid, |b, grid| {
                let sampler = GridSampler {
                    grid,
                    palette: &DEFAULT_MATERIAL_PALETTE,
                };
                b.iter(|| build_octree_direct(&sampler, depth, world_size));
            });
        }
    }
    group.finish();
}

fn svdag_compress(c: &mut Criterion) {
    let mut group = c.benchmark_group("svdag_compress");

    for (label, grid, edge) in [
        ("hollow_room_64", hollow_room(64), 64usize),
        ("checkerboard_64", checkerboard(64), 64),
    ] {
        let depth = (edge as u32).next_power_of_two().trailing_zeros();
        let world_size = (1u32 << depth) as f32 * VOXEL_SCALE;
        let svo = BuildOctreeUseCase::new(&DEFAULT_MATERIAL_PALETTE).execute(&grid, depth, world_size);
        group.bench_with_input(BenchmarkId::new("compress", label), &svo, |b, svo| {
            b.iter(|| compress_svdag(svo));
        });
    }

    let real_grid = real_level_zero_chunk();
    let real_svo = BuildOctreeUseCase::new(&DEFAULT_MATERIAL_PALETTE).execute(&real_grid, 6, 12.8);
    let (_, stats) = compress_svdag(&real_svo);
    eprintln!(
        "svdag_compress/real_level_zero_chunk: {} -> {} nodes ({:.1}%)",
        stats.nodes_before,
        stats.nodes_after,
        100.0 * stats.nodes_after as f64 / stats.nodes_before as f64
    );
    group.bench_with_input(
        BenchmarkId::new("compress", "real_level_zero_chunk"),
        &real_svo,
        |b, svo| {
            b.iter(|| compress_svdag(svo));
        },
    );

    group.finish();
}

fn region_plan_by_density(c: &mut Criterion) {
    let mut group = c.benchmark_group("region_plan");
    let noise = SimpleNoiseProvider::new();
    for density in [0.5f32, 1.0, 2.0, 4.0] {
        let mut config = GeneratorConfig::low_spec();
        config.tuning.pillars = density;
        config.tuning.walls = density;
        config.tuning.atria = density;
        group.bench_with_input(
            BenchmarkId::new("pillars_walls_atria_x", density),
            &config,
            |b, config| {
                b.iter(|| {
                    generate_region_plan(
                        42,
                        Position::new(0.0, 0.0),
                        REGION_SIZE,
                        config,
                        &noise,
                    )
                });
            },
        );
    }
    group.finish();
}

criterion_group! {
    name = benches;
    // The checkerboard worst case at the larger edge sizes is slow per
    // iteration; cap sample count so the suite finishes in a reasonable
    // window rather than criterion's default auto-extending to 100 samples.
    config = Criterion::default().sample_size(20);
    targets = octree_build, octree_direct_vs_dense, svdag_compress, region_plan_by_density
}
criterion_main!(benches);
