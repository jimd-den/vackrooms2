// Instanced surfel strategy: the surface as a cloud of oriented discs.
//
// The other raster strategies draw the mesher's topology -- merged quads as
// triangles, or one rectangle per merged quad. Both can only draw less by
// re-meshing at a coarser voxel scale, which changes what the geometry is.
// A surfel cloud samples that same surface into discs at a chosen spacing,
// so density is a dial rather than a rebuild.
//
// The work splits like the splat path's, and for the same reason:
//
//   vertex   -- irradiance once per disc, flat interpolated. A disc that
//               covers more than a handful of pixels beats per-fragment
//               shading outright.
//   fragment -- the disc mask, albedo, fog. No light loop.

// There is deliberately no rim feather here.
//
// Fading a disc's albedo toward its edge looks like the obvious way to let
// neighbours blend, and in an opaque pass it is exactly wrong: with no
// alpha blending the fade is not a cross-fade, it is a darkening, so every
// disc draws its own outline and the wall comes out as fish scales. The
// first version of this shader did that, and the image was convincing
// enough to look intentional.
//
// Overlap alone is what makes the surface continuous: each disc reaches
// the corner of its own cell (see `radius_for_spacing`), so coverage is
// complete without any edge treatment. Blending, if it is ever wanted,
// needs a real weighted-average resolve, not a multiply.

/// Minecraft's per-face directional constants, shared with the splat path
/// so the two strategies light an identical wall identically.
fn surfel_direction_gain(axis: u32) -> f32 {
    switch axis {
        case 0u: { return 1.00; }  // up
        case 1u: { return 0.52; }  // down
        case 2u, 3u: { return 0.80; }  // north / south
        default: { return 0.64; }  // east / west
    }
}

struct SurfelVertex {
    @builtin(position) clip_position: vec4<f32>,
    @location(0) world: vec3<f32>,
    /// Position within the disc; the fragment stage cuts the circle.
    @location(1) disc: vec2<f32>,
    /// Interpolated, not flat -- see the note in the vertex stage.
    @location(2) irradiance: vec3<f32>,
    @location(3) @interpolate(flat) visual: u32,
    @location(4) @interpolate(flat) material: u32,
    @location(5) @interpolate(flat) axis: u32,
};

@vertex
fn surfel_vertex(
    @builtin(vertex_index) corner_index: u32,
    @builtin(instance_index) surfel_index: u32,
) -> SurfelVertex {
    let packed = packed_surfels[surfel_index];
    let axis = surfel_axis(packed);
    let material = packed.surface & 255u;
    let baked = f32((packed.surface >> 8u) & 255u) / 15.0;
    let ao = 1.0 - f32((packed.surface >> 16u) & 1u);
    let normal = normal_for_axis(axis);
    let world = surfel_world_position(packed, corner_index);

    // Shade at the corner, and interpolate.
    //
    // The splat path shades once per face and flat-interpolates, which is
    // what makes a voxel read as a block. Copying that here produced a
    // wall of fish scales, and the reason is structural rather than
    // cosmetic: splat faces *tile*, so a constant colour per face has no
    // visible seam, while surfels *overlap* by design -- 41% of each
    // disc's area, so its radius can reach the corners of its own cell.
    // Two overlapping constants meet along a circular arc, and every one
    // of those arcs is visible. Sampling the shared lighting function at
    // the corner's own position instead makes two discs agree wherever
    // they overlap, because they are asking the same question about the
    // same point in the world.
    let irradiance = raster_surface_radiance(
        world,
        normal,
        vec3<f32>(1.0),
        baked,
        ao,
        chunk.draw.x,
        chunk.draw.y
    ) * surfel_direction_gain(axis);

    var output: SurfelVertex;
    output.clip_position = frame.view_projection * vec4<f32>(world, 1.0);
    output.world = world;
    output.disc = vec2<f32>(
        select(-1.0, 1.0, corner_index == 1u || corner_index == 3u),
        select(-1.0, 1.0, corner_index >= 2u)
    );
    output.irradiance = irradiance;
    output.visual = packed.visual;
    output.material = material;
    output.axis = axis;
    return output;
}

@fragment
fn surfel_fragment(input: SurfelVertex) -> @location(0) vec4<f32> {
    // The disc. Without this the cloud is a field of squares, which tiles
    // suspiciously well and hides exactly the coverage gaps the radius was
    // chosen to close -- a bug that looks like success.
    let r = length(input.disc);
    if r > 1.0 {
        discard;
    }

    let voxel = chunk.bounds_voxel_size.w;
    let normal = normal_for_axis(input.axis);
    let albedo = apply_material_pattern(
        input.material,
        packed_srgb_to_linear(input.visual),
        input.world,
        normal,
        voxel
    );

    let emission = f32(input.visual >> 24u) / 25.5;
    let emits = emission > 0.0 && normal.y < -0.5;
    let radiance = select(albedo * input.irradiance, albedo * emission, emits);
    return present_radiance(radiance, input.world, input.clip_position.xy);
}
