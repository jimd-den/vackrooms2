//! Horizontal-slice dump of generated voxels: the verification surface for
//! *what actually got built*.
//!
//! `plan_view` draws the region plan — the architect's intent, before the
//! fabric, the anomalies and the galleries have had their say. This renders
//! the other end of the pipeline: real chunks, generated exactly as the
//! engine generates them, cut at one height and looked at from above.
//!
//! That distinction is the whole point. A plan can read beautifully while
//! the built world is a field of orphan posts, because posts come from
//! samplers the plan never mentions. Only the slice shows the building the
//! player walks through.
//!
//! ```sh
//! cargo run --release --example slice_view
//! SPAN=160 SCALE=5 SEED=7 cargo run --release --example slice_view
//! ```
//!
//! Writes `target/slice-views/slice_<seed>_<x>_<z>.png`. Environment knobs:
//! `SEED` (default 42), `ORIGIN` (`x,z` world corner, default `0,0`),
//! `SPAN` (world units per side, default 160), `SCALE` (pixels per world
//! unit, default 5), `HEIGHT` (world units above the floor to cut at,
//! default 1.0 — knee height, which reads walls and pillars without the
//! ceiling covering everything), `LEVEL` (default 0).

use std::fs;
use std::path::PathBuf;

use vackrooms::adapters::material_palette::material_visual;
use vackrooms::adapters::png_writer::encode_rgb;
use vackrooms::domain::entities::position::Position;
use vackrooms::domain::entities::voxel_grid::VOXEL_AIR;
use vackrooms::frameworks_drivers::simple_noise::SimpleNoiseProvider;
use vackrooms::use_cases::generate_chunk::{GenerateChunkArchitectureUseCase, GeneratorConfig};

fn main() {
    let seed: u32 = env_or("SEED", 42);
    let span: f32 = env_or("SPAN", 160.0);
    let scale: f32 = env_or("SCALE", 5.0);
    let height: f32 = env_or("HEIGHT", 1.0);
    let level: u32 = env_or("LEVEL", 0);
    let (ox, oz) = origin();

    let noise = SimpleNoiseProvider::new();
    let mut config = GeneratorConfig::low_spec();
    config.level = level;
    let generator = GenerateChunkArchitectureUseCase::new(&noise);

    let side = (span * scale).round() as u32;
    let mut rgb = vec![0u8; (side as usize) * (side as usize) * 3];

    // One chunk at a time, painted into the image as it is generated: a
    // whole square of world held as voxels at once would be gigabytes, and
    // the engine never does that either.
    let chunk_span = config.chunk_size;
    let chunks = (span / chunk_span).ceil() as i32;
    let mut solid = 0usize;
    let mut sampled = 0usize;
    for cz in 0..chunks {
        for cx in 0..chunks {
            let origin = Position::new(ox + cx as f32 * chunk_span, oz + cz as f32 * chunk_span);
            let grid = generator.execute(origin, seed, config.clone());
            let y = ((height / config.voxel_scale).round() as usize).min(grid.height() - 1);
            for vz in 0..grid.depth() {
                for vx in 0..grid.width() {
                    let world_x = origin.x + vx as f32 * config.voxel_scale;
                    let world_z = origin.z + vz as f32 * config.voxel_scale;
                    // A voxel covers a *square* of the image, not a pixel.
                    // Painting one pixel each looked right at 1 px per
                    // voxel and dissolved into a starfield the moment the
                    // view was zoomed in — the one magnification where you
                    // most need to trust what you are seeing.
                    let (x0, x1) = pixel_span(world_x - ox, config.voxel_scale, scale, side);
                    let (z0, z1) = pixel_span(world_z - oz, config.voxel_scale, scale, side);
                    if x0 >= x1 || z0 >= z1 {
                        continue;
                    }
                    let material = grid.get(vx, y, vz);
                    sampled += 1;
                    solid += usize::from(material != VOXEL_AIR);
                    // Air at knee height is floor you can stand on, so it
                    // is drawn as the carpet under it rather than as a
                    // hole — the slice is a plan, not an X-ray.
                    let color = if material == VOXEL_AIR {
                        floor_colour(grid.get(vx, 0, vz))
                    } else {
                        material_visual(material).color
                    };
                    for pz in z0..z1 {
                        for px in x0..x1 {
                            let i = (pz * side as usize + px) * 3;
                            rgb[i] = (color >> 16) as u8;
                            rgb[i + 1] = (color >> 8) as u8;
                            rgb[i + 2] = color as u8;
                        }
                    }
                }
            }
        }
    }

    let out_dir = PathBuf::from("target/slice-views");
    fs::create_dir_all(&out_dir).expect("create slice-view directory");
    let path = out_dir.join(format!("slice_{seed}_{ox}_{oz}.png"));
    fs::write(&path, encode_rgb(side, side, &rgb)).expect("write slice png");

    println!("level {level} seed {seed}: {span} u square at ({ox}, {oz}), cut {height} u up",);
    println!(
        "{} of {} sampled columns are solid ({:.1}%)",
        solid,
        sampled,
        100.0 * solid as f32 / sampled.max(1) as f32
    );
    println!("wrote {}", path.display());
}

/// The half-open pixel range a voxel covers along one axis, clipped to the
/// image. At least one pixel wide wherever the voxel is on screen at all,
/// so zooming out never drops geometry either.
fn pixel_span(offset: f32, voxel: f32, scale: f32, side: u32) -> (usize, usize) {
    let lo = (offset * scale).floor();
    let hi = ((offset + voxel) * scale).ceil().max(lo + 1.0);
    (
        lo.clamp(0.0, side as f32) as usize,
        hi.clamp(0.0, side as f32) as usize,
    )
}

/// The colour to draw open floor as, taken from the floor material itself
/// so carpet regimes (dry, deep, sticky, stained) stay legible.
fn floor_colour(material: u8) -> u32 {
    if material == VOXEL_AIR {
        0x101014
    } else {
        material_visual(material).color
    }
}

fn env_or<T: std::str::FromStr>(key: &str, fallback: T) -> T {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(fallback)
}

fn origin() -> (f32, f32) {
    let raw = std::env::var("ORIGIN").unwrap_or_else(|_| "0,0".into());
    let mut parts = raw.split(',').filter_map(|p| p.trim().parse().ok());
    (parts.next().unwrap_or(0.0), parts.next().unwrap_or(0.0))
}
