use crate::adapters::json_presenter::JsonPresenter;
use crate::adapters::material_palette::DEFAULT_MATERIAL_PALETTE;
use crate::adapters::octree_gpu_serializer::OctreeGpuSerializer;
use crate::adapters::voxel_mapper::VoxelMapper;
use crate::domain::entities::voxel_grid::VoxelGrid;
use crate::use_cases::build_octree::BuildOctreeUseCase;

/// WebRendererAdapter acts as a Presenter in Clean Architecture.
/// It translates the core domain object (VoxelGrid) into formats
/// suitable for the Web Presentation layer.
pub struct WebRendererAdapter;

impl WebRendererAdapter {
    /// Translates VoxelGrid into a JSON string representing greedy-meshed quads using default Exact policy.
    pub fn to_json(grid: &VoxelGrid, voxel_scale: f32) -> String {
        let mapper = VoxelMapper::new(voxel_scale, &DEFAULT_MATERIAL_PALETTE);
        let quads = mapper.map_voxel_grid(grid);
        JsonPresenter::render_voxels(&quads)
    }

    /// Translates VoxelGrid into a JSON string using LowSpec greedy surface meshing policy.
    pub fn to_low_spec_json(grid: &VoxelGrid, voxel_scale: f32) -> String {
        let mapper = VoxelMapper::with_policy(
            voxel_scale,
            &DEFAULT_MATERIAL_PALETTE,
            crate::adapters::voxel_mapper::SurfaceMeshingPolicy::LowSpec,
        );
        let quads = mapper.map_voxel_grid(grid);
        JsonPresenter::render_voxels(&quads)
    }

    /// Converts VoxelGrid and its downsampled ProbeGrid into a high-performance binary surface payload.
    ///
    /// BINARY SCHEMATIC (Byte offset -> payload):
    /// - `0..4`: `quad_count` (u32, little-endian)
    /// - `4..8`: `probe_width` (u32, little-endian)
    /// - `8..12`: `probe_height` (u32, little-endian)
    /// - `12..16`: `probe_depth` (u32, little-endian)
    /// - `16..20`: `voxel_scale` (f32, little-endian)
    /// - `20..24`: `policy` (u32, little-endian; 0=Exact, 1=LowSpec)
    /// - `24..24 + quad_count * 28`: packed quads (x, y, z, w, h [f32], dir, material, light, ao [u8], color [u32])
    /// - `..end`: raw packed RGB8 probe grid bytes
    pub fn to_low_spec_surface_binary(
        grid: &VoxelGrid,
        voxel_scale: f32,
        probe_resolution: (usize, usize, usize),
    ) -> Vec<u8> {
        use crate::adapters::voxel_mapper::{FaceDirection, SurfaceMeshingPolicy};
        use crate::domain::entities::probe_grid::ProbeGrid;

        let mapper = VoxelMapper::with_policy(
            voxel_scale,
            &DEFAULT_MATERIAL_PALETTE,
            SurfaceMeshingPolicy::LowSpec,
        );
        let quads = mapper.map_voxel_grid(grid);
        let probe_grid = ProbeGrid::from_voxel_grid(
            grid,
            probe_resolution.0,
            probe_resolution.1,
            probe_resolution.2,
        );

        let header_size = 24;
        let quad_size = 28;
        let probe_size = probe_grid.as_bytes().len();
        let total_size = header_size + quads.len() * quad_size + probe_size;

        let mut binary = Vec::with_capacity(total_size);

        // Header
        binary.extend_from_slice(&(quads.len() as u32).to_le_bytes());
        binary.extend_from_slice(&(probe_resolution.0 as u32).to_le_bytes());
        binary.extend_from_slice(&(probe_resolution.1 as u32).to_le_bytes());
        binary.extend_from_slice(&(probe_resolution.2 as u32).to_le_bytes());
        binary.extend_from_slice(&voxel_scale.to_le_bytes());
        binary.extend_from_slice(&1u32.to_le_bytes()); // 1 = LowSpec

        // Packed quads
        for q in &quads {
            binary.extend_from_slice(&q.x.to_le_bytes());
            binary.extend_from_slice(&q.y.to_le_bytes());
            binary.extend_from_slice(&q.z.to_le_bytes());
            binary.extend_from_slice(&q.w.to_le_bytes());
            binary.extend_from_slice(&q.h.to_le_bytes());

            let dir_u8 = match q.dir {
                FaceDirection::Up => 0u8,
                FaceDirection::Down => 1u8,
                FaceDirection::North => 2u8,
                FaceDirection::South => 3u8,
                FaceDirection::East => 4u8,
                FaceDirection::West => 5u8,
            };
            binary.push(dir_u8);
            binary.push(q.material);
            binary.push(q.light);
            binary.push(q.ao);
            binary.extend_from_slice(&q.color.to_le_bytes());
        }

        // Probe grid payload
        binary.extend_from_slice(probe_grid.as_bytes());

        binary
    }

    /// Converts VoxelGrid into a Sparse Voxel Octree (SVO) and serializes
    /// the flattened node texture buffer and structural metadata to JSON.
    pub fn to_octree_json(grid: &VoxelGrid, depth: u32, world_size: f32) -> String {
        let builder = BuildOctreeUseCase::new(&DEFAULT_MATERIAL_PALETTE);
        let svo = builder.execute(grid, depth, world_size);
        let gpu_data = OctreeGpuSerializer::serialize_to_gpu_data(&svo);

        let mut json = String::with_capacity(gpu_data.texel_data.len() * 8 + 128);
        json.push_str("{\"root\":");
        json.push_str(&svo.root.to_string());
        json.push_str(",\"depth\":");
        json.push_str(&svo.depth.to_string());
        json.push_str(",\"world_size\":");
        json.push_str(&svo.world_size.to_string());
        json.push_str(",\"nodes\":[");

        for (i, &val) in gpu_data.texel_data.iter().enumerate() {
            json.push_str(&val.to_string());
            if i < gpu_data.texel_data.len() - 1 {
                json.push(',');
            }
        }
        json.push_str("]}");
        json
    }

    /// High-performance Binary Serializer for SVO Raymarching:
    /// Compiles SVO nodes directly to little-endian bytes to avoid JSON parsing cost.
    ///
    /// BINARY SCHEMATIC (Byte offset -> payload):
    /// - `0..4`: `root` node index (u32, little-endian)
    /// - `4..8`: `depth` of tree (u32, little-endian)
    /// - `8..12`: `world_size` scale (f32, little-endian)
    /// - `12..16`: `texture_width` (u32, little-endian)
    /// - `16..20`: `texture_height` (u32, little-endian)
    /// - `20..24`: `voxel_scale` (f32, little-endian)
    /// - `24..28`: `chunk_size` (f32, little-endian)
    /// - `28..`: flat `u32` texel array data (4 * width * height * 4 bytes)
    pub fn to_octree_binary(
        grid: &VoxelGrid,
        depth: u32,
        world_size: f32,
        voxel_scale: f32,
        chunk_size: f32,
    ) -> Vec<u8> {
        let builder = BuildOctreeUseCase::new(&DEFAULT_MATERIAL_PALETTE);
        let svo = builder.execute(grid, depth, world_size);
        let gpu_data = OctreeGpuSerializer::serialize_to_gpu_data(&svo);

        let header_size = 28; // 7 values * 4 bytes
        let mut binary = Vec::with_capacity(header_size + gpu_data.texel_data.len() * 4);

        binary.extend_from_slice(&(svo.root as u32).to_le_bytes());
        binary.extend_from_slice(&depth.to_le_bytes());
        binary.extend_from_slice(&world_size.to_le_bytes());
        binary.extend_from_slice(&gpu_data.texture_width.to_le_bytes());
        binary.extend_from_slice(&gpu_data.texture_height.to_le_bytes());
        binary.extend_from_slice(&voxel_scale.to_le_bytes());
        binary.extend_from_slice(&chunk_size.to_le_bytes());

        for &val in &gpu_data.texel_data {
            binary.extend_from_slice(&val.to_le_bytes());
        }

        binary
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::entities::voxel_grid::VOXEL_WALL;

    #[test]
    fn test_to_json_format() {
        let mut grid = VoxelGrid::new(2, 2, 2);
        grid.set(0, 0, 0, VOXEL_WALL);
        let json = WebRendererAdapter::to_json(&grid, 2.0);
        assert!(json.starts_with("["));
        assert!(
            json.contains("\"w\":2"),
            "voxel scale must reach the mapper"
        );
    }

    #[test]
    fn test_to_octree_binary_format() {
        let mut grid = VoxelGrid::new(2, 2, 2);
        grid.set(0, 0, 0, VOXEL_WALL);
        let binary = WebRendererAdapter::to_octree_binary(&grid, 8, 25.6, 0.1, 20.0);

        // Header is 28 bytes
        assert!(binary.len() >= 28);
        // Total bytes must be aligned to 4-byte boundaries
        assert_eq!(binary.len() % 4, 0);
    }

    #[test]
    fn test_to_low_spec_json_merging() {
        let mut grid = VoxelGrid::new(2, 1, 1);
        grid.set(0, 0, 0, VOXEL_WALL);
        grid.set(1, 0, 0, VOXEL_WALL);
        grid.set_light(0, 0, 0, 3);
        grid.set_light(1, 0, 0, 14);

        let low_json = WebRendererAdapter::to_low_spec_json(&grid, 1.0);
        // LowSpec mode merges the top face into 1 quad with width 2
        assert!(low_json.contains("\"w\":2"));
    }

    #[test]
    fn test_to_low_spec_surface_binary_payload() {
        let mut grid = VoxelGrid::new(4, 2, 4);
        grid.set(0, 0, 0, VOXEL_WALL);

        let binary = WebRendererAdapter::to_low_spec_surface_binary(&grid, 1.0, (4, 2, 4));
        let header_size = 24;
        let probe_bytes = 4 * 2 * 4 * 3; // 96 bytes

        assert!(binary.len() > header_size + probe_bytes);

        // Check quad_count in header (bytes 0..4)
        let quad_count = u32::from_le_bytes(binary[0..4].try_into().unwrap());
        assert!(quad_count > 0);

        // Check policy in header (bytes 20..24)
        let policy = u32::from_le_bytes(binary[20..24].try_into().unwrap());
        assert_eq!(policy, 1, "policy field must be 1 (LowSpec)");
    }
}
