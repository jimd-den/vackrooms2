//! # Surface Rendering Diagnostic Scenarios
//!
//! Evaluates Exact vs LowSpec meshing efficiency across 6 standard benchmark scenarios:
//! 1. Long uniform hallway (same material wall/floor/ceiling).
//! 2. Uniform rectangular room.
//! 3. Room with varied baked light (tests merging across light variation).
//! 4. Material boundary room (tests material boundary preservation).
//! 5. Chunk boundary/halo test (tests halo face culling).
//! 6. Scene exceeding visible chunk budget (tests budget culling).

use vackrooms::adapters::material_palette::DEFAULT_MATERIAL_PALETTE;
use vackrooms::adapters::voxel_mapper::{FaceDirection, SurfaceMeshingPolicy, VoxelMapper};
use vackrooms::domain::entities::position::Position;
use vackrooms::domain::entities::surface_quality_profile::SurfaceQualityTier;
use vackrooms::domain::entities::voxel_grid::{VOXEL_FLOOR, VOXEL_WALL, VoxelGrid};
use vackrooms::use_cases::frame_quality_controller::{
    ChunkSurfaceCandidate, FrameQualityController,
};

#[test]
fn scenario_1_long_uniform_hallway() {
    // 20x3x3 hallway: walls along Z=0 and Z=2, floor at Y=0, ceiling at Y=2
    let mut grid = VoxelGrid::new(20, 3, 3);
    for x in 0..20 {
        grid.set(x, 0, 0, VOXEL_WALL);
        grid.set(x, 0, 1, VOXEL_WALL);
        grid.set(x, 0, 2, VOXEL_WALL); // floor

        grid.set(x, 2, 0, VOXEL_WALL);
        grid.set(x, 2, 1, VOXEL_WALL);
        grid.set(x, 2, 2, VOXEL_WALL); // ceiling

        grid.set(x, 1, 0, VOXEL_WALL); // left wall
        grid.set(x, 1, 2, VOXEL_WALL); // right wall
    }

    let exact_mapper =
        VoxelMapper::with_policy(1.0, &DEFAULT_MATERIAL_PALETTE, SurfaceMeshingPolicy::Exact);
    let low_mapper = VoxelMapper::with_policy(
        1.0,
        &DEFAULT_MATERIAL_PALETTE,
        SurfaceMeshingPolicy::LowSpec,
    );

    let exact_quads = exact_mapper.map_voxel_grid(&grid);
    let low_quads = low_mapper.map_voxel_grid(&grid);

    println!("[Diagnostic] Scenario 1 (Long Uniform Hallway):");
    println!(
        "  Exact quads: {}, LowSpec quads: {}",
        exact_quads.len(),
        low_quads.len()
    );
    println!(
        "  Exact triangles: {}, LowSpec triangles: {}",
        exact_quads.len() * 2,
        low_quads.len() * 2
    );

    assert!(
        low_quads.len() <= exact_quads.len(),
        "LowSpec mode must produce <= quads than Exact mode"
    );
}

#[test]
fn scenario_2_uniform_rectangular_room() {
    let mut grid = VoxelGrid::new(10, 1, 10);
    for x in 0..10 {
        for z in 0..10 {
            grid.set(x, 0, z, VOXEL_WALL);
        }
    }

    let exact_mapper =
        VoxelMapper::with_policy(1.0, &DEFAULT_MATERIAL_PALETTE, SurfaceMeshingPolicy::Exact);
    let low_mapper = VoxelMapper::with_policy(
        1.0,
        &DEFAULT_MATERIAL_PALETTE,
        SurfaceMeshingPolicy::LowSpec,
    );

    let exact_up = exact_mapper
        .map_voxel_grid(&grid)
        .into_iter()
        .filter(|q| q.dir == FaceDirection::Up)
        .count();
    let low_up = low_mapper
        .map_voxel_grid(&grid)
        .into_iter()
        .filter(|q| q.dir == FaceDirection::Up)
        .count();

    println!("[Diagnostic] Scenario 2 (Uniform Rectangular Room 10x10 Floor):");
    println!(
        "  Exact floor quads: {}, LowSpec floor quads: {}",
        exact_up, low_up
    );

    assert_eq!(
        low_up, 1,
        "Uniform 10x10 floor must merge into exactly 1 quad in LowSpec mode"
    );
}

#[test]
fn scenario_3_room_with_varied_baked_light() {
    let mut grid = VoxelGrid::new(10, 1, 10);
    for x in 0..10 {
        for z in 0..10 {
            grid.set(x, 0, z, VOXEL_WALL);
            grid.set_light(x, 0, z, ((x * 3 + z * 7) % 15) as u8); // Varying light values
        }
    }

    let exact_mapper =
        VoxelMapper::with_policy(1.0, &DEFAULT_MATERIAL_PALETTE, SurfaceMeshingPolicy::Exact);
    let low_mapper = VoxelMapper::with_policy(
        1.0,
        &DEFAULT_MATERIAL_PALETTE,
        SurfaceMeshingPolicy::LowSpec,
    );

    let exact_up = exact_mapper
        .map_voxel_grid(&grid)
        .into_iter()
        .filter(|q| q.dir == FaceDirection::Up)
        .count();
    let low_up = low_mapper
        .map_voxel_grid(&grid)
        .into_iter()
        .filter(|q| q.dir == FaceDirection::Up)
        .count();

    println!("[Diagnostic] Scenario 3 (Room with Varied Baked Light):");
    println!(
        "  Exact floor quads: {} (split by light), LowSpec floor quads: {} (merged across light)",
        exact_up, low_up
    );

    assert!(
        exact_up > 1,
        "Exact mode must split quads when baked light varies"
    );
    assert_eq!(
        low_up, 1,
        "LowSpec mode must merge quads despite light variation"
    );
}

#[test]
fn scenario_4_material_boundary_test_room() {
    let mut grid = VoxelGrid::new(4, 1, 4);
    for x in 0..4 {
        for z in 0..4 {
            let mat = if x < 2 { VOXEL_WALL } else { VOXEL_FLOOR };
            grid.set(x, 0, z, mat);
        }
    }

    let low_mapper = VoxelMapper::with_policy(
        1.0,
        &DEFAULT_MATERIAL_PALETTE,
        SurfaceMeshingPolicy::LowSpec,
    );
    let low_up = low_mapper
        .map_voxel_grid(&grid)
        .into_iter()
        .filter(|q| q.dir == FaceDirection::Up)
        .collect::<Vec<_>>();

    println!("[Diagnostic] Scenario 4 (Material Boundary Room):");
    println!(
        "  LowSpec floor quads: {} (material boundary preserved)",
        low_up.len()
    );

    assert_eq!(
        low_up.len(),
        2,
        "Material boundary must split into 2 quads even in LowSpec mode"
    );
}

#[test]
fn scenario_5_chunk_boundary_halo_test() {
    let mut grid = VoxelGrid::new(4, 1, 3);
    grid.set(2, 0, 1, VOXEL_WALL); // Interior block
    grid.set(3, 0, 1, VOXEL_WALL); // Halo block

    let low_mapper = VoxelMapper::with_policy(
        1.0,
        &DEFAULT_MATERIAL_PALETTE,
        SurfaceMeshingPolicy::LowSpec,
    );
    let quads = low_mapper.map_voxel_grid_with_padding(&grid, 1);

    assert!(
        !quads.iter().any(|q| q.dir == FaceDirection::East),
        "Halo block must suppress boundary face"
    );
    println!("[Diagnostic] Scenario 5 (Chunk Boundary Halo): Face correctly suppressed");
}

#[test]
fn scenario_6_budget_overflow_culling() {
    let controller = FrameQualityController::new(SurfaceQualityTier::UltraLow);
    let player = Position::new(0.0, 0.0);

    let candidates: Vec<_> = (0..50)
        .map(|i| ChunkSurfaceCandidate {
            chunk_x: i,
            chunk_z: 0,
            origin: Position::new((i * 20) as f32, 0.0),
            quad_count: 100,
        })
        .collect();

    let (accepted, telemetry) =
        controller.evaluate_and_cull_chunks(player, &candidates, "2026-07-29T20:56:00Z");

    println!("[Diagnostic] Scenario 6 (Budget Overflow):");
    println!("  Total candidate chunks: {}", telemetry.candidate_chunks);
    println!("  Accepted chunks: {}", telemetry.visible_surface_chunks);
    println!("  Budget culled chunks: {}", telemetry.budget_culled_chunks);
    println!("  Submitted triangles: {}", telemetry.submitted_triangles);

    assert_eq!(
        accepted.len(),
        16,
        "UltraLow budget must cap visible chunks to 16"
    );
    assert_eq!(telemetry.budget_culled_chunks, 34);
}
