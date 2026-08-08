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

/// Detail as a function of distance, so the resident world can reach past
/// the fog without paying full resolution for all of it.
///
/// The engine used to have exactly two detail levels: full, within
/// `fine_distance`, and one coarse level everywhere else. That put a hard
/// ceiling on how far the world could reach, because the outer ring — which
/// is almost all of it, growing as the square of the radius — cost a
/// quarter of full resolution no matter how far away it was.
///
/// Measured cost of one 10 u chunk's surface mesh, which is what the ladder
/// is trading against:
///
/// ```text
/// lod 0   252 KB   33.3 ms
/// lod 1    94 KB    8.4 ms
/// lod 2    21 KB    4.7 ms
/// lod 3     2 KB    4.8 ms
/// ```
///
/// A chunk twelve times cheaper is a ring that can be twelve times larger,
/// and the far ring is exactly where detail is least visible — Level 0's fog
/// (`exp(-0.018 * (d - 12))`) has already removed 92% of a surface by 155 u.
///
/// Distances are world units, not chunks, so one ladder describes both the
/// 10 u and 20 u chunk profiles.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LodLadder {
    /// Full resolution within this distance.
    pub fine_distance: f32,
    /// Half resolution within this distance.
    pub mid_distance: f32,
    /// Quarter resolution within this distance; eighth beyond it.
    pub far_distance: f32,
}

impl LodLadder {
    /// The detail a chunk at this distance should be held at.
    pub fn lod_at(&self, distance: f32) -> u8 {
        // A NaN distance must not silently become the most expensive level:
        // every comparison below is false for NaN, so it falls through to
        // the cheapest, which is the safe direction to fail.
        if distance <= self.fine_distance {
            0
        } else if distance <= self.mid_distance {
            1
        } else if distance <= self.far_distance {
            2
        } else {
            3
        }
    }

    /// Chunks of `chunk_size` needed to reach the end of the ladder.
    ///
    /// This is what the visual streaming radius should be: holding chunks
    /// beyond it would spend memory on geometry the ladder has already
    /// decided is barely worth resolving.
    pub fn radius_in_chunks(&self, chunk_size: f32) -> i32 {
        if !(chunk_size > 0.0) || !self.far_distance.is_finite() {
            return 1;
        }
        (self.far_distance / chunk_size).ceil().max(1.0) as i32
    }
}

impl Default for LodLadder {
    /// Sized against Level 0's fog rather than against a memory budget.
    ///
    /// At 155 u a surface is down to 7.6% transmittance, so a chunk arriving
    /// at the far edge is a change to something already almost invisible —
    /// which is the whole point: the player must never watch geometry appear.
    /// For 10 u chunks this is a 31x31 footprint costing roughly 43 MB of
    /// mesh, against the 9x9 (~8 MB) it replaces.
    fn default() -> Self {
        Self {
            fine_distance: 30.0,
            mid_distance: 75.0,
            far_distance: 155.0,
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
    fn the_ladder_coarsens_monotonically_with_distance() {
        let ladder = LodLadder::default();
        let mut previous = 0u8;
        let mut d = 0.0f32;
        while d < ladder.far_distance * 2.0 {
            let lod = ladder.lod_at(d);
            assert!(
                lod >= previous,
                "detail increased with distance at {d} u: {previous} -> {lod}"
            );
            previous = lod;
            d += 1.0;
        }
        assert_eq!(previous, 3, "the ladder never reached its cheapest rung");
    }

    #[test]
    fn each_band_holds_its_own_edge() {
        let ladder = LodLadder {
            fine_distance: 10.0,
            mid_distance: 20.0,
            far_distance: 30.0,
        };
        // Boundaries belong to the finer band: a chunk exactly at the edge
        // renders at the better level, never the worse one.
        assert_eq!(ladder.lod_at(0.0), 0);
        assert_eq!(ladder.lod_at(10.0), 0);
        assert_eq!(ladder.lod_at(10.01), 1);
        assert_eq!(ladder.lod_at(20.0), 1);
        assert_eq!(ladder.lod_at(20.01), 2);
        assert_eq!(ladder.lod_at(30.0), 2);
        assert_eq!(ladder.lod_at(30.01), 3);
    }

    #[test]
    fn a_nonsense_distance_falls_to_the_cheapest_rung() {
        // Failing toward cheap is the safe direction: a NaN distance that
        // resolved to lod 0 would generate the most expensive chunk in the
        // engine for a position that does not exist.
        let ladder = LodLadder::default();
        assert_eq!(ladder.lod_at(f32::NAN), 3);
        assert_eq!(ladder.lod_at(f32::INFINITY), 3);
    }

    #[test]
    fn the_radius_reaches_the_end_of_the_ladder() {
        let ladder = LodLadder::default();
        // Both chunk profiles must cover the same world distance.
        for chunk_size in [10.0f32, 20.0] {
            let radius = ladder.radius_in_chunks(chunk_size);
            assert!(
                radius as f32 * chunk_size >= ladder.far_distance,
                "radius {radius} of {chunk_size} u chunks falls short of {} u",
                ladder.far_distance
            );
        }
        assert_eq!(ladder.radius_in_chunks(0.0), 1, "a degenerate chunk size");
    }

    #[test]
    fn the_footprint_reaches_past_where_level_zero_can_be_seen() {
        // The property the whole ladder exists for. Level 0's fog is
        // `exp(-density * (d - start))`; a chunk arriving where less than 8%
        // of it survives is not something a player can watch appear.
        let ladder = LodLadder::default();
        let (fog_start, fog_density) = (12.0f32, 0.018f32);
        let transmittance = (-fog_density * (ladder.far_distance - fog_start)).exp();
        assert!(
            transmittance < 0.08,
            "the streaming frontier sits at {:.0}% visibility",
            transmittance * 100.0
        );
    }

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
