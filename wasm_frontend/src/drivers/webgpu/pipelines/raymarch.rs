//! Fullscreen SVO traversal pipeline and atlas lifecycle.

use std::num::NonZeroU64;

use wgpu::util::DeviceExt;

use crate::adapters::cpu_splatter::atlas::{MipNode, build_mips};
use crate::application::ports::{ChunkDraw, FrameParams};
use crate::application::render_settings::RenderToggles;
use crate::application::rendering::encode_display_color;

use crate::drivers::webgpu::frame_resources::FrameResources;
use crate::drivers::webgpu::gpu_types::{
    collect_frame_lights, ChunkGrid, GpuLight, GpuRayChunk, GpuRayScene, LightGrid,
};
use crate::drivers::webgpu::shader::ShaderProgram;

const ATLAS_TEXELS_PER_ROW: u64 = 1024;
const WORDS_PER_NODE: u64 = 4;

/// Mutable profile values which affect ray-scene selection and secondary rays.
/// Keeping them separate from GPU object construction lets quality profiles be
/// changed without rebuilding the pipeline.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RaymarchRuntimeOptions {
    maximum_draw_distance: f32,
    direct_light_visibility: bool,
    area_light_visibility_samples: u8,
}

impl RaymarchRuntimeOptions {
    pub fn new(
        maximum_draw_distance: f32,
        direct_light_visibility: bool,
        area_light_visibility_samples: u8,
    ) -> Self {
        Self {
            maximum_draw_distance: if maximum_draw_distance.is_finite()
                && maximum_draw_distance > 0.0
            {
                maximum_draw_distance
            } else {
                f32::INFINITY
            },
            direct_light_visibility,
            area_light_visibility_samples: if area_light_visibility_samples == 4 {
                4
            } else {
                1
            },
        }
    }
}

impl Default for RaymarchRuntimeOptions {
    fn default() -> Self {
        Self::new(f32::INFINITY, false, 1)
    }
}

pub struct RaymarchPipeline {
    pipeline: wgpu::RenderPipeline,
    scene_layout: wgpu::BindGroupLayout,
    atlas: wgpu::Buffer,
    chunks: wgpu::Buffer,
    grid: wgpu::Buffer,
    scene: wgpu::Buffer,
    /// Mip pyramid, one `vec4<u32>` per atlas node (see `pack_ray_mips`).
    mips: wgpu::Buffer,
    /// Light grid, packed offsets then indices (see `LightGrid::packed`).
    light_grid: wgpu::Buffer,
    /// CPU copy of the atlas words, so row updates can rebuild the mips.
    atlas_words: Vec<u32>,
    bind_group: wgpu::BindGroup,
    atlas_word_capacity: usize,
    atlas_initialized: bool,
    max_chunks: usize,
    runtime_options: RaymarchRuntimeOptions,
}

impl RaymarchPipeline {
    pub fn new(
        device: &wgpu::Device,
        target_format: wgpu::TextureFormat,
        frame_layout: &wgpu::BindGroupLayout,
        max_chunks: usize,
    ) -> Self {
        let max_chunks = max_chunks.clamp(1, 256);
        let scene_layout = create_scene_layout(device);
        let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("webgpu.raymarch.pipeline-layout"),
            bind_group_layouts: &[Some(frame_layout), Some(&scene_layout)],
            immediate_size: 0,
        });
        let shader = ShaderProgram::Raymarch.create_module(device);
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("webgpu.raymarch.pipeline"),
            layout: Some(&layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("raymarch_vertex"),
                compilation_options: Default::default(),
                buffers: &[],
            },
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleList,
                cull_mode: None,
                ..Default::default()
            },
            depth_stencil: None,
            multisample: Default::default(),
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("raymarch_fragment"),
                compilation_options: Default::default(),
                targets: &[Some(target_format.into())],
            }),
            multiview_mask: None,
            cache: None,
        });
        let atlas_word_capacity = WORDS_PER_NODE as usize;
        let atlas = create_atlas_buffer(device, &[0; WORDS_PER_NODE as usize]);
        let chunks = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("webgpu.raymarch.chunks"),
            size: (max_chunks * size_of::<GpuRayChunk>()) as u64,
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let grid = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("webgpu.raymarch.chunk-grid"),
            size: (ChunkGrid::MAX_CELLS * size_of::<u32>()) as u64,
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let scene = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("webgpu.raymarch.scene"),
            size: size_of::<GpuRayScene>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let mips = create_mip_buffer(device, &[0; WORDS_PER_NODE as usize]);
        let light_grid = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("webgpu.raymarch.light-grid"),
            size: ((LightGrid::MAX_CELLS + 1 + LightGrid::MAX_LIGHTS) * size_of::<u32>())
                as u64,
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let bind_group = create_bind_group(
            device,
            &scene_layout,
            &atlas,
            &chunks,
            &grid,
            &scene,
            &mips,
            &light_grid,
        );
        Self {
            pipeline,
            scene_layout,
            atlas,
            chunks,
            grid,
            scene,
            mips,
            light_grid,
            atlas_words: Vec::new(),
            bind_group,
            atlas_word_capacity,
            atlas_initialized: false,
            max_chunks,
            runtime_options: RaymarchRuntimeOptions::default(),
        }
    }

    pub fn configure(&mut self, options: RaymarchRuntimeOptions) {
        self.runtime_options = options;
    }

    pub fn upload_atlas(&mut self, device: &wgpu::Device, texels: &[u32]) {
        let words: &[u32] = if texels.is_empty() {
            &[0, 0, 0, 0]
        } else {
            texels
        };
        self.atlas = create_atlas_buffer(device, words);
        self.atlas_word_capacity = words.len();
        self.atlas_initialized = !texels.is_empty();
        self.atlas_words = words.to_vec();
        self.mips = create_mip_buffer(device, &pack_ray_mips(words));
        self.bind_group = create_bind_group(
            device,
            &self.scene_layout,
            &self.atlas,
            &self.chunks,
            &self.grid,
            &self.scene,
            &self.mips,
            &self.light_grid,
        );
    }

    pub fn upload_atlas_rows(
        &mut self,
        queue: &wgpu::Queue,
        first_row: u32,
        texels: &[u32],
    ) -> bool {
        let row_words = (ATLAS_TEXELS_PER_ROW * WORDS_PER_NODE) as usize;
        if !self.atlas_initialized || texels.is_empty() || texels.len() % row_words != 0 {
            return false;
        }
        let first_word = first_row as usize * row_words;
        let Some(end_word) = first_word.checked_add(texels.len()) else {
            return false;
        };
        if end_word > self.atlas_word_capacity {
            return false;
        }
        // Rebuild the mips from the updated words before writing anything,
        // so a row update can never leave the pyramid stale. The node count
        // cannot change on a row update, so the mip buffer keeps its size.
        let mut words = self.atlas_words.clone();
        if end_word > words.len() {
            return false;
        }
        words[first_word..end_word].copy_from_slice(texels);
        let mip_words = pack_ray_mips(&words);
        if mip_words.len() * size_of::<u32>() != self.mips.size() as usize {
            return false;
        }
        queue.write_buffer(
            &self.atlas,
            (first_word * size_of::<u32>()) as u64,
            bytemuck::cast_slice(texels),
        );
        queue.write_buffer(&self.mips, 0, bytemuck::cast_slice(&mip_words));
        self.atlas_words = words;
        true
    }

    #[allow(clippy::too_many_arguments)]
    pub fn draw(
        &self,
        queue: &wgpu::Queue,
        encoder: &mut wgpu::CommandEncoder,
        target: &wgpu::TextureView,
        frame_resources: &FrameResources,
        frame: &FrameParams,
        chunks: &[ChunkDraw],
        toggles: RenderToggles,
    ) {
        let chunks: Vec<_> = select_ray_chunks(
            chunks,
            frame.camera_pos,
            self.max_chunks,
            toggles.front_to_back,
            toggles.distance_cull,
            self.runtime_options.maximum_draw_distance,
        )
        .into_iter()
        .map(|(source_index, chunk)| pack_ray_chunk(source_index, chunk))
        .collect();
        if !chunks.is_empty() {
            queue.write_buffer(&self.chunks, 0, bytemuck::cast_slice(&chunks));
        }
        let grid = derive_chunk_grid(&chunks);
        if let Some(grid) = &grid {
            queue.write_buffer(&self.grid, 0, bytemuck::cast_slice(&grid.map));
        }
        // The light grid is derived from the same frame lights the shared
        // resources upload, so its indices address the lights buffer.
        let light_grid = derive_light_grid(&collect_frame_lights(frame));
        if let Some(light_grid) = &light_grid {
            queue.write_buffer(
                &self.light_grid,
                0,
                bytemuck::cast_slice(&light_grid.packed()),
            );
        }
        queue.write_buffer(
            &self.scene,
            0,
            bytemuck::bytes_of(&GpuRayScene::from_policy(
                chunks.len() as u32,
                toggles.empty_space_skip,
                toggles.front_to_back,
                direct_light_visibility_enabled(
                    self.runtime_options.direct_light_visibility,
                    toggles.shadow_pass,
                ),
                self.runtime_options.area_light_visibility_samples,
                grid.as_ref(),
                light_grid.as_ref(),
            )),
        );

        let environment = frame.environment;
        let clear_linear = if environment.outdoor {
            environment.sky_color
        } else {
            environment.fog_color
        };
        let clear = encode_display_color(clear_linear);
        let color_attachments = [Some(wgpu::RenderPassColorAttachment {
            view: target,
            resolve_target: None,
            depth_slice: None,
            ops: wgpu::Operations {
                load: wgpu::LoadOp::Clear(wgpu::Color {
                    r: clear[0] as f64,
                    g: clear[1] as f64,
                    b: clear[2] as f64,
                    a: 1.0,
                }),
                store: wgpu::StoreOp::Store,
            },
        })];
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("webgpu.raymarch.pass"),
            color_attachments: &color_attachments,
            ..Default::default()
        });
        pass.set_pipeline(&self.pipeline);
        pass.set_bind_group(0, frame_resources.bind_group(), &[]);
        pass.set_bind_group(1, &self.bind_group, &[]);
        pass.draw(0..3, 0..1);
    }
}

const fn direct_light_visibility_enabled(profile_capability: bool, rt_shadows: bool) -> bool {
    profile_capability && rt_shadows
}

/// Selects the bounded resident set before packing it for the GPU. Disabled
/// policy preserves exact source order; enabled policy uses stable distance
/// to each complete chunk cube, so a large cube containing the camera wins
/// over a smaller cube whose origin merely happens to be nearby.
fn select_ray_chunks(
    chunks: &[ChunkDraw],
    camera_position: [f32; 3],
    maximum: usize,
    front_to_back: bool,
    distance_cull: bool,
    maximum_draw_distance: f32,
) -> Vec<(usize, &ChunkDraw)> {
    let mut selected: Vec<_> = chunks.iter().enumerate().collect();
    if distance_cull {
        let maximum_distance_squared = maximum_draw_distance * maximum_draw_distance;
        selected.retain(|(_, chunk)| {
            let distance_squared = distance_squared_to_chunk(camera_position, chunk);
            distance_squared.is_finite()
                && (maximum_distance_squared.is_infinite()
                    || distance_squared <= maximum_distance_squared)
        });
    }
    if front_to_back {
        selected.sort_by(|(left_index, left), (right_index, right)| {
            distance_squared_to_chunk(camera_position, left)
                .total_cmp(&distance_squared_to_chunk(camera_position, right))
                .then_with(|| left_index.cmp(right_index))
        });
    }
    selected.truncate(maximum);
    selected
}

fn distance_squared_to_chunk(camera_position: [f32; 3], chunk: &ChunkDraw) -> f32 {
    if camera_position.iter().any(|value| !value.is_finite())
        || chunk.origin.iter().any(|value| !value.is_finite())
        || !chunk.world_size.is_finite()
        || chunk.world_size <= 0.0
    {
        return f32::INFINITY;
    }

    let mut distance_squared = 0.0;
    for axis in 0..3 {
        let minimum = chunk.origin[axis];
        let maximum = minimum + chunk.world_size;
        if !maximum.is_finite() {
            return f32::INFINITY;
        }
        let delta = if camera_position[axis] < minimum {
            minimum - camera_position[axis]
        } else if camera_position[axis] > maximum {
            camera_position[axis] - maximum
        } else {
            0.0
        };
        distance_squared += delta * delta;
    }
    if distance_squared.is_finite() {
        distance_squared
    } else {
        f32::INFINITY
    }
}

fn pack_ray_chunk(source_index: usize, chunk: &ChunkDraw) -> GpuRayChunk {
    GpuRayChunk {
        origin_world_size: [
            chunk.origin[0],
            chunk.origin[1],
            chunk.origin[2],
            chunk.world_size,
        ],
        voxel_depth: [chunk.voxel_size, f32::from(chunk.svo_depth), 0.0, 0.0],
        // Keep the stable source ordinal so reordering cannot change which
        // overlapping chunk wins an exactly equal-distance hit.
        indices: [
            chunk.root_index.max(0) as u32,
            source_index.min(u32::MAX as usize) as u32,
            0,
            0,
        ],
    }
}

/// sRGB encode of one linear channel, 0..255. Mirrors the shader's
/// `packed_srgb_to_linear` in reverse, so a mip hit shades exactly like
/// a leaf whose packed color holds the same sRGB triple.
fn linear_channel_to_srgb(channel: f32) -> u32 {
    let clamped = channel.clamp(0.0, 1.0);
    let encoded = if clamped <= 0.0031308 {
        12.92 * clamped
    } else {
        1.055 * clamped.powf(1.0 / 2.4) - 0.055
    };
    (encoded * 255.0).round().clamp(0.0, 255.0) as u32
}

/// One mip node as the shader reads it: `vec4<u32>` per atlas node, same
/// index as `atlas_words / 4`. x holds the packed sRGB albedo (0xRRGGBB,
/// the leaf color format), y the dominant voxel material, z the emissive
/// coverage as f32 bits, w is reserved.
fn pack_ray_mip_node(mip: &MipNode) -> [u32; 4] {
    let [r, g, b] = mip.linear_albedo;
    let packed = (linear_channel_to_srgb(r) << 16)
        | (linear_channel_to_srgb(g) << 8)
        | linear_channel_to_srgb(b);
    [
        packed,
        mip.dominant_voxel_type,
        mip.emissive_coverage.to_bits(),
        0,
    ]
}

/// The whole mip pyramid as GPU words, one `vec4<u32>` per atlas node.
fn pack_ray_mips(texels: &[u32]) -> Vec<u32> {
    build_mips(texels)
        .iter()
        .flat_map(|mip| pack_ray_mip_node(mip))
        .collect()
}

fn create_scene_layout(device: &wgpu::Device) -> wgpu::BindGroupLayout {
    device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some("webgpu.raymarch.scene-layout"),
        entries: &[
            wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Storage { read_only: true },
                    has_dynamic_offset: false,
                    min_binding_size: NonZeroU64::new(16),
                },
                count: None,
            },
            wgpu::BindGroupLayoutEntry {
                binding: 1,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Storage { read_only: true },
                    has_dynamic_offset: false,
                    min_binding_size: NonZeroU64::new(size_of::<GpuRayChunk>() as u64),
                },
                count: None,
            },
            wgpu::BindGroupLayoutEntry {
                binding: 2,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: NonZeroU64::new(size_of::<GpuRayScene>() as u64),
                },
                count: None,
            },
            wgpu::BindGroupLayoutEntry {
                binding: 3,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Storage { read_only: true },
                    has_dynamic_offset: false,
                    min_binding_size: NonZeroU64::new(size_of::<u32>() as u64),
                },
                count: None,
            },
            wgpu::BindGroupLayoutEntry {
                binding: 4,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Storage { read_only: true },
                    has_dynamic_offset: false,
                    min_binding_size: NonZeroU64::new(16),
                },
                count: None,
            },
            wgpu::BindGroupLayoutEntry {
                binding: 5,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Storage { read_only: true },
                    has_dynamic_offset: false,
                    min_binding_size: NonZeroU64::new(size_of::<u32>() as u64),
                },
                count: None,
            },
        ],
    })
}

fn create_atlas_buffer(device: &wgpu::Device, words: &[u32]) -> wgpu::Buffer {
    device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
        label: Some("webgpu.raymarch.atlas"),
        contents: bytemuck::cast_slice(words),
        usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
    })
}

fn create_mip_buffer(device: &wgpu::Device, words: &[u32]) -> wgpu::Buffer {
    device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
        label: Some("webgpu.raymarch.mips"),
        contents: bytemuck::cast_slice(words),
        usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
    })
}

#[allow(clippy::too_many_arguments)]
fn create_bind_group(
    device: &wgpu::Device,
    layout: &wgpu::BindGroupLayout,
    atlas: &wgpu::Buffer,
    chunks: &wgpu::Buffer,
    grid: &wgpu::Buffer,
    scene: &wgpu::Buffer,
    mips: &wgpu::Buffer,
    light_grid: &wgpu::Buffer,
) -> wgpu::BindGroup {
    device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("webgpu.raymarch.scene-bind-group"),
        layout,
        entries: &[
            wgpu::BindGroupEntry {
                binding: 0,
                resource: atlas.as_entire_binding(),
            },
            wgpu::BindGroupEntry {
                binding: 1,
                resource: chunks.as_entire_binding(),
            },
            wgpu::BindGroupEntry {
                binding: 2,
                resource: scene.as_entire_binding(),
            },
            wgpu::BindGroupEntry {
                binding: 3,
                resource: grid.as_entire_binding(),
            },
            wgpu::BindGroupEntry {
                binding: 4,
                resource: mips.as_entire_binding(),
            },
            wgpu::BindGroupEntry {
                binding: 5,
                resource: light_grid.as_entire_binding(),
            },
        ],
    })
}

/// Certifies the packed resident chunks as a regular lattice for the
/// shader's top-level DDA walk: one chunk per (x, z) cell, one shared
/// cube size, one y layer, every origin on the lattice. Anything else —
/// mixed cube sizes, off-lattice origins, a second chunk in one cell,
/// or a bounding box too sparse to map — returns `None` and the shader
/// falls back to box-testing every chunk.
fn derive_chunk_grid(chunks: &[GpuRayChunk]) -> Option<ChunkGrid> {
    if chunks.is_empty() || chunks.len() > 256 {
        return None;
    }
    let cube_size = chunks[0].origin_world_size[3];
    if !cube_size.is_finite() || cube_size <= 0.0 {
        return None;
    }
    if chunks.iter().any(|chunk| {
        !chunk.origin_world_size[0].is_finite()
            || !chunk.origin_world_size[1].is_finite()
            || !chunk.origin_world_size[2].is_finite()
            || chunk.origin_world_size[3] != cube_size
    }) {
        return None;
    }
    let origin_y = chunks[0].origin_world_size[1];
    if chunks
        .iter()
        .any(|chunk| chunk.origin_world_size[1] != origin_y)
    {
        return None;
    }
    let cell_x = lattice_spacing(chunks.iter().map(|chunk| chunk.origin_world_size[0]))
        .unwrap_or(cube_size);
    let cell_z = lattice_spacing(chunks.iter().map(|chunk| chunk.origin_world_size[2]))
        .unwrap_or(cube_size);
    if !cell_x.is_finite() || cell_x <= 0.0 || !cell_z.is_finite() || cell_z <= 0.0 {
        return None;
    }
    let min_x = chunks
        .iter()
        .map(|chunk| chunk.origin_world_size[0])
        .fold(f32::INFINITY, f32::min);
    let max_x = chunks
        .iter()
        .map(|chunk| chunk.origin_world_size[0])
        .fold(f32::NEG_INFINITY, f32::max);
    let min_z = chunks
        .iter()
        .map(|chunk| chunk.origin_world_size[2])
        .fold(f32::INFINITY, f32::min);
    let max_z = chunks
        .iter()
        .map(|chunk| chunk.origin_world_size[2])
        .fold(f32::NEG_INFINITY, f32::max);
    let dims_x = ((max_x - min_x) / cell_x).round() as usize + 1;
    let dims_z = ((max_z - min_z) / cell_z).round() as usize + 1;
    if dims_x == 0 || dims_z == 0 || dims_x * dims_z > ChunkGrid::MAX_CELLS {
        return None;
    }
    let mut map = vec![u32::MAX; dims_x * dims_z];
    for (index, chunk) in chunks.iter().enumerate() {
        let cell_x_index = ((chunk.origin_world_size[0] - min_x) / cell_x).round();
        let cell_z_index = ((chunk.origin_world_size[2] - min_z) / cell_z).round();
        if (cell_x_index - ((chunk.origin_world_size[0] - min_x) / cell_x)).abs() > 1e-3
            || (cell_z_index - ((chunk.origin_world_size[2] - min_z) / cell_z)).abs() > 1e-3
            || cell_x_index < 0.0
            || cell_z_index < 0.0
            || cell_x_index >= dims_x as f32
            || cell_z_index >= dims_z as f32
        {
            return None;
        }
        let slot = cell_z_index as usize * dims_x + cell_x_index as usize;
        if map[slot] != u32::MAX {
            return None;
        }
        map[slot] = index as u32;
    }
    Some(ChunkGrid {
        origin: [min_x, origin_y, min_z],
        cube_size,
        cell: [cell_x, cell_z],
        dims: [dims_x as u32, dims_z as u32],
        map,
    })
}

/// Certifies the frame's lights as a spatial index for the shader's
/// direct-light loop: each light lives in exactly one cell (the cell
/// containing its position), and a receiver looks up only the cells its
/// reach can touch. Anything else — an empty set, too many lights, or a
/// grid too sparse to map — returns `None` and the shader falls back to
/// iterating every light.
fn derive_light_grid(lights: &[GpuLight]) -> Option<LightGrid> {
    if lights.is_empty() || lights.len() > LightGrid::MAX_LIGHTS {
        return None;
    }
    // A light can only reach a receiver within `radius + max(half_size)`
    // of its own position (the rect test measures to the fixture's AABB,
    // which sits within half_size of the center), so the reach is the
    // maximum of that sum over the set.
    let mut reach = 0.0f32;
    for light in lights {
        if !light.position_radius[0].is_finite()
            || !light.position_radius[1].is_finite()
            || !light.position_radius[2].is_finite()
            || !light.position_radius[3].is_finite()
            || light.position_radius[3] < 0.0
            || !light.half_size_kind[0].is_finite()
            || !light.half_size_kind[1].is_finite()
            || light.half_size_kind[0] < 0.0
            || light.half_size_kind[1] < 0.0
        {
            return None;
        }
        let light_reach = light.position_radius[3]
            + light.half_size_kind[0].max(light.half_size_kind[1]);
        reach = reach.max(light_reach);
    }
    if !reach.is_finite() {
        return None;
    }
    // One square cell per reach: a receiver's lookup is always its own
    // cell plus one ring of neighbours, whatever the light range is.
    let cell = reach.max(1.0);
    let min_x = lights
        .iter()
        .map(|light| light.position_radius[0])
        .fold(f32::INFINITY, f32::min);
    let max_x = lights
        .iter()
        .map(|light| light.position_radius[0])
        .fold(f32::NEG_INFINITY, f32::max);
    let min_z = lights
        .iter()
        .map(|light| light.position_radius[2])
        .fold(f32::INFINITY, f32::min);
    let max_z = lights
        .iter()
        .map(|light| light.position_radius[2])
        .fold(f32::NEG_INFINITY, f32::max);
    let dims_x = ((max_x - min_x) / cell).floor() as usize + 1;
    let dims_z = ((max_z - min_z) / cell).floor() as usize + 1;
    if dims_x == 0 || dims_z == 0 || dims_x * dims_z > LightGrid::MAX_CELLS {
        return None;
    }
    let mut per_cell: Vec<Vec<u32>> = vec![Vec::new(); dims_x * dims_z];
    for (index, light) in lights.iter().enumerate() {
        let cell_x = ((light.position_radius[0] - min_x) / cell).floor() as usize;
        let cell_z = ((light.position_radius[2] - min_z) / cell).floor() as usize;
        if cell_x >= dims_x || cell_z >= dims_z {
            return None;
        }
        per_cell[cell_z * dims_x + cell_x].push(index as u32);
    }
    let mut offsets = Vec::with_capacity(dims_x * dims_z + 1);
    let mut indices = Vec::with_capacity(lights.len());
    for cell_lights in &per_cell {
        offsets.push(indices.len() as u32);
        indices.extend_from_slice(cell_lights);
    }
    offsets.push(indices.len() as u32);
    Some(LightGrid {
        origin: [min_x, 0.0, min_z],
        cell: [cell, cell],
        dims: [dims_x as u32, dims_z as u32],
        reach,
        offsets,
        indices,
    })
}

/// Lights the shader's grid lookup would visit for a receiver: the
/// indices in every cell the `[receiver ± reach]` square overlaps.
/// This is the CPU mirror of the WGSL loop, used by the tests to prove
/// the lookup set always contains every light within reach.
#[cfg(test)]
fn lights_near(grid: &LightGrid, receiver: [f32; 3]) -> Vec<u32> {
    let dims_x = grid.dims[0] as i32;
    let dims_z = grid.dims[1] as i32;
    if dims_x <= 0 || dims_z <= 0 || grid.cell[0] <= 0.0 || grid.cell[1] <= 0.0 {
        return Vec::new();
    }
    let min_cx = ((receiver[0] - grid.reach - grid.origin[0]) / grid.cell[0]).floor() as i32;
    let max_cx = ((receiver[0] + grid.reach - grid.origin[0]) / grid.cell[0]).floor() as i32;
    let min_cz = ((receiver[2] - grid.reach - grid.origin[2]) / grid.cell[1]).floor() as i32;
    let max_cz = ((receiver[2] + grid.reach - grid.origin[2]) / grid.cell[1]).floor() as i32;
    let mut out = Vec::new();
    for cz in min_cz.max(0)..=max_cz.min(dims_z - 1) {
        for cx in min_cx.max(0)..=max_cx.min(dims_x - 1) {
            let cell = (cz * dims_x + cx) as usize;
            let start = grid.offsets[cell] as usize;
            let end = grid.offsets[cell + 1] as usize;
            out.extend_from_slice(&grid.indices[start..end]);
        }
    }
    out
}

/// Smallest positive gap between distinct coordinates, or `None` when
/// there is only one distinct coordinate (a single column or row).
fn lattice_spacing(values: impl Iterator<Item = f32>) -> Option<f32> {
    let mut sorted: Vec<f32> = values.collect();
    sorted.sort_by(|a, b| a.total_cmp(b));
    sorted.dedup_by(|a, b| (*a - *b).abs() <= 1e-3);
    let mut spacing: Option<f32> = None;
    for pair in sorted.windows(2) {
        let gap = pair[1] - pair[0];
        if gap > 1e-3 && gap.is_finite() {
            spacing = Some(spacing.map_or(gap, |best: f32| best.min(gap)));
        }
    }
    spacing
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chunk(origin: [f32; 3], world_size: f32, root_index: i32) -> ChunkDraw {
        ChunkDraw {
            origin,
            root_index,
            world_size,
            voxel_size: 0.25,
            svo_depth: 6,
        }
    }

    #[test]
    fn front_to_back_selects_nearest_candidates_before_the_chunk_cap() {
        let chunks = [
            chunk([20.0, 0.0, 0.0], 1.0, 10),
            chunk([-1.0, -1.0, -1.0], 2.0, 20),
            chunk([5.0, 0.0, 0.0], 1.0, 30),
        ];
        let selected = select_ray_chunks(&chunks, [0.0; 3], 2, true, false, f32::INFINITY);
        assert_eq!(
            selected
                .iter()
                .map(|(index, chunk)| (*index, chunk.root_index))
                .collect::<Vec<_>>(),
            vec![(1, 20), (2, 30)]
        );

        let packed: Vec<_> = selected
            .into_iter()
            .map(|(index, chunk)| pack_ray_chunk(index, chunk))
            .collect();
        assert_eq!(packed[0].indices[..2], [20, 1]);
        assert_eq!(packed[1].indices[..2], [30, 2]);
    }

    #[test]
    fn disabled_front_to_back_preserves_source_order_and_truncation() {
        let chunks = [
            chunk([20.0, 0.0, 0.0], 1.0, 10),
            chunk([0.0, 0.0, 0.0], 1.0, 20),
            chunk([5.0, 0.0, 0.0], 1.0, 30),
        ];
        let selected = select_ray_chunks(&chunks, [0.0; 3], 2, false, false, f32::INFINITY);
        assert_eq!(
            selected
                .iter()
                .map(|(index, chunk)| (*index, chunk.root_index))
                .collect::<Vec<_>>(),
            vec![(0, 10), (1, 20)]
        );
    }

    #[test]
    fn distance_order_uses_complete_bounds_and_stable_source_ties() {
        let chunks = [
            chunk([-10.0, -10.0, -10.0], 20.0, 10),
            chunk([2.0, 0.0, 0.0], 1.0, 20),
            chunk([-3.0, 0.0, 0.0], 1.0, 30),
        ];
        let selected = select_ray_chunks(&chunks, [0.0; 3], 3, true, false, f32::INFINITY);
        assert_eq!(
            selected.iter().map(|(index, _)| *index).collect::<Vec<_>>(),
            vec![0, 1, 2]
        );
    }

    #[test]
    fn distance_culling_filters_complete_bounds_before_the_resident_cap() {
        let chunks = [
            chunk([20.0, 0.0, 0.0], 1.0, 10),
            chunk([2.5, -1.0, -1.0], 2.0, 20),
            chunk([-1.0, -1.0, -1.0], 2.0, 30),
        ];
        let selected = select_ray_chunks(&chunks, [0.0; 3], 2, false, true, 3.0);
        assert_eq!(
            selected
                .iter()
                .map(|(index, chunk)| (*index, chunk.root_index))
                .collect::<Vec<_>>(),
            vec![(1, 20), (2, 30)]
        );
    }

    #[test]
    fn disabled_distance_culling_preserves_source_order() {
        let chunks = [
            chunk([20.0, 0.0, 0.0], 1.0, 10),
            chunk([0.0, 0.0, 0.0], 1.0, 20),
        ];
        let selected = select_ray_chunks(&chunks, [0.0; 3], 2, false, false, 1.0);
        assert_eq!(
            selected.iter().map(|(index, _)| *index).collect::<Vec<_>>(),
            vec![0, 1]
        );
    }

    #[test]
    fn runtime_options_validate_open_distance_and_area_sample_quality() {
        let defaults = RaymarchRuntimeOptions::default();
        assert!(defaults.maximum_draw_distance.is_infinite());
        assert!(!defaults.direct_light_visibility);
        assert_eq!(defaults.area_light_visibility_samples, 1);

        let configured = RaymarchRuntimeOptions::new(80.0, true, 4);
        assert_eq!(configured.maximum_draw_distance, 80.0);
        assert!(configured.direct_light_visibility);
        assert_eq!(configured.area_light_visibility_samples, 4);

        let invalid = RaymarchRuntimeOptions::new(f32::NAN, true, 2);
        assert!(invalid.maximum_draw_distance.is_infinite());
        assert_eq!(invalid.area_light_visibility_samples, 1);
    }

    #[test]
    fn direct_visibility_requires_both_profile_capability_and_runtime_toggle() {
        assert!(direct_light_visibility_enabled(true, true));
        assert!(!direct_light_visibility_enabled(true, false));
        assert!(!direct_light_visibility_enabled(false, true));
        assert!(!direct_light_visibility_enabled(false, false));
    }

    /// The traversal is one stack walk per chunk: the descent path lives on
    /// a fixed-size stack, so no sample ever re-descends from the root. A
    /// solid leaf is resolved by its own box intersection (exact entry
    /// distance and entry-face normal), which is what the old finest-voxel
    /// DDA refinement used to reconstruct — the DDA and its skip-walk
    /// scaffolding are gone, not layered underneath.
    #[test]
    fn shader_traverses_svo_with_one_stack_walk_and_no_re_descent() {
        let source = include_str!("../shaders/raymarch.wgsl");
        assert!(source.contains("fn svo_stack_walk("));
        assert!(source.contains("array<SvoStackFrame, 12>"));
        assert!(source.contains("fn trace_chunk("));
        assert!(source.contains("let walk = svo_stack_walk("));
        assert!(!source.contains("fn lookup_leaf("));
        assert!(!source.contains("fn trace_chunk_dda("));
        assert!(!source.contains("fn trace_chunk_skipping_empty_leaves("));
        assert!(!source.contains("let refinement_interval = BoxHit("));
        assert!(source.contains("abs(times.x - nearest) <= tolerance"));
        assert!(source.contains("abs(times.y - nearest) <= tolerance"));
        assert!(source.contains("chunk.indices.y < nearest_source_index"));
    }

    fn packed(origin: [f32; 3], world_size: f32) -> GpuRayChunk {
        GpuRayChunk {
            origin_world_size: [origin[0], origin[1], origin[2], world_size],
            voxel_depth: [0.25, 6.0, 0.0, 0.0],
            indices: [0, 0, 0, 0],
        }
    }

    #[test]
    fn chunk_grid_certifies_a_regular_lattice() {
        let chunks: Vec<_> = (-1..=1)
            .flat_map(|dz| (-1..=1).map(move |dx| (dx, dz)))
            .map(|(dx, dz)| packed([dx as f32 * 10.0, 0.0, dz as f32 * 10.0], 12.8))
            .collect();
        let grid = derive_chunk_grid(&chunks).expect("3x3 lattice certifies");
        assert_eq!(grid.origin, [-10.0, 0.0, -10.0]);
        assert_eq!(grid.cube_size, 12.8);
        assert_eq!(grid.cell, [10.0, 10.0]);
        assert_eq!(grid.dims, [3, 3]);
        assert_eq!(grid.map, (0..9).collect::<Vec<_>>());
    }

    #[test]
    fn chunk_grid_certifies_a_large_lattice_past_the_old_cap() {
        // 6x6 = 36 chunks: past the old 25-chunk cap, inside the 256-cell
        // grid. The DDA visits only crossed cells, so the per-pixel cost
        // stays bounded as the world grows.
        let chunks: Vec<_> = (-3..=2)
            .flat_map(|dz| (-3..=2).map(move |dx| (dx, dz)))
            .map(|(dx, dz)| packed([dx as f32 * 10.0, 0.0, dz as f32 * 10.0], 12.8))
            .collect();
        assert_eq!(chunks.len(), 36);
        let grid = derive_chunk_grid(&chunks).expect("6x6 lattice certifies");
        assert_eq!(grid.dims, [6, 6]);
        assert_eq!(grid.map.len(), 36);
        assert_eq!(grid.map.iter().filter(|cell| **cell != u32::MAX).count(), 36);
    }

    #[test]
    fn chunk_grid_leaves_a_hole_empty() {
        let chunks: Vec<_> = (-1..=1)
            .flat_map(|dz| (-1..=1).map(move |dx| (dx, dz)))
            .filter(|(dx, dz)| !(*dx == 0 && *dz == 0))
            .map(|(dx, dz)| packed([dx as f32 * 10.0, 0.0, dz as f32 * 10.0], 12.8))
            .collect();
        let grid = derive_chunk_grid(&chunks).expect("lattice with a hole certifies");
        assert_eq!(grid.dims, [3, 3]);
        assert_eq!(grid.map[4], u32::MAX);
        assert_eq!(grid.map.iter().filter(|cell| **cell != u32::MAX).count(), 8);
    }

    #[test]
    fn chunk_grid_single_chunk_uses_cube_as_cell() {
        let grid = derive_chunk_grid(&[packed([4.0, 0.0, -6.0], 12.8)])
            .expect("one chunk certifies");
        assert_eq!(grid.origin, [4.0, 0.0, -6.0]);
        assert_eq!(grid.cell, [12.8, 12.8]);
        assert_eq!(grid.dims, [1, 1]);
        assert_eq!(grid.map, vec![0]);
    }

    #[test]
    fn chunk_grid_rejects_mixed_cube_sizes() {
        let chunks = [
            packed([0.0, 0.0, 0.0], 12.8),
            packed([10.0, 0.0, 0.0], 6.4),
        ];
        assert!(derive_chunk_grid(&chunks).is_none());
    }

    #[test]
    fn chunk_grid_rejects_off_lattice_origins() {
        let chunks = [
            packed([0.0, 0.0, 0.0], 12.8),
            packed([10.0, 0.0, 0.0], 12.8),
            packed([23.0, 0.0, 0.0], 12.8),
        ];
        assert!(derive_chunk_grid(&chunks).is_none());
    }

    #[test]
    fn chunk_grid_rejects_a_second_chunk_in_one_cell() {
        let chunks = [
            packed([0.0, 0.0, 0.0], 12.8),
            packed([0.0, 0.0, 0.0], 12.8),
        ];
        assert!(derive_chunk_grid(&chunks).is_none());
    }

    #[test]
    fn chunk_grid_rejects_an_oversized_bounding_box() {
        // 18x18 origins at 1.0 spacing: 324 cells, past the 256-cell grid.
        let mut chunks = Vec::new();
        for index in 0..18 {
            chunks.push(packed([index as f32, 0.0, 0.0], 12.8));
        }
        for index in 1..18 {
            chunks.push(packed([0.0, 0.0, index as f32], 12.8));
        }
        assert!(derive_chunk_grid(&chunks).is_none());
    }

    #[test]
    fn chunk_grid_rejects_an_empty_resident_set() {
        assert!(derive_chunk_grid(&[]).is_none());
    }

    /// The primary-ray loop walks the chunk grid in ray order and still
    /// never traces a candidate chunk past the closest hit already found
    /// — the clip-to-known-distance reasoning is unchanged, it just lives
    /// in the per-chunk helper the grid walk calls.
    #[test]
    fn primary_rays_clip_the_search_interval_to_the_closest_hit_so_far() {
        let source = include_str!("../shaders/raymarch.wgsl");
        assert!(source.contains("fn chunk_grid_walk_init("));
        assert!(source.contains("fn consider_primary_chunk("));
        let fragment = &source[source.find("@fragment").expect("fragment entry")..];
        assert!(fragment.contains("chunk_grid_walk_init("));
        assert!(fragment.contains("consider_primary_chunk("));
        let helper_start = source
            .find("fn trace_primary_chunk(")
            .expect("primary trace helper");
        let helper_end = source[helper_start..]
            .find("\n}\n")
            .expect("helper end")
            + helper_start;
        let helper = &source[helper_start..helper_end];
        assert!(helper.contains("min(interval.exit, clip_distance)"));
        assert!(helper.contains(
            "return trace_chunk(local_origin, ray_direction, chunk, clipped_interval);",
        ));
        let consider_start = source
            .find("fn consider_primary_chunk(")
            .expect("primary consider helper");
        let consider_end = source[consider_start..]
            .find("\n}\n")
            .expect("helper end")
            + consider_start;
        let consider = &source[consider_start..consider_end];
        assert!(consider.contains(
            "trace_primary_chunk(chunk, ray_origin, ray_direction, nearest.distance)",
        ));
        assert!(consider.contains("chunk.indices.y < nearest_source_index"));
    }

    #[test]
    fn shader_secondary_rays_are_finite_exact_and_lazily_evaluated() {
        let source = include_str!("../shaders/raymarch.wgsl");
        let visibility_start = source
            .find("fn trace_visibility_chunk_stack(")
            .expect("secondary stack trace");
        let visibility_end = source.find("fn camera_ray(").expect("camera ray");
        let visibility = &source[visibility_start..visibility_end];

        assert!(source.contains("DIRECT_VISIBILITY_TRACE_BUDGET: u32 = 4096u"));
        assert!(!visibility.contains("frame.render_options.w"));
        assert!(!visibility.contains("ray_policy_enabled(RAY_POLICY_EMPTY_SPACE_SKIP)"));
        assert!(visibility.contains("fn trace_visibility_chunk_stack("));
        assert!(visibility.contains("svo_stack_walk("));
        assert!(visibility.contains("let biased_origin = receiver_world + receiver_normal"));
        assert!(visibility.contains("let maximum_distance = endpoint_distance - endpoint_bias"));
        // Shadow rays walk the chunk grid in ray order; the per-chunk
        // loop survives only as the fallback for an uncertified lattice.
        assert!(visibility.contains("fn chunk_grid_walk_init("));
        assert!(visibility.contains("fn test_visibility_chunk("));
        assert!(visibility.contains("for (var index = 0u; index < chunk_count; index += 1u)"));
        assert!(visibility.contains("let finite_exit = min(slab.exit, maximum_distance)"));
        assert!(visibility.contains("let trace = test_visibility_chunk("));
        assert!(visibility.contains("if trace.blocked {"));

        assert!(visibility.contains("point_light_sample_irradiance("));
        assert!(visibility.contains("rectangle_gauss_sample_position("));
        assert!(visibility.contains("rectangle_light_sample_weight("));
        assert!(visibility.contains("rectangle_light_irradiance_from_integral("));
        assert!(visibility.contains("let unoccluded = evaluate_rectangle_light("));
        assert!(visibility.contains("if !has_positive_rgb(sample_irradiance)"));

        let fragment = &source[source.find("@fragment").expect("fragment entry")..];
        let emissive_branch = fragment.find("if emits {").expect("emissive branch");
        let visibility_branch = fragment
            .find("else if ray_policy_enabled(RAY_POLICY_DIRECT_LIGHT_VISIBILITY)")
            .expect("visibility branch");
        assert!(emissive_branch < visibility_branch);
        assert!(!fragment.contains("let radiance = select("));
        assert!(fragment.contains("surface_radiance_from_analytic_direct("));
    }

    /// Whole-light culling is exact: a light is skipped only when its
    /// contribution is provably zero, so the per-sample gates below
    /// stay reachable and no pixel can change.
    #[test]
    fn light_culling_skips_only_lights_that_contribute_nothing() {
        let source = include_str!("../shaders/raymarch.wgsl");
        assert!(source.contains("fn light_can_contribute("));
        let light_start = source
            .find("fn evaluate_visible_scene_light(")
            .expect("light evaluator");
        let light_end = source[light_start..]
            .find("\n}\n")
            .expect("evaluator end")
            + light_start;
        let evaluator = &source[light_start..light_end];
        assert!(evaluator.contains("if !light_can_contribute(world, normal, light) {"));
        assert!(evaluator.contains("if !has_positive_rgb(unoccluded)"));
        let helper_start = source
            .find("fn light_can_contribute(")
            .expect("culling helper");
        let helper_end = source[helper_start..]
            .find("\n}\n")
            .expect("helper end")
            + helper_start;
        let helper = &source[helper_start..helper_end];
        assert!(helper.contains("if light.color_intensity.w <= 0.0"));
        assert!(helper.contains("if dot(delta, delta) >= range * range"));
        assert!(helper.contains("if dot(normal, delta) <= 0.0"));
        assert!(helper.contains("if world.y >= light.position_radius.y"));
        assert!(helper.contains("if dx * dx + dy * dy + dz * dz >= range * range"));
    }

    #[test]
    fn linear_channel_to_srgb_round_trips_through_the_leaf_format() {
        assert_eq!(linear_channel_to_srgb(0.0), 0);
        assert_eq!(linear_channel_to_srgb(1.0), 255);
        assert_eq!(linear_channel_to_srgb(0.5), 188);
        // 128/255 sRGB decodes to ~0.21586 linear (see decode_srgb_albedo).
        assert_eq!(linear_channel_to_srgb(0.215_86), 128);
        assert_eq!(linear_channel_to_srgb(-1.0), 0);
        assert_eq!(linear_channel_to_srgb(2.0), 255);
    }

    #[test]
    fn pack_ray_mips_round_trips_a_single_leaf() {
        // One leaf node: type 1, material 1, packed mid-grey, no light.
        let atlas = [1u32, 1, 0x808080, 0];
        let packed = pack_ray_mips(&atlas);
        assert_eq!(packed.len(), 4);
        assert_eq!(packed[0], 0x808080);
        assert_eq!(packed[1], 1);
        assert_eq!(f32::from_bits(packed[2]), 0.0);
        assert_eq!(packed[3], 0);
    }

    fn test_light(position: [f32; 3], radius: f32, half_size: [f32; 2]) -> GpuLight {
        GpuLight {
            position_radius: [position[0], position[1], position[2], radius],
            color_intensity: [1.0, 1.0, 1.0, 1.0],
            half_size_kind: [half_size[0], half_size[1], 1.0, 1.0],
        }
    }

    #[test]
    fn light_grid_groups_lights_by_home_cell() {
        let lights = [
            test_light([0.0, 2.0, 0.0], 6.0, [0.0, 0.0]),
            test_light([25.0, 2.0, 0.0], 6.0, [0.0, 0.0]),
            test_light([0.0, 2.0, 25.0], 6.0, [0.0, 0.0]),
        ];
        let grid = derive_light_grid(&lights).expect("three lights certify");
        assert_eq!(grid.reach, 6.0);
        assert_eq!(grid.cell, [6.0, 6.0]);
        assert_eq!(grid.dims, [5, 5]);
        assert_eq!(grid.cell_count(), 25);
        assert_eq!(grid.indices, vec![0, 1, 2]);
        assert_eq!(grid.offsets[0], 0);
        assert_eq!(grid.offsets[1], 1);
        assert_eq!(grid.offsets[4], 1);
        assert_eq!(grid.offsets[5], 2);
        assert_eq!(grid.offsets[20], 2);
        assert_eq!(grid.offsets[21], 3);
        assert_eq!(grid.offsets[25], 3);
        assert_eq!(grid.packed().len(), 26 + 3);
    }

    #[test]
    fn light_grid_lookup_contains_every_light_within_reach() {
        let lights = [
            test_light([0.0, 2.0, 0.0], 6.0, [0.0, 0.0]),
            test_light([10.0, 2.0, 0.0], 6.0, [0.0, 0.0]),
            test_light([0.0, 2.0, 10.0], 6.0, [0.0, 0.0]),
            test_light([40.0, 2.0, 0.0], 6.0, [0.0, 0.0]),
        ];
        let grid = derive_light_grid(&lights).expect("four lights certify");
        // Receiver at (5, 0, 0): lights 0 and 1 are 5 units away, inside
        // the 6-unit reach; light 3 is 35 away and must not be visited.
        let near = lights_near(&grid, [5.0, 1.7, 0.0]);
        assert!(near.contains(&0));
        assert!(near.contains(&1));
        assert!(!near.contains(&3));
        // Every light within reach of any receiver on a sweep is found.
        for receiver_x in [0.0, 5.0, 10.0, 20.0, 30.0, 40.0] {
            let receiver = [receiver_x, 1.7, 0.0];
            let near = lights_near(&grid, receiver);
            for (index, light) in lights.iter().enumerate() {
                let dx = light.position_radius[0] - receiver[0];
                let dz = light.position_radius[2] - receiver[2];
                if dx * dx + dz * dz <= grid.reach * grid.reach {
                    assert!(
                        near.contains(&(index as u32)),
                        "light {index} within reach of {receiver_x} must be visited"
                    );
                }
            }
        }
    }

    #[test]
    fn light_grid_lookup_excludes_lights_far_beyond_reach() {
        let lights = [
            test_light([0.0, 2.0, 0.0], 6.0, [0.0, 0.0]),
            test_light([100.0, 2.0, 0.0], 6.0, [0.0, 0.0]),
        ];
        let grid = derive_light_grid(&lights).expect("two lights certify");
        assert_eq!(lights_near(&grid, [0.0, 1.7, 0.0]), vec![0]);
        assert_eq!(lights_near(&grid, [100.0, 1.7, 0.0]), vec![1]);
    }

    #[test]
    fn light_grid_rejects_an_empty_light_set() {
        assert!(derive_light_grid(&[]).is_none());
    }

    #[test]
    fn light_grid_rejects_too_many_lights() {
        let lights: Vec<_> = (0..1025)
            .map(|_| test_light([0.0, 2.0, 0.0], 6.0, [0.0, 0.0]))
            .collect();
        assert!(derive_light_grid(&lights).is_none());
    }

    #[test]
    fn light_grid_rejects_a_grid_past_the_cell_cap() {
        // 500 lights 4 units apart with a 0.5 reach: cell 1.0, so the
        // grid would need ~2000 cells in x alone, past the 1024 cap.
        let lights: Vec<_> = (0..500)
            .map(|i| test_light([i as f32 * 4.0, 2.0, 0.0], 0.5, [0.0, 0.0]))
            .collect();
        assert!(derive_light_grid(&lights).is_none());
    }

    #[test]
    fn light_grid_single_light_covers_its_cell() {
        let lights = [test_light([3.0, 2.0, 4.0], 6.0, [0.0, 0.0])];
        let grid = derive_light_grid(&lights).expect("one light certifies");
        assert_eq!(grid.dims, [1, 1]);
        assert_eq!(grid.offsets, vec![0, 1]);
        assert_eq!(grid.indices, vec![0]);
        assert_eq!(lights_near(&grid, [3.0, 1.7, 4.0]), vec![0]);
        assert!(lights_near(&grid, [100.0, 1.7, 100.0]).is_empty());
    }

    #[test]
    fn light_grid_cell_boundary_light_lands_in_one_cell() {
        let lights = [
            test_light([0.0, 2.0, 0.0], 6.0, [0.0, 0.0]),
            test_light([6.0, 2.0, 0.0], 6.0, [0.0, 0.0]),
        ];
        let grid = derive_light_grid(&lights).expect("boundary lights certify");
        assert_eq!(grid.dims, [2, 1]);
        assert_eq!(grid.indices, vec![0, 1]);
        assert_eq!(grid.offsets, vec![0, 1, 2]);
        let near = lights_near(&grid, [0.0, 1.7, 0.0]);
        assert_eq!(near.len(), 2);
        assert!(near.contains(&0) && near.contains(&1));
    }

    #[test]
    fn light_grid_reach_covers_a_rect_lights_half_size() {
        // A strip light: radius 6 plus a 2-unit half-size reaches 8.
        let lights = [test_light([0.0, 2.0, 0.0], 6.0, [2.0, 0.5])];
        let grid = derive_light_grid(&lights).expect("strip light certifies");
        assert_eq!(grid.reach, 8.0);
        assert_eq!(grid.cell, [8.0, 8.0]);
    }

    #[test]
    fn shader_light_grid_visits_only_nearby_lights() {
        let source = include_str!("../shaders/raymarch.wgsl");
        assert!(source.contains("@group(1) @binding(5) var<storage, read> light_grid:"));
        assert!(source.contains("light_grid_valid"));
        assert!(source.contains("light_grid_reach"));
        let analytic_start = source
            .find("fn raymarch_analytic_direct(")
            .expect("analytic direct");
        let analytic_end = source[analytic_start..]
            .find("\n}\n")
            .expect("analytic end")
            + analytic_start;
        let analytic = &source[analytic_start..analytic_end];
        assert!(analytic.contains("light_grid_valid == 1u"));
        assert!(analytic.contains("light_grid"));
    }

    #[test]
    fn shader_stops_descent_at_a_pixel_matched_mip_node() {
        let source = include_str!("../shaders/raymarch.wgsl");
        assert!(source.contains("@group(1) @binding(4) var<storage, read> mip_nodes:"));
        assert!(source.contains("fn svo_stack_walk("));
        assert!(source.contains("lod_k: f32"));
        let walk_start = source
            .find("fn svo_stack_walk(")
            .expect("stack walk");
        let walk_end = source[walk_start..]
            .find("\nfn trace_chunk(")
            .expect("walk end")
            + walk_start;
        let walk = &source[walk_start..walk_end];
        assert!(walk.contains("if node_size <= lod_k * max(t_entry, 0.0)"));
        assert!(walk.contains("let mip = mip_nodes[node_index];"));
        assert!(walk.contains("if emissive <= 0.0 || authored_emission_strength(mip.y) > 0.0"));
        assert!(walk.contains("voxel_size,"));
        let trace_start = source.find("fn trace_chunk(").expect("primary trace");
        let trace_end = source[trace_start..]
            .find("\nfn direct_visibility_bias(")
            .expect("trace end")
            + trace_start;
        let trace = &source[trace_start..trace_end];
        assert!(trace.contains("let lod_k = 2.0 * frame.viewport_fov_start.z"));
        assert!(trace.contains("walk.voxel_size"));
    }

    #[test]
    fn shader_keeps_shadow_rays_exact() {
        let source = include_str!("../shaders/raymarch.wgsl");
        let visibility_start = source
            .find("fn trace_visibility_chunk_stack(")
            .expect("visibility trace");
        let visibility_end = source[visibility_start..]
            .find("\n// Top-level chunk-grid walk")
            .expect("visibility end")
            + visibility_start;
        let visibility = &source[visibility_start..visibility_end];
        assert!(visibility.contains("step_budget, 0.0)"));
        assert!(!visibility.contains("lod_k *"));
        assert!(!visibility.contains("lod_k >"));
    }
}
