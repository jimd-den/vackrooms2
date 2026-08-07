//! Vertical-section dump of generated voxels: the verification surface for
//! interior fit-out work.
//!
//! `plan_view` answers "does the floor plan read as a plan". It cannot show
//! a baseboard, a ceiling grid, a plenum or a missing tile, because all of
//! those are things you only see in *section* — the drawing a builder reads
//! to know what a wall is made of. This renders that cut.
//!
//! ```sh
//! cargo run --release --example section_view
//! ```
//!
//! Writes `target/section-views/section_z<z>.svg` and prints an ASCII
//! section per cut. Environment knobs: `SEED` (default 42), `ORIGIN`
//! (`x,z` world corner of the chunk to cut, default `40,40`), `CUTS`
//! (how many z rows to cut, default 3).

use std::fs;
use std::path::PathBuf;

use vackrooms::adapters::material_palette::material_visual;
use vackrooms::domain::entities::position::Position;
use vackrooms::domain::entities::voxel_grid::{VOXEL_AIR, VoxelGrid};
use vackrooms::frameworks_drivers::simple_noise::SimpleNoiseProvider;
use vackrooms::use_cases::generate_chunk::{GenerateChunkArchitectureUseCase, GeneratorConfig};

fn main() {
    let seed: u32 = env_or("SEED", 42);
    let cuts: usize = env_or("CUTS", 3);
    let (ox, oz) = origin();

    let noise = SimpleNoiseProvider::new();
    let config = GeneratorConfig::low_spec();
    let generator = GenerateChunkArchitectureUseCase::new(&noise);
    let grid = generator.execute(Position::new(ox, oz), seed, config.clone());

    let out_dir = PathBuf::from("target/section-views");
    fs::create_dir_all(&out_dir).unwrap();

    println!(
        "chunk at ({ox}, {oz}) seed {seed}: {}x{}x{} voxels at {} u",
        grid.width(),
        grid.height(),
        grid.depth(),
        config.voxel_scale
    );

    // Cut through rows spread across the chunk rather than all at one edge.
    for k in 0..cuts.max(1) {
        let z = (grid.depth() * (k + 1)) / (cuts.max(1) + 1);
        print_ascii_section(&grid, z, config.voxel_scale);
        let svg = render_section_svg(&grid, z, config.voxel_scale);
        let path = out_dir.join(format!("section_z{z}.svg"));
        fs::write(&path, svg).unwrap();
        println!("wrote {}", path.display());
    }
}

fn env_or<T: std::str::FromStr>(key: &str, default: T) -> T {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn origin() -> (f32, f32) {
    let raw = std::env::var("ORIGIN").unwrap_or_else(|_| "40,40".into());
    let mut parts = raw.split(',').filter_map(|p| p.trim().parse().ok());
    (parts.next().unwrap_or(40.0), parts.next().unwrap_or(40.0))
}

/// One character per material class, so a section is readable in a terminal.
fn glyph(material: u8) -> char {
    match material_visual(material).name {
        "air" => '.',
        "floor" | "drycarpet" | "deepcarpet" | "stickycarpet" | "stainedcarpet" => '_',
        "wall" | "agedwallpaper" | "palewall" | "damagedwall" | "concretewall" => '#',
        "redwall" => 'R',
        "baseboard" => 'b',
        "ceiling" => '-',
        "ceilinggrid" => 'T',
        "slab" => '=',
        "duct" => 'D',
        "pipe" => 'p',
        "light" | "redlight" => '*',
        "metaldoor" => 'd',
        "crate" => 'c',
        "tilefloor" => ':',
        _ => '?',
    }
}

fn print_ascii_section(grid: &VoxelGrid, z: usize, voxel: f32) {
    println!("\n--- section at z index {z} (looking +z), y up ---");
    // Top row first so the print reads the way a section drawing does.
    for y in (0..grid.height()).rev() {
        let row: String = (0..grid.width()).map(|x| glyph(grid.get(x, y, z))).collect();
        println!("{:>5.1}u |{row}", y as f32 * voxel);
    }
    println!(
        "       legend: _ floor  b base  # wall  T grid  - tile  = slab  D duct  p pipe  * light  d door  . air"
    );
}

/// One rect per non-air voxel, colored from the shared material palette —
/// the same colors the renderer uses, so a section cannot drift from what
/// the game actually shows.
fn render_section_svg(grid: &VoxelGrid, z: usize, voxel: f32) -> String {
    const PX: usize = 10;
    let (w, h) = (grid.width(), grid.height());
    let mut svg = format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"{}\" height=\"{}\" \
         viewBox=\"0 0 {} {}\"><rect width=\"100%\" height=\"100%\" fill=\"#0d1117\"/>",
        w * PX,
        h * PX,
        w * PX,
        h * PX
    );
    for y in 0..h {
        for x in 0..w {
            let material = grid.get(x, y, z);
            if material == VOXEL_AIR {
                continue;
            }
            // SVG y grows downward; the section reads with y up.
            let py = (h - 1 - y) * PX;
            svg.push_str(&format!(
                "<rect x=\"{}\" y=\"{py}\" width=\"{PX}\" height=\"{PX}\" fill=\"#{:06X}\"/>",
                x * PX,
                material_visual(material).color
            ));
        }
    }
    let _ = voxel;
    svg.push_str("</svg>");
    svg
}
