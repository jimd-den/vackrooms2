# Part 1 — Core Domain Audit (`src/domain/**` + SVDAG compression)

**Auditor:** static code audit (pure file reading; nothing compiled, built, run, or tested)
**Date:** audit session, part 1
**Scope:** all 33 files under `src/domain/` (~6.1k lines) plus `src/use_cases/compress_svdag.rs` (the SVDAG compressor referenced by the SVO pipeline; head-start finding #4) and `src/use_cases/build_octree.rs` (the SVO builder, needed to reason about arena/mask invariants).
**Method:** head-start findings were re-verified against the source with line numbers; every file was skimmed, all algorithmic hot paths and data-structure choices were analyzed for Big-O, recursion depth, allocation churn, and determinism; the whole `src/domain` tree was grepped for shared mutable state (unsafe/static/thread_local/Atomics/Mutex/RefCell/thread::spawn — **none found**).

---

## 1. Head-start findings — confirmed with line numbers

### HS-1. SVO `set_recursive` never clears `child_mask` on air writes — CONFIRMED
`src/domain/entities/sparse_voxel_octree.rs:202-204`
```rust
if v_type != 0 {
    child_mask |= 1 << octant_idx;
}
```
There is no `else` branch, so writing air (`v_type == 0`) never clears the octant bit. Consequences:
- A partially cleared subtree keeps its mask bit set and keeps the (now-air) leaf allocated; `get()` still returns air (sparse_voxel_octree.rs:98-100 returns `Some((0,0,[0;3],0))` for masked-out octants, identical to reading an air leaf), so lookups stay correct — the cost is *wasted traversal* (GPU-side serializers that skip children via the mask now descend into air leaves) and *memory that is never pruned by edits*.
- `is_empty()` (sparse_voxel_octree.rs:48-53) can transiently report non-empty for a subtree whose voxels were all cleared (mask `0xFF` over air leaves); only the bottom-up `collapse_if_uniform` pass eventually folds fully-uniform subtrees back to a single air leaf.
- The invariant "mask bit set ⇒ child is solid" is violated in the *other* direction too: SVDAG canonicalization and any consumer that trusts the mask to enumerate solid children will visit air leaves.
**Fix:** when `v_type == 0`, clear the bit (`child_mask &= !(1 << octant_idx)`) after the recursive write confirms the child subtree is uniformly air (the existing `collapse_if_uniform` result tells you this), and prune the child block.

### HS-2. SVO collapse leaves dead nodes in the arena — CONFIRMED
`src/domain/entities/sparse_voxel_octree.rs:230-278` (`collapse_if_uniform` overwrites the parent `Internal` with a `Leaf`), combined with the split at lines 170-186 (each split appends 8 nodes).
- When a node is collapsed, its 8 children remain in `nodes` forever — they are unreachable from `root`, but the arena never reclaims them. There is no free-list and no compaction on `SparseVoxelOctree`.
- Every split-then-collapse cycle (e.g. `set` a voxel, then `set` it back) leaks 8 nodes. Repeated edits → **unbounded arena growth**.
- Mitigation that exists today: `compress_svdag` (src/use_cases/compress_svdag.rs:127-151) rebuilds the arena from the root, so dead nodes are dropped at compression time — but compression is an explicit, upload-time step, not automatic.
**Fix:** reuse the dead slots (free-list keyed by `child_base_index`), or make `collapse_if_uniform` append the freed block to a free list; alternatively run compaction when `nodes.len()` exceeds live-node count by a threshold.

### HS-3. Path-tracer bake is O(S·R·steps) ≈ 1,920 grid probes per solid voxel — CONFIRMED
`src/domain/use_cases/path_tracer.rs:104-106` (`rays_per_voxel = 64`, `max_steps = 30`) and loops at 116-184; `cast_ray` DDA at 7-96.
- Per solid voxel: 64 rays × up to 30 DDA steps = up to **1,920** bounds-checked `grid.get()` calls. For a 256×8×256 grid (~524k voxels, mostly solid), that is on the order of **0.5–1.0 billion probes** per bake.
- Single-threaded (fixed seed `StdRng::seed_from_u64(42)` at line 108), no early-out once the light estimate is saturated (clamped at line 177-179).
- This is a one-time bake, so it is a *note*, not a hot-path bug; but it is trivially parallelizable (rayon is already in dev-deps) and each slab/ray batch is independent.
**Fix:** parallelize over voxels or rays with rayon; early-skip voxels whose 6-neighborhood scan (lines 136-150) already shows no adjacent air; consider terminating a voxel's ray budget once `total_light/rays` saturates past 15/30.

### HS-4. `compress_svdag` canonicalizes all 8 children regardless of `child_mask` — CONFIRMED
`src/use_cases/compress_svdag.rs:103-120`: `for (octant, slot) in block.iter_mut().enumerate() { *slot = self.canonicalize(source, child_base_index + octant); }` — unconditional over 0..8.
- Masked-out octants are *provably* uniform air: the mask bit is only ever set when a solid is written (sparse_voxel_octree.rs:202-204 and build_octree.rs:163-174), and a mask-0 slot can only hold an air leaf (the only writes that reach a mask-0 slot are air writes, which leave it air). So the recursion into masked-out children visits whole air subtrees that always canonicalize to the same `AIR_LEAF`.
- With HS-1's stale masks, this is worse: air leaves that *should* be pruned are walked and hashed into the intern table (they dedupe to one block, but the traversal cost remains).
- Wasteful but safe (results verified by tests at compress_svdag.rs:184-209).
**Fix:** for `child_mask & (1 << octant) == 0`, substitute the canonical air leaf directly and skip the recursive walk. This is safe as long as the mask invariant from HS-1's fix is restored; without it, skipping is *still* safe for mask-0 octants (they are always air leaves), but not for the stale mask-1 air leaves — so combine with HS-1.

---

## 2. New findings

### F-1 (Medium). `ProbeGrid::sample_normalized` is nearest-neighbor while docs promise trilinear interpolation and a 1-probe halo — visual chunk-seam risk
`src/domain/entities/probe_grid.rs:9-10` (module docs: "1-probe halo duplication at grid boundaries to guarantee bilinear/trilinear … without dark seam discontinuities"), `:173-174` (doc: "Trilinearly interpolates or samples"), but the implementation at `:175-185` rounds to the nearest probe (`(norm * (dim-1)).round() as usize`) and reads one texel. No halo duplication exists in `from_voxel_grid` either (`:119-168` maps strictly inside the grid).
- Claimed seam-stability property is not implemented; adjacent chunks whose probes disagree will show a hard per-probe step instead of the promised smooth gradient.
**Fix:** implement real trilinear interpolation (or clamp-and-fetch 8 texels with linear weights), or downgrade the docs to "nearest-neighbor" and add the halo pass if seam continuity matters.

### F-2 (Medium). BSP build/clip worst case: O(P²) time, O(P) recursion depth, and fragment blow-up on adversarial input
`src/domain/use_cases/csg/bsp.rs:28-56` (`build` splits every polygon against the first polygon's plane, recursively), `:77-105` (`clip_polygons`), `:132-160` (`contains_point` recursion).
- `build` is O(P·depth) expected but O(P²) worst case (a degenerate chain where every polygon spans the current node's plane); a polygon that spans many planes accumulates split vertices, and fragment counts can grow by a constant factor per level in the worst case (classic csg.js pathology). `contains_point` and `clip_polygons` recurse to tree depth, which is O(P) in the degenerate case — stack risk for large authored solids.
- Authoring-time and rare (cuboids/extrudes are small), so Low-Medium.
**Fix:** pick the splitting plane to balance the tree (e.g. the plane of the median polygon, or an axis-aligned mid-plane of the AABB), cap depth, and/or fall back to voxelization for pathological inputs.

### F-3 (Low). `Solid::subtract`/`intersect` materialize the polygon soup twice
`src/domain/use_cases/csg/solid.rs:115-130` (subtract) and `:133-147` (intersect): `a.all_polygons()` + `b.all_polygons()` are collected into a soup, then `BspNode::from_polygons(soup)` re-splits every polygon (O(P log P) expected), then `result.all_polygons()` clones every polygon again to build the returned `Solid`. `union` (`:102-112`) avoids the rebuild but still collects both soups.
- Every polygon is cloned ~2-3× per boolean op; for large authored solids (hundreds of polygons) this is noticeable but not hot.
**Fix:** have `subtract`/`intersect` reuse the already-computed BSP result and collect once; or invert the tree in place and return its polygons without re-building.

### F-4 (Low). `AssemblyInstance::host()` is a linear scan, making `validate_architecture` O(H² + O²·H + L·O)
`src/domain/entities/architecture.rs:595-597` (`host()` = `self.hosts.iter().find(...)`), used inside `validate_architecture`:
- `:651-657` — hosts-overlap is O(H²) with H hosts;
- `:687-702` — for each pair of openings it calls `self.host(a.host)` (O(H)) → O(O²·H);
- `:703-715` — per door leaf, `self.openings.iter().any(...)` → O(L·O).
- Validation-only, and typical counts are small, but the O²·H term is needless.
**Fix:** index hosts by `HostId` in a `HashMap` once at validation start.

### F-5 (Low). `RealitySnapshot::with_advanced_gate` clones and re-canonicalizes the entire snapshot on every gate event
`src/domain/entities/anomaly/reality.rs:369-412`: clones `stamps` (`:392`), does `retain`+`push` (`:393-403`), then re-runs `with_drifts_and_supplies` (`:406-410`) which rebuilds two `BTreeMap`s and re-sorts `consumed_supplies` — O(S log S + D + C) per crossing, plus the full clone. Events are rare (player crosses a threshold), so this is acceptable; the idempotency early-out (`:375-379`) covers duplicate events.
**Fix (optional):** a small `HashMap<AnomalyId, AnomalyStateStamp>` in the struct instead of re-sorting on every transition, or make the update in place.

### F-6 (Micro). Hot-path accessors in `VoxelGrid` are bounds-checked Option chains
`src/domain/entities/voxel_grid.rs:218-238` (`index`/`get`/`set`): every probe in the path-tracer bake (≈1B calls, path_tracer.rs:90) and every meshing pass pays a 3-way bounds check + Option unwrap + `y*(w*d)+z*w+x` recompute. Also `light_data` (scalar) duplicates `max(r,g,b)` of `light_rgb` (voxel_grid.rs:255-260) — 1 byte/voxel of redundant storage kept in sync by hand.
**Fix:** add `#[inline]` unchecked index helpers for interior loops (or slice-based `chunks_exact_mut` iterators); keep the safe API for external callers. Consider deriving scalar light on demand instead of storing it.

### F-7 (Micro). Maze generator allocates a `Vec` per frontier cell
`src/domain/use_cases/generate_maze.rs:55-65`: `let mut unvisited = Vec::new();` inside the per-cell loop — 4 possible neighbors; one small heap allocation per cell (O(cells) allocations for a O(w·d) algorithm). The algorithm itself is clean: `swap_remove` (`:68`) keeps the frontier O(1) amortized; loop-adding pass is bounded by `attempts < extra_openings * 3` (`:92`) so it stays O(w·d).
**Fix:** stack array `[(usize,usize,usize,usize); 4]` + count, or `SmallVec`.

### F-8 (Micro). `Plane::split_polygon` allocates a `types: Vec<u8>` per polygon
`src/domain/use_cases/csg/plane.rs:57-72`: one heap Vec per split call, and split is called once per polygon per BSP node (bsp.rs:39-47, 85-93). Allocation churn on large solids.
**Fix:** small inline buffer (polygons are typically 3-6 vertices), or classify twice (cheap) without storing types.

### F-9 (Info). Doc/behavior mismatches worth flagging
- `ProbeGrid` docs claim trilinear + halo (see F-1).
- `path_tracer.rs:176` uses a magic `* 15.0 * 2.0` brightness factor with no named constant; light levels saturate at 15 (fine, but undocumented).
- `bsp.rs:3-7` correctly documents the no-front/outside, no-back/inside convention — verified consistent with `clip_polygons` (`:97-103`) and `contains_point` (`:132-160`). Good.
- `Solid::contains_point` (solid.rs:85-87) builds a fresh BSP per query — documented as one-off; the voxelizer correctly caches `solid.bsp()` (src/use_cases/csg_voxelizer.rs:38, 61). No action needed, but future callers should follow the same pattern.

---

## 3. (a) Big-O table of main algorithms

| Algorithm | Time | Space | Where |
|---|---|---|---|
| Morton encode/decode | O(1) | O(1) | entities/morton.rs:36-47 |
| SVO `get` | O(depth) | O(1) | entities/sparse_voxel_octree.rs:56-107 |
| SVO `set` (incl. split+collapse) | O(depth), O(8) collapse scan per level; allocates 8 nodes per split | O(nodes), unbounded w/o compaction (HS-2) | sparse_voxel_octree.rs:110-228 |
| SVO `collapse_if_uniform` | O(8) per call | O(1) | sparse_voxel_octree.rs:230-278 |
| Octree build from dense grid | O(V) (V = w·h·d voxels), prunes outside-grid subtrees | O(leaves + internal nodes) | src/use_cases/build_octree.rs:85-182 |
| `compress_svdag` | O(N) (each arena node canonicalized once) | O(N) arena + intern table (8-entry keys) | src/use_cases/compress_svdag.rs:90-151 |
| BSP `build` | O(P·depth) expected, O(P²) worst; fragment growth in adversarial spanning cases | O(total fragments) | csg/bsp.rs:28-56 |
| BSP `clip_polygons` | O(F·depth) per call | O(F) buffers | csg/bsp.rs:77-105 |
| BSP `invert` | O(total polygons in tree) | O(1) | csg/bsp.rs:59-73 |
| BSP `contains_point` | O(depth) expected, O(P) worst | O(depth) stack | csg/bsp.rs:132-160 |
| `Plane::split_polygon` | O(vertices) per polygon + 1 Vec alloc | O(vertices) | csg/plane.rs:48-110 |
| Solid union/subtract/intersect | O((Pa+Pb)·depth) per clip pass, several passes + double soup materialization (F-3) | O(fragments) | csg/solid.rs:102-147 |
| Growing-tree maze | O(w·d), O(1) amortized frontier ops | O(w·d) grid + frontier | use_cases/generate_maze.rs:41-84 |
| Maze loop-adding pass | O(w·d) (attempts capped at 3× extra) | O(1) | generate_maze.rs:86-127 |
| Level 0 voxel builder | O(w·d·rs²·wh) = O(V) | O(V) | use_cases/level_builder.rs:31-86 |
| Path-tracer bake | O(S·R·steps), S solid voxels, R=64, steps≤30 ⇒ ≈1,920 probes/voxel (~0.5-1B probes for 256×8×256) | O(1) | use_cases/path_tracer.rs:104-184 |
| Probe downsample `from_voxel_grid` | O(V) (each voxel in ≤1 region; ≤small overlap when upsampling) | O(probes) | entities/probe_grid.rs:119-168 |
| Probe `sample_normalized` | O(1) (nearest) | O(1) | probe_grid.rs:175-185 |
| `pit_hazards_for_bounds` | O(k log k), k = lattice cells in bounds | O(k) | entities/anomaly/phenomena.rs:250-315 |
| `PillarLattice::pillar_shape` | O(1) (splitmix64 rolls) | O(1) | phenomena.rs:100-136 |
| Reality gate transition | O(S log S + D + C) with full clone per event | O(S+D+C) | entities/anomaly/reality.rs:369-412 |
| Drift epoch / supply lookup | O(log D) / O(log C) (sorted vectors, binary search) | O(1) | reality.rs:331-337, 302-304 |
| Snapshot fingerprint/to_words | O(S+D+C) per call | O(S+D+C) | reality.rs:423-475 |
| `CirculationSpine::nearest` | O(segments) per query | O(1) | entities/architecture.rs:763-782 |
| `Polygon2::contains` (even-odd) | O(vertices) | O(1) | architecture.rs:170-183 |
| `validate_architecture` | O(H² + O²·H + L·O) | O(H+O) | architecture.rs:635-717 |
| `PlayerVitals::advanced` | O(1) | O(1) | entities/player_vitals.rs:58-73 |

Notes: no accidental O(n²) was found in any hot loop of the domain layer. The only super-linear constructs are BSP worst cases (F-2), validation (F-4), and the one-time bake (HS-3). `RealitySnapshot` uses `BTreeMap`/sorted `Vec` everywhere (canonical order, binary-search lookups) — deliberate and correct.

## 4. (b) Concurrency inventory

**Surface within `src/domain`:** none. A full grep of `src/domain` for `unsafe`, `static mut`, `thread_local`, `Mutex`, `RwLock`, `Atomic`, `RefCell`, `Cell<`, `thread::`, `spawn` returns **zero hits**. Every domain type is a plain owned value (`Vec`, `Option`, enums, floats); all types are therefore automatically `Send + Sync`, with no interior mutability, no global RNG, and no static state. This is the correct posture for the worker model used elsewhere in the repo (workers receive a `RealitySnapshot` by value and produce a chunk deterministically).

| Shared/transferred state | Synchronization | Analysis |
|---|---|---|
| `RealitySnapshot` (stamps/drifts/consumed/delirium) | Immutable value semantics; passed by value to workers (reality.rs:1-5 "workers receive it by value"); every transition returns a new `Self` | Safe for concurrent generation; no torn reads possible. Cost: deep `Vec` clones per request (F-5) — O(S+D+C) per clone; could be `Arc`-shared for read-mostly handoff. |
| Snapshot canonical order | `BTreeMap` (ordered) + `sort_unstable`/`dedup` (reality.rs:253-283) and `binary_search` lookups (:302-304, :331-337, :362-367) | Deterministic across workers; no HashMap iteration-order dependence in the domain (only `compress_svdag` uses HashMap, and only for membership lookup — no ordering leaks). |
| RNG | `MazeGenerator::generate<R: Rng>` receives `&mut R` injected by caller (generate_maze.rs:13) — no shared RNG | Deterministic per seed; the maze result is a pure function of grid + rng stream. |
| Path-tracer RNG | Local `StdRng::seed_from_u64(42)` (path_tracer.rs:108) | Fixed seed ⇒ deterministic bake; never shared across threads. Same bake for every chunk (no per-chunk variation) — deterministic, if monotonous. |
| Stable ids / hashing | `mix64` splitmix64 finalizer, private to anomaly aggregate (anomaly.rs:28-34) | Pure function; stable across threads and sessions. |
| SVO/SVDAG | Owned `Vec<SvoNode>` arena; `compress_svdag` is a pure function `&SparseVoxelOctree -> (SparseVoxelOctree, SvdagStats)` | No sharing; safe. The DAG caveat is documented (compress_svdag.rs:11-14): DAG must not be fed to walks that assume each node visited once per world position (e.g. collision extraction). |
| BSP/CSG | Pure data structures, no caching beyond caller-owned `BspNode` | No concurrency surface. |

**Out-of-scope surfaces the domain contract must keep satisfying** (owned by other audit parts): the single `thread::spawn` in src/main.rs, `Atomic` statics in wasm_frontend/src/lib.rs, and the Web Worker pipeline (worker_source.rs / static/worker.js / generation_worker_policy.rs / generation_worker_requests.rs). From the domain side, the load-bearing properties are: (1) all domain values are `Send + Sync` plain data; (2) generation is a pure function of (seed, coordinate, `RealitySnapshot`) — nothing in the domain mutates global state; (3) `RealitySnapshot` round-trips losslessly through `to_words`/`from_words` (fingerprint-verified, reality.rs:477-538), so worker replies and retries cannot diverge. No deadlock/livelock/re-entrancy risk exists inside the domain because it holds no locks and performs no I/O.

## 5. (c) Highest-leverage optimizations, ranked

1. **Fix SVO air-write mask clearing + pruning** (sparse_voxel_octree.rs:202-204). Today every edit that clears a voxel leaves a stale mask bit, a permanent air leaf, and (via HS-2) 8 dead arena nodes; repeated edits grow the arena without bound and make every downstream walk (GPU serializer, SVDAG compress) descend into air. One `else` branch plus reusing the existing `collapse_if_uniform` result turns this into real pruning. *Impact: memory bound + faster traversal on all edited SVOs; unblocks #2.*
2. **Skip masked-out octants in `compress_svdag`** (compress_svdag.rs:113-115). With mask-0 octants provably air, substituting the canonical air leaf avoids recursing into whole air subtrees; combined with #1 this also avoids interning the stale-mask air leaves that dominate edited SVOs. *Impact: compression wall-time and arena size drop substantially on sparse/repetitive chunks (the Pi upload path, compress_svdag.rs:219-245).*
3. **Parallelize the path-tracer bake** (path_tracer.rs:116-184). ~0.5-1B DDA probes single-threaded is the largest one-time cost in the domain; voxels/rays are embarrassingly independent and rayon is already a dev-dependency. Add early saturation exit and keep the fixed seed per-slab so results stay reproducible. *Impact: bake time ÷ cores; one-time, so secondary.*
4. **Implement real trilinear probe sampling (or fix the docs)** (probe_grid.rs:175-185). The seam-continuity property the module is designed for is not delivered by nearest-neighbor sampling; with probe volumes at 8×4×8 per chunk this is a visible lighting artifact across chunk borders. *Impact: visual correctness (and it is the stated purpose of the entity).*
5. **De-duplicate polygon soup materialization in CSG booleans** (solid.rs:115-147) and index `AssemblyInstance::host` for validation (architecture.rs:595-597, 687-702). Both are authoring-time, but the first roughly halves allocation/clone volume per boolean op and the second removes an O(O²·H) term. *Impact: authoring latency on large CAD-style edits.*
6. **Unchecked interior accessors for `VoxelGrid` hot loops** (voxel_grid.rs:218-238). The bake and any meshing pass pay a bounds-checked Option chain per probe; an `#[inline]` unchecked index for interior loops (with safe wrappers preserved) is free performance. *Impact: ~1B calls in the bake, plus per-voxel meshing everywhere.*

---

## Appendix: files reviewed
`entities/anomaly.rs`, `entities/anomaly/geometry.rs`, `entities/anomaly/phenomena.rs`, `entities/anomaly/reality.rs`, `entities/anomaly/traversal.rs`, `entities/architecture.rs`, `entities/cad.rs`, `entities/cell.rs`, `entities/chunk_entities.rs`, `entities/environment.rs`, `entities/fixture.rs`, `entities/grid.rs`, `entities/mod.rs`, `entities/morton.rs`, `entities/player_vitals.rs`, `entities/position.rs`, `entities/probe_grid.rs`, `entities/sparse_voxel_octree.rs`, `entities/supplies.rs`, `entities/surface_quality_profile.rs`, `entities/voxel_grid.rs`, `entities/world_topology.rs`, `use_cases/csg/bsp.rs`, `use_cases/csg/mod.rs`, `use_cases/csg/plane.rs`, `use_cases/csg/polygon.rs`, `use_cases/csg/solid.rs`, `use_cases/csg/vec3.rs`, `use_cases/generate_maze.rs`, `use_cases/level_builder.rs`, `use_cases/mod.rs`, `use_cases/path_tracer.rs` (+ context: `src/use_cases/compress_svdag.rs`, `src/use_cases/build_octree.rs`, `src/use_cases/csg_voxelizer.rs:38,61`).
