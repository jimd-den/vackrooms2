// Instanced face-splat strategy. One compact 16-byte record becomes a quad
// in the vertex stage; face geometry is generated entirely on the GPU.
//
// Splat is the native voxel path and deliberately borrows Minecraft's
// visual grammar at a finer grain: a visible texel lattice, delineated
// voxel cells, and light that arrives in discrete calculated steps rather
// than as a smooth analytic falloff.
//
// The work is split to keep this the cheapest renderer:
//
//   vertex   -- irradiance, once per face, flat interpolated. This is the
//               expensive part (it walks the clustered light list) and a
//               face covering more than four pixels beats the surface
//               path's per-fragment shading outright.
//   fragment -- albedo on a quantized texel lattice plus cell outlines.
//               A floor and a hash, with no light loop.

/// Texels per voxel edge. Minecraft is 16 texels across a 1 m block; at a
/// 0.2 m voxel this is 20 texels per metre, so the grid reads as pixel art
/// that is finer than Minecraft rather than coarser.
const TEXELS_PER_VOXEL: f32 = 4.0;
/// Discrete light steps. Matches the authored 0..=15 baked spectrum, so
/// analytic and baked contributions quantize onto the same ladder.
const LIGHT_LEVELS: f32 = 15.0;
/// Fraction of a voxel cell darkened at its border.
const OUTLINE_WIDTH: f32 = 0.055;
const OUTLINE_DARKEN: f32 = 0.80;

/// Minecraft's per-face directional constants: which way a face points
/// changes its brightness even under identical illumination. This is what
/// keeps flat-shaded blocks legible as solids instead of silhouettes.
fn face_direction_gain(axis: u32) -> f32 {
    switch axis {
        case 0u: { return 1.00; }  // up
        case 1u: { return 0.52; }  // down
        case 2u, 3u: { return 0.80; }  // north / south
        default: { return 0.64; }  // east / west
    }
}

struct SplatVertex {
    @builtin(position) clip_position: vec4<f32>,
    @location(0) world: vec3<f32>,
    /// Face-constant irradiance. `flat` is what makes a voxel a block.
    @location(1) @interpolate(flat) irradiance: vec3<f32>,
    @location(2) @interpolate(flat) visual: u32,
    @location(3) @interpolate(flat) material: u32,
    @location(4) @interpolate(flat) axis: u32,
};

@vertex
fn splat_vertex(
    @builtin(vertex_index) corner_index: u32,
    @builtin(instance_index) face_index: u32,
) -> SplatVertex {
    let packed = packed_faces[face_index];
    let axis = packed.surface & 255u;
    let material = (packed.surface >> 8u) & 255u;
    let baked = f32((packed.surface >> 16u) & 255u) / 15.0;
    let ao = 1.0 - f32((packed.surface >> 24u) & 1u);
    let normal = normal_for_axis(axis);
    let world = splat_face_world_position(packed, corner_index);

    // Shade at the face centre, never at a corner: all four vertices must
    // agree, or the flat-interpolated value would depend on which vertex
    // the driver treats as provoking.
    let centre = splat_face_center(packed);

    // Unit albedo yields pure irradiance, so the fragment stage can apply
    // its own per-texel colour to a light term computed once per face.
    var irradiance = raster_surface_radiance(
        centre,
        normal,
        vec3<f32>(1.0),
        baked,
        ao,
        chunk.draw.x,
        chunk.draw.y
    ) * face_direction_gain(axis);
    // Light arrives in levels, not on a continuum -- but a level is a
    // scalar, exactly as it is in Minecraft. Quantizing each channel
    // independently snaps them onto different rungs and shifts hue: under
    // the amber indoor ambient, dim faces round to red-only.
    let level = max(irradiance.r, max(irradiance.g, irradiance.b));
    let stepped = floor(level * LIGHT_LEVELS + 0.5) / LIGHT_LEVELS;
    irradiance = irradiance * (stepped / max(level, 1e-5));

    var output: SplatVertex;
    output.clip_position = frame.view_projection * vec4<f32>(world, 1.0);
    output.world = world;
    output.irradiance = irradiance;
    output.visual = packed.visual;
    output.material = material;
    output.axis = axis;
    return output;
}

@fragment
fn splat_fragment(input: SplatVertex) -> @location(0) vec4<f32> {
    let voxel = chunk.bounds_voxel_size.w;
    let normal = normal_for_axis(input.axis);

    // Snap the pattern sample to a texel centre so the material reads as
    // hard pixel art instead of a gradient.
    let texel = voxel / TEXELS_PER_VOXEL;
    let sample = (floor(input.world / texel) + vec3<f32>(0.5)) * texel;
    var albedo = apply_material_pattern(
        input.material,
        packed_srgb_to_linear(input.visual),
        sample,
        normal,
        voxel
    );

    // Delineate voxel cells. Adding `abs(normal)` lifts the face's own
    // normal axis out of the minimum, leaving the two in-plane axes.
    let to_border = vec3<f32>(0.5) - abs(fract(input.world / voxel) - vec3<f32>(0.5));
    let masked = to_border + abs(normal);
    let border = min(masked.x, min(masked.y, masked.z));
    albedo = albedo * mix(OUTLINE_DARKEN, 1.0, smoothstep(0.0, OUTLINE_WIDTH, border));

    let emission = f32(input.visual >> 24u) / 25.5;
    let emits = emission > 0.0 && normal.y < -0.5;
    let radiance = select(albedo * input.irradiance, albedo * emission, emits);
    return present_radiance(radiance, input.world, input.clip_position.xy);
}
