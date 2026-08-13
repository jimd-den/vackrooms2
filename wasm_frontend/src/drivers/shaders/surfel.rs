//! Instanced surfel program — the GPU disc splatter.
//!
//! One instance is one oriented disc (`PackedSurfel`). The vertex shader
//! builds a square in the disc's own tangent plane from `gl_VertexID`
//! (4-vertex TRIANGLE_STRIP) and the fragment shader discards everything
//! outside the inscribed circle.
//!
//! ## Why the quad lies in the surface, not across the screen
//!
//! The obvious way to draw a disc is a camera-facing billboard. That is
//! wrong here for a reason that only shows up in a corner: a billboard is
//! perpendicular to the view, so where two walls meet, the two clouds'
//! billboards interpenetrate and the seam crawls as the camera turns. A
//! quad lying *in* the surface has the surface's own depth at every
//! fragment, so two walls meet exactly along their true intersection and
//! stay there.
//!
//! It also removes the CPU splatter's hardest problem for free. That
//! implementation carries one depth per surfel and needs a per-pixel depth
//! *window* to stop coplanar neighbours arbitrating for pixels; an
//! in-surface quad gets depth interpolated per fragment, so neighbours
//! agree exactly where they overlap and the ordinary depth test is enough.
//!
//! Lighting is the splat path's, minus the shadow map: a surfel cloud has
//! no retained mesh to cast with, and inventing one would make the two
//! representations disagree about the silhouette.

use super::{chunks, encode_display_color};

const VERTEX_HEADER: &str = r#"#version 300 es
precision highp float;
precision highp sampler3D;

in vec3 aSurfelPos;    // disc centre, chunk-local fixed point (1/1024 u)
in vec4 aSurfelMeta;   // radius (1/32 u steps), normal_axis, material, baked_light
in vec2 aSurfelFlags;  // ao, flags (bit 0: emissive)

uniform mat4 uProjection;
uniform mat4 uView;
uniform vec3 uChunkOrigin;
uniform vec3 uChunkSize;
uniform float uVoxelScale;
uniform vec3 uCameraPosition;
uniform vec3 uCamForward;
uniform int uFlashlightEnabled;
uniform sampler3D uLightVolume;

uniform int uLightCount;
uniform vec3 uLightPositions[16];
uniform vec3 uLightColors[16];
uniform vec4 uLightParams[16];

uniform int uOutdoor;
uniform float uAmbientScale;

out vec3 vWorldPos;
out vec3 vColor;
out vec2 vDisc;        // position within the disc, unit circle
flat out vec3 vNormal;
flat out float vMaterial;
flat out float vEmissive;
flat out float vGrainAmp;
"#;

const VERTEX_BODY: &str = r#"
vec3 normalForAxis(float axis) {
    if (axis < 0.5) return vec3(0.0, 1.0, 0.0);
    if (axis < 1.5) return vec3(0.0, -1.0, 0.0);
    if (axis < 2.5) return vec3(0.0, 0.0, -1.0);
    if (axis < 3.5) return vec3(0.0, 0.0, 1.0);
    if (axis < 4.5) return vec3(1.0, 0.0, 0.0);
    return vec3(-1.0, 0.0, 0.0);
}

// In-plane basis with U x V = outward normal, so the strip order
// (-1,-1)(1,-1)(-1,1)(1,1) is counter-clockwise seen from the front.
void faceBasis(float axis, out vec3 u, out vec3 v) {
    if (axis < 0.5)      { u = vec3(1.0, 0.0, 0.0); v = vec3(0.0, 0.0, -1.0); }
    else if (axis < 1.5) { u = vec3(1.0, 0.0, 0.0); v = vec3(0.0, 0.0, 1.0); }
    else if (axis < 2.5) { u = vec3(1.0, 0.0, 0.0); v = vec3(0.0, -1.0, 0.0); }
    else if (axis < 3.5) { u = vec3(1.0, 0.0, 0.0); v = vec3(0.0, 1.0, 0.0); }
    else if (axis < 4.5) { u = vec3(0.0, 1.0, 0.0); v = vec3(0.0, 0.0, 1.0); }
    else                 { u = vec3(0.0, 1.0, 0.0); v = vec3(0.0, 0.0, -1.0); }
}

void main() {
    vec2 corner = vec2(
        (gl_VertexID == 1 || gl_VertexID == 3) ? 1.0 : -1.0,
        (gl_VertexID >= 2) ? 1.0 : -1.0
    );
    vDisc = corner;

    float axis = aSurfelMeta.y;
    vec3 N = normalForAxis(axis);
    vec3 U, V;
    faceBasis(axis, U, V);

    // RADIUS_QUANT, mirrored from adapters::surfel_cloud. The disc is
    // inscribed in this square, so the square is the full diameter across.
    float radius = aSurfelMeta.x * (1.0 / 32.0);
    vec3 centre = uChunkOrigin + aSurfelPos * (1.0 / 1024.0);
    // Lift the quad a hair off its own surface along the normal. Discs and
    // the surface they sample are exactly coplanar, and on a depth buffer
    // exact coplanarity is the one thing that reliably z-fights.
    vec3 world = centre + N * (0.0015)
        + corner.x * radius * U + corner.y * radius * V;

    vWorldPos = world;
    vNormal = N;
    vMaterial = aSurfelMeta.z;
    gl_Position = uProjection * uView * vec4(world, 1.0);

    vec3 albedo = srgbToLinear(materialColor(aSurfelMeta.z));
    bool emissive = aSurfelFlags.y > 0.5;
    vEmissive = emissive ? 1.0 : 0.0;
    bool isCeiling = aSurfelMeta.z > 2.5 && aSurfelMeta.z < 3.5;
    vGrainAmp = (emissive || isCeiling) ? 0.0 : 0.02;
    if (emissive) {
        vColor = albedo * 1.5;
        return;
    }

    vec3 samplePos = centre + N * (0.5 * uVoxelScale);
    vec3 uvw = (samplePos - uChunkOrigin) / uChunkSize;
    vec3 irradiance = textureLod(uLightVolume, clamp(uvw, 0.0, 1.0), 0.0).rgb;
    vec3 roomAmbient = vec3(0.045, 0.040, 0.024);
    irradiance = roomAmbient + irradiance * 0.12;
    if (uOutdoor == 1) {
        irradiance = max(irradiance, vec3(0.30, 0.32, 0.36));
    }

    float faceResponse;
    if (N.y > 0.5) {
        faceResponse = 1.0;
    } else if (N.y < -0.5) {
        faceResponse = 0.35;
    } else {
        faceResponse = 0.65 + 0.1 * N.x + 0.05 * N.z;
    }

    float ao = mix(1.0, 0.62, clamp(aSurfelFlags.x, 0.0, 1.0));
    vec3 ambientUp = vec3(1.05, 1.0, 0.72) * irradiance;
    vec3 ambientDown = vec3(0.58, 0.52, 0.30) * irradiance;
    if (uOutdoor == 1) {
        ambientUp = vec3(0.98, 1.02, 1.08) * irradiance;
        ambientDown = vec3(0.56, 0.58, 0.55) * irradiance;
    }
    vec3 ambient = mix(ambientDown, ambientUp, N.y * 0.5 + 0.5)
        * ao * faceResponse * uAmbientScale;

    vec3 directDiffuse = vec3(0.0);
    for (int i = 0; i < 16; ++i) {
        if (i >= uLightCount) break;
        vec3 toLight = uLightPositions[i] - world;
        float d2 = dot(toLight, toLight);
        float range = uLightParams[i].x;
        float range2 = range * range;
        if (d2 >= range2) continue;

        float d = sqrt(d2);
        vec3 L = toLight / max(d, 0.001);
        float ndotl = max(dot(N, L), 0.0);
        float x = clamp(d2 / range2, 0.0, 1.0);
        float falloff = (1.0 - x) * (1.0 - x);
        float lightHash = hash3D(uLightPositions[i] * 10.0);
        float directMod = 1.0 + 0.05 * (lightHash - 0.5);
        vec3 lightColor = mix(uLightColors[i], vec3(1.0, 0.96, 0.85), 0.3);
        directDiffuse += lightColor * uLightParams[i].y * falloff * ndotl * directMod;
    }

    vec3 flashlight = vec3(0.0);
    if (uFlashlightEnabled == 1) {
        flashlight = spotBeam(uCameraPosition, uCamForward, centre, N);
    }

    const float INV_PI = 0.3183098861837907;
    vColor = albedo * ((ambient + directDiffuse) * INV_PI + flashlight);
}
"#;

const FRAGMENT_HEADER: &str = r#"#version 300 es
precision highp float;
precision highp int;

in vec3 vWorldPos;
in vec3 vColor;
in vec2 vDisc;
flat in vec3 vNormal;
flat in float vMaterial;
flat in float vEmissive;
flat in float vGrainAmp;

uniform vec3 uCameraPosition;
uniform float uVoxelScale;
uniform int uDitherEnabled;

uniform int uCoreCount;
uniform vec4 uCores[4];
uniform vec3 uCoreColors[4];

uniform int uOutdoor;
uniform vec3 uFogColor;

out vec4 fragColor;
"#;

const FRAGMENT_BODY: &str = r#"
void main() {
    // The disc. Everything outside the inscribed circle of the quad is not
    // part of this surfel -- without this the cloud is a field of squares,
    // which tiles suspiciously well and hides exactly the coverage gaps
    // the disc radius was chosen to close.
    if (dot(vDisc, vDisc) > 1.0) {
        discard;
    }

    vec3 color = applyMaterialPattern(
        vMaterial, vColor, vWorldPos, vNormal, uVoxelScale
    );
    float distanceToCamera = length(uCameraPosition - vWorldPos);

    if (vEmissive < 0.5) {
        vec3 lattice = floor((vWorldPos - vNormal * (0.5 * uVoxelScale)) / uVoxelScale);
        float grain = 1.0 + (hash3D(lattice) - 0.5) * 2.0 * vGrainAmp * float(uDitherEnabled);
        color *= grain;
    }

    float fogOnset = 14.0;
    float fogDist = max(0.0, distanceToCamera - fogOnset);
    float fogDensity = 0.032;
    float heightFactor = 1.0 + 0.35 * smoothstep(0.0, 3.4, vWorldPos.y);
    float fogAmount = 1.0 - exp(-fogDist * fogDensity * heightFactor);
    fogAmount = max(fogAmount, smoothstep(34.0, 48.0, distanceToCamera));
    vec3 currentFogColor = vec3(0.0);
    if (uOutdoor == 1) {
        currentFogColor = uFogColor;
    }
    color = mix(color, currentFogColor, clamp(fogAmount, 0.0, 1.0));

    color += flareCores(vWorldPos, uCameraPosition);

    color = encodeDisplayColor(color);
    color += (ign(gl_FragCoord.xy) - 0.5) * (1.0 / 128.0) * (1.0 - fogAmount) * float(uDitherEnabled);

    fragColor = vec4(color, 1.0);
}
"#;

/// Assembles the vertex shader (disc quad + lighting, evaluated per corner).
pub fn vertex_source() -> String {
    let material_color = chunks::material_color_glsl();
    let mut source = String::with_capacity(
        VERTEX_HEADER.len()
            + material_color.len()
            + chunks::NOISE_GLSL.len()
            + encode_display_color::GLSL.len()
            + chunks::SPOT_CONE_GLSL.len()
            + VERTEX_BODY.len(),
    );
    source.push_str(VERTEX_HEADER);
    source.push_str(&material_color);
    source.push_str(chunks::NOISE_GLSL);
    source.push_str(encode_display_color::GLSL);
    source.push_str(chunks::SPOT_CONE_GLSL);
    source.push_str(VERTEX_BODY);
    source
}

/// Assembles the fragment shader (disc mask, finish, fog, encoding).
pub fn fragment_source() -> String {
    let mut source = String::with_capacity(
        FRAGMENT_HEADER.len()
            + chunks::NOISE_GLSL.len()
            + chunks::MATERIAL_PATTERN_GLSL.len()
            + encode_display_color::GLSL.len()
            + chunks::FLARE_CORES_GLSL.len()
            + FRAGMENT_BODY.len(),
    );
    source.push_str(FRAGMENT_HEADER);
    source.push_str(chunks::NOISE_GLSL);
    source.push_str(chunks::MATERIAL_PATTERN_GLSL);
    source.push_str(encode_display_color::GLSL);
    source.push_str(chunks::FLARE_CORES_GLSL);
    source.push_str(FRAGMENT_BODY);
    source
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The disc mask is the whole difference between a surfel cloud and a
    /// field of squares. If it is ever optimized away the image still looks
    /// plausible, which is why it is pinned here.
    #[test]
    fn the_fragment_shader_discards_outside_the_disc() {
        let source = fragment_source();
        assert!(source.contains("dot(vDisc, vDisc) > 1.0"));
        assert!(source.contains("discard"));
    }

    /// The quad must be built in the surface's own tangent plane. A
    /// camera-facing billboard would interpenetrate at every corner where
    /// two walls meet, and the seam would crawl as the camera turned.
    #[test]
    fn the_quad_lies_in_the_surface_not_across_the_view() {
        let source = vertex_source();
        assert!(source.contains("faceBasis(axis, U, V)"));
        assert!(
            !source.contains("uCamRight") && !source.contains("uCamUp"),
            "the quad is being built from a camera basis"
        );
    }

    /// Radius decodes with the same constant the CPU side encodes with.
    /// Two copies of a quantization step is exactly the kind of thing that
    /// drifts and then produces a cloud with holes on one path only.
    #[test]
    fn the_radius_quantization_matches_the_cpu_side() {
        let step = crate::adapters::surfel_cloud::RADIUS_QUANT;
        assert_eq!(step, 1.0 / 32.0);
        assert!(vertex_source().contains("(1.0 / 32.0)"));
    }

    #[test]
    fn shared_finishes_run_before_display_encoding() {
        let source = fragment_source();
        let pattern = source.find("applyMaterialPattern(").expect("finish");
        let display = source.rfind("encodeDisplayColor(").expect("encoding");
        assert!(pattern < display);
    }
}
