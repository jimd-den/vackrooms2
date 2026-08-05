// Compact face expansion shared by visible and caster passes.

struct PackedFace {
    xy: u32,
    z_extents: u32,
    surface: u32,
    // low 24: domain sRGB color; high 8: authored emission over 0..=10.
    visual: u32,
};

@group(1) @binding(1) var<storage, read> packed_faces: array<PackedFace>;

fn face_u(axis: u32) -> vec3<f32> {
    if axis <= 3u { return vec3<f32>(1.0, 0.0, 0.0); }
    return vec3<f32>(0.0, 1.0, 0.0);
}

fn face_v(axis: u32) -> vec3<f32> {
    switch axis {
        case 0u: { return vec3<f32>(0.0, 0.0, -1.0); }
        case 1u: { return vec3<f32>(0.0, 0.0, 1.0); }
        case 2u: { return vec3<f32>(0.0, -1.0, 0.0); }
        case 3u: { return vec3<f32>(0.0, 1.0, 0.0); }
        case 4u: { return vec3<f32>(0.0, 0.0, 1.0); }
        default: { return vec3<f32>(0.0, 0.0, -1.0); }
    }
}

fn splat_face_center(packed: PackedFace) -> vec3<f32> {
    let center_local = vec3<f32>(
        f32(packed.xy & 65535u),
        f32(packed.xy >> 16u),
        f32(packed.z_extents & 65535u)
    ) * (1.0 / 1024.0);
    return chunk.origin_world_size.xyz + center_local;
}

// Reality-unfold. `chunk.origin_world_size.w` is the chunk's resolve
// progress: 0 is an unresolved lattice of suspended voxel specks, 1 is
// settled architecture. Faces grow about their own centre and converge
// along their normal, so a materializing chunk reads as its substrate
// assembling rather than as geometry fading up from nothing.
//
// This lives in the vertex path deliberately: the fragment path is the
// renderer's bottleneck, and this costs it nothing.
//
// Duplicated from `common.wgsl::normal_for_axis` on purpose -- the splat
// *shadow* program composes this file without `common.wgsl`.
fn unfold_face_normal(axis: u32) -> vec3<f32> {
    switch axis {
        case 0u: { return vec3<f32>(0.0, 1.0, 0.0); }
        case 1u: { return vec3<f32>(0.0, -1.0, 0.0); }
        case 2u: { return vec3<f32>(0.0, 0.0, -1.0); }
        case 3u: { return vec3<f32>(0.0, 0.0, 1.0); }
        case 4u: { return vec3<f32>(1.0, 0.0, 0.0); }
        default: { return vec3<f32>(-1.0, 0.0, 0.0); }
    }
}

/// Fraction of the unfold window spent staggering face start times, so the
/// lattice fills in unevenly instead of inflating as one rigid block.
const UNFOLD_STAGGER: f32 = 0.45;
/// How far, in voxels, an unresolved face floats off its final position.
const UNFOLD_SCATTER: f32 = 5.0;
/// Size an unresolved face keeps so the lattice stays visible as specks
/// rather than collapsing to zero-area invisibility.
const UNFOLD_MIN_SIZE: f32 = 0.16;

fn unfold_face_hash(packed: PackedFace) -> f32 {
    var h = packed.xy ^ (packed.z_extents * 2654435761u);
    h = h ^ (h >> 15u);
    h = h * 2246822519u;
    h = h ^ (h >> 13u);
    return f32(h & 65535u) * (1.0 / 65535.0);
}

fn splat_face_world_position(packed: PackedFace, corner_index: u32) -> vec3<f32> {
    let extent_u = f32((packed.z_extents >> 16u) & 255u);
    let extent_v = f32(packed.z_extents >> 24u);
    let axis = packed.surface & 255u;
    let corner = vec2<f32>(
        select(-1.0, 1.0, corner_index == 1u || corner_index == 3u),
        select(-1.0, 1.0, corner_index >= 2u)
    );

    let unfold = clamp(chunk.origin_world_size.w, 0.0, 1.0);
    let stagger = unfold_face_hash(packed);
    let local = clamp(
        (unfold - stagger * UNFOLD_STAGGER) / (1.0 - UNFOLD_STAGGER),
        0.0,
        1.0
    );
    let grow = local * local * (3.0 - 2.0 * local);
    let size = mix(UNFOLD_MIN_SIZE, 1.0, grow);
    let scatter = (1.0 - grow)
        * (stagger - 0.5)
        * chunk.bounds_voxel_size.w
        * UNFOLD_SCATTER;

    return splat_face_center(packed)
        + unfold_face_normal(axis) * scatter
        + face_u(axis) * corner.x * extent_u * chunk.bounds_voxel_size.w * 0.5 * size
        + face_v(axis) * corner.y * extent_v * chunk.bounds_voxel_size.w * 0.5 * size;
}
