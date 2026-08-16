# Audit Report Part 3: Core Generation & Rest (src/use_cases)

**Scope:** All `src/use_cases/**` except `level_zero/` and `level_one/` (50 files).
**Method:** Static reading only — no compilation, build, or execution.
**Status:** WORK IN PROGRESS — continuously appended.

---

## 0. Seed-derived findings (from prior audit passes — VERIFYING line numbers)

| # | Finding | Location (seed) | Verified? |
|---|---------|-----------------|-----------|
| S1 | `solve_wfc` `lowest_entropy_cell` scans ALL N cells per collapse → O(N²) | wfc3d.rs:144-147,160-180 | VERIFIED §1.1 |
| S2 | `build_octree` recursion visits every node O(8^depth) = O(N) full grid, PRUNE_OUTSIDE short-circuit | build_octree.rs:85-182 | VERIFIED §1.2 |
| S3 | `compress_svdag` canonicalize: HashMap<[CanonNode;8], u32> interning, 128-byte keys | compress_svdag.rs:69-122 | VERIFIED §1.3 |
| S4 | `region_plan/layout.rs` `lay_out_suites`: rounds×program×legs×anchors×sides candidates × `taken.iter().any()` O(\|taken\|) | layout.rs:221-228 | VERIFIED §1.4 |
| S5 | MAJOR: `level_zero/generate.rs:331` `RecursiveLevelWindow::around_chunk` built on EVERY chunk generation → re-plans region window per chunk | level_zero (out of scope, cite for context) | VERIFIED (§1.7) |
| S6 | bake_voxel_lighting: 3 × V × u32 distance arrays (~118MB for 9.9M voxels) | propagate_through_air.rs:37 | VERIFIED §1.9 |
| S7 | frame_quality_controller: O(n log n) sort of candidates per frame | frame_quality_controller.rs:193-203 | VERIFIED §2.7 |
| S8 | territories.rs `largest_free_rectangle` O(n²) per call, loop up to 64 iterations | territories.rs:119-139,147-190 | VERIFIED §1.5 |
| S9 | macro_planner: ~121 anchors × 18 noise evals ≈ 2178 noise evals per region plan query | macro_planner.rs:278-324; world_topology.rs:145-194 | VERIFIED §1.6 |
| S10 | authoring session: unbounded Vec growth, `apply_to_grid` re-walks all elements | authoring.rs:62-87,186-215 | VERIFIED §2.8 |
| S11 | red_rooms/recursive_level.rs: `around_chunk` builds full InfiniteRegionWindow per chunk | recursive_level.rs:50-77; generate.rs:331 | VERIFIED §1.7, §2.10 |
| S12 | `vec![cell]` allocation per collapse iteration in solve_wfc | wfc3d.rs:146 | VERIFIED §1.1 |

---

## 1. Verified findings — WFC, octree/SVDAG, region planning, anomalies, lighting, pipeline

### 1.1 wfc3d.rs — O(N²) entropy scan (CONFIRMED, seed S1)
- **Severity: High (algorithmic; latent in current usage)** — `solve_wfc` loops `while let Some(cell) = lowest_entropy_cell(set, &cells)` (wfc3d.rs:144) and `lowest_entropy_cell` re-scans ALL N cells and recomputes each uncollapsed cell's entropy from scratch per iteration (wfc3d.rs:160-180, loop at 162, per-cell set-bit walk + `weight.ln()` at 166-174). Each collapse iteration collapses exactly one cell (explicit `collapse` at :145; `propagate` may also reduce cells to 1 candidate, so iterations ≤ N). Total: **O(N²)** where N = w·h·d cells, with ~N heap allocations from `vec![cell]` at :146 (S12).
- Current call site: level_zero/menger_expanse.rs:283 with dims (9,1,9)=81 cells, memoized per plate in a 128-entry thread-local LRU-ish cache (menger_expanse.rs:226-237, cleared at >128). Practical impact today is mild (81² ≈ 6.5k entropy computations per plate miss), but the algorithm is the classic WFC hotspot and any grid growth (e.g. a 64³ WFC) makes it the dominant cost (N² = 2.7e8 for 64³).
- Fix: maintain a min-heap (BinaryHeap<(entropy, cell)>) or per-cell cached entropy recomputed only for cells whose mask changed during `propagate` (have `propagate` return the changed-cell list). Determinism note: ties "first occurrence wins" — a heap must break ties by index to preserve output.

### 1.2 build_octree.rs — full-cube recursion, no interior occupancy pruning (CONFIRMED, S2)
- **Severity: Low/Medium** — `build_recursive` (build_octree.rs:85-182) descends all 8 children at every level (:114-117); only the PRUNE_OUTSIDE short-circuit (build_octree.rs:97-99, `region_is_outside_grid` :184-188) skips regions past grid bounds. Fully-empty subcubes *inside* the grid are still descended to depth 0. Time O(8^depth) = O(cube_volume); for a 202×202×242 grid at depth 8 (256³ cube) ≈ 8.2M node visits even for sparse content. Stack depth = depth ≤ 8 (fine, no recursion risk). Space O(#nodes).
- Fix: add an occupancy bitmask / per-region "max" query (e.g., a coarse mip of the grid, or track `has_solid` while sampling) to short-circuit uniform-air subcubes; or build from a run-length/span representation. Cost is per chunk on the CPU before GPU upload (web path), so it is a real per-chunk constant.

### 1.3 compress_svdag.rs — interning with 128-byte keys, full-8-slot recursion (CONFIRMED, S3)
- **Severity: Low** — `canonicalize` (compress_svdag.rs:90-122) walks the whole tree bottom-up, interning `[CanonNode; 8]` blocks in `HashMap<[CanonNode; 8], u32>` (compress_svdag.rs:69-73, `intern` :78-86). `CanonNode` is an enum with a 9-byte Leaf + 5-byte Internal → ~16 B with padding → 128 B keys; hashing O(128) per internal node, so O(V·128) hash work + HashMap churn (V = arena nodes; up to ~2.4M for a depth-8 chunk). Linear, but a heavy constant; the doc-comment caveat (compress_svdag.rs:11-14) about DAG-unaware consumers is respected.
- Also: internal nodes recurse all 8 slots regardless of `child_mask` (compress_svdag.rs:107-115) — air slots are cheap (canonical Leaf), so minor; but blocks could skip interning when `child_mask == 0` (all-air) since that block is globally unique.
- Fix: intern leaves first and key blocks by a 64-bit hash of canonical child ids with equality re-check (or `HashMap<u64, u32>` + Vec<[u32;8]> collision table); skip interning for uniform-air blocks. One-time per chunk before GPU upload.

### 1.4 region_plan/layout.rs — scored greedy placement: O(C × (T+S+E·S+P)) (CONFIRMED, S4)
- **Severity: Low (bounded inputs, but a real O(C·T) pattern)** — `lay_out_suites` (layout.rs:129-257) simulates ROUNDS(2) × PROGRAM_BUDGET(11) × legs(L≈2-6) × anchors(≤36 @ 2.0 pitch, :162-239) × sides(2) ≈ 3k-9.5k candidates per region. Each candidate: `place_suite` (suites.rs:530-647) with its own `taken.iter().any()` (suites.rs:569, O(|taken|)), a full spines scan (suites.rs:575-579, O(S) with `distance` O(path segments)), `entrance_faces_route` per entrance (layout.rs:216-218 → suites.rs:504-524, O(S)); then the head-start `taken.iter().any()` at **layout.rs:221-228** (O(|taken|)) and `score_placement` (layout.rs:229-234 → :265-322, O(|territories| + |placed|)). `taken` grows to ~22 (region_plan/mod.rs:151, :249).
- Per region ≈ C × ~50 ops ≈ 500k — fine as a one-time per-region plan, but note this runs *per region plan* and region plans are re-derived per chunk window in the streaming path (see 1.7): total cost scales as chunks × regions × C × T.
- Fix: keep an occupancy grid (region is 80 u at 0.8 u pitch = 100×100 bools) or interval list for O(1) overlap; precompute per-program size distributions so `place_suite`'s random draw doesn't re-run; at minimum replace `taken.iter().any` with a spatial hash. Also `score_placement`'s same-program scan could be incremental (only newest placement matters per program... actually needs min over all same-program rooms; a k-d tree or per-program list is fine).

### 1.5 region_plan/territories.rs — largest-free-rectangle carving (CONFIRMED, S8)
- **Severity: Low** — `free_territories` (territories.rs:74-141): n = (80-6.4)/0.8 ≈ 92 → 8464-cell boolean grid; initial fill O(n²·(S+T)) (territories.rs:99-111); then greedy loop `largest_free_rectangle` (territories.rs:147-190) — O(n²) per call with a per-row monotonic stack (allocates `Vec::new()` per row, :164, 92×64 ≈ 5.9k small allocs) — capped at 64 territories (territories.rs:136-138), so ≤ 64 × n² ≈ 540k ops. Bounded, fine.
- Fix (optional): reuse the `heights` and `stack` buffers across iterations; skip re-scanning rows with no free cells.

### 1.6 anomalies/macro_planner.rs + world_topology.rs — noise-eval amplification per region query (CONFIRMED, S9)
- **Severity: Medium** — `plan_macro_anomalies` (macro_planner.rs:264-326) expands the region query by MAX_QUERY_MARGIN=400 u (macro_planner.rs:22, :278) → an 80 u region query becomes 880 u → (880/160)² ≈ 11×11 = 121 macro anchors (:285-286), and **each anchor calls `sample_fields`** (:293-299) = 6 fields × 3 octaves = 18 `noise.evaluate_2d` calls (world_topology.rs:101-124 field(), :145-194 sample_fields). ≈ 2,178 noise evals per region query, recomputed for every region plan with zero memoization across overlapping queries (each anchor is re-evaluated by ~9 region queries within its 400 u radius). Per WorldBlock (324 regions) ≈ 700k noise evals; per streaming chunk window (4-9 region plans) ≈ 9-20k.
- Related: `red_room_event_for_cell` (world_topology.rs:232-264) runs a 3×3 tournament of `red_room_candidate` (:239-252), each = 1 `sample_fields` → 162 noise evals per cell; called from `plan_macro_cell` (:384) and, per region, through `red_room_event_for_region` (world_topology.rs:269-283) from region_plan/mod.rs:201-218 path (`vertical_link_for_region` :293-336 = 1 sample_fields) and corruption. `plan_macro_cell` (world_topology.rs:376-429) ≈ 18 + 162 + 4×(18+18) ≈ 324 noise evals per macro cell.
- Fix: memoize `sample_fields` per (seed, cell-of-anchor) in a bounded cache (the anchor lattice is 160 u — a HashMap keyed on anchor coords with LRU cap), or restructure queries to request anchor cells once per window and share the results across region plans in the same window (e.g. `WorldBlock` already shares plans across chunks — extend that to the anomaly lattice).
- Note: `sample_fields` is also called per region at region_plan/mod.rs:132 and per region in classify_region (world_topology.rs:357) — same memoization covers those.

### 1.7 Chunk-pipeline end-to-end Big-O + the RecursiveLevelWindow finding (CONFIRMED, S5/S11)
- **Severity: High (red-room worlds replan per chunk; any None-plans caller replans per chunk)** — `BackroomsLevel::generate_from_plans` (level_zero/generate.rs:294-409) is invoked per chunk from `GenerateChunkArchitectureUseCase::execute_from_plans` (generate_chunk.rs:363-431). Inside:
  - **level_zero/generate.rs:331** `RecursiveLevelWindow::around_chunk(...)` is built on **every chunk generation** (also :321-328: when `provided_plans` is None — the streaming path — `region_plans_for` → `InfiniteRegionWindow::around_chunk` (level_zero/generate.rs:267-275, infinite_level.rs:109-133) re-derives the full base region window per chunk). When a red room is committed, the recursive window additionally plans a second full `InfiniteRegionWindow` (red_rooms/recursive_level.rs:50-77, esp. :63) — up to 2×2 = 4 region plans per chunk window (a 10-20 u chunk + 1 u halo spans ≤ 2 regions per axis, infinite_level.rs:74-97), each a full `generate_region_plan` (region_plan/mod.rs:115-245: genome derivation, corridors, territories, lay_out_suites, corruption, vertical circulation, anomaly planning ≈ 2,200 noise evals per region when anomalies are enabled — see §1.6). No memoization between chunks: adjacent chunk windows overlap ~75-100% of their regions.
  - Net: streaming generation cost ≈ chunks × windowsize(≤4 regions) × region-plan cost when plans are derived per chunk (level_zero/generate.rs:321-329), and the recursive window replans ≤4 regions per chunk whenever a red room is committed (level_zero/generate.rs:331-339). The WorldBlock seam (world_block.rs:98-122, used by the browser via local_chunk_source.rs:204-218 and block_cache.rs) amortizes the BASE window — but NOT the recursive red-room window, which is rebuilt per chunk with no cache.
  - Per-column voxel loop: `plan_at` per column is O(1) (infinite_level.rs:137-155); `plan_column_in_reality` is O(C_corridors + A_anomalies + Asm_assemblies) per column with small constants (level_zero/compose_column.rs — out of scope but confirmed by prior agent); columns = 202×202 ≈ 40.8k (generate_chunk.rs:28) → O(width·depth × const) for voxelization (level_zero/generate.rs:373-409).
  - **Lighting bake runs per chunk** after generation (generate_chunk.rs:404-409): O(V) with 3 profiles, V = 202×202×242 ≈ 9.87M (generate_chunk.rs:38) → per chunk bake ≈ 3 BFS passes + 3 full-grid scans + write/project passes (bake_voxel_lighting/mod.rs:40-46). Time is fine; **transient memory is 3 × V × 4 B = ~118 MB** of `distance_steps` arrays (propagate_through_air.rs:13-17, :37) at the max budget — worth a Medium note for the Pi-class target (could be u16 with range ≤ 6 u / 0.05 = 120 steps).
  - Level 90 path: `legacy_blueprint::generate_legacy_blueprint` (generate_chunk.rs:423) — see §2.

### 1.8 anomalies/mod.rs — sort + dedup (CONFIRMED fine)
- `plan_anomalies_for_region` (anomalies/mod.rs:25-46): O(k log k) sort + dedup on instance id, k small. Fine.

### 1.9 bake_voxel_lighting propagation (CONFIRMED details)
- Per profile BFS is O(V): each air cell enqueued at most once (the `next_steps < distance_steps[neighbor]` guard at propagate_through_air.rs:66 combined with FIFO order makes re-enqueue with equal distance impossible). Seed scan `exposed_downward_air_cells` is O(V) (exposed_emissive_faces.rs:44-56). `project_onto_solid_receivers` O(V×3×6) (project_onto_solid_receivers.rs:17-49). `write_air_irradiance` O(V×3) (write_air_irradiance.rs:13-32). `clear_previous_bake` O(V) (clear_previous_bake.rs:5-10). Total O(V), ~12 passes over V.

## 2. Verified findings — remaining use cases

### 2.1 legacy_blueprint.rs — whole-chunk O(V) pipeline (CONFIRMED fine)
- `generate_legacy_blueprint` (legacy_blueprint.rs:184-934) is O(V + C log C): zone assignment O(C) with 1 noise eval/cell (:208-249); GrowingTree maze O(C) (:302, domain); room segmentation 15 × ≤9 cells O(C) (:310-355); corridor reservation amortized O(C) (shuffle :369; extensions :371-432 mark each cell at most once — breaks on room/corridor so no re-walk); voxel draw O(V) (floor/ceiling :455-493, walls :508-650, hall narrowing :686-743); room stamps O(rooms × room area) (:849-903); corridor lighting O(W×D/256) (:906-912); final lighting bake O(V) (:922). Determinism preserved by two independent RNG streams (:285-297) and resolution-independent draws (:858-862).
- No findings beyond style.

### 2.2 build_brick_pool.rs — linear fold, fine (CONFIRMED)
- `emit_into` (build_brick_pool.rs:197-252) visits each octree node once; bricks sample 64 voxels × O(depth≤2) (:254-277); `uniform_value` short-circuits all-air and uniform subtrees (:321-332). O(V + bricks×64). `BrickPool::get` O(depth). Fine.

### 2.3 build_octree_direct.rs / csg_voxelizer.rs — surface-tracking sampler build (CONFIRMED fine)
- `emit_cube` (build_octree_direct.rs:114-160) descends only where `uniform_hint` can't prove uniformity; `SolidSampler::uniform_hint` prunes outside the solid AABB (csg_voxelizer.rs:72-82) so cost tracks the authored solid's surface; `sample` is O(BSP depth) per voxel (csg_voxelizer.rs:58-70). Stack depth ≤ 8. Fine.

### 2.4 cellular_decay.rs — O(V) per step, wavefront worst case (CONFIRMED)
- `step_cellular_decay` (cellular_decay.rs:76-94) scans all voxels × 8-neighbor census per step; `changes` Vec O(V) transient. Transitions are monotone, but dampness cascades one ring per step, so an integrator running to quiescence is O(V × max(W,D)) worst case (each step advances the front ≤ 1 cell). No in-repo caller today (grep: none outside the module) — latent, Low.

### 2.5 menger_bsp.rs — bounded 3×3 recursion (CONFIRMED fine)
- `generate_recursive` (menger_bsp.rs:79-114): 3×3 subdivision, center cell corridor leaf, ≤8 recursing ring cells, depth ≤ max_depth (≤ 10) and span-limited by `subdividable` (:66-70). Tree ≤ 8^depth nodes (bounded small). Recursion depth ≤ 10 — no stack risk. Deterministic per world position. Fine.

### 2.6 grassland_level.rs — O(W×D) columns (CONFIRMED fine)
- Per column: 1 lake noise eval (grassland_level.rs:66) + ≤3 tree-hash evals (:88-93) + ≤18 vertical voxels (:103-111). ~40.8k columns/chunk → ~120k noise evals, plus the shared O(V) bake in generate_chunk.rs:409. Fine.

### 2.7 frame_quality_controller.rs — per-frame re-sort (CONFIRMED, S7)
- **Severity: Low** — `evaluate_and_cull_chunks` (frame_quality_controller.rs:179-233) rebuilds and re-sorts all candidates by distance every frame (:193-203), O(n log n) per frame over the candidate set (dozens to hundreds). Candidates barely change frame-to-frame; a small priority structure or carry-over of the previous order would remove the per-frame sort, but at this n it is negligible. `report_frame_time` O(1) (:113-135).

### 2.8 authoring.rs — unbounded session, O(E × volume) per bake (CONFIRMED, S10)
- **Severity: Medium (user-scale)** — `AuthoringSession.elements` grows without cap (authoring.rs:62-87); `apply_to_grid` (authoring.rs:186-215) re-walks the ENTIRE edit history per chunk bake — O(E × AABB_volume × BSP_depth) with no per-chunk delta or spatial index (authoring.rs:187). A long edit session makes every new chunk generation pay the full history; a wall crossing a seam is intentionally applied to both chunks (:416-437). Fix: spatial bucketing of elements by chunk + incremental dirty tracking; cap history or compact Add/Cut pairs.

### 2.9 world_block.rs — the amortization seam (CONFIRMED)
- `WorldBlock::load` (world_block.rs:98-122) plans a 16×16 block + 1-region halo = 324 region plans once; `preload_targets` O(1) (:162-204). The browser path uses it through `BlockCache` (bounded LRU, capacity 4, block_cache.rs:37, :96-131) — see §1.7 for the per-chunk path that does NOT.

### 2.10 red_rooms — no actual call-stack recursion (CONFIRMED)
- `RecursiveLevelWindow` (red_rooms/recursive_level.rs:42-154) is a *flat* translated window, not a recursive generator: the branch is addressed by integer translation + derived seed (:156-247) and every region plan is a pure function of that address. Chunk generation builds at most ONE window from the active red-room stamp (recursive_level.rs:179-185), so nested red rooms cannot recurse in the call stack. The cost finding is the per-chunk construction (§1.7), not unbounded recursion. `active_red_room` (recursive_level.rs:253-267) is O(stamps). `plan_red_rooms` (red_rooms/planning.rs:130-147) O(assemblies).

### 2.11 region_plan/corruption.rs — bounded (CONFIRMED)
- `corrupt` (corruption.rs:22-82): duplication sweep ≤ ~4 skews × ~40 offsets × `duplicate_suite_candidate` (clone O(assembly) + O(S) route check + O(T) overlap, :172-210) — bounded small. `realize_red_room_event` (:328-354) adds the 162-noise-eval red-room tournament (§1.6). `abandon_assembly` O(H×O) (:214-228). Fine.

### 2.12 region_plan/circulation.rs, genome.rs, vertical_circulation.rs — O(1)-ish (CONFIRMED fine)
- `build_corridors` (circulation.rs:25-102): a handful of spines, O(1). `derive_genome` (genome.rs:27-162): 1 noise eval. `place_stairwell` (vertical_circulation.rs:75-182): O(legs + taken + spines); `apply_stair_profile` O(1)/column (:186-230). Fine.

### 2.13 anomalies/geometry.rs — O(1) per column (CONFIRMED fine)
- `sample_anomaly` (geometry.rs:49-88) and each family sampler are O(1) per column; `sample_blackout_expanse` (:250-330+) delegates the substrate to `BackroomsLevel::column_plan_in_reality` (level_zero, out of scope; per-column O(C+A+Asm) with small constants per prior analysis). No per-column scans over instances.

## 3. Concurrency / multithreading audit

### 3.1 Concurrency inventory (shared mutable state + synchronization)

| Shared state | Location | Synchronization | Assessment |
|---|---|---|---|
| `device_lost: Arc<AtomicBool>` | wasm_frontend/src/drivers/webgpu/browser_context.rs:20,61-70,148-150 | store Release (:65) / load Acquire (:149) | Correct release/acquire; monotonic false→true; consumed at renderer.rs:196-198 before any frame work. No torn read, no lost update. The device-lost callback may fire from a wgpu internal thread; the atomic makes that safe. Low: no recovery path (lost device is terminal), acceptable for demo. |
| `DOOM_CONTROLS`, `INVERT_Y`, `ANOMALY_DEBUG`, `ASSISTED_CONSUMPTION` (AtomicBool), `MOUSE_SENSITIVITY_BITS`, `RENDER_SCALE_BITS`, `CPU_*_BITS`, `RENDER_TOGGLE_BITS` (AtomicU32) | wasm_frontend/src/lib.rs:32-122 | Relaxed single-word loads/stores (:60,66,72,78,90,103,202,218,230,235-241,266-272) | Safe (word-sized atomics on wasm32, single JS thread + workers are separate instances). **Low**: `store_cpu_settings` (:264-273) writes 7 atomics as a non-atomic group and `get_cpu_settings` (:234-254) reads them individually — a mid-frame update can yield one frame with a mixed snapshot (new scale + old radius); `.validated()` (:253) bounds the damage. Fix: a single `AtomicU64` settings version + snapshot struct, or accept as cosmetic. |
| `completed: Rc<RefCell<Vec<CompletedChunk>>>`, `failed: Rc<RefCell<Vec<ChunkRequest>>>` | worker_source.rs:38-39,60-61,93,101,108,126-127,231-233,259-260 | RefCell on the single JS thread; drained per tick via `poll_completed`/`poll_failed_requests` (:269-275) | No race (all callbacks and polls run on the main-thread event loop). **Low**: buffers grow until the engine polls each tick (engine.rs:828-840) — bounded by tick cadence. |
| `requests: Rc<RefCell<GenerationWorkerRequests>>` — per-worker `in_flight: VecDeque<ChunkRequest>` | worker_source.rs:40; generation_worker_requests.rs:14-17,76-80 | RefCell; `next_accepting` only checks `accepting_work` (:70-74) — **no per-worker queue cap** | **Medium**: `assign` (:79) pushes without bound. The engine's `issue_requests` caps TOTAL pending at `max(max_concurrent_requests, max_loads_per_tick*2, 4)` (engine.rs:928-932) so per-worker depth is ≈1 in practice, but the source itself has no defense if a caller ignores the cap; a wedged worker (e.g. a hung `worker_generate`) would accumulate an unbounded queue and `take_exact`'s O(Q) scan + `VecDeque::remove` shift (generation_worker_requests.rs:112-118) makes replies O(Q²) worst case. Fix: cap in-flight per worker (e.g. reject/fallback when depth > K) and fail the oldest on a timeout. |
| `worker.js` promise chain `queue` | static/worker.js:26,99-106 | serializes init + gen per worker | Correct ordering (init before gen, serial gen). **Low**: the chain retains every posted message until processed — unbounded if the main thread over-posts (again mitigated by engine.rs:928-932). Worker panics are caught per item (:103-105) and reported as `failed`/`fatal`; buffer transferred zero-copy (:82-96). |
| Worker protocol echoes | worker_source.rs:84-118,288-306; worker.js:13-15,37-51,82-96 | exact-request matching via `is_same_request` (request_id + coords + level + lod + artifacts + reality), engine.rs:828-837,858-864 | Correct stale-reply rejection: an old result cannot retire or install over a newer pending request; malformed replies fail the oldest serial request (generation_worker_requests.rs:100-102). `_request_id` ignored in wasm (lib.rs:1157) is fine — the envelope carries identity. |
| `thread_local! PLATE_CACHE` (menger_expanse.rs:220-236), `SHEET_CACHE` (level_zero/fabric_ca.rs:84+), `SOURCE` (lib.rs:1132-1143), `BAND` (lib.rs:1237+) | per-thread RefCell caches | thread-local | Correct: one per worker/main thread; no cross-thread sharing. PLATE_CACHE and SHEET_CACHE are capped (menger_expanse.rs:229-231 clears at >128). Worker `SOURCE` archive is budgeted at 32 MB (lib.rs:1110, with_budget → bounded clears at archived_chunk_source.rs:186-189). |
| `blocks: RefCell<BlockCache>` | local_chunk_source.rs:74,94; block_cache.rs:37,96-131 | RefCell, LRU capacity 4 | Bounded; LRU eviction correct (block_cache.rs:112-121). |
| `thread::spawn` per HTTP connection | src/main.rs:22-33,25 | none (each thread fully independent) | **Medium**: thread-per-connection with no pool or cap — a burst of `/maze` requests spawns unbounded OS threads, each allocating a dense VoxelGrid + running the full generate + bake pipeline (per-chunk bake transient up to 3×V×4 B, §1.9) — memory can spike to hundreds of MB with a handful of concurrent requests, and the listener accepts without backpressure. Fix: a small fixed worker pool (or `ThreadPool`/`std::thread::scope` with N workers + bounded queue) and/or request rate limiting; reuse one `GenerateChunkArchitectureUseCase` per worker. |
| `StdRng` instances (legacy_blueprint.rs:285-297; wfc3d.rs:136) | per-call, seed-derived | none shared | Deterministic per (seed,pos)/per solve; no shared RNG, no `static mut` anywhere in the repo (grep verified). LOD stability is preserved by separate detail stream (legacy_blueprint.rs:288-297). |
| No std Mutex/RwLock in scope; rayon only in dev-deps | — | — | The only real parallelism is: main.rs threads (independent), Web Workers (independent instances), wgpu callback (atomic flag). No lock-ordering/deadlock surface found. |

### 3.2 Worker lifecycle & message-ordering assessment
- Spawn: pool created once in `WorkerChunkSource::new` (worker_source.rs:53-155), bounded 1..=32 by policy (worker_source.rs:54, generation_worker_policy.rs:14,36-57). Init message posted before any gen (worker_source.rs:132-136; worker.js serializes).
- Termination: `terminate()` on fatal/error/postMessage-failure (worker_source.rs:109,128,261) with `disable_and_drain` (generation_worker_requests.rs:106-110) so abandoned requests are retried by the engine (engine.rs:828-837). Idempotent.
- Lost replies: every reply echoes the full request; stale/unknown replies are ignored without corrupting the ledger (generation_worker_requests.rs:84-95,112-118; tests :193-253). Fatal replies drain the worker.
- Backpressure: engine caps pending at source concurrency (engine.rs:928-932) and time-slices installation (engine.rs:841-913, `completed_backlog` deferral) — 32 workers never mean 32 atlas uploads in one frame. Per-worker cap missing (§3.1).
- Re-entrancy: all handler/poll/request code runs on the JS main thread; RefCells are never borrowed across await points inside a handler; `spawn_local` boot (lib.rs:1060-1074) doesn't touch worker state. rAF frame loop and worker messages interleave at tick granularity (poll_completed per tick), so no mid-install mutation.
- Determinism: worlds are pure functions of (seed, query); workers derive config from the same query (lib.rs:1113-1143, worker.js:18); reality is part of request identity. No RNG sharing.

### 3.3 Concurrency findings
- **C1 (Medium)** generation_worker_requests.rs:76-80 + worker_source.rs:211 — per-worker in-flight queue unbounded; add a per-worker cap (defense in depth).
- **C2 (Medium)** src/main.rs:25 — thread-per-connection, no pool/backpressure; each request runs the full pipeline (dense grid + 118 MB bake transient at finest budget, generate_chunk.rs:38); bound the pool.
- **C3 (Low)** wasm_frontend/src/lib.rs:264-273 — multi-atomic settings snapshot can mix old/new fields for one frame; pack into one AtomicU64 versioned snapshot.
- **C4 (Low)** worker_source.rs:329 — reply payload copied (`Uint8Array::to_vec`) instead of zero-copy slice into Rust; per-chunk payloads are small, but `decode_chunk_payload` re-copies again; acceptable.
- **C5 (Low)** worker.js:26,99-106 — promise-chain queue retains messages until processed; bounded in practice by engine.rs:928-932 but worth a comment/assert.
- **C6 (info)** browser_context.rs:62-70 — device_lost atomic is correct (Release store / Acquire load); no recovery path beyond renderer short-circuit (renderer.rs:196).

## 4. Big-O table (variables: V = dense voxels per chunk; W,D = width/depth in voxels; N = WFC cells; R = regions in window; C = placement candidates; T = taken rooms; S = corridors/spines; P = placed rooms; E = elements; A = anomaly anchors)

| Algorithm | Time | Space | Where |
|---|---|---|---|
| `solve_wfc` total (entropy scan per collapse) | O(N²) | O(N) | wfc3d.rs:144-147,160-180 |
| `solve_wfc` per collapse propagate (amortized; each cell restricted ≤63×) | O(N·64·6) amortized | O(N) worklist | wfc3d.rs:204-236 |
| `build_octree` (dense grid → SVO) | O(8^depth)=O(cube volume) | O(nodes) | build_octree.rs:85-182 |
| `build_octree_direct` + CSG sampler | O(surface × BSP depth) | O(nodes) | build_octree_direct.rs:114-160; csg_voxelizer.rs:58-82 |
| `compress_svdag` | O(V_nodes × 128B hash) | O(V_nodes) | compress_svdag.rs:90-122 |
| `build_brick_pool` | O(nodes + bricks×64) | O(nodes + bricks×128B) | build_brick_pool.rs:197-277 |
| `free_territories` (92×92 grid, ≤64 carves) | O(n²·(S+T) + 64·n²) | O(n²) | territories.rs:99-139,147-190 |
| `lay_out_suites` per region | O(C·(T+S+E·S+P)) ≈ 3k-9.5k × ~50 | O(T+P) | layout.rs:147-254; suites.rs:530-647 |
| `place_suite` | O(T+S+partitions²) | O(room) | suites.rs:530-647,448-484 |
| `generate_region_plan` (per region) | ≈ O(C·(T+S+P) + 800-1100 noise evals + 162 red-room evals) | O(plan) | region_plan/mod.rs:115-245 |
| `plan_macro_anomalies` per region query | O(A·18 noise evals), A≈121 anchors | O(instances) | macro_planner.rs:278-324 |
| `InfiniteRegionWindow::covering` | O(R × region-plan cost), R = window regions | O(R × plan) | infinite_level.rs:54-106 |
| Per-chunk level-0 voxelization (plans given) | O(W·D·(C+A+Asm)) + O(V) bake | O(V) + bake transients | level_zero/generate.rs:373-409; generate_chunk.rs:404-409 |
| Chunk pipeline streaming (no plans) | O(chunks × R × region-plan cost) — replans per chunk | per chunk | level_zero/generate.rs:321-329,331-339 |
| Lighting bake (per chunk) | O(V) (~12 passes incl. 3×BFS) | 3×V×4 B transient (~118 MB max) | bake_voxel_lighting/mod.rs:40-46; propagate_through_air.rs:36-77 |
| `generate_legacy_blueprint` | O(V + C log C) | O(V) | legacy_blueprint.rs:184-934 |
| `step_cellular_decay` per step / to quiescence | O(V·8) per step; O(V·max(W,D)) worst | O(V) changes | cellular_decay.rs:76-94 |
| `generate_menger_bsp` | O(8^depth) bounded (≤ 8^10 leaves max, span-limited) | O(tree) | menger_bsp.rs:79-114 |
| `grassland_level` | O(W·D) + bake O(V) | O(V) | grassland_level.rs:60-114 |
| `evaluate_and_cull_chunks` per frame | O(n log n) | O(n) | frame_quality_controller.rs:193-225 |
| `apply_to_grid` (authoring) | O(E × AABB_volume × BSP depth) per bake | O(E) | authoring.rs:186-215 |
| `WorkerChunkSource::request` + replies | O(1) amortized; `take_exact` O(Q), Q=per-worker in-flight | O(Q) | worker_source.rs:200-267; generation_worker_requests.rs:112-118 |
| `BlockCache::get_or_plan` | O(capacity) scan + plan on miss | O(capacity × block) | block_cache.rs:96-131 |

## 5. Highest-leverage optimizations (ranked)

1. **Amortize region planning across chunks (S5)** — level_zero/generate.rs:321-339: derive the base window once per block (WorldBlock + BlockCache already exist; the browser path uses them via local_chunk_source.rs:204-218) and **memoize `RecursiveLevelWindow` per red-room address** (recursive_level.rs:50-77) instead of rebuilding ≤4 region plans per chunk whenever a red room is committed. Expected: removes ~75-100% of planning work in red-room worlds; one `HashMap<(instance_id, seed), Rc<RecursiveLevelWindow>>` with LRU cap.
2. **WFC entropy heap (S1)** — wfc3d.rs:144-180: replace the per-iteration full-grid entropy scan with a priority queue / per-cell cached entropy updated only for cells touched by `propagate` (return the changed list). O(N²) → O(N log N); matters immediately if WFC grids grow beyond 9×9 (currently 81 cells, menger_expanse.rs:283).
3. **Memoize macro-field noise evaluations (S9)** — macro_planner.rs:293-299 + world_topology.rs:145-194: cache `sample_fields` per 160 u anchor (or per macro cell) across the region plans of one window/block; ~2,178 noise evals per region query × 324 regions per block ≈ 700k evals → ~1 per anchor. Bounded LRU keyed on (seed, cell).
4. **Bounded native server concurrency (C2)** — src/main.rs:25: fixed thread pool (e.g. min(hw_threads, 4)) + bounded queue; each request currently runs the full dense pipeline including the ~118 MB bake transient (generate_chunk.rs:38,404-409), so unbounded thread-per-connection is an OOM/DoS risk.
5. **Occupancy-pruned octree build (S2)** — build_octree.rs:85-182: short-circuit fully-empty interior subcubes (maintain a per-level occupancy bitmask or a coarse "any solid in cube" query) so sparse chunks stop paying full-cube recursion; for 202×202×242 grids in a 256³ cube this removes a large constant of leaf visits.
6. **Lighting-bake transient memory (S6)** — propagate_through_air.rs:37: store `distance_steps` as u16 (max range 6 u / 0.05 = 120 steps, world_space_attenuation.rs:18) instead of u32 → 59 MB → 20 MB at the max budget; also reuse one distance buffer across the 3 profiles by processing profiles sequentially and accumulating into the output pass.

## 6. Summary — severity-ranked findings

### Critical (correctness / data race / deadlock)
- None found. No `static mut`, no shared RNG, no lock-based sharing; the only cross-thread state is the `device_lost` atomic (correct Release/Acquire, browser_context.rs:61-70) and Web Worker message passing (exact-request echo matching prevents stale-result corruption, generation_worker_requests.rs:84-118 + engine.rs:858-864). Red-room "recursion" is flat addressing, not call-stack recursion (§2.10).

### High
1. **Per-chunk region-window re-planning** — level_zero/generate.rs:321-339: streaming callers using `None` plans re-derive the full region window per chunk, and the recursive red-room window (recursive_level.rs:50-77) is rebuilt on every chunk with no cache; each region plan costs ≈2,200 noise evals + layout solver (§1.6, §1.7). Fix: block-level planning (exists) + memoized recursive window.
2. **WFC O(N²) entropy scan** — wfc3d.rs:144-180 (latent: N=81 today at menger_expanse.rs:283, but the classic hotspot; §1.1).

### Medium
3. **Thread-per-connection native server** — src/main.rs:25: unbounded threads, each running the full dense pipeline with up to ~118 MB bake transients (generate_chunk.rs:38,404-409) → OOM/DoS under concurrency (C2).
4. **Per-worker in-flight queue unbounded** — generation_worker_requests.rs:76-80 (mitigated only by the engine's total-pending cap, engine.rs:928-932; C1).
5. **Macro-field noise amplification** — macro_planner.rs:278-324: ~121 anchors × 18 noise evals per region query, recomputed per overlapping query with no memoization (§1.6).
6. **Authoring bake re-walks full history** — authoring.rs:186-215 (§2.8).
7. **Lighting-bake transient memory** — 3 × V × u32 ≈ 118 MB at max budget, propagate_through_air.rs:37 (§1.9).

### Low
- layout.rs:221-228 taken-scan pattern (§1.4); territories.rs:119-139 bounded carves (§1.5); compress_svdag 128-byte keys + all-8-slot recursion (compress_svdag.rs:107-115); build_octree full-cube recursion without interior occupancy pruning (build_octree.rs:85-182); frame_quality_controller per-frame sort (frame_quality_controller.rs:193-203); multi-atomic settings snapshot (lib.rs:264-273, C3); worker.js promise-chain retention (worker.js:26,99-106, C5); reply buffer copy (worker_source.rs:329, C4); cellular_decay wavefront worst case (cellular_decay.rs:76-94); `vec![cell]` per collapse (wfc3d.rs:146).

### Not findings (verified fine)
- bake_voxel_lighting time is O(V) with each air cell enqueued ≤ once per profile (propagate_through_air.rs:66).
- legacy_blueprint O(V) with deterministic dual RNG streams.
- build_octree_direct/CSG tracks surface via AABB uniform hints.
- BlockCache is a correct bounded LRU (block_cache.rs:37,112-121); worker archive is budget-bounded (archived_chunk_source.rs:186-189; lib.rs:1110).
- Plan-purity: every planner is a pure function of (seed, world position); adjacent region/chunk/window queries agree by construction (tests at world_block.rs:360-403, infinite_level.rs:259-280).

---
*Report generated by static audit (read-only). All line numbers verified against the working tree. See §1-§3 for per-finding detail and §4 for the Big-O table.*
