use crate::application::ports::FrameParams;
use crate::application::rendering::camera_basis;

/// Conservative chunk/cell sphere test shared by raster strategies.
pub(super) fn sphere_is_visible(
    center: [f32; 3],
    radius: f32,
    frame: &FrameParams,
    max_distance: f32,
) -> bool {
    let delta = [
        center[0] - frame.camera_pos[0],
        center[1] - frame.camera_pos[1],
        center[2] - frame.camera_pos[2],
    ];
    let distance_squared = delta[0] * delta[0] + delta[1] * delta[1] + delta[2] * delta[2];
    let reach = max_distance + radius.max(0.0);
    if distance_squared > reach * reach {
        return false;
    }
    let forward = camera_basis(frame.yaw, frame.pitch).forward;
    let forward_distance = delta[0] * forward[0] + delta[1] * forward[1] + delta[2] * forward[2];
    forward_distance + radius >= -0.03
}

pub(super) fn bounds_sphere(origin: [f32; 3], bounds_max: [f32; 3]) -> ([f32; 3], f32) {
    let half = [
        bounds_max[0] * 0.5,
        bounds_max[1] * 0.5,
        bounds_max[2] * 0.5,
    ];
    (
        [
            origin[0] + half[0],
            origin[1] + half[1],
            origin[2] + half[2],
        ],
        (half[0] * half[0] + half[1] * half[1] + half[2] * half[2]).sqrt(),
    )
}

/// Signed world-space distance from a sphere center to one side plane of the
/// perspective frustum, positive meaning inside. `axis` is `right` or `up`;
/// `tan_half` is that axis's half-angle tangent (horizontal uses
/// `fov_tan * aspect`, vertical uses `fov_tan` alone); `sign` selects which
/// of the paired planes (+1 for the right/top plane, -1 for left/bottom).
///
/// A point at forward-distance `along` and lateral offset `across` lies
/// exactly on the `+axis` boundary plane when `across == tan_half * along`.
/// The perpendicular distance from an arbitrary point to that plane is
/// `(tan_half * along - across) * cos(theta_half)`, and
/// `cos(theta_half) == 1 / sqrt(1 + tan_half^2)` exactly — no small-angle
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
/// job of [`sphere_is_visible`], which this is meant to be combined with.
pub(super) fn frustum_side_planes_visible(
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
    let basis = camera_basis(frame.yaw, frame.pitch);
    let tan_h = fov_tan * aspect.max(1e-6);
    let tan_v = fov_tan;

    [
        side_plane_distance(delta, basis.forward, basis.right, tan_h, 1.0),
        side_plane_distance(delta, basis.forward, basis.right, tan_h, -1.0),
        side_plane_distance(delta, basis.forward, basis.up, tan_v, 1.0),
        side_plane_distance(delta, basis.forward, basis.up, tan_v, -1.0),
    ]
    .iter()
    .all(|&d| d >= -radius)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conservative_test_keeps_intersecting_and_rejects_far_behind() {
        let frame = FrameParams::default();
        assert!(sphere_is_visible([0.0, 0.0, -5.0], 1.0, &frame, 10.0));
        assert!(!sphere_is_visible([0.0, 0.0, 5.0], 1.0, &frame, 10.0));
        assert!(!sphere_is_visible([0.0, 0.0, -30.0], 1.0, &frame, 10.0));
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
    fn arbitrary_yaw_and_pitch_still_treats_straight_ahead_as_visible() {
        let mut frame = FrameParams::default();
        frame.yaw = 1.234;
        frame.pitch = 0.3;
        let forward = camera_basis(frame.yaw, frame.pitch).forward;
        let target = [forward[0] * 10.0, forward[1] * 10.0, forward[2] * 10.0];
        assert!(frustum_side_planes_visible(target, 0.1, &frame, 1.0, 1.0));
    }

    #[test]
    fn large_sphere_straddling_the_boundary_stays_conservatively_visible() {
        let frame = canonical_frame();
        let tan_h = 1.0;
        let along = 10.0;
        let center = [tan_h * along * 1.3, 0.0, -along];
        assert!(frustum_side_planes_visible(center, 6.0, &frame, 1.0, 1.0));
    }
}
