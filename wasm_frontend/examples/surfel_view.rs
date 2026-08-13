//! Renders the real spawn neighbourhood as a surfel cloud, at several
//! densities, and reports what each one cost.
//!
//! `vulkan_snapshot` shows the production pipelines; this shows the one
//! that has no GPU path yet. Same generation, same chunk source, same
//! camera basis — the only difference is that the surface is drawn as
//! oriented discs rather than as triangles or axis-aligned rectangles.
//!
//! ```sh
//! cargo run -p wasm_frontend --example surfel_view --release
//! SPACING=0.2,0.8 SEED=7 cargo run -p wasm_frontend --example surfel_view --release
//! ```
//!
//! Output lands in `target/surfel-views/*.png`. Environment knobs: `SEED`
//! (default 42), `SIZE` (`WxH`, default 960x540), `RADIUS` (chunks either
//! side of the spawn chunk, default 2), `SPACING` (comma-separated world
//! units between disc centres, default `0.2,0.4,0.8,1.6`), `POS` (`x,z`)
//! and `YAW` (radians) to move the camera.

use std::fs;
use std::path::PathBuf;
use std::time::Instant;

use vackrooms::adapters::png_writer::encode_rgb;
use vackrooms::domain::entities::position::Position;
use vackrooms::frameworks_drivers::simple_noise::SimpleNoiseProvider;
use vackrooms::use_cases::generate_chunk::GeneratorConfig;
use vackrooms::use_cases::region_plan::spawn_point;
use wasm_frontend::adapters::cpu_surfel::{SurfelCamera, SurfelRenderSettings, render_surfels};
use wasm_frontend::adapters::local_chunk_source::LocalChunkSource;
use wasm_frontend::adapters::surfel_cloud::PackedSurfel;

fn main() {
    let seed: u32 = env_or("SEED", 42);
    let radius: i32 = env_or("RADIUS", 2);
    let (width, height) = size();
    let spacings = spacings();

    let config = GeneratorConfig::low_spec();
    let source = LocalChunkSource::new(SimpleNoiseProvider::new(), seed, config.clone());

    let spawn = position("POS").unwrap_or_else(|| spawn_point(seed));
    // The snapshot tool's default heading, so the two sets of images frame
    // the same thing and can be compared directly.
    let yaw: f32 = env_or("YAW", -std::f32::consts::FRAC_PI_2);
    let camera = SurfelCamera {
        position: [spawn.x, 1.7, spawn.z],
        yaw,
        pitch: 0.0,
        fov_tan: (70f32.to_radians() * 0.5).tan(),
    };

    let origin_x = (spawn.x / config.chunk_size).floor() * config.chunk_size;
    let origin_z = (spawn.z / config.chunk_size).floor() * config.chunk_size;

    let out_dir = PathBuf::from("target/surfel-views");
    fs::create_dir_all(&out_dir).expect("create surfel-view directory");

    println!(
        "seed {seed}, spawn ({:.1}, {:.1}), {} chunks, {width}x{height}",
        spawn.x,
        spawn.z,
        (radius * 2 + 1).pow(2)
    );
    println!(
        "{:>8}  {:>10}  {:>9}  {:>8}  {:>8}  {:>9}  {:>8}",
        "spacing", "surfels", "memory", "build", "draw", "fragments", "coverage"
    );

    for spacing in spacings {
        let built = Instant::now();
        let mut clouds: Vec<(Vec<PackedSurfel>, [f32; 3])> = Vec::new();
        for dz in -radius..=radius {
            for dx in -radius..=radius {
                let x = origin_x + dx as f32 * config.chunk_size;
                let z = origin_z + dz as f32 * config.chunk_size;
                let cloud = source.load_surfel_cloud(x, z, 0, 0, spacing);
                clouds.push((cloud.surfels, [x, 0.0, z]));
            }
        }
        let build_ms = built.elapsed().as_secs_f32() * 1000.0;

        let total: usize = clouds.iter().map(|(s, _)| s.len()).sum();
        let borrowed: Vec<(&[PackedSurfel], [f32; 3])> = clouds
            .iter()
            .map(|(s, origin)| (s.as_slice(), *origin))
            .collect();

        let settings = SurfelRenderSettings::new(width, height);
        let drawn = Instant::now();
        let image = render_surfels(&borrowed, &camera, &settings);
        let draw_ms = drawn.elapsed().as_secs_f32() * 1000.0;

        let path = out_dir.join(format!("surfel_{seed}_{:.0}mm.png", spacing * 1000.0));
        fs::write(&path, encode_rgb(width as u32, height as u32, &image.rgb))
            .expect("write surfel png");

        println!(
            "{:>7.2}u  {:>10}  {:>8.1}MB  {:>6.0}ms  {:>6.0}ms  {:>9}  {:>7.1}%",
            spacing,
            total,
            (total * std::mem::size_of::<PackedSurfel>()) as f32 / (1024.0 * 1024.0),
            build_ms,
            draw_ms,
            image.stats.fragments,
            image.coverage * 100.0
        );
        println!("          wrote {}", path.display());
    }
}

fn env_or<T: std::str::FromStr>(key: &str, fallback: T) -> T {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(fallback)
}

fn size() -> (usize, usize) {
    let raw = std::env::var("SIZE").unwrap_or_else(|_| "960x540".into());
    let (w, h) = raw.split_once('x').unwrap_or(("960", "540"));
    (
        w.trim().parse().unwrap_or(960),
        h.trim().parse().unwrap_or(540),
    )
}

fn position(key: &str) -> Option<Position> {
    let raw = std::env::var(key).ok()?;
    let (x, z) = raw.split_once(',')?;
    Some(Position::new(
        x.trim().parse().ok()?,
        z.trim().parse().ok()?,
    ))
}

fn spacings() -> Vec<f32> {
    std::env::var("SPACING")
        .ok()
        .map(|raw| {
            raw.split(',')
                .filter_map(|part| part.trim().parse().ok())
                .collect::<Vec<f32>>()
        })
        .filter(|v: &Vec<f32>| !v.is_empty())
        .unwrap_or_else(|| vec![0.2, 0.4, 0.8, 1.6])
}
