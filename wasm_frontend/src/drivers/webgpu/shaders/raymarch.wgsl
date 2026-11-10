// SVO raymarch strategy. Nodes remain in the canonical four-u32 encoding;
// point lookup is stateless, and optional empty-leaf stepping changes only
// how far an empty sample advances.

struct RayChunk {
    origin_world_size: vec4<f32>,
    voxel_depth: vec4<f32>,
    // x: root node index in the merged atlas; y: stable source ordinal.
    indices: vec4<u32>,
};

struct RayScene {
    chunk_count: u32,
    policy_flags: u32,
    _reserved: vec2<u32>,
};

const RAY_POLICY_EMPTY_SPACE_SKIP: u32 = 1u;
const RAY_POLICY_FRONT_TO_BACK: u32 = 2u;
const RAY_POLICY_DIRECT_LIGHT_VISIBILITY: u32 = 4u;
const TRACE_INFINITY: f32 = 1e30;
const TRACE_MIN_TIE_EPSILON: f32 = 1e-7;
const DIRECT_VISIBILITY_TRACE_BUDGET: u32 = 4096u;

@group(1) @binding(0) var<storage, read> atlas_words: array<u32>;
@group(1) @binding(1) var<storage, read> ray_chunks: array<RayChunk>;
@group(1) @binding(2) var<uniform> ray_scene: RayScene;

struct FullscreenVertex {
    @builtin(position) clip_position: vec4<f32>,
};

struct BoxHit {
    hit: bool,
    entry: f32,
    exit: f32,
    normal: vec3<f32>,
};


struct VoxelHit {
    hit: bool,
    distance: f32,
    normal: vec3<f32>,
    material: u32,
    color: u32,
    light_word: u32,
    voxel_size: f32,
};

struct VisibilityTrace {
    blocked: bool,
    steps: u32,
};

@vertex
fn raymarch_vertex(@builtin(vertex_index) vertex_index: u32) -> FullscreenVertex {
    var output: FullscreenVertex;
    let x = select(-1.0, 3.0, vertex_index == 1u);
    let y = select(-1.0, 3.0, vertex_index == 2u);
    output.clip_position = vec4<f32>(x, y, 0.0, 1.0);
    return output;
}

fn safe_inverse(direction: f32) -> f32 {
    if abs(direction) < 1e-20 {
        return select(TRACE_INFINITY, -TRACE_INFINITY, direction < 0.0);
    }
    return 1.0 / direction;
}

fn distance_tie_tolerance(distance: f32) -> f32 {
    return max(abs(distance) * 1e-6, TRACE_MIN_TIE_EPSILON);
}

fn traversal_sample_bias(voxel_size: f32) -> f32 {
    return max(voxel_size * 1e-5, TRACE_MIN_TIE_EPSILON);
}

fn ray_policy_enabled(flag: u32) -> bool {
    return (ray_scene.policy_flags & flag) != 0u;
}

fn box_intersection(
    ray_origin: vec3<f32>,
    ray_direction: vec3<f32>,
    bounds_min: vec3<f32>,
    bounds_max: vec3<f32>,
) -> BoxHit {
    if (abs(ray_direction.x) < 1e-20 && (ray_origin.x < bounds_min.x || ray_origin.x > bounds_max.x))
        || (abs(ray_direction.y) < 1e-20 && (ray_origin.y < bounds_min.y || ray_origin.y > bounds_max.y))
        || (abs(ray_direction.z) < 1e-20 && (ray_origin.z < bounds_min.z || ray_origin.z > bounds_max.z)) {
        return BoxHit(false, 0.0, 0.0, vec3<f32>(0.0));
    }
    let inverse = vec3<f32>(
        safe_inverse(ray_direction.x),
        safe_inverse(ray_direction.y),
        safe_inverse(ray_direction.z)
    );
    var near_times = min((bounds_min - ray_origin) * inverse, (bounds_max - ray_origin) * inverse);
    var far_times = max((bounds_min - ray_origin) * inverse, (bounds_max - ray_origin) * inverse);
    if abs(ray_direction.x) < 1e-20 { near_times.x = -1e30; far_times.x = 1e30; }
    if abs(ray_direction.y) < 1e-20 { near_times.y = -1e30; far_times.y = 1e30; }
    if abs(ray_direction.z) < 1e-20 { near_times.z = -1e30; far_times.z = 1e30; }
    let entry = max(near_times.x, max(near_times.y, near_times.z));
    let exit = min(far_times.x, min(far_times.y, far_times.z));
    if entry > exit || exit < 0.0 {
        return BoxHit(false, entry, exit, vec3<f32>(0.0));
    }
    var normal = vec3<f32>(0.0);
    if entry <= 0.0 {
        let absolute = abs(ray_direction);
        if absolute.x >= absolute.y && absolute.x >= absolute.z {
            normal = vec3<f32>(-sign(ray_direction.x), 0.0, 0.0);
        } else if absolute.y >= absolute.z {
            normal = vec3<f32>(0.0, -sign(ray_direction.y), 0.0);
        } else {
            normal = vec3<f32>(0.0, 0.0, -sign(ray_direction.z));
        }
    } else if near_times.x >= near_times.y && near_times.x >= near_times.z {
        normal = vec3<f32>(select(1.0, -1.0, ray_direction.x > 0.0), 0.0, 0.0);
    } else if near_times.y >= near_times.z {
        normal = vec3<f32>(0.0, select(1.0, -1.0, ray_direction.y > 0.0), 0.0);
    } else {
        normal = vec3<f32>(0.0, 0.0, select(1.0, -1.0, ray_direction.z > 0.0));
    }
    return BoxHit(true, max(entry, 0.0), exit, normal);
}


fn next_leaf_exit(
    ray_origin: vec3<f32>,
    ray_direction: vec3<f32>,
    bounds_min: vec3<f32>,
    bounds_max: vec3<f32>,
) -> vec4<f32> {
    var times = vec3<f32>(TRACE_INFINITY);
    if abs(ray_direction.x) >= 1e-20 {
        times.x = (select(bounds_min.x, bounds_max.x, ray_direction.x > 0.0) - ray_origin.x) / ray_direction.x;
    }
    if abs(ray_direction.y) >= 1e-20 {
        times.y = (select(bounds_min.y, bounds_max.y, ray_direction.y > 0.0) - ray_origin.y) / ray_direction.y;
    }
    if abs(ray_direction.z) >= 1e-20 {
        times.z = (select(bounds_min.z, bounds_max.z, ray_direction.z > 0.0) - ray_origin.z) / ray_direction.z;
    }
    let nearest = min(times.x, min(times.y, times.z));
    let tolerance = distance_tie_tolerance(nearest);
    // Match finest-DDA X/Y/Z precedence for mathematically tied exits. The
    // traversal bias is intentionally not reused as an equality tolerance.
    if abs(times.x - nearest) <= tolerance {
        return vec4<f32>(nearest, -sign(ray_direction.x), 0.0, 0.0);
    }
    if abs(times.y - nearest) <= tolerance {
        return vec4<f32>(nearest, 0.0, -sign(ray_direction.y), 0.0);
    }
    return vec4<f32>(nearest, 0.0, 0.0, -sign(ray_direction.z));
}



struct SvoStackFrame {
    node_index: u32,
    bounds_min: vec3<f32>,
    bounds_max: vec3<f32>,
    level: u32,
};

struct SvoWalkResult {
    found: bool,
    distance: f32,
    normal: vec3<f32>,
    material: u32,
    color: u32,
    light_word: u32,
    steps: u32,
};

// One stack walk over a chunk's SVO, shared by primary rays and shadow
// rays. The descent path lives on the stack, so the walk never re-descends
// from the root: advancing past an empty leaf costs one pop and one
// re-descent from the parent, not a fresh walk from the root. A solid leaf
// is resolved by its own box intersection, which gives the same exact
// entry distance and entry-face normal the finest-voxel DDA used to
// reconstruct — the DDA's refinement re-walk disappears with it.
//
// `step_budget` bounds the walk: exhaustion reports "not found" with
// steps >= budget, and the caller decides what that means (a primary ray
// misses; a shadow ray is conservatively opaque).
fn svo_stack_walk(
    ray_origin: vec3<f32>,
    ray_direction: vec3<f32>,
    chunk: RayChunk,
    interval: BoxHit,
    step_budget: u32,
) -> SvoWalkResult {
    let voxel_size = chunk.voxel_depth.x;
    let depth = u32(chunk.voxel_depth.y);
    let bias = traversal_sample_bias(voxel_size);

    // Sample slightly past the interval entry, mirroring the DDA's
    // distance + bias sample: a ray starting exactly on a voxel boundary
    // resolves to the voxel ahead, never the one behind.
    var t = interval.entry;
    var p = ray_origin + ray_direction * min(t + bias, interval.exit);
    var node_index = chunk.indices.x;
    var bounds_min = vec3<f32>(0.0);
    var bounds_max = vec3<f32>(chunk.origin_world_size.w);
    var level = 0u;
    var stack: array<SvoStackFrame, 12>;
    var stack_ptr = 0u;
    var steps = 0u;

    for (var iter = 0u; iter < 4096u; iter += 1u) {
        if iter >= step_budget || t > interval.exit {
            break;
        }
        steps += 1u;
        let word = node_index * 4u;
        let node_type = atlas_words[word];
        let payload = atlas_words[word + 1u];
        let color_or_mask = atlas_words[word + 2u];
        let light_word = atlas_words[word + 3u];
        // The box the ray must advance past when this node proves empty.
        // Defaults to the current node's bounds (the empty-leaf case);
        // the empty-child case overrides it below.
        var exit_min = bounds_min;
        var exit_max = bounds_max;
        if node_type == 1u || level >= depth {
            if payload != 0u {
                let entry = box_intersection(
                    ray_origin,
                    ray_direction,
                    bounds_min,
                    bounds_max
                );
                return SvoWalkResult(
                    true,
                    max(entry.entry, interval.entry),
                    entry.normal,
                    payload,
                    color_or_mask,
                    light_word,
                    steps
                );
            }
        } else {
            let center = (bounds_min + bounds_max) * 0.5;
            let child_x = select(0u, 1u, p.x >= center.x);
            let child_y = select(0u, 1u, p.y >= center.y);
            let child_z = select(0u, 1u, p.z >= center.z);
            let child = (child_z << 2u) | (child_y << 1u) | child_x;
            let child_min = vec3<f32>(
                select(bounds_min.x, center.x, child_x == 1u),
                select(bounds_min.y, center.y, child_y == 1u),
                select(bounds_min.z, center.z, child_z == 1u)
            );
            let child_max = vec3<f32>(
                select(center.x, bounds_max.x, child_x == 1u),
                select(center.y, bounds_max.y, child_y == 1u),
                select(center.z, bounds_max.z, child_z == 1u)
            );
            if (color_or_mask & (1u << child)) != 0u {
                if stack_ptr < 12u {
                    stack[stack_ptr] = SvoStackFrame(
                        node_index,
                        bounds_min,
                        bounds_max,
                        level
                    );
                    stack_ptr += 1u;
                }
                node_index = payload + child;
                bounds_min = child_min;
                bounds_max = child_max;
                level += 1u;
                continue;
            }
            exit_min = child_min;
            exit_max = child_max;
        }
        // Advance past the empty box, then climb to the smallest ancestor
        // whose bounds still contain the advanced point. If there is none
        // the ray has left the chunk.
        let exit = next_leaf_exit(ray_origin, ray_direction, exit_min, exit_max);
        var next_t = exit.x;
        if next_t <= t + bias * 0.25 {
            next_t = t + bias;
        }
        t = next_t;
        // Nudge past the exit plane along the ray so the point is strictly
        // beyond the boundary, never exactly on it.
        p = ray_origin + ray_direction * t - exit.yzw * bias;
        var inside_ancestor = false;
        for (var climb = 0u; climb < 12u; climb += 1u) {
            if p.x >= bounds_min.x && p.x <= bounds_max.x
                && p.y >= bounds_min.y && p.y <= bounds_max.y
                && p.z >= bounds_min.z && p.z <= bounds_max.z {
                inside_ancestor = true;
                break;
            }
            if stack_ptr == 0u {
                break;
            }
            stack_ptr -= 1u;
            node_index = stack[stack_ptr].node_index;
            bounds_min = stack[stack_ptr].bounds_min;
            bounds_max = stack[stack_ptr].bounds_max;
            level = stack[stack_ptr].level;
        }
        if !inside_ancestor {
            break;
        }
    }
    return SvoWalkResult(false, 0.0, vec3<f32>(0.0), 0u, 0u, 0u, steps);
}

fn trace_chunk(
    ray_origin: vec3<f32>,
    ray_direction: vec3<f32>,
    chunk: RayChunk,
    interval: BoxHit,
) -> VoxelHit {
    let budget = clamp(frame.render_options.w, 1u, 4096u);
    let walk = svo_stack_walk(ray_origin, ray_direction, chunk, interval, budget);
    if walk.found {
        return VoxelHit(
            true,
            walk.distance,
            walk.normal,
            walk.material,
            walk.color,
            walk.light_word,
            chunk.voxel_depth.x
        );
    }
    return VoxelHit(false, 0.0, vec3<f32>(0.0), 0u, 0u, 0u, 0.0);
}

fn direct_visibility_bias(voxel_size: f32) -> f32 {
    return max(voxel_size * 1e-3, 1e-5);
}

// Secondary visibility has its own hard budget. It deliberately does not
// consult the primary-ray trace budget or empty-space policy toggle.
fn trace_visibility_chunk_stack(
    ray_origin: vec3<f32>,
    ray_direction: vec3<f32>,
    chunk: RayChunk,
    interval: BoxHit,
    step_budget: u32,
) -> VisibilityTrace {
    let walk = svo_stack_walk(ray_origin, ray_direction, chunk, interval, step_budget);
    if walk.found {
        return VisibilityTrace(true, walk.steps);
    }
    // Exhaustion is conservatively opaque, avoiding a light leak.
    return VisibilityTrace(walk.steps >= step_budget, walk.steps);
}


fn finite_segment_visible(
    receiver_world: vec3<f32>,
    receiver_normal: vec3<f32>,
    receiver_voxel_size: f32,
    endpoint_world: vec3<f32>,
) -> bool {
    let endpoint_bias = direct_visibility_bias(receiver_voxel_size);
    let biased_origin = receiver_world + receiver_normal * endpoint_bias;
    let segment = endpoint_world - biased_origin;
    let endpoint_distance = length(segment);
    if endpoint_distance <= endpoint_bias * 2.0 {
        return true;
    }
    let ray_direction = segment / endpoint_distance;
    // Do not classify the light fixture's own endpoint as an occluder.
    let maximum_distance = endpoint_distance - endpoint_bias;
    var remaining_steps = DIRECT_VISIBILITY_TRACE_BUDGET;
    let chunk_count = min(ray_scene.chunk_count, 25u);

    for (var index = 0u; index < chunk_count; index += 1u) {
        if remaining_steps == 0u {
            return false;
        }
        let chunk = ray_chunks[index];
        let local_origin = biased_origin - chunk.origin_world_size.xyz;
        let slab = box_intersection(
            local_origin,
            ray_direction,
            vec3<f32>(0.0),
            vec3<f32>(chunk.origin_world_size.w)
        );
        let finite_exit = min(slab.exit, maximum_distance);
        if !slab.hit || finite_exit <= slab.entry {
            continue;
        }
        let interval = BoxHit(true, slab.entry, finite_exit, slab.normal);
        let trace = trace_visibility_chunk_stack(
            local_origin,
            ray_direction,
            chunk,
            interval,
            remaining_steps
        );
        remaining_steps -= min(trace.steps, remaining_steps);
        if trace.blocked {
            return false;
        }
    }
    return true;
}

fn has_positive_rgb(value: vec3<f32>) -> bool {
    return max(value.x, max(value.y, value.z)) > 0.0;
}

fn rectangle_visibility_sample_count() -> u32 {
    return select(1u, 4u, ray_scene._reserved.x == 4u);
}

fn evaluate_visible_scene_light(
    world: vec3<f32>,
    normal: vec3<f32>,
    receiver_voxel_size: f32,
    light: GpuLight,
) -> vec3<f32> {
    let kind = u32(max(light.half_size_kind.z, 0.0) + 0.5);
    if kind == LIGHT_POINT {
        let endpoint = light.position_radius.xyz;
        let unoccluded = point_light_sample_irradiance(world, normal, light, endpoint);
        if !has_positive_rgb(unoccluded) {
            return vec3<f32>(0.0);
        }
        if finite_segment_visible(world, normal, receiver_voxel_size, endpoint) {
            return unoccluded;
        }
        return vec3<f32>(0.0);
    }

    if rectangle_visibility_sample_count() == 4u {
        var visible_integral = 0.0;
        for (var sample_index = 0u; sample_index < 4u; sample_index += 1u) {
            let endpoint = rectangle_gauss_sample_position(light, sample_index);
            let sample_weight = rectangle_light_sample_weight(
                world,
                normal,
                light,
                kind,
                endpoint
            );
            let sample_irradiance = rectangle_light_irradiance_from_integral(
                light,
                sample_weight
            );
            if !has_positive_rgb(sample_irradiance) {
                continue;
            }
            if finite_segment_visible(world, normal, receiver_voxel_size, endpoint) {
                visible_integral += sample_weight;
            }
        }
        return rectangle_light_irradiance_from_integral(light, visible_integral);
    }

    // Low quality retains the existing four-point unoccluded quadrature and
    // gates its integral with one finite segment to the rectangle center.
    let unoccluded = evaluate_rectangle_light(world, normal, light, kind);
    if !has_positive_rgb(unoccluded) {
        return vec3<f32>(0.0);
    }
    if finite_segment_visible(
        world,
        normal,
        receiver_voxel_size,
        light.position_radius.xyz
    ) {
        return unoccluded;
    }
    return vec3<f32>(0.0);
}

fn raymarch_analytic_direct(
    world: vec3<f32>,
    normal: vec3<f32>,
    receiver_voxel_size: f32,
) -> vec3<f32> {
    var analytic_direct = vec3<f32>(0.0);
    for (var index = 0u; index < frame.render_options.x; index += 1u) {
        let light = lights[index];
        if light.half_size_kind.w < 0.5 {
            continue;
        }
        analytic_direct += evaluate_visible_scene_light(
            world,
            normal,
            receiver_voxel_size,
            light
        );
    }
    return analytic_direct;
}

fn camera_ray(pixel: vec2<f32>) -> vec3<f32> {
    let viewport = max(frame.viewport_fov_start.xy, vec2<f32>(1.0));
    let normalized = pixel / viewport;
    let aspect = viewport.x / viewport.y;
    let screen = vec2<f32>(
        (normalized.x * 2.0 - 1.0) * aspect,
        1.0 - normalized.y * 2.0
    ) * frame.viewport_fov_start.z;
    return normalize(
        frame.camera_forward.xyz
        + frame.camera_right.xyz * screen.x
        + frame.camera_up.xyz * screen.y
    );
}

@fragment
fn raymarch_fragment(@builtin(position) pixel: vec4<f32>) -> @location(0) vec4<f32> {
    let direction = camera_ray(pixel.xy);
    var nearest = VoxelHit(false, 1e30, vec3<f32>(0.0), 0u, 0u, 0u, 0.0);
    var nearest_source_index = 0xffffffffu;
    let chunk_count = min(ray_scene.chunk_count, 25u);
    for (var index = 0u; index < chunk_count; index += 1u) {
        let chunk = ray_chunks[index];
        let local_origin = frame.camera_position.xyz - chunk.origin_world_size.xyz;
        let interval = box_intersection(
            local_origin,
            direction,
            vec3<f32>(0.0),
            vec3<f32>(chunk.origin_world_size.w)
        );
        if !interval.hit || interval.entry > nearest.distance { continue; }
        // Never search past the closest candidate already found: a hit in
        // this chunk beyond nearest.distance could not win the comparison
        // below regardless, so there is nothing to gain by tracing that far.
        // Mirrors finite_segment_visible's shadow-ray clip to a light's own
        // distance (line ~490) — same reasoning, applied to primary rays.
        let clipped_interval = BoxHit(
            interval.hit,
            interval.entry,
            min(interval.exit, nearest.distance),
            interval.normal
        );
        let hit = trace_chunk(local_origin, direction, chunk, clipped_interval);
        let stable_tie = hit.distance == nearest.distance
            && chunk.indices.y < nearest_source_index;
        if hit.hit && (hit.distance < nearest.distance || stable_tie) {
            nearest = hit;
            nearest_source_index = chunk.indices.y;
        }
    }

    if !nearest.hit {
        return vec4<f32>(display_color(frame.sky_color_ambient.xyz), 1.0);
    }
    let world = frame.camera_position.xyz + direction * nearest.distance;
    let albedo = apply_material_pattern(
        nearest.material,
        packed_srgb_to_linear(nearest.color),
        world,
        nearest.normal,
        nearest.voxel_size
    );
    let emission = authored_emission_strength(nearest.material);
    let emits = emission > 0.0 && nearest.normal.y < -0.5;
    var radiance = vec3<f32>(0.0);
    if emits {
        radiance = albedo * emission;
    } else if ray_policy_enabled(RAY_POLICY_DIRECT_LIGHT_VISIBILITY) {
        let analytic_direct = raymarch_analytic_direct(
            world,
            nearest.normal,
            nearest.voxel_size
        );
        radiance = surface_radiance_from_analytic_direct(
            world,
            nearest.normal,
            albedo,
            0.0,
            1.0,
            analytic_direct
        );
    } else {
        radiance = surface_radiance(
            world,
            nearest.normal,
            albedo,
            0.0,
            1.0,
            0u,
            frame.render_options.x
        );
    }
    return present_radiance(radiance, world, pixel.xy);
}
