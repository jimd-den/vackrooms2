//! Conservative visibility tests shared by the raster drivers.

use crate::application::ports::FrameParams;

/// Shared raster far plane and conservative culling boundary (world units).
/// Beer--Lambert fog never becomes exactly opaque, so projection and culling
/// must use the identical distance rather than hiding a ten-unit mismatch.
pub const MAX_DRAW_DISTANCE: f32 = 110.0;

/// Sphere visibility: inside the draw distance and not entirely behind the
/// camera plane. Returns the squared distance for near-to-far sorting, or
/// `None` when culled. Deliberately conservative (no side-plane frustum
/// test): a false "visible" costs a few draws, a false cull costs a hole
/// in the world.
pub fn sphere_visible(
    center: [f32; 3],
    radius: f32,
    frame: &FrameParams,
    max_draw_distance: f32,
) -> Option<f32> {
    let to = [
        center[0] - frame.camera_pos[0],
        center[1] - frame.camera_pos[1],
        center[2] - frame.camera_pos[2],
    ];
    let distance2 = to[0] * to[0] + to[1] * to[1] + to[2] * to[2];
    // Cull only when the *whole sphere* lies beyond the distance boundary.
    // Testing the center against `max_draw_distance` cuts chunks/cells that
    // still intersect the visible volume and creates a hard pop at the fog
    // edge.
    let farthest_visible_center = max_draw_distance + radius;
    if distance2 > farthest_visible_center * farthest_visible_center {
        return None;
    }
    let (sp, cp) = frame.pitch.sin_cos();
    let (sy, cy) = frame.yaw.sin_cos();
    let forward = [-cp * sy, sp, -cp * cy];
    let dot = to[0] * forward[0] + to[1] * forward[1] + to[2] * forward[2];
    if dot >= -radius {
        Some(distance2)
    } else {
        None
    }
}

/// Center + bounding-sphere radius of a chunk from its origin and local
/// bounds-max — the shape both mesh and splat chunk records share.
pub fn chunk_bounding_sphere(origin: [f32; 3], bounds_max: [f32; 3]) -> ([f32; 3], f32) {
    let center = [
        origin[0] + bounds_max[0] * 0.5,
        origin[1] + bounds_max[1] * 0.5,
        origin[2] + bounds_max[2] * 0.5,
    ];
    let radius = (bounds_max[0] * bounds_max[0]
        + bounds_max[1] * bounds_max[1]
        + bounds_max[2] * bounds_max[2])
        .sqrt()
        * 0.5;
    (center, radius)
}

/// Conservative AABB vs. Frustum test with safety inflation margin (0.5 voxel).
///
/// # Rationale
/// Prevents false culls on side view boundaries while rejecting chunks strictly outside
/// the extended camera view cone.
pub fn aabb_frustum_visible(
    origin: [f32; 3],
    bounds_max: [f32; 3],
    frame: &FrameParams,
    margin: f32,
) -> bool {
    let (center, radius) = chunk_bounding_sphere(origin, bounds_max);
    let inflated_radius = radius + margin;
    sphere_visible(center, inflated_radius, frame, MAX_DRAW_DISTANCE).is_some()
}

/// Signed world-space distance from a sphere center to one side plane of the
/// perspective frustum, positive meaning inside. `axis` is `right` or `up`;
/// `tan_half` is the tangent of that axis's half-angle (horizontal uses
/// `fov_tan * aspect`, vertical uses `fov_tan` alone); `sign` selects which
/// of the paired planes (+1 for the right/top plane, -1 for left/bottom).
///
/// Derivation: a point at forward-distance `along` and lateral offset
/// `across` (both relative to the camera, along `forward`/`axis`) lies
/// exactly on the `+axis` boundary plane when `across == tan_half * along`.
/// The perpendicular distance from an arbitrary point to that plane is
/// `(tan_half * along - across) * cos(theta_half)`, and
/// `cos(theta_half) == 1 / sqrt(1 + tan_half^2)` — this is exact, not an
/// approximation, so it carries no extra risk of an over-eager cull.
fn side_plane_distance(
    delta: [f32; 3],
    forward: [f32; 3],
    axis: [f32; 3],
    tan_half: f32,
    sign: f32,
) -> f32 {
    let dot = |a: [f32; 3], b: [f32; 3]| a[0] * b[0] + a[1] * b[1] + a[2] * b[2];
    let along = dot(delta, forward);
    let across = dot(delta, axis) * sign;
    (tan_half * along - across) / (1.0 + tan_half * tan_half).sqrt()
}

/// Conservative sphere-vs-frustum side-plane test: `false` only when the
/// sphere lies entirely outside at least one of the four side planes
/// (left/right/top/bottom). Deliberately excludes near/far — those stay the
/// job of [`sphere_visible`], which this is meant to be combined with.
pub fn frustum_side_planes_visible(
    center: [f32; 3],
    radius: f32,
    frame: &FrameParams,
    fov_tan: f32,
    aspect: f32,
) -> bool {
    let delta = [
        center[0] - frame.camera_pos[0],
        center[1] - frame.camera_pos[1],
        center[2] - frame.camera_pos[2],
    ];
    let (sp, cp) = frame.pitch.sin_cos();
    let (sy, cy) = frame.yaw.sin_cos();
    let right = [cy, 0.0, -sy];
    let up = [sp * sy, cp, sp * cy];
    let forward = [-cp * sy, sp, -cp * cy];
    let tan_h = fov_tan * aspect.max(1e-6);
    let tan_v = fov_tan;

    [
        side_plane_distance(delta, forward, right, tan_h, 1.0),
        side_plane_distance(delta, forward, right, tan_h, -1.0),
        side_plane_distance(delta, forward, up, tan_v, 1.0),
        side_plane_distance(delta, forward, up, tan_v, -1.0),
    ]
    .iter()
    .all(|&d| d >= -radius)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_chunk_bounding_sphere_computation() {
        let (center, radius) = chunk_bounding_sphere([0.0, 0.0, 0.0], [16.0, 8.0, 16.0]);
        assert_eq!(center, [8.0, 4.0, 8.0]);
        assert!(radius > 0.0);
    }

    fn canonical_frame() -> FrameParams {
        // yaw = 0, pitch = 0 -> forward = -Z, right = +X, up = +Y.
        FrameParams::default()
    }

    #[test]
    fn point_straight_ahead_is_inside_every_side_plane() {
        let frame = canonical_frame();
        assert!(frustum_side_planes_visible(
            [0.0, 0.0, -10.0],
            0.1,
            &frame,
            1.0,
            1.0
        ));
    }

    #[test]
    fn point_on_axis_far_to_the_right_is_culled() {
        let frame = canonical_frame();
        // fov_tan = aspect = 1.0 -> 45 degree half-angle each side. A point
        // 50 units right at only 10 units forward is far outside the cone.
        assert!(!frustum_side_planes_visible(
            [50.0, 0.0, -10.0],
            0.1,
            &frame,
            1.0,
            1.0
        ));
    }

    #[test]
    fn point_just_inside_the_right_edge_is_visible_and_just_outside_is_culled() {
        let frame = canonical_frame();
        let tan_h = 1.0; // 45 degrees
        let along = 10.0;
        // Exactly on the boundary plane: across == tan_h * along.
        let on_edge = [tan_h * along * 0.999, 0.0, -along];
        let past_edge = [tan_h * along * 1.2, 0.0, -along];
        assert!(frustum_side_planes_visible(on_edge, 0.01, &frame, 1.0, 1.0));
        assert!(!frustum_side_planes_visible(
            past_edge, 0.01, &frame, 1.0, 1.0
        ));
    }

    #[test]
    fn point_above_or_below_the_vertical_cone_is_culled() {
        let frame = canonical_frame();
        assert!(!frustum_side_planes_visible(
            [0.0, 50.0, -10.0],
            0.1,
            &frame,
            1.0,
            1.0
        ));
        assert!(!frustum_side_planes_visible(
            [0.0, -50.0, -10.0],
            0.1,
            &frame,
            1.0,
            1.0
        ));
    }

    #[test]
    fn wide_aspect_widens_only_the_horizontal_cone() {
        let frame = canonical_frame();
        // At aspect 2.0 the horizontal half-angle tangent doubles to 2.0, so
        // a point at across = 1.5 * along (outside the 45-degree vertical
        // cone's matching horizontal tan of 1.0) is now inside horizontally,
        // while the same offset applied vertically (still tan_v = 1.0)
        // stays culled.
        assert!(frustum_side_planes_visible(
            [15.0, 0.0, -10.0],
            0.1,
            &frame,
            1.0,
            2.0
        ));
        assert!(!frustum_side_planes_visible(
            [0.0, 15.0, -10.0],
            0.1,
            &frame,
            1.0,
            2.0
        ));
    }

    #[test]
    fn large_sphere_straddling_the_boundary_stays_conservatively_visible() {
        let frame = canonical_frame();
        // Center is just past the right edge, but a large radius still
        // overlaps the frustum: must not be culled.
        let tan_h = 1.0;
        let along = 10.0;
        let center = [tan_h * along * 1.3, 0.0, -along];
        assert!(frustum_side_planes_visible(center, 6.0, &frame, 1.0, 1.0));
    }
}
