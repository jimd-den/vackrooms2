//! Look at a generated place: a first-person render and the plan it was
//! taken from, with the camera drawn on it.
//!
//! This is the verification path for generator work. `plan_view` shows the
//! floor plan and `section_view` shows the construction, but neither answers
//! "what does standing there actually look like" — and a screenshot with no
//! map does not say *where* you were standing, which makes it useless for
//! judging whether the thing you changed is the thing you are looking at.
//! This renders both from one camera and labels them with it.
//!
//! Rasterized through the production `SurfacePipeline` — the same greedy
//! surface mesh the game rasterizes — not the raymarcher, so what you see is
//! what the mesh path produces.
//!
//! ```sh
//! cargo run -p wasm_frontend --release --example scene_view
//! SEED=42 AT=52,40 YAW=1.57 cargo run -p wasm_frontend --release --example scene_view
//! ```
//!
//! Knobs, all optional:
//!   `SEED`   world seed (default 42)
//!   `AT`     camera x,z in world units (default 52,40)
//!   `EYE`    eye height above the floor (default 1.6)
//!   `YAW`    radians, 0 = +x, pi/2 = +z (default 0)
//!   `PITCH`  radians, + is up (default 0)
//!   `RADIUS` chunks streamed around the camera (default 3)
//!   `SIZE`   output pixels, `wxh` (default 1280x720)
//!   `NAME`   output basename (default `scene`)
//!
//! Writes `target/scene-views/<name>.view.png` and `<name>.plan.svg`.

use std::fs;
use std::path::PathBuf;
use std::sync::mpsc;

use vackrooms::adapters::blueprint_renderer::{BlueprintOptions, render_region_blueprint_svg};
use vackrooms::domain::entities::anomaly::RealitySnapshot;
use vackrooms::domain::entities::position::Position;
use vackrooms::frameworks_drivers::simple_noise::SimpleNoiseProvider;
use vackrooms::use_cases::generate_chunk::{GenerateChunkArchitectureUseCase, GeneratorConfig};
use vackrooms::use_cases::region_plan::{REGION_SIZE, generate_region_plan};

use wasm_frontend::adapters::collect_emissive_lights::collect_emissive_lights;
use wasm_frontend::adapters::surface_mesh::build_surface_artifacts;
use wasm_frontend::application::ports::{
    Environment, FrameParams, RenderArtifactNeeds, SurfaceChunk, SurfaceMeshPayload,
};
use wasm_frontend::application::render_settings::RenderToggles;
use wasm_frontend::drivers::webgpu::frame_resources::FrameResources;
use wasm_frontend::drivers::webgpu::gpu_types::{GpuFrameUniforms, collect_frame_lights};
use wasm_frontend::drivers::webgpu::pipelines::SurfacePipeline;

const TARGET_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8Unorm;
/// Vertical field of view, as tan(fov/2). Matches the play binary.
const FOV_TAN: f32 = 0.767_327;

fn main() {
    let seed: u32 = env_or("SEED", 42);
    let (cam_x, cam_z) = pair("AT", 52.0, 40.0);
    let eye: f32 = env_or("EYE", 1.6);
    let yaw: f32 = env_or("YAW", 0.0);
    let pitch: f32 = env_or("PITCH", 0.0);
    let radius: i32 = env_or("RADIUS", 3);
    let (width, height) = size();
    let name = std::env::var("NAME").unwrap_or_else(|_| "scene".into());

    let out_dir = PathBuf::from("target/scene-views");
    fs::create_dir_all(&out_dir).expect("create output directory");

    let noise = SimpleNoiseProvider::new();
    let config = GeneratorConfig::low_spec();

    // --- what is actually here -------------------------------------------
    // Report the plan before rendering it: if the camera is standing in
    // unplanned fabric, the render is not evidence about room fit-out, and
    // it is better to say so than to let the picture imply otherwise.
    let (rx, rz) = (
        (cam_x / REGION_SIZE).floor() as i64,
        (cam_z / REGION_SIZE).floor() as i64,
    );
    let plan = generate_region_plan(
        seed,
        Position::new(rx as f32 * REGION_SIZE, rz as f32 * REGION_SIZE),
        REGION_SIZE,
        &config,
        &noise,
    );
    println!("seed {seed}, camera ({cam_x}, {cam_z}) in region ({rx}, {rz})");
    let standing_in = plan.assemblies.iter().find(|a| {
        let b = a.footprint.bounds();
        cam_x >= b.0 && cam_x <= b.2 && cam_z >= b.1 && cam_z <= b.3
    });
    match standing_in {
        Some(a) => println!(
            "  standing in #{} {:?} (abandoned={})",
            a.id,
            a.program,
            a.corruption.abandoned
        ),
        None => println!("  standing in unplanned fabric (no assembly covers this point)"),
    }
    for a in &plan.assemblies {
        let b = a.footprint.bounds();
        println!(
            "  #{:<3} {:<20} ({:6.1},{:6.1})-({:6.1},{:6.1})",
            a.id,
            format!("{:?}", a.program),
            b.0,
            b.1,
            b.2,
            b.3
        );
    }

    // --- the plan, with the camera on it ----------------------------------
    let plan_svg = render_region_blueprint_svg(
        &plan,
        &[],
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
    let plan_path = out_dir.join(format!("{name}.plan.svg"));
    fs::write(
        &plan_path,
        with_camera_marker(&plan_svg, &plan, cam_x, cam_z, yaw, 12.0),
    )
    .expect("write plan");
    println!("wrote {}", plan_path.display());

    // --- the view ----------------------------------------------------------
    let gpu = GpuContext::new();
    let mut frame_resources = FrameResources::new(&gpu.device);
    let mut surface = SurfacePipeline::new(
        &gpu.device,
        TARGET_FORMAT,
        frame_resources.layout(),
        config.chunk_size,
    );

    // Stream the chunks around the camera exactly as the game does: generate
    // with a one-voxel halo and mesh from the halo, so boundary faces between
    // neighbouring chunks are decided the same way they are in play.
    let generator = GenerateChunkArchitectureUseCase::new(&noise);
    let reality = RealitySnapshot::empty();
    let chunk = config.chunk_size;
    let base_cx = (cam_x / chunk).floor() as i32;
    let base_cz = (cam_z / chunk).floor() as i32;

    let mut meshes: Vec<(i32, i32, SurfaceMeshPayload)> = Vec::new();
    let mut scene_lights = Vec::new();
    let mut world = World {
        voxel: config.voxel_scale,
        grids: Vec::new(),
    };
    for dz in -radius..=radius {
        for dx in -radius..=radius {
            let (cx, cz) = (base_cx + dx, base_cz + dz);
            let (ox, oz) = (cx as f32 * chunk, cz as f32 * chunk);
            let halo_config = GeneratorConfig {
                chunk_size: chunk + config.voxel_scale * 2.0,
                ..config.clone()
            };
            let halo_origin = [ox - config.voxel_scale, 0.0, oz - config.voxel_scale];
            let halo = generator.execute_with_reality(
                Position::new(halo_origin[0], halo_origin[2]),
                seed,
                halo_config,
                &reality,
            );
            scene_lights.extend(collect_emissive_lights(
                &halo,
                config.voxel_scale,
                halo_origin,
                1,
            ));
            let mesh = build_surface_artifacts(
                &halo,
                config.voxel_scale,
                0,
                1,
                RenderArtifactNeeds::SURFACE,
            );
            meshes.push((cx, cz, mesh));
            world
                .grids
                .push((halo_origin[0], halo_origin[2], halo.grid));
        }
    }
    let quads: usize = meshes.iter().map(|(_, _, m)| m.indices.len() / 6).sum();
    println!(
        "streamed {} chunks, {} surface quads, {} lights",
        meshes.len(),
        quads,
        scene_lights.len()
    );

    // Stand somewhere you can actually see from.
    //
    // Typing a coordinate and hoping is how a survey burns five shots on the
    // inside of a wall — which is exactly what happened at (95, 25), and it
    // cost an investigation into lighting that was really a camera in a
    // partition. The search asks the voxels instead: a spot must have head
    // room and floor, and among those it takes the one you can see furthest
    // from, which is also the one worth photographing.
    let want = Want::parse(&std::env::var("WANT").unwrap_or_else(|_| "hall".into()));
    let aimed = std::env::var("YAW").is_ok();
    let (cam_x, cam_z, yaw) = if env_or("AUTOSPOT", 1) != 0 {
        match world.best_viewpoint(cam_x, cam_z, eye, env_or("SEARCH", 14.0f32), want) {
            Some(found) => {
                println!(
                    "autospot: ({:.1}, {:.1}) -> ({:.1}, {:.1})  reach {:.1} u, breadth {:.1} u, \
                     elongation {:.2}",
                    cam_x,
                    cam_z,
                    found.x,
                    found.z,
                    found.open.reach,
                    found.open.breadth,
                    found.open.elongation()
                );
                // An explicit YAW is an instruction; otherwise face the long
                // axis, because a shot that does not face what made the spot
                // interesting is a wasted shot.
                let yaw = if aimed { yaw } else { found.open.yaw };
                (found.x, found.z, yaw)
            }
            None => {
                println!("autospot: found nothing standable near ({cam_x}, {cam_z}); keeping it");
                (cam_x, cam_z, yaw)
            }
        }
    } else {
        (cam_x, cam_z, yaw)
    };

    let toggles = RenderToggles::default();
    let chunks: Vec<SurfaceChunk> = meshes
        .iter()
        .map(|(cx, cz, mesh)| SurfaceChunk {
            key: (*cx as i64, *cz as i64),
            origin: [*cx as f32 * chunk, 0.0, *cz as f32 * chunk],
            mesh,
        })
        .collect();
    surface.upload(&gpu.device, &chunks);

    // One streamed world, many cameras. Re-generating chunks per shot made a
    // survey cost minutes and discouraged taking one, which is how a
    // generator change goes unlooked-at.
    println!(
        "\n{:<14} {:>7} {:>7} {:>7} {:>7}  {}",
        "shot", "mean", "p05", "p95", "black%", "file"
    );
    for shot in shots(cam_x, cam_z, yaw, pitch) {
        let frame = FrameParams {
            camera_pos: [shot.x, eye, shot.z],
            yaw: shot.yaw,
            pitch: shot.pitch,
            scene_lights: scene_lights.clone(),
            environment: Environment::interior(),
            ..FrameParams::default()
        };
        let lights = collect_frame_lights(&frame);
        let uniforms = GpuFrameUniforms::from_frame(
            &frame,
            width,
            height,
            FOV_TAN,
            chunk,
            512,
            lights.len(),
            toggles,
        );
        frame_resources.write(&gpu.device, &gpu.queue, &uniforms, &lights);

        let pixels = gpu.render(width, height, |encoder, color, depth| {
            surface.draw(
                &gpu.queue,
                encoder,
                color,
                depth,
                &frame_resources,
                &frame,
                toggles,
                frame.scene_lights.len() as u32,
                FOV_TAN,
                width as f32 / height as f32,
            );
        });

        let file = format!("{name}.{}.png", shot.label);
        let path = out_dir.join(&file);
        image::save_buffer(&path, &pixels, width, height, image::ColorType::Rgba8)
            .expect("write view png");

        let stats = Luminance::of(&pixels);
        println!(
            "{:<14} {:>7.1} {:>7.1} {:>7.1} {:>6.1}%  {}",
            shot.label,
            stats.mean,
            stats.p05,
            stats.p95,
            stats.black_share * 100.0,
            file
        );
    }
}

/// The streamed voxels, so the harness can ask the world questions instead
/// of trusting a typed coordinate.
struct World {
    voxel: f32,
    /// (halo origin x, halo origin z, grid) per streamed chunk.
    grids: Vec<(f32, f32, vackrooms::domain::entities::voxel_grid::VoxelGrid)>,
}

/// How open a place is, in the two ways that differ.
#[derive(Clone, Copy)]
struct Openness {
    /// Longest clear sight line, world units.
    reach: f32,
    /// Mean clear distance over all directions, world units.
    breadth: f32,
    /// Bearing of the longest sight line.
    yaw: f32,
}

impl Openness {
    /// Reach over breadth. ~1 is a room open in every direction; large is a
    /// route you can see down but not across.
    fn elongation(&self) -> f32 {
        self.reach / self.breadth.max(0.1)
    }
}

/// What kind of place a survey is hunting for.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Want {
    /// Open in every direction: halls, expanses.
    Hall,
    /// Long and narrow: circulation.
    Corridor,
    /// Anything you can see out of.
    Any,
}

impl Want {
    fn parse(raw: &str) -> Self {
        match raw.trim().to_ascii_lowercase().as_str() {
            "corridor" | "route" => Want::Corridor,
            "any" => Want::Any,
            _ => Want::Hall,
        }
    }

    fn score(self, open: &Openness) -> f32 {
        match self {
            Want::Hall => open.breadth,
            // Reward the long axis, but only once it is genuinely longer
            // than the space is wide -- otherwise every big room wins on
            // reach alone and corridors are never selected.
            Want::Corridor => open.reach * open.elongation(),
            Want::Any => open.reach,
        }
    }
}

/// A standable spot, why it won, and how far you can see from it.
struct Viewpoint {
    x: f32,
    z: f32,
    score: f32,
    open: Openness,
}

impl World {
    /// Material at a world point, or `None` outside everything streamed.
    fn material_at(&self, x: f32, y: f32, z: f32) -> Option<u8> {
        for (ox, oz, grid) in &self.grids {
            let (ix, iy, iz) = (
                ((x - ox) / self.voxel).floor(),
                (y / self.voxel).floor(),
                ((z - oz) / self.voxel).floor(),
            );
            if ix < 0.0 || iy < 0.0 || iz < 0.0 {
                continue;
            }
            let (ix, iy, iz) = (ix as usize, iy as usize, iz as usize);
            if ix >= grid.width() || iy >= grid.height() || iz >= grid.depth() {
                continue;
            }
            return Some(grid.get(ix, iy, iz));
        }
        None
    }

    /// Does something block a body here?
    ///
    /// Outside everything streamed counts as blocked, so the search never
    /// walks the camera off the edge of what was generated.
    fn solid_at(&self, x: f32, y: f32, z: f32) -> bool {
        use vackrooms::domain::entities::voxel_grid::SOLID_MATERIALS;
        match self.material_at(x, y, z) {
            Some(material) => SOLID_MATERIALS.contains(&material),
            None => true,
        }
    }

    /// Can a person stand here — floor under foot, body and head clear?
    fn standable(&self, x: f32, z: f32, eye: f32) -> bool {
        use vackrooms::domain::entities::voxel_grid::VOXEL_AIR;
        // Floor is *present*, not *solid*: carpet and slab are walkable, so
        // they are deliberately absent from SOLID_MATERIALS. Testing the
        // floor for solidity rejected every ordinary room in the level.
        if self.material_at(x, 0.0, z).unwrap_or(VOXEL_AIR) == VOXEL_AIR {
            return false;
        }
        let mut y = self.voxel;
        while y <= eye + 0.2 {
            if self.solid_at(x, y, z) {
                return false;
            }
            y += self.voxel;
        }
        true
    }

    /// How open a spot is, and which way to look.
    ///
    /// Averaging sight lines was wrong once corridors existed: a route is
    /// open along one axis and closed across it, so an average scores it
    /// *below* a small square room and the search actively rejected the
    /// thing it was supposed to find. Reach and breadth are different
    /// questions and are now asked separately.
    ///
    /// `reach` is the longest clear sight line -- how far you can see.
    /// `breadth` is the mean over all directions -- how open it is around
    /// you. A hall has both. A corridor has reach without breadth. A closet
    /// has neither. Their ratio is what tells the three apart.
    fn survey(&self, x: f32, z: f32, eye: f32) -> Openness {
        const REACH: f32 = 40.0;
        const RAYS: usize = 16;
        let mut distances = [0.0f32; RAYS];
        for (k, slot) in distances.iter_mut().enumerate() {
            let a = k as f32 * std::f32::consts::TAU / RAYS as f32;
            let (dx, dz) = (a.cos(), a.sin());
            let mut d = self.voxel;
            while d < REACH {
                if self.solid_at(x + dx * d, eye, z + dz * d) {
                    break;
                }
                d += self.voxel * 2.0;
            }
            *slot = d;
        }
        let breadth = distances.iter().sum::<f32>() / RAYS as f32;
        let (best, reach) = distances
            .iter()
            .enumerate()
            .fold(
                (0usize, 0.0f32),
                |acc, (i, d)| {
                    if *d > acc.1 { (i, *d) } else { acc }
                },
            );
        Openness {
            reach,
            breadth,
            // Look down the long axis. A shot that does not face the thing
            // that made the spot interesting is a wasted shot, and aiming
            // by hand is how the corridors went unphotographed.
            yaw: best as f32 * std::f32::consts::TAU / RAYS as f32,
        }
    }

    /// The best standable spot within `search`, judged by what we are
    /// hunting for.
    fn best_viewpoint(
        &self,
        x: f32,
        z: f32,
        eye: f32,
        search: f32,
        want: Want,
    ) -> Option<Viewpoint> {
        let step = 1.2f32;
        let mut best: Option<Viewpoint> = None;
        let mut oz = -search;
        while oz <= search {
            let mut ox = -search;
            while ox <= search {
                let (cx, cz) = (x + ox, z + oz);
                if self.standable(cx, cz, eye) {
                    let open = self.survey(cx, cz, eye);
                    let score = want.score(&open);
                    if best.as_ref().is_none_or(|b| score > b.score) {
                        best = Some(Viewpoint {
                            x: cx,
                            z: cz,
                            score,
                            open,
                        });
                    }
                }
                ox += step;
            }
            oz += step;
        }
        best
    }
}

/// One camera in a batch.
struct Shot {
    label: String,
    x: f32,
    z: f32,
    yaw: f32,
    pitch: f32,
}

/// The batch to render.
///
/// `SHOTS` is `label:dx,dz,yaw,pitch` entries separated by `;`, where
/// `dx,dz` are offsets *from the camera* — which, with autospot on, is a
/// standable place rather than wherever the coordinate landed. Absolute
/// coordinates would silently undo the search that just found open ground,
/// and a batch aimed at world origin renders five pictures of nothing.
///
/// With no `SHOTS`, the default is a panorama — four cardinal yaws plus one
/// looking up — because a single frame cannot distinguish "this place is
/// dark" from "the camera happened to face a wall", and that ambiguity is
/// exactly what wastes time when a render comes back black.
fn shots(cam_x: f32, cam_z: f32, yaw: f32, pitch: f32) -> Vec<Shot> {
    if let Ok(raw) = std::env::var("SHOTS") {
        return raw
            .split(';')
            .filter(|s| !s.trim().is_empty())
            .enumerate()
            .map(|(i, entry)| {
                let (label, values) = entry.split_once(':').unwrap_or(("shot", entry));
                let v: Vec<f32> = values
                    .split(',')
                    .filter_map(|p| p.trim().parse().ok())
                    .collect();
                Shot {
                    label: if label == "shot" {
                        format!("shot{i}")
                    } else {
                        label.trim().into()
                    },
                    x: cam_x + v.first().copied().unwrap_or(0.0),
                    z: cam_z + v.get(1).copied().unwrap_or(0.0),
                    yaw: v.get(2).copied().unwrap_or(yaw),
                    pitch: v.get(3).copied().unwrap_or(pitch),
                }
            })
            .collect();
    }
    let quarter = std::f32::consts::FRAC_PI_2;
    let mut out: Vec<Shot> = (0..4)
        .map(|k| Shot {
            label: ["east", "south", "west", "north"][k].into(),
            x: cam_x,
            z: cam_z,
            yaw: yaw + k as f32 * quarter,
            pitch,
        })
        .collect();
    out.push(Shot {
        label: "up".into(),
        x: cam_x,
        z: cam_z,
        yaw,
        pitch: 0.8,
    });
    out
}

/// Brightness of a rendered frame, so "the light is gone" is a measurement.
///
/// The mean alone hides the two failures that matter: a frame that is
/// uniformly dim reads the same as one that is mostly black with a bright
/// fixture in it. The percentiles and the black share separate them.
struct Luminance {
    mean: f32,
    p05: f32,
    p95: f32,
    black_share: f32,
}

impl Luminance {
    fn of(pixels: &[u8]) -> Self {
        let mut values: Vec<f32> = pixels
            .chunks_exact(4)
            .map(|p| 0.2126 * p[0] as f32 + 0.7152 * p[1] as f32 + 0.0722 * p[2] as f32)
            .collect();
        let mean = values.iter().sum::<f32>() / values.len().max(1) as f32;
        let black_share =
            values.iter().filter(|v| **v < 8.0).count() as f32 / values.len().max(1) as f32;
        values.sort_by(f32::total_cmp);
        let at = |q: f32| values[((values.len() as f32 - 1.0) * q) as usize];
        Self {
            mean,
            p05: at(0.05),
            p95: at(0.95),
            black_share,
        }
    }
}

// ---------------------------------------------------------------------------
// Plan annotation
// ---------------------------------------------------------------------------

/// Draws the camera and its view cone onto the region blueprint.
///
/// Without this the render and the plan are two unrelated pictures. The cone
/// is the actual horizontal field of view, so what is inside it in plan is
/// what should be visible in the render — which is the whole point of
/// producing them together.
fn with_camera_marker(
    svg: &str,
    plan: &vackrooms::domain::entities::architecture::RegionPlan,
    cam_x: f32,
    cam_z: f32,
    yaw: f32,
    px_per_unit: f32,
) -> String {
    let px = (cam_x - plan.origin_world.x) * px_per_unit;
    let pz = (cam_z - plan.origin_world.z) * px_per_unit;
    // Horizontal half-angle from the vertical one at 16:9.
    let half = (FOV_TAN * (16.0 / 9.0)).atan();
    let reach = 18.0 * px_per_unit;
    let (lx, lz) = (
        px + reach * (yaw - half).cos(),
        pz + reach * (yaw - half).sin(),
    );
    let (rx, rz) = (
        px + reach * (yaw + half).cos(),
        pz + reach * (yaw + half).sin(),
    );
    let marker = format!(
        r##"  <g id="camera">
    <path d="M {px:.1} {pz:.1} L {lx:.1} {lz:.1} A {reach:.1} {reach:.1} 0 0 1 {rx:.1} {rz:.1} Z"
          fill="#38bdf8" fill-opacity="0.15" stroke="#38bdf8" stroke-opacity="0.5" stroke-width="1.5"/>
    <circle cx="{px:.1}" cy="{pz:.1}" r="5" fill="#38bdf8" stroke="#0b0f19" stroke-width="2"/>
    <text x="{tx:.1}" y="{tz:.1}" fill="#38bdf8" font-family="monospace" font-size="13">camera ({cam_x:.1}, {cam_z:.1})</text>
  </g>
</svg>"##,
        tx = px + 10.0,
        tz = pz - 10.0,
    );
    svg.replace("</svg>", &marker)
}

// ---------------------------------------------------------------------------
// Headless GPU
// ---------------------------------------------------------------------------

struct GpuContext {
    _instance: wgpu::Instance,
    device: wgpu::Device,
    queue: wgpu::Queue,
}

impl GpuContext {
    fn new() -> Self {
        let mut descriptor = wgpu::InstanceDescriptor::new_without_display_handle();
        descriptor.backends = wgpu::Backends::VULKAN;
        let instance = wgpu::Instance::new(descriptor);
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::None,
            force_fallback_adapter: false,
            compatible_surface: None,
            apply_limit_buckets: false,
        }))
        .expect("a Vulkan adapter (Mesa lavapipe is enough) is required to render offscreen");
        let info = adapter.get_info();
        println!("gpu: {} ({})", info.name, info.driver);
        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("vackrooms.scene-view"),
            required_features: wgpu::Features::empty(),
            required_limits: wgpu::Limits::defaults(),
            ..Default::default()
        }))
        .expect("create device");
        Self {
            _instance: instance,
            device,
            queue,
        }
    }

    /// Renders one frame offscreen and returns tightly packed RGBA8.
    fn render(
        &self,
        width: u32,
        height: u32,
        draw: impl FnOnce(&mut wgpu::CommandEncoder, &wgpu::TextureView, &wgpu::TextureView),
    ) -> Vec<u8> {
        let extent = wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        };
        let color = self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("scene-view.color"),
            size: extent,
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: TARGET_FORMAT,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let depth = self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("scene-view.depth"),
            size: extent,
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Depth32Float,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            view_formats: &[],
        });
        let color_view = color.create_view(&Default::default());
        let depth_view = depth.create_view(&Default::default());

        // Readback rows must be 256-byte aligned; unpad after mapping.
        let unpadded = width * 4;
        let align = wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
        let padded = unpadded.div_ceil(align) * align;
        let readback = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("scene-view.readback"),
            size: u64::from(padded * height),
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });

        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("scene-view"),
            });
        draw(&mut encoder, &color_view, &depth_view);
        encoder.copy_texture_to_buffer(
            color.as_image_copy(),
            wgpu::TexelCopyBufferInfo {
                buffer: &readback,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(padded),
                    rows_per_image: Some(height),
                },
            },
            extent,
        );
        self.queue.submit([encoder.finish()]);

        let (tx, rx) = mpsc::channel();
        readback.slice(..).map_async(wgpu::MapMode::Read, move |r| {
            tx.send(r).ok();
        });
        self.device
            .poll(wgpu::PollType::wait_indefinitely())
            .expect("poll");
        rx.recv().expect("map callback").expect("map");

        let mapped = readback.slice(..).get_mapped_range().expect("mapped range");
        let mut out = Vec::with_capacity((unpadded * height) as usize);
        for row in 0..height {
            let start = (row * padded) as usize;
            out.extend_from_slice(&mapped[start..start + unpadded as usize]);
        }
        drop(mapped);
        readback.unmap();
        out
    }
}

// ---------------------------------------------------------------------------
// Environment knobs
// ---------------------------------------------------------------------------

fn env_or<T: std::str::FromStr>(key: &str, default: T) -> T {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn pair(key: &str, dx: f32, dz: f32) -> (f32, f32) {
    let Ok(raw) = std::env::var(key) else {
        return (dx, dz);
    };
    let mut parts = raw.split(',').filter_map(|p| p.trim().parse().ok());
    (parts.next().unwrap_or(dx), parts.next().unwrap_or(dz))
}

fn size() -> (u32, u32) {
    let raw = std::env::var("SIZE").unwrap_or_else(|_| "1280x720".into());
    let mut parts = raw.split(['x', 'X']).filter_map(|p| p.trim().parse().ok());
    (parts.next().unwrap_or(1280), parts.next().unwrap_or(720))
}
