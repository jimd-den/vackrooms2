//! Plan-view dump of region plans: the verification surface for
//! architecture work.
//!
//! A first-person screenshot cannot show whether a floor plan reads as a
//! plan -- it shows one wall at a time, in the dark. This renders the
//! region plan itself, so room placement, circulation and openings can be
//! judged as an architect would judge them.
//!
//! ```sh
//! cargo run --release --example plan_view
//! ```
//!
//! Writes `target/plan-views/region_<rx>_<rz>.svg` (plus PNG when
//! ImageMagick is installed) and prints the ASCII plan for each region.
//! Environment knobs: `SEED` (default 42), `REGIONS` (comma-separated
//! `rx:rz` pairs, default a 2x2 block at the origin), `ASCII_STEP`
//! (world units per character, default 1.0).

use std::fs;
use std::path::PathBuf;
use std::process::Command;

use vackrooms::adapters::blueprint_renderer::{
    BlueprintOptions, ChunkBounds, render_region_blueprint_svg,
};
use vackrooms::domain::entities::position::Position;
use vackrooms::frameworks_drivers::simple_noise::SimpleNoiseProvider;
use vackrooms::use_cases::generate_chunk::GeneratorConfig;
use vackrooms::use_cases::region_plan::{REGION_SIZE, debug_region_ascii, generate_region_plan};

fn main() {
    let seed: u32 = std::env::var("SEED")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(42);
    let step: f32 = std::env::var("ASCII_STEP")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1.0);
    let regions = requested_regions();

    let config = GeneratorConfig::low_spec();
    let noise = SimpleNoiseProvider::new();
    let out_dir = PathBuf::from("target/plan-views");
    fs::create_dir_all(&out_dir).expect("create plan-view directory");

    for (rx, rz) in regions {
        let origin = Position::new(rx as f32 * REGION_SIZE, rz as f32 * REGION_SIZE);
        let plan = generate_region_plan(seed, origin, REGION_SIZE, &config, &noise);

        println!("== region ({rx}, {rz}) seed {seed} ==");
        println!(
            "{} assemblies, {} corridors, {} anomalies",
            plan.assemblies.len(),
            plan.corridors.len(),
            plan.anomalies.len()
        );
        println!("{}", debug_region_ascii(&plan, step));

        let svg = render_region_blueprint_svg(
            &plan,
            &chunk_grid(origin, config.chunk_size),
            BlueprintOptions {
                pixels_per_unit: 12.0,
                show_chunk_grid: false,
                show_fixtures: true,
                show_structure: true,
                seed,
                level: 0,
                voxel_scale: config.voxel_scale,
            },
        );
        let stem = out_dir.join(format!("region_{rx}_{rz}"));
        let svg_path = stem.with_extension("svg");
        fs::write(&svg_path, svg).expect("write svg");
        rasterize(&svg_path);
        println!("wrote {}", svg_path.display());
    }
}

fn requested_regions() -> Vec<(i64, i64)> {
    match std::env::var("REGIONS") {
        Ok(list) => list
            .split(',')
            .filter_map(|entry| {
                let (a, b) = entry.trim().split_once(':')?;
                Some((a.trim().parse().ok()?, b.trim().parse().ok()?))
            })
            .collect(),
        Err(_) => vec![(0, 0), (1, 0), (0, 1), (1, 1)],
    }
}

/// Chunk outlines, so a plan can be read against the streaming grid.
fn chunk_grid(origin: Position, chunk_size: f32) -> Vec<ChunkBounds> {
    let per_side = (REGION_SIZE / chunk_size).round() as i64;
    let mut bounds = Vec::new();
    for iz in 0..per_side {
        for ix in 0..per_side {
            let min_x = origin.x + ix as f32 * chunk_size;
            let min_z = origin.z + iz as f32 * chunk_size;
            bounds.push(ChunkBounds {
                min_x,
                min_z,
                max_x: min_x + chunk_size,
                max_z: min_z + chunk_size,
                label: format!("{ix},{iz}"),
            });
        }
    }
    bounds
}

/// Best-effort PNG beside the SVG. Absent ImageMagick is not an error --
/// the SVG is the artifact, the PNG is only for viewing.
fn rasterize(svg_path: &std::path::Path) {
    let png = svg_path.with_extension("png");
    let _ = Command::new("magick")
        .arg(svg_path)
        .arg(&png)
        .status()
        .map(|status| {
            if status.success() {
                println!("wrote {}", png.display());
            }
        });
}
