//! Surfel cache and instanced disc pass.
//!
//! Structurally the splat pipeline with two deliberate omissions:
//!
//! * **No hero shadow pass.** The splat path casts shadows from a retained
//!   greedy mesh. A surfel cloud has no mesh, and building one purely to
//!   cast with would let the caster and the visible surface disagree about
//!   the silhouette — a shadow whose outline does not match the thing
//!   throwing it.
//! * **No cell-range culling.** The face-instance page is bucketed into
//!   culling cells at extraction; a surfel cloud is a flat array, so a
//!   chunk is drawn or skipped whole. Bucketing it the same way is a
//!   worthwhile follow-up and is deliberately not faked here.

use std::cmp::Ordering;
use std::collections::BTreeMap;

use wgpu::util::DeviceExt;

use crate::application::ports::{FrameParams, SurfaceChunk, SurfaceChunkKey};
use crate::application::render_settings::RenderToggles;
use crate::application::rendering::encode_display_color;

use super::raster_shadow::{HeroShadowMap, HeroShadowOptions};
use super::surface::create_chunk_layout;
use super::visibility::{bounds_sphere, frustum_side_planes_visible, sphere_is_visible};
use crate::drivers::webgpu::frame_resources::FrameResources;
use crate::drivers::webgpu::gpu_types::{GpuChunkUniforms, GpuPackedSurfel};
use crate::drivers::webgpu::shader::ShaderProgram;

struct SurfelChunkGpu {
    surfels: wgpu::Buffer,
    uniforms: wgpu::Buffer,
    bind_group: wgpu::BindGroup,
    surfel_count: u32,
    origin: [f32; 3],
    bounds_max: [f32; 3],
    voxel_size: f32,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SurfelFrameStats {
    pub surfels_drawn: u32,
    pub surfels_dropped: u32,
    pub draw_calls: u32,
}

pub struct SurfelPipeline {
    pipeline: wgpu::RenderPipeline,
    /// Owned but never rendered into. The shared raster lighting the vertex
    /// stage calls reads the receiver group, so the binding must exist and
    /// be valid; `prepare(.., false)` writes the disabled uniform, which is
    /// the honest state for a cloud that casts nothing.
    shadow: HeroShadowMap,
    chunk_layout: wgpu::BindGroupLayout,
    chunks: BTreeMap<SurfaceChunkKey, SurfelChunkGpu>,
    max_draw_distance: f32,
    surfel_budget: u32,
    stats: SurfelFrameStats,
}

impl SurfelPipeline {
    pub fn new(
        device: &wgpu::Device,
        target_format: wgpu::TextureFormat,
        frame_layout: &wgpu::BindGroupLayout,
        max_draw_distance: f32,
        surfel_budget: u32,
    ) -> Self {
        let chunk_layout = create_chunk_layout(device, size_of::<GpuPackedSurfel>() as u64);
        let shadow = HeroShadowMap::new(device, HeroShadowOptions::default());
        let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("webgpu.surfel.pipeline-layout"),
            // The shadow receiver group is still bound: the shared raster
            // lighting the vertex stage calls reads it. The cloud simply
            // never contributes a caster.
            bind_group_layouts: &[
                Some(frame_layout),
                Some(&chunk_layout),
                Some(shadow.receiver_layout()),
            ],
            immediate_size: 0,
        });
        let shader = ShaderProgram::Surfel.create_module(device);
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("webgpu.surfel.pipeline"),
            layout: Some(&layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("surfel_vertex"),
                compilation_options: Default::default(),
                buffers: &[],
            },
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleStrip,
                strip_index_format: None,
                front_face: wgpu::FrontFace::Ccw,
                cull_mode: Some(wgpu::Face::Back),
                ..Default::default()
            },
            depth_stencil: Some(wgpu::DepthStencilState {
                format: wgpu::TextureFormat::Depth32Float,
                depth_write_enabled: Some(true),
                depth_compare: Some(wgpu::CompareFunction::LessEqual),
                stencil: Default::default(),
                bias: Default::default(),
            }),
            multisample: Default::default(),
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("surfel_fragment"),
                compilation_options: Default::default(),
                targets: &[Some(target_format.into())],
            }),
            multiview_mask: None,
            cache: None,
        });
        Self {
            pipeline,
            shadow,
            chunk_layout,
            chunks: BTreeMap::new(),
            max_draw_distance,
            surfel_budget,
            stats: SurfelFrameStats::default(),
        }
    }

    pub fn upload(&mut self, device: &wgpu::Device, chunks: &[SurfaceChunk<'_>]) {
        for &chunk in chunks {
            self.chunks.remove(&chunk.key);
            if chunk.mesh.surfels.is_empty() {
                continue;
            }
            let surfels: Vec<_> = chunk
                .mesh
                .surfels
                .surfels
                .iter()
                .map(GpuPackedSurfel::from)
                .collect();
            let surfel_buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("webgpu.surfel.surfels"),
                contents: bytemuck::cast_slice(&surfels),
                usage: wgpu::BufferUsages::STORAGE,
            });
            let uniform_buffer = device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("webgpu.surfel.chunk-uniforms"),
                size: size_of::<GpuChunkUniforms>() as u64,
                usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
            let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("webgpu.surfel.chunk-bind-group"),
                layout: &self.chunk_layout,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: uniform_buffer.as_entire_binding(),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: surfel_buffer.as_entire_binding(),
                    },
                ],
            });
            self.chunks.insert(
                chunk.key,
                SurfelChunkGpu {
                    surfels: surfel_buffer,
                    uniforms: uniform_buffer,
                    bind_group,
                    surfel_count: surfels.len().min(u32::MAX as usize) as u32,
                    origin: chunk.origin,
                    bounds_max: chunk.mesh.bounds.max,
                    voxel_size: chunk.mesh.voxel_scale,
                },
            );
        }
    }

    pub fn remove(&mut self, keys: &[SurfaceChunkKey]) {
        for key in keys {
            self.chunks.remove(key);
        }
    }

    pub fn clear(&mut self) {
        self.chunks.clear();
    }

    pub fn stats(&self) -> SurfelFrameStats {
        self.stats
    }

    #[allow(clippy::too_many_arguments)]
    pub fn draw(
        &mut self,
        queue: &wgpu::Queue,
        encoder: &mut wgpu::CommandEncoder,
        target: &wgpu::TextureView,
        depth: &wgpu::TextureView,
        frame_resources: &FrameResources,
        frame: &FrameParams,
        toggles: RenderToggles,
        light_count: u32,
        fov_tan: f32,
        aspect: f32,
    ) {
        for chunk in self.chunks.values() {
            let uniforms = GpuChunkUniforms {
                // The unfold slot is 1.0: a surfel cloud has no
                // reality-unfold animation of its own, and claiming
                // otherwise would leave the shared chunk contract holding a
                // number nothing reads.
                origin_world_size: [chunk.origin[0], chunk.origin[1], chunk.origin[2], 1.0],
                bounds_voxel_size: [
                    chunk.bounds_max[0],
                    chunk.bounds_max[1],
                    chunk.bounds_max[2],
                    chunk.voxel_size,
                ],
                draw: [0, light_count, 0, 0],
            };
            queue.write_buffer(&chunk.uniforms, 0, bytemuck::bytes_of(&uniforms));
        }

        let mut chunks: Vec<_> = self.chunks.values().collect();
        if toggles.front_to_back || toggles.face_budget {
            chunks.sort_by(|left, right| {
                distance_squared(left.origin, frame.camera_pos)
                    .partial_cmp(&distance_squared(right.origin, frame.camera_pos))
                    .unwrap_or(Ordering::Equal)
            });
        }
        // Writes the "no hero shadow" uniform without ever running a
        // caster pass, so the receiver group is valid and reads as unshadowed.
        self.shadow.prepare(queue, frame, false);
        self.stats = SurfelFrameStats::default();

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
            label: Some("webgpu.surfel.pass"),
            color_attachments: &color_attachments,
            depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                view: depth,
                depth_ops: Some(wgpu::Operations {
                    load: wgpu::LoadOp::Clear(1.0),
                    store: wgpu::StoreOp::Store,
                }),
                stencil_ops: None,
            }),
            ..Default::default()
        });
        pass.set_pipeline(&self.pipeline);
        pass.set_bind_group(0, frame_resources.bind_group(), &[]);
        pass.set_bind_group(2, self.shadow.receiver_bind_group(), &[]);

        for chunk in chunks {
            if toggles.distance_cull {
                let (center, radius) = bounds_sphere(chunk.origin, chunk.bounds_max);
                if !sphere_is_visible(center, radius, frame, self.max_draw_distance)
                    || !frustum_side_planes_visible(center, radius, frame, fov_tan, aspect)
                {
                    self.stats.surfels_dropped = self
                        .stats
                        .surfels_dropped
                        .saturating_add(chunk.surfel_count);
                    continue;
                }
            }
            if toggles.face_budget && self.stats.surfels_drawn >= self.surfel_budget {
                self.stats.surfels_dropped = self
                    .stats
                    .surfels_dropped
                    .saturating_add(chunk.surfel_count);
                continue;
            }
            let _keep_alive = &chunk.surfels;
            pass.set_bind_group(1, &chunk.bind_group, &[]);
            pass.draw(0..4, 0..chunk.surfel_count);
            self.stats.surfels_drawn = self.stats.surfels_drawn.saturating_add(chunk.surfel_count);
            self.stats.draw_calls = self.stats.draw_calls.saturating_add(1);
        }
    }
}

fn distance_squared(a: [f32; 3], b: [f32; 3]) -> f32 {
    let d = [a[0] - b[0], a[1] - b[1], a[2] - b[2]];
    d[0] * d[0] + d[1] * d[1] + d[2] * d[2]
}
