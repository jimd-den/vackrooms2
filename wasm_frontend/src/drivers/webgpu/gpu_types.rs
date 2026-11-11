//! Explicit CPU-to-GPU records.
//!
//! Every struct is 16-byte aligned where WGSL requires it. Compact mesh and
//! face records use integer words and are decoded by vertex pulling, so the
//! worker codec remains independent of wgpu and no compiler layout is relied
//! upon.

use bytemuck::{Pod, Zeroable};
#[cfg(test)]
use vackrooms::adapters::material_palette::MATERIAL_VISUALS;
use vackrooms::adapters::material_palette::material_visual;

use crate::application::ports::{
    DynamicLight, FrameParams, LightKind, LightSource, PackedFaceInstance, PackedVertex,
    SupplySprite,
};
use crate::application::render_settings::RenderToggles;
use crate::application::rendering::{camera_basis, webgpu_view_projection};
use vackrooms::domain::entities::voxel_grid::material_emission_strength;

/// The high byte of a packed material visual stores authored emission over
/// the current domain range of `0..=10`. The low 24 bits remain the canonical
/// `0xRRGGBB` palette entry, so raster shaders never maintain a second palette.
const PACKED_EMISSION_UNITS_PER_RADIANCE: f32 = u8::MAX as f32 / 10.0;

fn pack_material_visual(material: u8) -> u32 {
    let color = material_visual(material).color & 0x00ff_ffff;
    let emission = material_emission_strength(material).unwrap_or_default();
    let quantized_emission = (emission * PACKED_EMISSION_UNITS_PER_RADIANCE)
        .round()
        .clamp(0.0, f32::from(u8::MAX)) as u32;
    color | (quantized_emission << 24)
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
pub struct GpuFrameUniforms {
    pub view_projection: [f32; 16],
    pub camera_position: [f32; 4],
    pub camera_right: [f32; 4],
    pub camera_up: [f32; 4],
    pub camera_forward: [f32; 4],
    pub fog_color_density: [f32; 4],
    pub sky_color_ambient: [f32; 4],
    pub viewport_fov_start: [f32; 4],
    pub render_options: [u32; 4],
    /// x: outdoor environment; y: cached diffuse-fill enabled; z/w reserved.
    pub lighting_options: [u32; 4],
}

impl GpuFrameUniforms {
    pub fn from_frame(
        frame: &FrameParams,
        width: u32,
        height: u32,
        fov_tan: f32,
        max_draw_distance: f32,
        trace_budget: u32,
        light_count: usize,
        toggles: RenderToggles,
    ) -> Self {
        let basis = camera_basis(frame.yaw, frame.pitch);
        let environment = frame.environment;
        let miss_color = if environment.outdoor {
            environment.sky_color
        } else {
            environment.fog_color
        };
        Self {
            view_projection: webgpu_view_projection(
                frame,
                width,
                height,
                max_draw_distance,
                fov_tan,
            ),
            camera_position: [
                frame.camera_pos[0],
                frame.camera_pos[1],
                frame.camera_pos[2],
                1.0,
            ],
            camera_right: [basis.right[0], basis.right[1], basis.right[2], 0.0],
            camera_up: [basis.up[0], basis.up[1], basis.up[2], 0.0],
            camera_forward: [basis.forward[0], basis.forward[1], basis.forward[2], 0.0],
            fog_color_density: [
                environment.fog_color[0],
                environment.fog_color[1],
                environment.fog_color[2],
                environment.fog_density,
            ],
            sky_color_ambient: [
                miss_color[0],
                miss_color[1],
                miss_color[2],
                environment.ambient_scale,
            ],
            viewport_fov_start: [
                width.max(1) as f32,
                height.max(1) as f32,
                fov_tan,
                environment.fog_start,
            ],
            render_options: [
                light_count.min(u32::MAX as usize) as u32,
                u32::from(frame.flashlight),
                u32::from(toggles.dither),
                trace_budget,
            ],
            lighting_options: [
                u32::from(environment.outdoor),
                u32::from(toggles.baked_lighting),
                0,
                0,
            ],
        }
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
pub struct GpuLight {
    pub position_radius: [f32; 4],
    pub color_intensity: [f32; 4],
    pub half_size_kind: [f32; 4],
}

impl From<&LightSource> for GpuLight {
    fn from(light: &LightSource) -> Self {
        Self {
            position_radius: [
                light.position[0],
                light.position[1],
                light.position[2],
                light.radius.max(0.0),
            ],
            color_intensity: [
                light.color[0],
                light.color[1],
                light.color[2],
                light.intensity.max(0.0),
            ],
            half_size_kind: [
                light.half_size[0].max(0.0),
                light.half_size[1].max(0.0),
                light.kind as u8 as f32,
                f32::from(light.enabled),
            ],
        }
    }
}

impl From<&DynamicLight> for GpuLight {
    fn from(light: &DynamicLight) -> Self {
        Self {
            position_radius: [
                light.position[0],
                light.position[1],
                light.position[2],
                light.radius.max(0.0),
            ],
            color_intensity: [
                light.color[0],
                light.color[1],
                light.color[2],
                light.intensity.max(0.0),
            ],
            half_size_kind: [0.0, 0.0, LightKind::Point as u8 as f32, 1.0],
        }
    }
}

pub fn collect_frame_lights(frame: &FrameParams) -> Vec<GpuLight> {
    frame
        .active_scene_lights()
        .iter()
        .filter(|light| light.enabled)
        .map(GpuLight::from)
        .chain(frame.active_dynamic_lights().iter().map(GpuLight::from))
        .collect()
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
pub struct GpuChunkUniforms {
    pub origin_world_size: [f32; 4],
    pub bounds_voxel_size: [f32; 4],
    pub draw: [u32; 4],
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable, PartialEq, Eq)]
pub struct GpuPackedSurfaceVertex {
    pub xy: u32,
    pub z_normal_material: u32,
    pub light_ao: u32,
    /// Low 24 bits: canonical sRGB color; high byte: emitted radiance.
    pub visual: u32,
}

impl From<&PackedVertex> for GpuPackedSurfaceVertex {
    fn from(vertex: &PackedVertex) -> Self {
        Self {
            xy: u32::from(vertex.position[0]) | (u32::from(vertex.position[1]) << 16),
            z_normal_material: u32::from(vertex.position[2])
                | (u32::from(vertex.normal_axis) << 16)
                | (u32::from(vertex.material) << 24),
            light_ao: u32::from(vertex.static_indirect) | (u32::from(vertex.ao) << 8),
            visual: pack_material_visual(vertex.material),
        }
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable, PartialEq, Eq)]
pub struct GpuPackedFace {
    pub xy: u32,
    pub z_extents: u32,
    pub surface: u32,
    /// Low 24 bits: canonical sRGB color; high byte: emitted radiance.
    pub visual: u32,
}

impl From<&PackedFaceInstance> for GpuPackedFace {
    fn from(face: &PackedFaceInstance) -> Self {
        Self {
            xy: u32::from(face.position[0]) | (u32::from(face.position[1]) << 16),
            z_extents: u32::from(face.position[2])
                | (u32::from(face.extent_u) << 16)
                | (u32::from(face.extent_v) << 24),
            surface: u32::from(face.normal_axis)
                | (u32::from(face.material) << 8)
                | (u32::from(face.baked_light) << 16)
                | (u32::from(face.ao) << 24),
            visual: pack_material_visual(face.material),
        }
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
pub struct GpuRayChunk {
    pub origin_world_size: [f32; 4],
    pub voxel_depth: [f32; 4],
    pub indices: [u32; 4],
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
pub struct GpuRayScene {
    pub chunk_count: u32,
    /// Bit 0: empty-leaf skip; bit 1: CPU front-to-back selection;
    /// bit 2: direct-light visibility rays.
    pub policy_flags: u32,
    /// x: validated rectangle-light visibility sample count (1 or 4);
    /// y: 1 when the chunk-grid walk below is certified, 0 to fall back
    /// to the per-chunk loop.
    pub reserved: [u32; 2],
    /// Minimum corner of grid cell (0, 0); y is the chunk layer base.
    pub grid_origin: [f32; 3],
    /// SVO cube edge: every resident chunk shares one cube size, so the
    /// grid layer is exactly this tall.
    pub grid_cube_size: f32,
    /// Lattice spacing in x and z (the streaming chunk size, which is
    /// smaller than the cube size: cubes overlap, cells do not).
    pub grid_cell: [f32; 2],
    /// Grid dimensions in x and z cells.
    pub grid_dims: [u32; 2],
}

/// Certified top-level structure over the resident chunks: a regular
/// lattice in x/z, one layer in y. The shader walks the lattice with a
/// 2D DDA instead of box-testing every chunk, so a trace visits only
/// the chunks its ray actually crosses. The map is cell-major
/// (index `z * dims[0] + x`); empty cells hold `u32::MAX`.
#[derive(Debug, Clone)]
pub struct ChunkGrid {
    pub origin: [f32; 3],
    pub cube_size: f32,
    pub cell: [f32; 2],
    pub dims: [u32; 2],
    pub map: Vec<u32>,
}

impl ChunkGrid {
    /// Largest grid the shader will walk; a sparser resident set falls
    /// back to the per-chunk loop instead.
    pub const MAX_CELLS: usize = 64;

    pub fn invalid() -> Self {
        Self {
            origin: [0.0; 3],
            cube_size: 0.0,
            cell: [0.0; 2],
            dims: [0; 2],
            map: Vec::new(),
        }
    }
}

pub const RAY_POLICY_EMPTY_SPACE_SKIP: u32 = 1 << 0;
pub const RAY_POLICY_FRONT_TO_BACK: u32 = 1 << 1;
pub const RAY_POLICY_DIRECT_LIGHT_VISIBILITY: u32 = 1 << 2;

impl GpuRayScene {
    pub const fn from_policy(
        chunk_count: u32,
        empty_space_skip: bool,
        front_to_back: bool,
        direct_light_visibility: bool,
        area_light_visibility_samples: u8,
        grid: Option<&ChunkGrid>,
    ) -> Self {
        let mut policy_flags = 0;
        if empty_space_skip {
            policy_flags |= RAY_POLICY_EMPTY_SPACE_SKIP;
        }
        if front_to_back {
            policy_flags |= RAY_POLICY_FRONT_TO_BACK;
        }
        if direct_light_visibility {
            policy_flags |= RAY_POLICY_DIRECT_LIGHT_VISIBILITY;
        }
        let (grid_origin, grid_cube_size, grid_cell, grid_dims, grid_valid) =
            match grid {
                Some(grid) => (
                    grid.origin,
                    grid.cube_size,
                    grid.cell,
                    grid.dims,
                    1u32,
                ),
                None => ([0.0; 3], 0.0, [0.0; 2], [0; 2], 0u32),
            };
        Self {
            chunk_count,
            policy_flags,
            reserved: [
                validated_area_light_visibility_samples(area_light_visibility_samples),
                grid_valid,
            ],
            grid_origin,
            grid_cube_size,
            grid_cell,
            grid_dims,
        }
    }
}

const fn validated_area_light_visibility_samples(samples: u8) -> u32 {
    if samples == 4 { 4 } else { 1 }
}

/// One camera-facing supply label. The vertex stage expands this point into
/// a six-vertex quad, keeping transient UI geometry off the CPU.
#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable, PartialEq)]
pub struct GpuSupplySprite {
    /// xyz: world-space center; w: stacked texture-atlas row.
    pub position_atlas_row: [f32; 4],
}

impl From<&SupplySprite> for GpuSupplySprite {
    fn from(sprite: &SupplySprite) -> Self {
        Self {
            position_atlas_row: [
                sprite.position[0],
                sprite.position[1],
                sprite.position[2],
                f32::from(sprite.atlas_row),
            ],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gpu_records_obey_wgsl_alignment_contract() {
        assert_eq!(size_of::<GpuFrameUniforms>() % 16, 0);
        assert_eq!(size_of::<GpuLight>(), 48);
        assert_eq!(size_of::<GpuChunkUniforms>(), 48);
        assert_eq!(size_of::<GpuPackedSurfaceVertex>(), 16);
        assert_eq!(size_of::<GpuPackedFace>(), 16);
        assert_eq!(size_of::<GpuRayChunk>(), 48);
        assert_eq!(size_of::<GpuRayScene>(), 48);
        assert_eq!(size_of::<GpuSupplySprite>(), 16);
    }

    #[test]
    fn frame_flags_carry_environment_and_live_bake_policy() {
        let mut frame = FrameParams {
            environment: crate::application::ports::Environment::daylight(),
            ..FrameParams::default()
        };
        let analytic = GpuFrameUniforms::from_frame(
            &frame,
            64,
            64,
            1.0,
            100.0,
            1,
            0,
            RenderToggles::default(),
        );
        assert_eq!(analytic.lighting_options, [1, 0, 0, 0]);

        frame.environment = crate::application::ports::Environment::interior();
        let mut toggles = RenderToggles::default();
        toggles.baked_lighting = true;
        let baked = GpuFrameUniforms::from_frame(&frame, 64, 64, 1.0, 100.0, 1, 0, toggles);
        assert_eq!(baked.lighting_options, [0, 1, 0, 0]);
    }

    #[test]
    fn ray_policy_and_visibility_quality_fit_the_stable_scene_record() {
        let grid = ChunkGrid {
            origin: [-10.0, 0.0, -10.0],
            cube_size: 12.8,
            cell: [10.0, 10.0],
            dims: [3, 3],
            map: vec![u32::MAX; 9],
        };
        let scene = GpuRayScene::from_policy(7, true, true, true, 4, Some(&grid));
        assert_eq!(scene.chunk_count, 7);
        assert_eq!(
            scene.policy_flags,
            RAY_POLICY_EMPTY_SPACE_SKIP
                | RAY_POLICY_FRONT_TO_BACK
                | RAY_POLICY_DIRECT_LIGHT_VISIBILITY
        );
        assert_eq!(scene.reserved, [4, 1]);
        assert_eq!(scene.grid_origin, [-10.0, 0.0, -10.0]);
        assert_eq!(scene.grid_cube_size, 12.8);
        assert_eq!(scene.grid_cell, [10.0, 10.0]);
        assert_eq!(scene.grid_dims, [3, 3]);

        let conservative = GpuRayScene::from_policy(2, false, false, false, 3, None);
        assert_eq!(conservative.policy_flags, 0);
        assert_eq!(conservative.reserved, [1, 0]);
        assert_eq!(conservative.grid_origin, [0.0; 3]);
        assert_eq!(conservative.grid_dims, [0; 2]);
        assert_eq!(size_of::<GpuRayScene>(), 48);
    }

    #[test]
    fn compact_surface_words_round_trip_fields() {
        let source = PackedVertex {
            position: [0x1234, 0xabcd, 0x789a],
            normal_axis: 5,
            material: 7,
            static_indirect: 13,
            ao: 201,
        };
        let packed = GpuPackedSurfaceVertex::from(&source);
        assert_eq!(packed.xy & 0xffff, 0x1234);
        assert_eq!(packed.xy >> 16, 0xabcd);
        assert_eq!(packed.z_normal_material & 0xffff, 0x789a);
        assert_eq!((packed.z_normal_material >> 16) & 0xff, 5);
        assert_eq!(packed.z_normal_material >> 24, 7);
        assert_eq!(packed.light_ao & 0xff, 13);
        assert_eq!((packed.light_ao >> 8) & 0xff, 201);
        assert_eq!(packed.visual & 0x00ff_ffff, MATERIAL_VISUALS[7].color);
    }

    #[test]
    fn packed_visuals_derive_palette_and_emission_from_the_domain_contract() {
        for (material, visual) in MATERIAL_VISUALS.iter().enumerate() {
            let material = material as u8;
            let packed = pack_material_visual(material);
            assert_eq!(packed & 0x00ff_ffff, visual.color, "material {material}");

            let decoded_emission = (packed >> 24) as f32 / PACKED_EMISSION_UNITS_PER_RADIANCE;
            let expected_emission = material_emission_strength(material).unwrap_or_default();
            let maximum_quantization_error = 0.5 / PACKED_EMISSION_UNITS_PER_RADIANCE;
            assert!(
                (decoded_emission - expected_emission).abs()
                    <= maximum_quantization_error + f32::EPSILON,
                "material {material}: expected {expected_emission}, decoded {decoded_emission}"
            );
        }
    }
}
