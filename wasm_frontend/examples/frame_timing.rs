//! Headless frame-timing baseline for the production WebGPU pipelines.
//!
//! `vulkan_snapshot` answers "what does it look like"; this answers "what
//! does it cost". It builds the same real spawn neighborhood through
//! `LocalChunkSource`, but at a configurable streaming radius and LOD
//! policy, then times repeated frames per renderer so changes to LOD
//! tiering and culling can be proven against numbers.
//!
//! ```sh
//! cargo run -p wasm_frontend --example frame_timing --release
//! ```
//!
//! Environment knobs:
//! `SEED` (default 42), `SIZE` (`WxH`, default 960x540),
//! `RADIUS` (Chebyshev chunk rings, default 4 -> 81 chunks, the resident
//! set the engine actually uses for surface strategies),
//! `FRAMES` (timed frames per renderer, default 60),
//! `WARMUP` (untimed frames first, default 10),
//! `LOD` (`flat0` | `flat1` | `tiered`, default `tiered`),
//! `ONLY` (comma-separated renderer names) to time a subset. Useful when
//! one back end is unstable on the adapter under test -- a driver that
//! loses the device on one pipeline should not cost you the numbers for
//! the others.
//!
//! `tiered` mirrors the engine's real policy: LOD 0 inside
//! `fine_distance`, coarse beyond. `flat0` is the worst case (everything
//! full resolution) and is the ceiling any tiering change is measured
//! against.

use std::collections::HashMap;
use std::time::Instant;

use vackrooms::domain::entities::anomaly::RealitySnapshot;
use vackrooms::frameworks_drivers::simple_noise::SimpleNoiseProvider;
use vackrooms::use_cases::generate_chunk::GeneratorConfig;
use vackrooms::use_cases::region_plan::spawn_point;
use wasm_frontend::adapters::local_chunk_source::LocalChunkSource;
use wasm_frontend::application::atlas::{AtlasPool, payload_rows};
use wasm_frontend::application::ports::{
    ChunkDraw, ChunkPayload, ChunkSourcePort, Environment, FrameParams, RenderArtifactNeeds,
    SurfaceChunk,
};
use wasm_frontend::application::render_settings::RenderToggles;
use wasm_frontend::application::streaming::chunk_key;
use wasm_frontend::drivers::webgpu::frame_resources::FrameResources;
use wasm_frontend::drivers::webgpu::gpu_types::{GpuFrameUniforms, collect_frame_lights};
use wasm_frontend::drivers::webgpu::pipelines::{
    RaymarchPipeline, RaymarchRuntimeOptions, SplatPipeline, SurfacePipeline, SurfelPipeline,
};

const TARGET_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8Unorm;
const FOV_TAN: f32 = 0.767_327;
const MAX_DRAW_DISTANCE: f32 = 48.0;
const TRACE_BUDGET: u32 = 512;
const FACE_BUDGET: u32 = 262_144;
/// Mirrors `application::engine::COARSE_LOD`.
const COARSE_LOD: u8 = 1;
/// Mirrors `EngineConfig::default().fine_distance`.
const FINE_DISTANCE: f32 = 15.0;

/// How a chunk's LOD is chosen from its distance to the camera.
#[derive(Clone, Copy, PartialEq, Eq)]
enum LodPolicy {
    /// Every chunk at full resolution: the cost ceiling.
    Flat0,
    /// Every chunk coarse: the cost floor for the current two-tier scheme.
    Flat1,
    /// What the engine does today: fine inside `fine_distance`, else coarse.
    Tiered,
}

impl LodPolicy {
    fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "flat0" => Some(Self::Flat0),
            "flat1" => Some(Self::Flat1),
            "tiered" => Some(Self::Tiered),
            _ => None,
        }
    }

    fn lod_for(self, distance: f32) -> u8 {
        match self {
            Self::Flat0 => 0,
            Self::Flat1 => COARSE_LOD,
            Self::Tiered => {
                if distance <= FINE_DISTANCE {
                    0
                } else {
                    COARSE_LOD
                }
            }
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Flat0 => "flat0",
            Self::Flat1 => "flat1",
            Self::Tiered => "tiered",
        }
    }
}

struct LoadedWorld {
    payloads: Vec<(f32, f32, ChunkPayload)>,
    chunks: Vec<ChunkDraw>,
    atlas: Vec<u32>,
    frame: FrameParams,
    /// Generation wall time, separate from per-frame render cost.
    build_ms: f64,
}

/// Sorted frame times, in milliseconds.
struct Timings(Vec<f64>);

impl Timings {
    fn quantile(&self, q: f64) -> f64 {
        if self.0.is_empty() {
            return f64::NAN;
        }
        let index = ((self.0.len() - 1) as f64 * q).round() as usize;
        self.0[index]
    }

    fn mean(&self) -> f64 {
        if self.0.is_empty() {
            return f64::NAN;
        }
        self.0.iter().sum::<f64>() / self.0.len() as f64
    }

    fn report(&self, name: &str) {
        let median = self.quantile(0.5);
        println!(
            "{name:<10} median {median:>7.2} ms ({:>6.1} fps)  p95 {:>7.2}  min {:>7.2}  mean {:>7.2}",
            1000.0 / median,
            self.quantile(0.95),
            self.quantile(0.0),
            self.mean(),
        );
    }
}

/// Renderers selected by `ONLY`; all of them when it is unset.
fn wanted(name: &str) -> bool {
    match std::env::var("ONLY") {
        Ok(list) => list.split(',').any(|entry| entry.trim() == name),
        Err(_) => true,
    }
}

fn main() {
    let seed: u32 = env_parse("SEED").unwrap_or(42);
    let (width, height) = size();
    let radius: i32 = env_parse("RADIUS").unwrap_or(4);
    let frames: usize = env_parse("FRAMES").unwrap_or(60);
    let warmup: usize = env_parse("WARMUP").unwrap_or(10);
    let policy = std::env::var("LOD")
        .ok()
        .and_then(|value| LodPolicy::parse(&value))
        .unwrap_or(LodPolicy::Tiered);

    let (device, queue) = create_vulkan_device();
    let world = load_world(seed, radius, policy);

    let resident = world.chunks.len();
    let triangles: usize = world
        .payloads
        .iter()
        .map(|(_, _, payload)| payload.surface.indices.len() / 3)
        .sum();
    let faces: usize = world
        .payloads
        .iter()
        .map(|(_, _, payload)| payload.surface.faces.instances.len())
        .sum();
    println!(
        "seed {seed}  radius {radius} ({resident} chunks)  lod {}  {width}x{height}",
        policy.label()
    );
    println!(
        "geometry: {triangles} tris, {faces} faces, {} atlas words, {} lights, built in {:.0} ms",
        world.atlas.len(),
        world.frame.scene_lights.len(),
        world.build_ms,
    );
    println!("timing {frames} frames after {warmup} warmup, GPU synced each frame\n");

    let toggles = RenderToggles::default();
    let lights = collect_frame_lights(&world.frame);
    let uniforms = GpuFrameUniforms::from_frame(
        &world.frame,
        width,
        height,
        FOV_TAN,
        MAX_DRAW_DISTANCE,
        TRACE_BUDGET,
        lights.len(),
        toggles,
    );
    let mut frame_resources = FrameResources::new(&device);
    frame_resources.write(&device, &queue, &uniforms, &lights);

    let targets = Targets::new(&device, width, height);
    let aspect = width as f32 / (height.max(1)) as f32;
    // `LIGHTS` shortens the per-corner/per-fragment light loop for every
    // pipeline. Not a rendering feature -- it is how the cost of the flat
    // frame light array is separated from the cost of the geometry.
    let light_count = env_parse::<u32>("LIGHTS")
        .unwrap_or_else(|| world.frame.scene_lights.len() as u32)
        .min(world.frame.scene_lights.len() as u32);

    let surface_chunks: Vec<SurfaceChunk> = world
        .payloads
        .iter()
        .map(|(x, z, payload)| SurfaceChunk {
            key: chunk_key(*x, *z),
            origin: [*x, 0.0, *z],
            mesh: &payload.surface,
        })
        .collect();

    // Surfels are opt-in (`RenderArtifactNeeds::ALL` excludes them), so
    // they need their own load pass, taken only when they are being timed.
    let mut surfel_payloads: Vec<(f32, f32, ChunkPayload)> = if wanted("surfel") {
        let source = LocalChunkSource::new(
            SimpleNoiseProvider::new(),
            seed,
            GeneratorConfig::low_spec(),
        );
        world
            .payloads
            .iter()
            .map(|(x, z, _)| {
                let payload = source.load_with_artifacts(
                    *x,
                    *z,
                    0,
                    0,
                    &RealitySnapshot::default(),
                    RenderArtifactNeeds::SURFEL,
                );
                (*x, *z, payload)
            })
            .collect()
    } else {
        Vec::new()
    };
    // `SURFEL_SPACING` resamples the same greedy quads at a chosen density.
    // Extraction bakes one disc per voxel cell; the point of a surfel cloud
    // is that density is a dial, so the dial has to be measurable.
    if let Some(spacing) = env_parse::<f32>("SURFEL_SPACING") {
        let source = LocalChunkSource::new(
            SimpleNoiseProvider::new(),
            seed,
            GeneratorConfig::low_spec(),
        );
        for (x, z, payload) in &mut surfel_payloads {
            let lod = payload.surface.lod;
            payload.surface.surfels = source.load_surfel_cloud(*x, *z, 0, lod, spacing);
        }
    }
    let surfel_chunks: Vec<SurfaceChunk> = surfel_payloads
        .iter()
        .map(|(x, z, payload)| SurfaceChunk {
            key: chunk_key(*x, *z),
            origin: [*x, 0.0, *z],
            mesh: &payload.surface,
        })
        .collect();
    if wanted("surfel") {
        let discs: usize = surfel_payloads
            .iter()
            .map(|(_, _, payload)| payload.surface.surfels.surfels.len())
            .sum();
        println!("surfel geometry: {discs} discs");

        // How much of the flat 360-light frame array a chunk actually needs.
        // Every pipeline currently passes the whole list, so this is the size
        // of the win available to `light_first`/`light_count`.
        let lights = world.frame.active_scene_lights();
        let mut counts: Vec<usize> = Vec::new();
        for chunk in &surfel_chunks {
            let min = chunk.origin;
            let max = [
                min[0] + chunk.mesh.bounds.max[0],
                min[1] + chunk.mesh.bounds.max[1],
                min[2] + chunk.mesh.bounds.max[2],
            ];
            let reaching = lights
                .iter()
                .filter(|light| light.enabled)
                .filter(|light| {
                    let mut d2 = 0.0f32;
                    for axis in 0..3 {
                        let p = light.position[axis];
                        let clamped = p.clamp(min[axis], max[axis]);
                        d2 += (p - clamped) * (p - clamped);
                    }
                    d2 <= light.radius * light.radius
                })
                .count();
            counts.push(reaching);
        }
        counts.sort_unstable();
        let total: usize = counts.iter().sum();
        println!(
            "lights per chunk: min {} median {} max {} mean {:.1} (of {} in the frame)",
            counts.first().copied().unwrap_or(0),
            counts[counts.len() / 2],
            counts.last().copied().unwrap_or(0),
            total as f32 / counts.len() as f32,
            lights.len(),
        );
    }

    if wanted("surface") {
        let mut surface = SurfacePipeline::new(
            &device,
            TARGET_FORMAT,
            frame_resources.layout(),
            MAX_DRAW_DISTANCE,
        );
        surface.upload(&device, &surface_chunks);
        time_renderer(&device, &queue, &targets, frames, warmup, |encoder| {
            surface.draw(
                &queue,
                encoder,
                &targets.color,
                &targets.depth,
                &frame_resources,
                &world.frame,
                toggles,
                light_count,
                FOV_TAN,
                aspect,
            );
        })
        .report("surface");
    }

    if wanted("splat") {
        let mut splat = SplatPipeline::new(
            &device,
            TARGET_FORMAT,
            frame_resources.layout(),
            MAX_DRAW_DISTANCE,
            FACE_BUDGET,
        );
        splat.upload(&device, &surface_chunks);
        time_renderer(&device, &queue, &targets, frames, warmup, |encoder| {
            splat.draw(
                &queue,
                encoder,
                &targets.color,
                &targets.depth,
                &frame_resources,
                &world.frame,
                toggles,
                light_count,
                FOV_TAN,
                aspect,
            );
        })
        .report("splat");
    }

    if wanted("surfel") {
        let mut surfel = SurfelPipeline::new(
            &device,
            TARGET_FORMAT,
            frame_resources.layout(),
            MAX_DRAW_DISTANCE,
            FACE_BUDGET,
        );
        surfel.upload(&device, &surfel_chunks);
        // Isolates the light loop from raster and fill: with zero lights the
        // vertex stage still runs, still expands the quad, still writes
        // depth -- it just stops summing 360 lights per corner.
        let surfel_lights = env_parse::<u32>("SURFEL_LIGHTS").unwrap_or(light_count);
        time_renderer(&device, &queue, &targets, frames, warmup, |encoder| {
            surfel.draw(
                &queue,
                encoder,
                &targets.color,
                &targets.depth,
                &frame_resources,
                &world.frame,
                toggles,
                surfel_lights,
                FOV_TAN,
                aspect,
            );
        })
        .report("surfel");
    }

    if wanted("raymarch") {
        let mut raymarch = RaymarchPipeline::new(
            &device,
            TARGET_FORMAT,
            frame_resources.layout(),
            world.chunks.len(),
        );
        raymarch.configure(RaymarchRuntimeOptions::new(MAX_DRAW_DISTANCE, true, 4));
        raymarch.upload_atlas(&device, &world.atlas);
        time_renderer(&device, &queue, &targets, frames, warmup, |encoder| {
            raymarch.draw(
                &queue,
                encoder,
                &targets.color,
                &frame_resources,
                &world.frame,
                &world.chunks,
                toggles,
            );
        })
        .report("raymarch");
    }
}

/// Render targets are created once so per-frame timings measure drawing,
/// not allocation.
struct Targets {
    color: wgpu::TextureView,
    depth: wgpu::TextureView,
}

impl Targets {
    fn new(device: &wgpu::Device, width: u32, height: u32) -> Self {
        let extent = wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        };
        let color = device
            .create_texture(&wgpu::TextureDescriptor {
                label: Some("vackrooms.timing-color"),
                size: extent,
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: TARGET_FORMAT,
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
                view_formats: &[],
            })
            .create_view(&Default::default());
        let depth = device
            .create_texture(&wgpu::TextureDescriptor {
                label: Some("vackrooms.timing-depth"),
                size: extent,
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::Depth32Float,
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
                view_formats: &[],
            })
            .create_view(&Default::default());
        Self { color, depth }
    }
}

/// Submits one frame at a time and blocks until the GPU reports it done,
/// so each sample is a full frame latency rather than queue-submit time.
/// There is no readback: only drawing is measured.
fn time_renderer(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    _targets: &Targets,
    frames: usize,
    warmup: usize,
    mut draw: impl FnMut(&mut wgpu::CommandEncoder),
) -> Timings {
    let mut samples = Vec::with_capacity(frames);
    for index in 0..(warmup + frames) {
        let started = Instant::now();
        let mut encoder =
            device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
        draw(&mut encoder);
        queue.submit([encoder.finish()]);
        device
            .poll(wgpu::PollType::wait_indefinitely())
            .expect("device poll for frame timing");
        if index >= warmup {
            samples.push(started.elapsed().as_secs_f64() * 1000.0);
        }
    }
    samples.sort_by(f64::total_cmp);
    Timings(samples)
}

/// Same generation path as `vulkan_snapshot`, widened to a configurable
/// radius so the timed resident set matches what the engine streams.
fn load_world(seed: u32, radius: i32, policy: LodPolicy) -> LoadedWorld {
    let config = GeneratorConfig::low_spec();
    let spawn = spawn_point(seed);
    let yaw = -std::f32::consts::FRAC_PI_2;
    let origin_x = (spawn.x / config.chunk_size).floor() * config.chunk_size;
    let origin_z = (spawn.z / config.chunk_size).floor() * config.chunk_size;
    let source = LocalChunkSource::new(SimpleNoiseProvider::new(), seed, config);

    let started = Instant::now();
    let mut payloads = Vec::new();
    for dz in -radius..=radius {
        for dx in -radius..=radius {
            let x = origin_x + dx as f32 * config.chunk_size;
            let z = origin_z + dz as f32 * config.chunk_size;
            // Distance from the camera to the chunk centre, the same
            // measure `Engine::chunk_dist2` refines against.
            let centre_x = x + config.chunk_size * 0.5;
            let centre_z = z + config.chunk_size * 0.5;
            let distance = ((centre_x - spawn.x).powi(2) + (centre_z - spawn.z).powi(2)).sqrt();
            payloads.push((x, z, source.load(x, z, 0, policy.lod_for(distance))));
        }
    }
    let build_ms = started.elapsed().as_secs_f64() * 1000.0;

    let max_rows = payloads
        .iter()
        .map(|(_, _, payload)| payload_rows(payload))
        .max()
        .expect("neighborhood is never empty");
    let mut pool = AtlasPool::new();
    pool.ensure_layout(payloads.len(), max_rows);

    let mut atlas = Vec::new();
    let mut chunks = Vec::new();
    let mut unique_lights = HashMap::new();
    for (x, z, payload) in &payloads {
        let key = chunk_key(*x, *z);
        let slot = pool.assign(key).expect("atlas sized for every chunk");
        let offset = slot * pool.slot_nodes();
        atlas.extend(pool.rebased_block(slot, payload));
        chunks.push(ChunkDraw {
            origin: [*x, 0.0, *z],
            root_index: (offset + payload.root as usize) as i32,
            world_size: payload.world_size,
            voxel_size: payload.voxel_size,
            svo_depth: payload.svo_depth,
        });
        for light in &payload.lights {
            let quantized = [
                (light.position[0] * 16.0) as i32,
                (light.position[1] * 16.0) as i32,
                (light.position[2] * 16.0) as i32,
            ];
            unique_lights.entry(quantized).or_insert(*light);
        }
    }

    // `LIGHTS=n` caps the analytic emitters kept for the frame, isolating
    // per-fragment shading cost from geometry cost.
    let mut scene_lights: Vec<_> = unique_lights.into_values().collect();
    if let Some(cap) = env_parse::<usize>("LIGHTS") {
        // Nearest-first so a small cap still lights the visible corridor.
        scene_lights.sort_by(|a, b| {
            let da = (a.position[0] - spawn.x).powi(2) + (a.position[2] - spawn.z).powi(2);
            let db = (b.position[0] - spawn.x).powi(2) + (b.position[2] - spawn.z).powi(2);
            da.total_cmp(&db)
        });
        scene_lights.truncate(cap);
    }

    let frame = FrameParams {
        camera_pos: [spawn.x, 1.7, spawn.z],
        yaw,
        pitch: 0.0,
        scene_lights,
        environment: Environment::interior(),
        ..FrameParams::default()
    };

    LoadedWorld {
        payloads,
        chunks,
        atlas,
        frame,
        build_ms,
    }
}

fn env_parse<T: std::str::FromStr>(key: &str) -> Option<T> {
    std::env::var(key).ok().and_then(|value| value.parse().ok())
}

fn size() -> (u32, u32) {
    let raw = std::env::var("SIZE").unwrap_or_default();
    let parsed = raw
        .split_once(['x', 'X'])
        .and_then(|(w, h)| Some((w.trim().parse::<u32>().ok()?, h.trim().parse::<u32>().ok()?)));
    let (width, height) = parsed.unwrap_or((960, 540));
    (width.max(64) & !63, height.max(1))
}

fn create_vulkan_device() -> (wgpu::Device, wgpu::Queue) {
    let mut descriptor = wgpu::InstanceDescriptor::new_without_display_handle();
    descriptor.backends = wgpu::Backends::VULKAN;
    let instance = wgpu::Instance::new(descriptor);
    let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
        power_preference: wgpu::PowerPreference::None,
        force_fallback_adapter: false,
        compatible_surface: None,
        apply_limit_buckets: false,
    }))
    .unwrap_or_else(|error| {
        panic!("no Vulkan adapter; install Mesa (Lavapipe) or expose a Vulkan GPU: {error}")
    });
    let info = adapter.get_info();
    eprintln!(
        "Vulkan adapter: {} ({}, {})",
        info.name, info.driver, info.driver_info
    );
    let limits = wgpu::Limits::default().or_worse_values_from(&adapter.limits());
    let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
        label: Some("vackrooms.frame-timing"),
        required_features: wgpu::Features::empty(),
        required_limits: limits,
        ..Default::default()
    }))
    .unwrap_or_else(|error| panic!("failed to create the Vulkan device: {error}"));
    (device, queue)
}
