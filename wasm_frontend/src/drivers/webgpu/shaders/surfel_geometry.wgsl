// Surfel expansion: one 16-byte record becomes a quad in the surface's own
// tangent plane, with the disc cut out in the fragment stage.

struct PackedSurfel {
    // x | y << 16, chunk-local fixed point (1/1024 u).
    xy: u32,
    // z | radius << 16 | normal_axis << 24. Radius is in 1/32 u steps,
    // matching adapters::surfel_cloud::RADIUS_QUANT.
    z_radius_axis: u32,
    // material | baked_light << 8 | ao << 16 | flags << 24.
    surface: u32,
    // low 24: domain sRGB color; high 8: authored emission over 0..=10.
    visual: u32,
};

@group(1) @binding(1) var<storage, read> packed_surfels: array<PackedSurfel>;

// In-plane basis with U x V = outward normal, so the strip corner order
// (-1,-1)(1,-1)(-1,1)(1,1) is counter-clockwise seen from the front and
// back-face culling removes the inside of every wall for free.
fn surfel_u(axis: u32) -> vec3<f32> {
    if axis <= 3u { return vec3<f32>(1.0, 0.0, 0.0); }
    return vec3<f32>(0.0, 1.0, 0.0);
}

fn surfel_v(axis: u32) -> vec3<f32> {
    switch axis {
        case 0u: { return vec3<f32>(0.0, 0.0, -1.0); }
        case 1u: { return vec3<f32>(0.0, 0.0, 1.0); }
        case 2u: { return vec3<f32>(0.0, -1.0, 0.0); }
        case 3u: { return vec3<f32>(0.0, 1.0, 0.0); }
        case 4u: { return vec3<f32>(0.0, 0.0, 1.0); }
        default: { return vec3<f32>(0.0, 0.0, -1.0); }
    }
}

fn surfel_centre(packed: PackedSurfel) -> vec3<f32> {
    let centre_local = vec3<f32>(
        f32(packed.xy & 65535u),
        f32(packed.xy >> 16u),
        f32(packed.z_radius_axis & 65535u)
    ) * (1.0 / 1024.0);
    return chunk.origin_world_size.xyz + centre_local;
}

fn surfel_radius(packed: PackedSurfel) -> f32 {
    return f32((packed.z_radius_axis >> 16u) & 255u) * (1.0 / 32.0);
}

fn surfel_axis(packed: PackedSurfel) -> u32 {
    return packed.z_radius_axis >> 24u;
}

// The quad lies *in* the surface rather than facing the camera.
//
// A camera-facing billboard is the obvious way to draw a disc and it is
// wrong here: where two walls meet, billboards from the two clouds
// interpenetrate, and the seam crawls as the camera turns. A quad in the
// tangent plane carries the surface's own depth at every fragment, so the
// walls meet along their true intersection and stay there.
//
// It also removes the software splatter's hardest problem. That path holds
// one depth per surfel and needs a per-pixel depth window to stop coplanar
// neighbours arbitrating for pixels; here depth is interpolated across the
// quad, so overlapping neighbours agree exactly and the ordinary depth test
// is enough.
fn surfel_world_position(packed: PackedSurfel, corner_index: u32) -> vec3<f32> {
    let axis = surfel_axis(packed);
    let normal = normal_for_axis(axis);
    let radius = surfel_radius(packed);
    let corner = vec2<f32>(
        select(-1.0, 1.0, corner_index == 1u || corner_index == 3u),
        select(-1.0, 1.0, corner_index >= 2u)
    );

    // Lift a hair along the normal. A disc and the surface it samples are
    // exactly coplanar, and exact coplanarity is the one configuration a
    // depth buffer reliably z-fights on.
    return surfel_centre(packed)
        + normal * 0.0015
        + surfel_u(axis) * corner.x * radius
        + surfel_v(axis) * corner.y * radius;
}
