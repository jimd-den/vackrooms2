//! Chunk streaming policy and the in-memory chunk store.
//!
//! The world is an infinite plane of square chunks. Each frame the policy
//! computes the set of chunk origins that should be resident around the
//! player; the engine loads missing ones (budgeted, to avoid frame hitches —
//! wasm has no threads, so time-slicing replaces the background meshing
//! thread a native engine would use) and evicts the rest.

use std::collections::HashMap;

use crate::application::collision::Aabb;
use crate::application::ports::ChunkPayload;
use vackrooms::domain::entities::anomaly::RealitySnapshot;
use vackrooms::domain::entities::anomaly::{LevelExit, PitHazard, TraversalGate};
use vackrooms::domain::entities::supplies::SupplyItem;

/// Integer key for a chunk, quantized from its world origin.
/// Origins are always integer multiples of `chunk_size`, so rounding to
/// millimetres gives a stable, hashable key.
pub type ChunkKey = (i64, i64);

pub fn chunk_key(origin_x: f32, origin_z: f32) -> ChunkKey {
    (
        (origin_x * 1000.0).round() as i64,
        (origin_z * 1000.0).round() as i64,
    )
}

/// Where the player is looking, for view-culled streaming.
#[derive(Debug, Clone, Copy)]
pub struct ViewCone {
    /// Normalized horizontal facing. The world is a plane, so a 2D cone
    /// bounds the 3D frustum for streaming purposes.
    pub forward: (f32, f32),
    /// Half-angle of the cone in radians, already widened by whatever
    /// margin the caller wants beyond the true horizontal field of view.
    pub half_angle: f32,
}

impl ViewCone {
    pub fn from_yaw(yaw: f32, half_angle: f32) -> Self {
        Self {
            forward: (yaw.sin(), -yaw.cos()),
            half_angle,
        }
    }
}

/// Decides which chunks should be resident for a given player position.
#[derive(Debug, Clone, Copy)]
pub struct StreamingPolicy {
    /// World-space side length of a chunk.
    pub chunk_size: f32,
    /// Chebyshev radius in chunks: 1 -> 3x3 resident set, 2 -> 5x5.
    pub radius: i32,
    /// Chunks within this Chebyshev radius stay resident no matter where
    /// the player looks, so turning can never expose an unloaded chunk.
    /// Only chunks *beyond* it are subject to view culling.
    pub core_radius: i32,
}

impl StreamingPolicy {
    /// Omnidirectional policy: every chunk in the square is desired.
    ///
    /// This is what collision and simulation must use. Physics residency
    /// may never depend on view direction, or walking backwards into an
    /// unloaded chunk would drop the player through the floor.
    pub fn omnidirectional(chunk_size: f32, radius: i32) -> Self {
        Self {
            chunk_size,
            radius,
            core_radius: radius,
        }
    }

    /// Returns the desired chunk origins, sorted nearest-first so the load
    /// budget is always spent on the most urgent chunk.
    pub fn desired_origins(&self, player_x: f32, player_z: f32) -> Vec<(f32, f32)> {
        self.desired_origins_in_view(player_x, player_z, None)
    }

    /// Returns the desired chunk origins, optionally culled to a view cone
    /// beyond `core_radius`.
    ///
    /// The test is conservative: a chunk counts as visible when its
    /// bounding circle touches the cone, not merely when its centre is
    /// inside. A chunk straddling the frustum edge still streams.
    pub fn desired_origins_in_view(
        &self,
        player_x: f32,
        player_z: f32,
        view: Option<ViewCone>,
    ) -> Vec<(f32, f32)> {
        let px = if player_x.is_finite() { player_x } else { 0.0 };
        let pz = if player_z.is_finite() { player_z } else { 0.0 };
        let base_x = (px / self.chunk_size).floor() * self.chunk_size;
        let base_z = (pz / self.chunk_size).floor() * self.chunk_size;
        // Half-diagonal of a chunk footprint: the bounding circle radius.
        let bound = self.chunk_size * std::f32::consts::SQRT_2 * 0.5;

        let mut origins = Vec::with_capacity(((2 * self.radius + 1).pow(2)) as usize);
        for dz in -self.radius..=self.radius {
            for dx in -self.radius..=self.radius {
                let origin_x = base_x + dx as f32 * self.chunk_size;
                let origin_z = base_z + dz as f32 * self.chunk_size;
                let core = dx.abs() <= self.core_radius && dz.abs() <= self.core_radius;
                if !core && !self.touches_cone(px, pz, origin_x, origin_z, bound, view) {
                    continue;
                }
                origins.push((origin_x, origin_z));
            }
        }
        origins.sort_by(|a, b| {
            let da = (a.0 + self.chunk_size * 0.5 - px).powi(2)
                + (a.1 + self.chunk_size * 0.5 - pz).powi(2);
            let db = (b.0 + self.chunk_size * 0.5 - px).powi(2)
                + (b.1 + self.chunk_size * 0.5 - pz).powi(2);
            da.total_cmp(&db)
        });
        origins
    }

    fn touches_cone(
        &self,
        px: f32,
        pz: f32,
        origin_x: f32,
        origin_z: f32,
        bound: f32,
        view: Option<ViewCone>,
    ) -> bool {
        let Some(view) = view else {
            return true;
        };
        let to_x = origin_x + self.chunk_size * 0.5 - px;
        let to_z = origin_z + self.chunk_size * 0.5 - pz;
        let distance = (to_x * to_x + to_z * to_z).sqrt();
        // Inside its own bounding circle the angle is meaningless.
        if distance <= bound || !distance.is_finite() {
            return true;
        }
        let cos = (to_x * view.forward.0 + to_z * view.forward.1) / distance;
        let angle = cos.clamp(-1.0, 1.0).acos();
        // Widen the cone by the chunk's angular radius so a partially
        // visible chunk is never dropped.
        angle <= view.half_angle + (bound / distance).clamp(-1.0, 1.0).asin()
    }
}

/// A resident chunk: payload plus its world origin and current level of
/// detail (0 = full resolution; higher = coarser, see `ChunkSourcePort`).
#[derive(Debug, Clone)]
pub struct LoadedChunk {
    pub origin: (f32, f32),
    pub lod: u8,
    /// Exact immutable encounter state from which every payload view
    /// (collision, SVO, mesh and lighting) was derived.
    pub reality: RealitySnapshot,
    /// Cached identity for diagnostics and fast rejection paths. Exact
    /// equality still uses `reality`, so a hash collision cannot mix worlds.
    pub reality_fingerprint: u64,
    pub payload: ChunkPayload,
}

impl LoadedChunk {
    pub fn new(
        origin: (f32, f32),
        lod: u8,
        reality: RealitySnapshot,
        payload: ChunkPayload,
    ) -> Self {
        let reality_fingerprint = reality.fingerprint();
        Self {
            origin,
            lod,
            reality,
            reality_fingerprint,
            payload,
        }
    }

    pub fn is_in_reality(&self, reality: &RealitySnapshot) -> bool {
        self.reality_fingerprint == reality.fingerprint() && &self.reality == reality
    }
}

/// Keyed store of resident chunks. Iteration order is the insertion order of
/// keys (tracked separately) so atlas offsets stay deterministic.
#[derive(Debug, Default)]
pub struct ChunkStore {
    chunks: HashMap<ChunkKey, LoadedChunk>,
    order: Vec<ChunkKey>,
}

impl ChunkStore {
    pub fn new() -> Self {
        Self {
            chunks: HashMap::new(),
            order: Vec::new(),
        }
    }

    pub fn len(&self) -> usize {
        self.order.len()
    }

    pub fn is_empty(&self) -> bool {
        self.order.is_empty()
    }

    pub fn contains(&self, key: ChunkKey) -> bool {
        self.chunks.contains_key(&key)
    }

    pub fn get(&self, key: ChunkKey) -> Option<&LoadedChunk> {
        self.chunks.get(&key)
    }

    pub fn insert(&mut self, key: ChunkKey, chunk: LoadedChunk) {
        if self.chunks.insert(key, chunk).is_none() {
            self.order.push(key);
        }
    }

    /// Removes every chunk whose key is not in `keep`, returning whether
    /// anything changed.
    pub fn retain_keys(&mut self, keep: &[ChunkKey]) -> bool {
        let before = self.order.len();
        self.order.retain(|k| keep.contains(k));
        self.chunks.retain(|k, _| keep.contains(k));
        before != self.order.len()
    }

    /// Chunks in deterministic (insertion) order.
    pub fn iter_ordered(&self) -> impl Iterator<Item = &LoadedChunk> {
        self.order.iter().filter_map(|k| self.chunks.get(k))
    }

    /// All collision boxes of all resident chunks.
    pub fn all_collision_boxes(&self) -> impl Iterator<Item = &Aabb> {
        self.iter_ordered().flat_map(|c| c.payload.collision.iter())
    }

    pub fn all_traversal_gates(&self) -> impl Iterator<Item = &TraversalGate> {
        self.iter_ordered()
            .flat_map(|c| c.payload.traversal_gates.iter())
    }

    pub fn all_supply_items(&self) -> impl Iterator<Item = &SupplyItem> {
        self.iter_ordered()
            .flat_map(|c| c.payload.supply_items.iter())
    }

    pub fn all_level_exits(&self) -> impl Iterator<Item = &LevelExit> {
        self.iter_ordered()
            .flat_map(|c| c.payload.level_exits.iter())
    }

    pub fn all_pit_hazards(&self) -> impl Iterator<Item = &PitHazard> {
        self.iter_ordered()
            .flat_map(|c| c.payload.pit_hazards.iter())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn desired_origins_covers_square_around_player() {
        let policy = StreamingPolicy::omnidirectional(10.0, 1);
        let origins = policy.desired_origins(5.0, 5.0);
        assert_eq!(origins.len(), 9);
        // Nearest-first: the chunk containing the player comes first.
        assert_eq!(origins[0], (0.0, 0.0));
        assert!(origins.contains(&(-10.0, -10.0)));
        assert!(origins.contains(&(10.0, 10.0)));
    }

    #[test]
    fn desired_origins_handles_negative_coordinates() {
        let policy = StreamingPolicy::omnidirectional(10.0, 1);
        let origins = policy.desired_origins(-0.1, -0.1);
        assert_eq!(origins[0], (-10.0, -10.0));
    }

    #[test]
    fn store_eviction_reports_change() {
        let mut store = ChunkStore::new();
        let payload = ChunkPayload {
            root: 0,
            nodes: vec![],
            brick_voxels: Vec::new(),
            world_size: 10.0,
            voxel_size: 0.2,
            svo_depth: 6,
            surface: crate::application::ports::SurfaceMeshPayload::empty(0),
            lights: vec![],
            collision: vec![],
            traversal_gates: vec![],
            pit_hazards: vec![],
            supply_items: vec![],
            level_exits: vec![],
        };
        store.insert(
            chunk_key(0.0, 0.0),
            LoadedChunk::new((0.0, 0.0), 0, RealitySnapshot::default(), payload.clone()),
        );
        store.insert(
            chunk_key(10.0, 0.0),
            LoadedChunk::new((10.0, 0.0), 0, RealitySnapshot::default(), payload),
        );
        assert_eq!(store.len(), 2);
        assert!(store.retain_keys(&[chunk_key(0.0, 0.0)]));
        assert_eq!(store.len(), 1);
        assert!(!store.retain_keys(&[chunk_key(0.0, 0.0)]));
    }
}

#[cfg(test)]
mod view_culling_tests {
    use super::*;

    const CHUNK: f32 = 10.0;

    fn visual_policy(radius: i32, core_radius: i32) -> StreamingPolicy {
        StreamingPolicy {
            chunk_size: CHUNK,
            radius,
            core_radius,
        }
    }

    #[test]
    fn omnidirectional_policies_keep_the_whole_square() {
        let policy = StreamingPolicy::omnidirectional(CHUNK, 2);
        assert_eq!(policy.desired_origins(0.0, 0.0).len(), 25);
        // A view cone must not shrink a policy whose core covers it all.
        let culled = policy.desired_origins_in_view(0.0, 0.0, Some(ViewCone::from_yaw(0.0, 0.5)));
        assert_eq!(culled.len(), 25);
    }

    #[test]
    fn view_culling_drops_chunks_behind_the_player() {
        let policy = visual_policy(4, 1);
        let all = policy.desired_origins(0.0, 0.0);
        // Yaw 0 faces -Z in this basis.
        let ahead = policy.desired_origins_in_view(0.0, 0.0, Some(ViewCone::from_yaw(0.0, 0.8)));
        assert!(
            ahead.len() < all.len(),
            "culling must remove something: {} vs {}",
            ahead.len(),
            all.len()
        );
        // Nothing far behind the player survives.
        assert!(
            !ahead.contains(&(0.0, 30.0)),
            "a chunk directly behind the player should be culled"
        );
        // The chunk straight ahead certainly does.
        assert!(ahead.contains(&(0.0, -40.0)));
    }

    #[test]
    fn the_core_radius_survives_any_view_direction() {
        let policy = visual_policy(4, 2);
        for yaw in [0.0, 1.0, 2.0, 3.0, 4.0, 5.0, 6.0] {
            let desired =
                policy.desired_origins_in_view(0.0, 0.0, Some(ViewCone::from_yaw(yaw, 0.7)));
            for dz in -2..=2 {
                for dx in -2..=2 {
                    let origin = (dx as f32 * CHUNK, dz as f32 * CHUNK);
                    assert!(
                        desired.contains(&origin),
                        "core chunk {origin:?} was culled at yaw {yaw}"
                    );
                }
            }
        }
    }

    #[test]
    fn every_direction_keeps_the_set_nearest_first() {
        let policy = visual_policy(4, 2);
        for yaw in [0.0, 0.9, 2.5, 4.2] {
            let desired =
                policy.desired_origins_in_view(5.0, 5.0, Some(ViewCone::from_yaw(yaw, 0.7)));
            let mut previous = f32::NEG_INFINITY;
            for (x, z) in desired {
                let d2 = (x + CHUNK * 0.5 - 5.0).powi(2) + (z + CHUNK * 0.5 - 5.0).powi(2);
                assert!(
                    d2 >= previous,
                    "streaming order stopped being nearest-first"
                );
                previous = d2;
            }
        }
    }

    #[test]
    fn partially_visible_chunks_are_kept() {
        // The point of the bounding-circle test: a chunk whose *centre*
        // lies outside the cone is still streamed when part of its
        // footprint is inside. Without this, geometry pops in at the
        // screen border as the player turns.
        let policy = visual_policy(4, 0);
        let half_angle = 0.30;
        let (px, pz) = (5.0, 5.0);
        let view = ViewCone::from_yaw(0.0, half_angle);
        let kept = policy.desired_origins_in_view(px, pz, Some(view));

        let centre_outside_cone = kept.iter().any(|&(x, z)| {
            let (to_x, to_z) = (x + CHUNK * 0.5 - px, z + CHUNK * 0.5 - pz);
            let distance = (to_x * to_x + to_z * to_z).sqrt();
            if distance <= f32::EPSILON {
                return false;
            }
            let cos = (to_x * view.forward.0 + to_z * view.forward.1) / distance;
            cos.clamp(-1.0, 1.0).acos() > half_angle
        });
        assert!(
            centre_outside_cone,
            "a strict centre-in-cone test would have dropped chunks this kept"
        );
    }

    #[test]
    fn non_finite_input_does_not_panic() {
        let policy = visual_policy(2, 1);
        let desired = policy.desired_origins_in_view(
            f32::NAN,
            f32::INFINITY,
            Some(ViewCone::from_yaw(f32::NAN, 0.7)),
        );
        assert!(!desired.is_empty());
    }
}
