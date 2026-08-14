//! Dense brick lookup: one texel fetch instead of a dependent pointer chase.
//!
//! GLSL port of the WebGPU raymarcher's `read_brick_voxel`
//! (`drivers/webgpu/shaders/raymarch.wgsl`). The two words `BrickPoolGpuData`
//! packs per voxel land in one RG32UI texel (`BrickVoxelTexture` in
//! `drivers/webgl/atlas.rs`), so this is exactly one `texelFetch` instead of
//! `lookupVoxelLeaf`'s per-level dependent chase.

pub const GLSL: &str = r#"
/// Voxels along one brick edge. Must match `BRICK_EDGE` in the core's
/// `build_brick_pool`.
const uint BRICK_EDGE = 4u;
/// Must match `BRICK_ATLAS_WIDTH` in `drivers/webgl/atlas.rs`.
const int BRICK_TEXTURE_WIDTH = 512;

/// The returned bounds are the *individual voxel's* box, never the brick's:
/// empty-space skipping advances to the far side of whatever bounds it is
/// handed, so returning the brick would step over the solid voxels sharing it.
VoxelLeaf readBrickVoxel(uint wordBase, vec3 point, vec3 boundsMin, vec3 boundsMax) {
    vec3 extent = (boundsMax - boundsMin) / float(BRICK_EDGE);
    float limit = float(BRICK_EDGE) - 1.0;
    // The DDA samples a hair inside the cell it means, but rounding can
    // still land a fraction outside the brick; clamping keeps the fetch in
    // bounds without moving any sample that was already correct.
    vec3 local = clamp(
        floor((point - boundsMin) / extent),
        vec3(0.0),
        vec3(limit)
    );
    uvec3 cell = uvec3(local);
    uint offset = cell.z * BRICK_EDGE * BRICK_EDGE + cell.y * BRICK_EDGE + cell.x;
    // wordBase is a word offset into the two-words-per-voxel arena and is
    // always voxel-aligned (a brick's own base is BRICK_EDGE^3 * 2 words in),
    // so this recovers the voxel index the RG32UI texture is addressed by.
    uint voxelIndex = wordBase / 2u + offset;
    ivec2 texel = ivec2(int(voxelIndex) % BRICK_TEXTURE_WIDTH, int(voxelIndex) / BRICK_TEXTURE_WIDTH);
    uvec4 packed = texelFetch(uBrickTexture, texel, 0);
    uint packedVoxel = packed.x;
    uint packedLight = packed.y;

    // Re-pack into the leaf light layout so every consumer above decodes
    // one encoding: scalar level, then occlusion at bit 8, then r/g/b.
    uint red = (packedLight >> 8u) & 0xFu;
    uint green = (packedLight >> 12u) & 0xFu;
    uint blue = (packedLight >> 16u) & 0xFu;
    uint lightWord = max(red, max(green, blue))
        | ((packedLight & 0xFFu) << 8u)
        | (red << 16u)
        | (green << 20u)
        | (blue << 24u);

    vec3 voxelMin = boundsMin + local * extent;
    return VoxelLeaf(
        packedVoxel >> 24u,
        packedVoxel & 0x00FFFFFFu,
        lightWord,
        voxelMin,
        voxelMin + extent
    );
}
"#;
