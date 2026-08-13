//! A software surfel splatter: the reference implementation of the disc
//! rasterizer, and the one that can draw a frame with no GPU at all.
//!
//! The engine's other CPU rasterizer walks an SVO and paints screen-space
//! squares. This one has no scene to traverse — it is handed a list of
//! discs and projects them. That is the whole method, and it is worth
//! stating plainly because two details make the difference between a
//! picture and a mess:
//!
//! ## A disc projects to an ellipse, not a circle
//!
//! Seen edge-on, a disc is a line. Splatting every surfel as a screen-space
//! circle of its projected radius therefore *inflates* every surface you
//! are looking along — floors bloom toward the horizon, and walls seen at a
//! glancing angle spill over their own silhouette. So the footprint here is
//! the real ellipse: both of the disc's in-plane tangents are projected,
//! and the pixel test inverts the 2×2 matrix they span. A pixel belongs to
//! the surfel when its offset, expressed in that basis, lands inside the
//! unit circle.
//!
//! ## Neighbours must blend, not fight
//!
//! Adjacent surfels on one flat wall are coplanar, so their projected
//! depths differ by a hair. Under a plain depth test, whichever arrives
//! first wins each pixel, and the wall comes out stippled with the
//! arbitration pattern. Instead each pixel keeps a depth *window*: a
//! fragment nearer than the window replaces it, a fragment inside it
//! accumulates, and one behind it is discarded. Coplanar neighbours land
//! inside the window and average; a real occluder in front replaces. This
//! is the standard resolution and it is not optional — without it the
//! representation looks broken even when the geometry is right.
//!
//! Platform-free and natively unit-tested, like the rest of the adapters
//! layer.

use vackrooms::adapters::material_palette::material_visual;

use crate::adapters::surfel_cloud::{PackedSurfel, SURFEL_FLAG_EMISSIVE};

/// Camera and presentation state for one surfel frame.
///
/// The basis formulas are the splatter's and the GPU shaders', copied so
/// that a surfel frame and a mesh frame of the same world state can be laid
/// side by side and differ only in how the surface was drawn.
#[derive(Debug, Clone, Copy)]
pub struct SurfelCamera {
    pub position: [f32; 3],
    pub yaw: f32,
    pub pitch: f32,
    /// `tan(vertical_fov / 2)`.
    pub fov_tan: f32,
}

/// Level 0's Beer-Lambert falloff and the colour it falls off toward.
#[derive(Debug, Clone, Copy)]
pub struct SurfelFog {
    pub start: f32,
    pub density: f32,
    pub color: [f32; 3],
}

impl Default for SurfelFog {
    fn default() -> Self {
        // Level 0's authored values: full visibility for twelve units,
        // then an exponential fade into the dark.
        Self {
            start: 12.0,
            density: 0.018,
            color: [0.02, 0.02, 0.025],
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct SurfelRenderSettings {
    pub width: usize,
    pub height: usize,
    pub fog: SurfelFog,
    /// Half-width of the per-pixel depth blending window, world units. Must
    /// exceed the depth spread of coplanar neighbours and stay under the
    /// gap to the next real surface behind.
    pub blend_window: f32,
    /// Discs smaller than this on screen are still drawn, at one pixel: a
    /// distant surface made of sub-pixel surfels must not disappear.
    pub min_radius_px: f32,
    /// Ambient floor so unlit faces read as dark rather than as void.
    pub ambient: f32,
}

impl SurfelRenderSettings {
    pub fn new(width: usize, height: usize) -> Self {
        Self {
            width,
            height,
            fog: SurfelFog::default(),
            blend_window: 0.12,
            min_radius_px: 0.5,
            ambient: 0.06,
        }
    }
}

/// What one render cost, for reporting against the other strategies.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SurfelStats {
    pub submitted: usize,
    pub behind_camera: usize,
    pub back_facing: usize,
    pub off_screen: usize,
    pub drawn: usize,
    pub fragments: usize,
}

/// An 8-bit RGB image, row-major.
pub struct SurfelImage {
    pub width: usize,
    pub height: usize,
    pub rgb: Vec<u8>,
    pub stats: SurfelStats,
    /// Fraction of pixels that any surfel covered. The hole detector: a
    /// correct cloud viewed from inside a room fills essentially all of it.
    pub coverage: f32,
}

struct Basis {
    right: [f32; 3],
    up: [f32; 3],
    forward: [f32; 3],
    focal_px: f32,
    half_w: f32,
    half_h: f32,
}

fn dot(a: [f32; 3], b: [f32; 3]) -> f32 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

impl Basis {
    fn new(camera: &SurfelCamera, settings: &SurfelRenderSettings) -> Self {
        let (sy, cy) = camera.yaw.sin_cos();
        let (sp, cp) = camera.pitch.sin_cos();
        Self {
            right: [cy, 0.0, -sy],
            up: [sp * sy, cp, sp * cy],
            forward: [-cp * sy, sp, -cp * cy],
            focal_px: settings.height as f32 / (2.0 * camera.fov_tan),
            half_w: settings.width as f32 / 2.0,
            half_h: settings.height as f32 / 2.0,
        }
    }
}

/// The two unit tangents spanning a surfel's plane.
///
/// Any pair perpendicular to the normal will do — the disc is rotationally
/// symmetric, so the choice cannot affect the footprint.
fn tangents(normal: [f32; 3]) -> ([f32; 3], [f32; 3]) {
    // Cross with whichever world axis the normal is least aligned to, so
    // the result never degenerates.
    let helper = if normal[1].abs() < 0.9 {
        [0.0, 1.0, 0.0]
    } else {
        [1.0, 0.0, 0.0]
    };
    let t1 = normalize(cross(normal, helper));
    (t1, cross(normal, t1))
}

fn cross(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [
        a[1] * b[2] - a[2] * b[1],
        a[2] * b[0] - a[0] * b[2],
        a[0] * b[1] - a[1] * b[0],
    ]
}

fn normalize(v: [f32; 3]) -> [f32; 3] {
    let len = dot(v, v).sqrt();
    if len <= 1e-6 {
        [1.0, 0.0, 0.0]
    } else {
        [v[0] / len, v[1] / len, v[2] / len]
    }
}

/// Renders one frame. `origin` is the world offset of the cloud's chunk,
/// because surfel positions are chunk-local fixed point.
pub fn render_surfels(
    clouds: &[(&[PackedSurfel], [f32; 3])],
    camera: &SurfelCamera,
    settings: &SurfelRenderSettings,
) -> SurfelImage {
    let basis = Basis::new(camera, settings);
    let pixels = settings.width * settings.height;

    // Per pixel: nearest depth seen, plus a weighted colour accumulator for
    // every fragment inside that pixel's blending window.
    let mut depth = vec![f32::INFINITY; pixels];
    let mut accum = vec![[0.0f32; 3]; pixels];
    let mut weight = vec![0.0f32; pixels];
    let mut stats = SurfelStats::default();

    for (surfels, origin) in clouds {
        for surfel in *surfels {
            stats.submitted += 1;
            let local = surfel.world_position();
            let world = [
                local[0] + origin[0],
                local[1] + origin[1],
                local[2] + origin[2],
            ];
            let to_surfel = [
                world[0] - camera.position[0],
                world[1] - camera.position[1],
                world[2] - camera.position[2],
            ];
            let view_depth = dot(to_surfel, basis.forward);
            if view_depth <= 0.05 {
                stats.behind_camera += 1;
                continue;
            }
            let normal = surfel.normal();
            // A disc is one-sided: its back is the inside of a wall.
            if dot(normal, to_surfel) >= 0.0 {
                stats.back_facing += 1;
                continue;
            }

            let radius = surfel.world_radius();
            let (t1, t2) = tangents(normal);
            let Some((centre, ndc)) = project(to_surfel, &basis) else {
                stats.behind_camera += 1;
                continue;
            };

            // Frustum cull before the footprint, not after.
            //
            // The Jacobian below linearizes the perspective divide about
            // this point, and that linearization is only meaningful while
            // the point is roughly in front of the eye. A surfel *beside*
            // the camera — a metre to the left, a hand's breadth ahead —
            // has a small depth and a large lateral offset, so its `ndc`
            // runs to the hundreds and its footprint to the size of a
            // continent. A handful of those wash the entire frame out to
            // flat grey, which is not a subtle failure but is a very
            // confusing one, because the geometry and the shading are both
            // fine.
            //
            // The margin is the disc's own angular radius, so a surfel
            // just off the edge of the screen still contributes the part of
            // itself that reaches onto it.
            let margin = radius / view_depth;
            if ndc[0].abs() > basis.half_w / basis.focal_px + margin
                || ndc[1].abs() > basis.half_h / basis.focal_px + margin
            {
                stats.off_screen += 1;
                continue;
            }

            let axis_a = screen_axis(t1, radius, ndc, view_depth, &basis);
            let axis_b = screen_axis(t2, radius, ndc, view_depth, &basis);

            if draw_ellipse(
                centre,
                axis_a,
                axis_b,
                view_depth,
                surfel,
                settings,
                &mut depth,
                &mut accum,
                &mut weight,
                &mut stats,
            ) {
                stats.drawn += 1;
            } else {
                stats.off_screen += 1;
            }
        }
    }

    let mut rgb = vec![0u8; pixels * 3];
    let mut covered = 0usize;
    for i in 0..pixels {
        let color = if weight[i] > 0.0 {
            covered += 1;
            let inv = 1.0 / weight[i];
            let lit = [accum[i][0] * inv, accum[i][1] * inv, accum[i][2] * inv];
            fog_blend(lit, depth[i], &settings.fog)
        } else {
            settings.fog.color
        };
        for c in 0..3 {
            rgb[i * 3 + c] = (color[c].clamp(0.0, 1.0).powf(1.0 / 2.2) * 255.0).round() as u8;
        }
    }

    SurfelImage {
        width: settings.width,
        height: settings.height,
        rgb,
        stats,
        coverage: covered as f32 / pixels.max(1) as f32,
    }
}

/// Perspective-projects a camera-relative point to pixels, returning the
/// normalized image coordinates alongside — the footprint's Jacobian needs
/// them, and recomputing a divide per tangent would be wasteful.
///
/// `None` when the point is at or behind the eye plane and has no image.
fn project(to_point: [f32; 3], basis: &Basis) -> Option<([f32; 2], [f32; 2])> {
    let depth = dot(to_point, basis.forward);
    (depth > 0.05).then(|| {
        let ndc = [
            dot(to_point, basis.right) / depth,
            dot(to_point, basis.up) / depth,
        ];
        (
            [
                basis.half_w + basis.focal_px * ndc[0],
                basis.half_h - basis.focal_px * ndc[1],
            ],
            ndc,
        )
    })
}

/// Where one of the disc's tangents lands on screen: the projection's
/// Jacobian at the surfel's centre, scaled by the radius.
///
/// Both terms matter, and each of them alone is a distinct, believable
/// failure:
///
/// * Keep only `t·right / depth` — the naive "project the offset at the
///   centre's depth" — and any tangent pointing along the view direction
///   contributes nothing. A floor disc then flattens to a horizontal line,
///   and the floor renders as a radial spray of dashes from the vanishing
///   point.
/// * Drop the Jacobian entirely and project the tangent's *endpoint*
///   exactly, and the secant explodes whenever the endpoint is much nearer
///   the eye than the centre. Every grazing disc smears across the frame.
///
/// The second term, `-ndc * (t·forward)`, is the perspective stretch: how
/// much the image slides because the offset changed the depth. With it, a
/// disc lying along the view direction elongates toward the horizon, which
/// is what a real floor tile does.
fn screen_axis(
    tangent: [f32; 3],
    radius: f32,
    ndc: [f32; 2],
    depth: f32,
    basis: &Basis,
) -> [f32; 2] {
    let along_view = dot(tangent, basis.forward);
    let scale = basis.focal_px * radius / depth;
    [
        scale * (dot(tangent, basis.right) - ndc[0] * along_view),
        -scale * (dot(tangent, basis.up) - ndc[1] * along_view),
    ]
}

#[allow(clippy::too_many_arguments)]
fn draw_ellipse(
    centre: [f32; 2],
    axis_a: [f32; 2],
    axis_b: [f32; 2],
    view_depth: f32,
    surfel: &PackedSurfel,
    settings: &SurfelRenderSettings,
    depth: &mut [f32],
    accum: &mut [[f32; 3]],
    weight: &mut [f32],
    stats: &mut SurfelStats,
) -> bool {
    // Screen-space bound of the ellipse: the extent of |a| and |b| on each
    // axis, floored so a surfel that has shrunk below a pixel still marks
    // one. A surface that vanishes at range is worse than one that aliases.
    let ext_x = (axis_a[0].abs() + axis_b[0].abs()).max(settings.min_radius_px);
    let ext_y = (axis_a[1].abs() + axis_b[1].abs()).max(settings.min_radius_px);
    let x0 = (centre[0] - ext_x).floor().max(0.0) as usize;
    let x1 = ((centre[0] + ext_x).ceil() as isize).clamp(0, settings.width as isize) as usize;
    let y0 = (centre[1] - ext_y).floor().max(0.0) as usize;
    let y1 = ((centre[1] + ext_y).ceil() as isize).clamp(0, settings.height as isize) as usize;
    if x0 >= x1 || y0 >= y1 {
        return false;
    }

    // Invert [a b] so a pixel offset can be expressed in disc coordinates.
    // A degenerate matrix means the disc is exactly edge-on and covers no
    // area; the min-radius path below still marks its centre pixel.
    let det = axis_a[0] * axis_b[1] - axis_a[1] * axis_b[0];
    let invertible = det.abs() > 1e-6;
    let color = shade(surfel, settings);

    let mut touched = false;
    for py in y0..y1 {
        for px in x0..x1 {
            let d = [px as f32 + 0.5 - centre[0], py as f32 + 0.5 - centre[1]];
            let inside = if invertible {
                let u = (d[0] * axis_b[1] - d[1] * axis_b[0]) / det;
                let v = (axis_a[0] * d[1] - axis_a[1] * d[0]) / det;
                u * u + v * v <= 1.0
            } else {
                d[0].abs() <= settings.min_radius_px && d[1].abs() <= settings.min_radius_px
            };
            if !inside {
                continue;
            }
            let i = py * settings.width + px;
            stats.fragments += 1;
            touched = true;

            if view_depth < depth[i] - settings.blend_window {
                // A genuinely nearer surface: the window moves to it and
                // everything accumulated behind is discarded.
                depth[i] = view_depth;
                accum[i] = [color[0], color[1], color[2]];
                weight[i] = 1.0;
            } else if view_depth <= depth[i] + settings.blend_window {
                // Coplanar with what is already here: average, and keep the
                // nearest depth so the window tracks the front surface.
                depth[i] = depth[i].min(view_depth);
                for c in 0..3 {
                    accum[i][c] += color[c];
                }
                weight[i] += 1.0;
            }
        }
    }
    touched
}

/// Albedo, baked light, occlusion and emission. Deliberately the simplest
/// shading that can be compared against the other renderers: any difference
/// in the resulting image is then a difference in the *representation*, not
/// in the lighting model.
fn shade(surfel: &PackedSurfel, settings: &SurfelRenderSettings) -> [f32; 3] {
    let packed = material_visual(surfel.material).color;
    let albedo = [
        ((packed >> 16) & 0xFF) as f32 / 255.0,
        ((packed >> 8) & 0xFF) as f32 / 255.0,
        (packed & 0xFF) as f32 / 255.0,
    ];
    if surfel.flags & SURFEL_FLAG_EMISSIVE != 0 {
        return albedo;
    }
    let light = surfel.baked_light as f32 / 15.0;
    let occlusion = if surfel.ao != 0 { 0.55 } else { 1.0 };
    let intensity = settings.ambient + light * occlusion;
    [
        albedo[0] * intensity,
        albedo[1] * intensity,
        albedo[2] * intensity,
    ]
}

fn fog_blend(color: [f32; 3], depth: f32, fog: &SurfelFog) -> [f32; 3] {
    let transmittance = (-fog.density * (depth - fog.start).max(0.0)).exp();
    [
        color[0] * transmittance + fog.color[0] * (1.0 - transmittance),
        color[1] * transmittance + fog.color[1] * (1.0 - transmittance),
        color[2] * transmittance + fog.color[2] * (1.0 - transmittance),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::surfel_cloud::{build_surfel_cloud, radius_for_spacing};
    use vackrooms::adapters::voxel_mapper::{FaceDirection, MergedQuad};

    fn camera() -> SurfelCamera {
        SurfelCamera {
            position: [10.0, 2.0, 0.0],
            yaw: 0.0,
            pitch: 0.0,
            fov_tan: (70f32.to_radians() * 0.5).tan(),
        }
    }

    /// A wall filling the view must fill the image. This is the test that
    /// catches a radius too small for its spacing: the picture still looks
    /// like a wall, but a percentage of it is holes.
    #[test]
    fn a_wall_in_front_of_the_camera_leaves_no_holes() {
        let quad = MergedQuad {
            x: 0.0,
            y: 0.0,
            z: 0.0,
            w: 40.0,
            h: 40.0,
            dir: FaceDirection::South,
            material: 1,
            color: 0x00DD_CC66,
            light: 15,
            ao: 0,
        };
        let cloud = build_surfel_cloud(&[quad], 0.2, 0.25);
        let settings = SurfelRenderSettings::new(160, 120);
        // Camera inside the wall's span, looking at it down -Z.
        let cam = SurfelCamera {
            position: [20.0, 20.0, 8.0],
            // yaw 0 looks toward -Z; these quads face +Z, so the camera
            // stands on their +Z side and looks back at them.
            yaw: 0.0,
            ..camera()
        };
        let image = render_surfels(&[(&cloud.surfels, [0.0, 0.0, 0.0])], &cam, &settings);
        assert!(
            image.coverage > 0.99,
            "the wall has holes in it: {:.1}% covered",
            image.coverage * 100.0
        );
    }

    /// The failure the ellipse exists to prevent. Splatting circles makes
    /// a surface seen edge-on cover far more pixels than it should, so a
    /// glancing wall bleeds past its own end.
    #[test]
    fn a_glancing_surface_does_not_bleed_past_its_silhouette() {
        let quad = MergedQuad {
            x: 0.0,
            y: 0.0,
            z: 0.0,
            w: 20.0,
            h: 4.0,
            dir: FaceDirection::South,
            material: 1,
            color: 0x00DD_CC66,
            light: 15,
            ao: 0,
        };
        let cloud = build_surfel_cloud(&[quad], 0.2, 0.5);
        let settings = SurfelRenderSettings::new(200, 120);
        // Looking almost along the wall's plane.
        let cam = SurfelCamera {
            position: [-2.0, 2.0, 1.0],
            yaw: -std::f32::consts::FRAC_PI_2,
            pitch: 0.0,
            fov_tan: (70f32.to_radians() * 0.5).tan(),
        };
        let image = render_surfels(&[(&cloud.surfels, [0.0, 0.0, 0.0])], &cam, &settings);
        // Edge-on, the wall is a thin band. Circular splats would smear it
        // across a large share of the frame.
        assert!(
            image.coverage < 0.5,
            "a wall seen edge-on covered {:.0}% of the frame",
            image.coverage * 100.0
        );
    }

    /// Coplanar neighbours must not arbitrate per pixel. Without the depth
    /// window a flat wall of one material comes out mottled.
    #[test]
    fn coplanar_neighbours_blend_instead_of_fighting() {
        let quad = MergedQuad {
            x: 0.0,
            y: 0.0,
            z: 0.0,
            w: 30.0,
            h: 30.0,
            dir: FaceDirection::South,
            material: 1,
            color: 0x00DD_CC66,
            light: 12,
            ao: 0,
        };
        let cloud = build_surfel_cloud(&[quad], 0.2, 0.3);
        let settings = SurfelRenderSettings::new(120, 90);
        let cam = SurfelCamera {
            position: [15.0, 15.0, 6.0],
            // yaw 0 looks toward -Z; these quads face +Z, so the camera
            // stands on their +Z side and looks back at them.
            yaw: 0.0,
            ..camera()
        };
        let image = render_surfels(&[(&cloud.surfels, [0.0, 0.0, 0.0])], &cam, &settings);
        // One material, one light level, one plane: every covered pixel
        // must come out the same colour.
        let mut seen = std::collections::HashSet::new();
        for px in image.rgb.chunks_exact(3) {
            seen.insert([px[0], px[1], px[2]]);
        }
        assert!(
            seen.len() <= 3,
            "a flat wall came out in {} colours",
            seen.len()
        );
    }

    /// A surfel behind an occluder must not show through it.
    #[test]
    fn a_nearer_surface_replaces_what_is_behind_it() {
        let near = MergedQuad {
            x: 0.0,
            y: 0.0,
            z: 4.0,
            w: 40.0,
            h: 40.0,
            dir: FaceDirection::South,
            material: 1,
            color: 0,
            light: 15,
            ao: 0,
        };
        let mut far = near.clone();
        far.z = 0.0;
        far.material = 5; // red wall: unmistakable if it leaks through
        let cloud = build_surfel_cloud(&[near, far], 0.2, 0.25);
        let settings = SurfelRenderSettings::new(120, 90);
        let cam = SurfelCamera {
            position: [20.0, 20.0, 12.0],
            // yaw 0 looks toward -Z; these quads face +Z, so the camera
            // stands on their +Z side and looks back at them.
            yaw: 0.0,
            ..camera()
        };
        let image = render_surfels(&[(&cloud.surfels, [0.0, 0.0, 0.0])], &cam, &settings);
        let reddish = image
            .rgb
            .chunks_exact(3)
            // Widened deliberately: `px[1] + 40` in u8 wraps for any bright
            // channel, and a wrapped comparison quietly calls yellow red.
            .filter(|px| {
                let (r, g, b) = (px[0] as i16, px[1] as i16, px[2] as i16);
                r > g + 40 && r > b + 40
            })
            .count();
        assert_eq!(reddish, 0, "{reddish} pixels of the hidden wall showed");
    }

    /// Back faces are the inside of the world's walls. Drawing them costs
    /// fill and can only ever be wrong.
    #[test]
    fn surfels_facing_away_are_not_drawn() {
        let quad = MergedQuad {
            x: 0.0,
            y: 0.0,
            z: 0.0,
            w: 10.0,
            h: 10.0,
            dir: FaceDirection::North, // faces -Z
            material: 1,
            color: 0,
            light: 15,
            ao: 0,
        };
        let cloud = build_surfel_cloud(&[quad], 0.2, 0.5);
        let settings = SurfelRenderSettings::new(80, 60);
        // Standing on the +Z side, looking back at the quad's back face.
        let cam = SurfelCamera {
            position: [5.0, 5.0, 8.0],
            // yaw 0 looks toward -Z; these quads face +Z, so the camera
            // stands on their +Z side and looks back at them.
            yaw: 0.0,
            ..camera()
        };
        let image = render_surfels(&[(&cloud.surfels, [0.0, 0.0, 0.0])], &cam, &settings);
        assert_eq!(image.stats.drawn, 0);
        assert_eq!(image.stats.back_facing, cloud.surfels.len());
    }

    /// The chunk offset has to be applied, or every chunk stacks at the
    /// world origin — which still renders, and still looks like geometry.
    #[test]
    fn a_cloud_is_drawn_at_its_chunk_origin() {
        let quad = MergedQuad {
            x: 0.0,
            y: 0.0,
            z: 0.0,
            w: 8.0,
            h: 8.0,
            dir: FaceDirection::South,
            material: 1,
            color: 0,
            light: 15,
            ao: 0,
        };
        let cloud = build_surfel_cloud(&[quad], 0.2, 0.5);
        let settings = SurfelRenderSettings::new(80, 60);
        let cam = SurfelCamera {
            position: [4.0, 4.0, 6.0],
            // yaw 0 looks toward -Z; these quads face +Z, so the camera
            // stands on their +Z side and looks back at them.
            yaw: 0.0,
            ..camera()
        };
        let here = render_surfels(&[(&cloud.surfels, [0.0, 0.0, 0.0])], &cam, &settings);
        let far_away = render_surfels(&[(&cloud.surfels, [500.0, 0.0, 0.0])], &cam, &settings);
        assert!(here.coverage > 0.5);
        assert_eq!(
            far_away.stats.drawn, 0,
            "a cloud offset 500 u away was still drawn in front of the camera"
        );
    }

    /// Radius and spacing are one decision. If they ever drift apart the
    /// hole test above is the only thing that notices, so pin the relation.
    #[test]
    fn radius_tracks_spacing() {
        for spacing in [0.1f32, 0.25, 1.0, 4.0] {
            let cloud = build_surfel_cloud(
                &[MergedQuad {
                    x: 0.0,
                    y: 0.0,
                    z: 0.0,
                    w: 8.0,
                    h: 8.0,
                    dir: FaceDirection::South,
                    material: 1,
                    color: 0,
                    light: 8,
                    ao: 0,
                }],
                0.2,
                spacing,
            );
            let want = radius_for_spacing(spacing.max(0.02));
            let got = cloud.surfels[0].world_radius();
            assert!(
                (got - want).abs() <= 0.05,
                "spacing {spacing} gave radius {got}, wanted about {want}"
            );
        }
    }
}
