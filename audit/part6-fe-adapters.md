# Part 6 — Frontend Adapters: Algorithm & Concurrency Audit

Repo: /home/dbslim/vackrooms · Scope: `wasm_frontend/src/adapters/**` (55 files)
Method: static file reading only (no compile/run). Line numbers verified against the
current working tree (seeds from two prior attempts cite slightly older revisions;
notable deltas are listed in the appendix).

---

## 1. Confirmed findings (from prior attempts, re-verified against current HEAD)

### C1 — HERO LIGHT-VISIBILITY CACHE NEVER INVALIDATED ON ATLAS EDITS (High)
- `resolve_hero_light_visibility.rs:57-98` — direct-mapped cache (`CACHE_ENTRIES = 65_536`, line 17; `RETRACE_PERIOD_FRAMES = 4`, line 18). Entry is keyed only by `hash_receiver_and_light(center, world_size, hero.id)` (line 70) and never by atlas content.
- `rasterizer.rs:590-594` (`upload_atlas`) and `rasterizer.rs:596-601` (`upload_atlas_rows`) mutate `self.atlas`/`self.mips` but never touch `hero_light_visibility`. Grep confirms the cache is only read in `resolve_hero_light_visibility` and advanced per frame (`render_cpu_frame.rs:97`); there is no epoch/invalidation path anywhere in adapters.
- Consequence: after a chunk edit (streaming update via `upload_atlas_rows`), receivers in the edited chunk keep the stale binary visibility (0.0/1.0) for up to `RETRACE_PERIOD_FRAMES = 4` frames (visibility is re-traced only when `(key + frame_index) % 4 == 0`, line 75-76). Because the key is a hash of position+size+light, *any* hash collision between two receivers also serves one receiver the other's visibility until its slot is re-traced — a worst-case 4-frame wrong shadow even without edits.
- Fix: bump an `atlas_epoch: u64` in `upload_atlas`/`upload_atlas_rows` and fold it into `query_key` (or clear `valid` flags on change). Folding into the key is O(1) and keeps the direct-mapped layout.

### C2 — PER-SPLAT SVO OCCLUSION QUERIES ARE UNBUDGETED IN `Full` SHADOW MODE (High)
- `shading/shade_visible_splat.rs:89-95` — `DirectLightVisibility::trace_scene(atlas, chunks, ...)` is constructed for **every shaded splat** when `settings.shadows == CpuShadowMode::Full`; there is no ray budget gate on this path (only the `Hero` mode gets a per-chunk budget: `rasterizer.rs:417-418`, `SHADOW_PIXEL_BUDGET` at `resolve_hero_light_visibility.rs:26`).
- `shading/trace_direct_light_visibility.rs:44-73` — `sample_is_visible` calls `svo_segment_is_occluded` (line 72) once per point light and once per 2x2 Gauss sample of each rectangle light (`shading/evaluate_fixture_irradiance.rs:159-203`, visibility call at line 192; point-light visibility at lines 229-231).
- `raycast.rs:444-492` — `svo_segment_is_occluded` slab-tests **every resident chunk** (line 455) and marches each crossed chunk up to `traversal_step_budget` steps (`raycast.rs:127-146`, 160..32_768).
- Worst case: S splats × L lights × G (1 point / 4 rectangle) × C chunks × B steps. At `Maximum` preset (defaults to `Full`, `settings/quality_preset.rs:28`) the frame budget allows >150k node visits and multi-megapixel write allowances — at 1920×1080 at least one full coverage (2.07M writes) and up to the 16M ceiling (`rasterizer/frame_work_budget.rs:20-22` and its tests), so this can exceed 10^9 march steps/frame.
- Mitigations already present (good): `rectangle_can_contribute` (evaluate_fixture_irradiance.rs:17-60) and `point_light_irradiance` range checks (111-142) skip zero-contribution lights; `select_lights_for_chunk` (select_lights_for_chunk.rs:15-25) pre-filters per chunk; `svo_segment_is_occluded` early-exits on first hit (line 482). These reduce the constant but not the O(S·L·G·C·B) shape.
- Fix: (a) give `Full` mode the same per-chunk ray budget as Hero; (b) replace the O(C) chunk loop with a spatial bucket (uniform grid keyed by chunk cell) so a segment only slab-tests chunks its AABB crosses; (c) amortize via a hero-style (receiver,light) cache.

### C3 — FLASHLIGHT OCCLUSION RAY PER SPLAT PER FRAME (High)
- `cpu_splatter/flashlight.rs:158-182` `segment_is_occluded` → `svo_segment_is_occluded` (line 181), invoked from `beam_contribution` (186-204) which `shade_visible_splat.rs:97-109` calls for every splat when `cam.flashlight && settings.flashlight_visibility() == TraceScene`.
- Same O(S·C·B) shape as C2 with no caching at all — the beam segment is re-traced for every splat of every frame, even though the lamp moves only with the camera and receivers repeat frame to frame.
- Fix: cache visibility per (receiver center quantized, receiver size, frame) like the hero cache, or trace one beam ray per coarse screen tile and reuse for the tile's splats.

### C4 — RAYMARCH BUDGET SEMANTICS (verified, OK)
- `raycast.rs:127-146` budget 160..32_768; used at 268. `ChunkMarch::Indeterminate` (budget exhausted / corrupt atlas) is treated as *occluded* by visibility queries (`raycast.rs:486`), i.e. conservative against light leaks. `trace_svo` returns hits only within `max_t` (lines 424-428). No issue — budget cannot hang a frame.

### C5 — FIXED-STACK RAYMARCH TRAVERSAL IS SAFE FOR REAL SVO DEPTHS (verified, OK)
- `raycast.rs:148-197` `TraversalCursor` holds `[TraversalFrame; 9]`; `push` silently drops when `stack_ptr >= 8` (line 164). Real SVO depth is 6 (low spec) / 8 (high spec) (`src/use_cases/generate_chunk.rs:288-291`, tests at 450-453), so 8 pushes fit. If a future config exceeds depth 8, a dropped push only causes redundant re-walking or a conservative `Clear` at chunk exit (`pop_exited`, 178-196) — never wrong geometry. Worth a comment, not a fix.

### C6 — PER-FRAME CHUNK SORT + VEC ALLOCATION (Medium)
- `rasterizer/render_cpu_frame.rs:33-34` → `chunks_in_draw_order` (111-122): allocates a fresh `Vec<&ChunkDraw>` (line 115) and sorts O(C log C) (118) every frame, even when the camera and chunk set are unchanged and `front_to_back` is off (sort skipped only when the toggle is off).
- Fix: keep a reusable scratch `Vec<&ChunkDraw>` in `SoftwareRasterizer` (like `chunk_light_scratch`, rasterizer.rs:96); skip the sort when the camera moved < epsilon or the chunk set is unchanged.

### C7 — `select_lights_for_chunk` O(C·L) PER FRAME, REPEATED PER BAND (Medium)
- `select_lights_for_chunk.rs:15-25` (clear + extend + O(L) filter), called from `render_cpu_frame.rs:74-77` inside the chunk loop (56) and the band loop (55). Production band count is 1 (`set_band_count` is only exercised by tests: rasterizer.rs:165-171, tests.rs:943), so today O(C·L); with N bands it becomes O(N·C·L). A chunk cluster with hundreds of emissive panels (e.g. the 64×64 grid in collect_emissive_lights tests yields 500+ lights) makes L large.
- Fix: build a light spatial index (or per-chunk light lists) once per light-set change, or hash lights into chunk cells.

### C8 — DEFERRED-SHADING DOUBLE FOOTPRINT SCAN (Medium)
- With `deferred_shading` on, each splat first runs `surface_splat_may_contribute` (rasterizer.rs:442-459 → `write_depth_tested_splats.rs:180-201` `request_may_contribute`, full footprint scan, early-out on first passing pixel) and then `write_splat` (245-312, second full scan). A splat that passes HZ but fails fine depth entirely pays 2× footprint scans (up to ~(2·half_px+2)² pixels; half_px ≤ 32 at the fallback cap, write_depth_tested_splats.rs:321-340).
- Fix: fuse the probe into the write (probe row-by-row and shade only passing rows), or have `request_may_contribute` return the first passing (row,x) to seed `write_splat`'s scan.

### C9 — PER-FRAME PLAN/AREA ALLOCATIONS (Medium)
- `rasterizer/frame_work_plan.rs`: `build` (65-100) allocates `areas` (74, C×Z usize), `chunk_areas` (77), `pixel_writes` (82), `node_visits` (83), `shadow_rays` (84) every frame. Z = height/8 ≈ 135 at 1080p; C ≈ 100-200 chunks → ~0.2-0.5 MB of zeroed/scrubbed allocations per frame (`chunk_zone_areas` 143-196 is O(C·Z) with one projected footprint per chunk).
- The renderer already reuses the important hot scratch (`chunk_light_scratch` rasterizer.rs:96, `mip_update_scratch` rasterizer.rs:97-ish, `zone_budgets` 204); the plan is the remaining per-frame allocation.
- Fix: cache the plan keyed by (camera pos hash, chunk list identity, settings, width/height) and rebuild only when those change; or reuse a `FrameWorkPlan` buffer with clear+refill.

### C10 — CPU SURFEL RENDERER ALLOCATES ~40 MB PER FRAME (Medium)
- `cpu_surfel.rs:188-282` `render_surfels`: `depth` (198, f32), `accum` (199, [f32;3]), `weight` (200, f32) = 20 B/px, plus `rgb` (282, 3 B/px) — at 1080p ≈ 26 MB zeroed/INF-filled per call, discarded on return. `draw_ellipse` (364-432) scans the full bbox of every disc with no tile-level early-out; `min_radius_px` (379-380) and the degenerate-det fallback (392-393, 404-406) are handled, but occluded discs still scan their whole bbox.
- Fix: hold the four buffers in a persistent renderer struct and clear only touched regions; add a coarse 8-px-tile z-buffer to reject fully-occluded ellipse bboxes before the per-pixel loop.

### C11 — `trace_svo` ALLOCATES + SORTS PER CALL (Low, API trap)
- `raycast.rs:350-433`: `Vec::with_capacity(chunks.len())` (363) + sort O(C log C) (389). Only reachable from tests and the public re-export (`cpu_splatter/mod.rs:41`); the hot visibility path deliberately uses `svo_segment_is_occluded` instead (doc comment 435-443). Keep as-is, but consider marking it `#[cfg(test)]`-only or documenting the cost on the public export.

### C12 — `collect_emissive_lights`: VERIFIED O(V) AMORTIZED — the "quadratic staircase" does not reproduce (Low / correction)
- `collect_emissive_lights.rs:90-118` outer scan + `collect_owned_rectangle` (128-157). The z-run `(start_x..max_x).all(...)` (148-150) re-scans rows that fail, but `rectangle_cell_matches` (151-157) rejects already-`visited` cells, so overlapping rectangles fail on the first cell (cheap), and every expensive failing-row scan is charged to the start-row cells that the same rectangle marks. A bit-exact Python simulation of the algorithm over adversarial hole patterns (diagonal, anti-diagonal, mid-column, alternating, checker `(x+z)%3`, 10%/25% random) measured **1.0-2.5 cell tests per marked cell**, i.e. linear in V, for N = 16..512.
- Residual costs: one `visited: Vec<bool>` of full halo volume per chunk load (line 97), and `sort_unstable_by_key` + `dedup_by` over the light list (116-117). All one-time per chunk generation; fine.
- (The task brief listed a confirmed quadratic "staircase of holes" in surface-exposure propagation. That does not reproduce in this file; the exposure propagation itself — `surface_exposure.rs` + `propagate_exposure_to_octant.rs` — is fixed-size O(6) per octant and O(n) in MIP build. I flag this as a *correction* of the brief, with simulation evidence in the appendix.)

### C13 — ARCHIVE BUDGET ENFORCEMENT CLEARS THE WHOLE ARCHIVE (Low/Medium, design trade)
- `archived_chunk_source.rs:141-148` (`fetch`): when `archive.bytes() + encoded.len() > max_bytes`, `archive.clear(storage)` empties **everything**, then stores the new chunk. A long walk through an area larger than the budget repeatedly clears and re-writes; `clears()` (70-72) exists to detect the thrash. Documented trade for an append-only log (module doc 1-19, chunk_archive docs), but the alternative — evicting oldest records or sharding by region — would avoid O(total) destructive clears.
- Also note: decode of a corrupt record falls through to regeneration (`fetch` 82-90), which is correct.

### C14 — CODEDEC: LINEAR, DEFENSIVE (verified, OK)
- `chunk_codec.rs`: encode single-pass with pre-sized `Vec` (10-24) — worst case one amortized realloc; decode fully bounds every count against remaining bytes before allocating (`Reader::len`, 333-345) so a hostile/truncated buffer cannot force a huge allocation; all field reads are `Option`-checked. O(N) both directions. No finding.

### C15 — RECURSION DEPTHS ALL BOUNDED (verified, OK)
- Per-frame traversal: `traverse_voxel_scene/mod.rs:99-156` + `visit_children` (406-426) recurse at most real SVO depth (6-8, see C5) + `max_virtual_depth` (≤8, `settings/quality_preset.rs` 25-32; capped in `select_voxel_detail.rs:17-22`) ≤ 16 frames.
- `build_mips`/`compute_mip` recursion (`atlas.rs:182-260`) bounded by SVO depth, memoized (`done`), cycle-safe (marked before descend, 186-191).
- `extract_collision_boxes::walk` (`local_chunk_source.rs:282-320`) bounded by SVO depth ≤ 8.
- No stack-overflow risk anywhere.

### C16 — BLOCK CACHE: BOUNDED LRU, DELIBERATE LINEAR SCAN (verified, OK)
- `block_cache.rs:65-71` `index_of` O(capacity=4) linear scan (documented), eviction `min_by_key` O(4) (99-104), `swap_remove` O(1) (105), `clock` u64 (109-110). Capacity fixed at 4 (`DEFAULT_BLOCK_CAPACITY`, 20-28). No unbounded growth, no leak. `preload_targets` never accidentally plans (`local_chunk_source.rs:83-101` returns the coord instead of planning).

### C17 — MIP/A TLAS ROW UPDATE: O(row), NO REALLOC (verified, OK)
- `update_atlas_rows.rs:24-39`: validate (42-70), `copy_from_slice` (35), `rebuild_mips_in_node_range` (`atlas.rs:158-180`, O(range) with reused `done_scratch`). Independence validation (75-93) rejects cross-slot pointers before mutation. `upload_atlas` full path (`rasterizer.rs:590-594`) is O(N) copy + O(N) `build_mips` (atlas.rs:144-152) — acceptable once per upload.

### C18 — RASTERIZER INNER LOOPS: SIMD LANES, NO O(n²) IN THE FILL (verified, OK)
- `write_depth_tested_splats.rs:245-312` `write_splat` is per-footprint-pixel with 4-wide batched depth tests (`simd_row_ops.rs`), scalar writes; `clear` (147-161) is O(P) per frame; `record_first_coverage` (419-428) scans a tile once at full coverage; `coarse_rect_occludes` (360-396) is O(tiles) with early-out; `band_zone_budgets_spent` (rasterizer.rs:430-437) short-circuits on the first unspent zone (O(1) while budgets remain). No accidental quadratic in the hot path.


---

## 2. Big-O table

Variables: S = shaded splats/frame, L = scene lights, G = visibility samples per light
(1 point / 4 rectangle), C = resident chunks, Z = framebuffer zones (height/8), P = pixels,
N = atlas nodes, V = voxel grid volume, Q = merged quads, B = raymarch step budget (160..32,768).

| Algorithm | Complexity | Where | Notes |
|---|---|---|---|
| Per-frame octree traversal (CPU splatter) | O(S) splats, O(nodes) visits ≤ node budget (≤8M ceiling) | traverse_voxel_scene/mod.rs:99-156, frame_work_budget.rs:20-22 | Budgeted; recursion depth ≤16 |
| Splat fill (write_splat) | O(footprint px) per splat, ~O(P) total | write_depth_tested_splats.rs:245-312 | 4-wide SIMD depth tests, scalar writes |
| Deferred pre-check + write | 2× O(footprint px) per splat | rasterizer.rs:442-459 + write_depth_tested_splats.rs:180-201,245-312 | C8 |
| HZ coarse query | O(tiles in rect), early-out | write_depth_tested_splats.rs:360-396 | |
| Hero visibility (cached) | O(1) amortized per splat; trace O(C·B) every 4th frame, budgeted | resolve_hero_light_visibility.rs:57-98; rasterizer.rs:417-418 | C1 staleness |
| Full shadow mode | O(S·L·G·C·B) | shade_visible_splat.rs:89-95, evaluate_fixture_irradiance.rs:159-203, raycast.rs:444-492 | C2, unbudgeted |
| Flashlight occlusion | O(S·C·B) | flashlight.rs:158-204, shade_visible_splat.rs:97-109 | C3, uncached |
| svo_segment_is_occluded | O(C) slab tests + O(B) march per crossed chunk | raycast.rs:444-492 | linear chunk scan per query (C2/C3 multiplier) |
| trace_svo | O(C log C) + alloc | raycast.rs:350-433 | test-only API |
| Chunk order per frame | O(C log C) + O(C) alloc | render_cpu_frame.rs:33-34,111-122 | C6 |
| Frame work plan | O(C·Z) time, O(C·Z) alloc/frame | frame_work_plan.rs:65-100,143-196 | C9 |
| select_lights_for_chunk | O(C·L) per frame (× bands) | render_cpu_frame.rs:74-77; select_lights_for_chunk.rs:15-25 | C7 |
| Atlas upload / row update | O(N) / O(row) | rasterizer.rs:590-601; update_atlas_rows.rs:24-39; atlas.rs:144-152,158-180 | C17 |
| build_mips | O(N) memoized, depth ≤ SVO depth | atlas.rs:144-152,182-260 | |
| collect_emissive_lights | O(V) amortized (≤ ~2.5 tests/cell) | collect_emissive_lights.rs:90-157 | C12 (quadratic claim disproven) |
| Surface exposure propagation | O(6) per octant, O(N) total | surface_geometry/propagate_exposure_to_octant.rs, atlas.rs:240-241 | fixed-size |
| Chunk generation payload | O(V + Q) (+ core gen/SVDAG) | local_chunk_source.rs:170-235 | one-time per chunk |
| Collision extraction | O(solid leaves) recursion, depth ≤8 | local_chunk_source.rs:282-320 | |
| face instances / surface mesh / surfel cloud | O(Q log Q) / O(V+Q) / O(surfels) | face_instances.rs:38-52; surface_mesh.rs:62-117; surfel_cloud.rs:88-108 | per chunk build |
| Codec encode/decode | O(N) | chunk_codec.rs:10-330 | C14 |
| BlockCache get_or_plan | O(capacity=4) | block_cache.rs:65-71,87-113 | bounded LRU |
| Archive fetch | O(record) + O(total) on budget clear | archived_chunk_source.rs:76-105,141-148 | C13 |
| section_locator::describe | O(plan) + String alloc per call; plan cached per region | section_locator.rs:62-104 | HUD-only |
| render_surfels | O(S_surf · bbox px) + O(P) buffers | cpu_surfel.rs:188-296,364-432 | C10 |
| query_config | O(query len), one-time | query_config.rs:57-120 | |
| input handling | O(events), O(1) per event | input.rs:38-191 | |

---

## 3. Concurrency inventory (adapters scope)

The adapters layer is **single-threaded by construction**: no `thread::spawn`, no atomics,
no `Mutex`, no channels anywhere under `wasm_frontend/src/adapters/` (grep-verified). The
only synchronization primitive is `RefCell` interior mutability, used to keep `&self` ports
with memo/archive state:

| Shared state | Sync mechanism | Where | Risk |
|---|---|---|---|
| `LocalChunkSource.blocks: RefCell<BlockCache>` | runtime borrow check | local_chunk_source.rs:41-46,157-166 | Re-entrancy would panic (none today: generator never calls back into the source). Not `Sync`, so each worker must own its source — matches the "pool shards the world" design note (archived_chunk_source.rs:18-19). |
| `ArchivedChunkSource.archive: RefCell<ChunkArchive>` | runtime borrow check | archived_chunk_source.rs:24-27,77-80,100-102 | Borrow order is consistent (storage immutable → archive mutable; then both mutable sequentially). No overlap, no deadlock possible. |
| `ArchivedChunkSource.storage: RefCell<Box<dyn ArchiveStorage>>` | runtime borrow check | archived_chunk_source.rs:25,77,100-101 | Same as above; a `Storage` impl that re-entered `fetch` would panic — driver impls (`MemoryStorage`/`FileStorage`) have no callbacks. |
| `ArchivedChunkSource.clears: RefCell<u64>` | runtime borrow check | archived_chunk_source.rs:26,70-72,147 | Trivial counter. |
| Worker↔main chunk transport | ownership transfer (encode → postMessage → decode) | chunk_codec.rs (whole file) | Codec is pure and defensive; the encoded `Vec<u8>` is the only cross-thread artifact and it is fully owned per call. No shared buffers, no memory-ordering concerns inside adapters. |
| Atomic statics (`DOOM_CONTROLS`, `MOUSE_SENSITIVITY_BITS`, `INVERT_Y`) | `Ordering::Relaxed` loads | input.rs:40,103-104 (loads only; statics live in wasm_frontend/src/lib.rs) | Relaxed is fine for toggles/settings (single writer, eventual visibility); no torn reads on aligned atomics. Out of adapters' ownership. |
| RNG / determinism | per-source `SimpleNoiseProvider`, no shared RNG, no `static mut`/`thread_local` | section_locator.rs:19-21; block_cache.rs; local_chunk_source.rs:31 | Deterministic; main thread and workers derive the same world from the same seed+query (query_config.rs:25-28). The hero cache's frame-to-frame dependence is deterministic (same inputs → same output). |

Conclusion: no data races, deadlocks, or lost updates are possible inside adapters. The
concurrency risks are upstream (worker pool lifecycle, unbounded request queues, rAF vs.
worker message races) and live in `worker_source.rs` / `static/worker.js` /
`generation_worker_*.rs`, outside this scope. The one adapter-side hazard class is
`RefCell` re-entrancy panics if a storage/provider impl ever calls back into its source;
worth a `debug_assert` or a comment at each `borrow_mut` site.

---

## 4. Highest-leverage optimizations (ranked)

1. **Chunk spatial index for visibility queries** (raycast.rs:455; kills the shared
   O(C) multiplier in C2 and C3). Bucket resident `ChunkDraw`s by coarse world-grid cell
   (uniform grid or `HashMap<(i64,i64,i64), SmallVec<idx>>`); march only cells the segment
   crosses (Amanatides & Woo). Turns each `svo_segment_is_occluded` from O(C·B) into
   O(crossed_chunks·B), usually O(B) for a short segment. ~10-100× on visibility-heavy frames.
2. **Budget and cache `Full` shadow mode** (C2): apply the existing per-chunk
   `shadow_ray_budget` mechanism (rasterizer.rs:417-418) to `Full` mode and add a
   (receiver,light) sample cache analogous to `HeroLightVisibilityCache`
   (resolve_hero_light_visibility.rs:17-98). Prevents the 10^9-step worst case and makes
   the `Maximum` preset's reference mode interactive.
3. **Cache the flashlight beam visibility per tile/frame** (C3): one beam ray per coarse
   screen tile (or a keyed cache like the hero one) instead of one per splat. Flashlight-on
   frames drop from O(S·C·B) to O(tiles·C·B) + O(S) lookups.
4. **Invalidate the hero cache on atlas edits** (C1): fold an `atlas_epoch` into
   `query_key` in `upload_atlas`/`upload_atlas_rows` (rasterizer.rs:590-601). One line of
   state, removes stale-shadow artifacts up to 4 frames after every streamed edit.
5. **Reuse the frame work plan + chunk order buffers** (C6, C9): cache
   `FrameWorkPlan`/`ordered_chunks` keyed by (camera, chunk-set, settings, size) identity;
   saves ~0.2-0.5 MB of per-frame allocation plus the O(C log C) sort when nothing moved.
6. **Fuse the deferred-shading probe and write** (C8): single footprint pass per splat
   (probe rows, shade only passing rows). Also apply the same reuse pattern to
   `render_surfels` buffers (C10: ~26 MB/frame at 1080p → persistent buffers + coarse
   tile z-cull).

---

## 5. Appendix — verification notes

- Line-number deltas vs. the task brief / seeds: `upload_atlas`/`upload_atlas_rows` are at
  `rasterizer.rs:590/596` in the current tree (brief said 504/510, an older revision).
  `resolve_hero_light_visibility` now carries a per-chunk `shadow_ray_budget`
  (rasterizer.rs:417-418) that the seeds' copy did not have; the budget gates only `Hero`
  mode (C2 stands).
- C12 simulation evidence: bit-exact re-implementation of `collect_emissive_lights`'s
  rectangle-growth loop; worst adversarial pattern found was a `(x+z)%3==0` hole checker at
  ~2.25 tests/marked cell and ~1.5 tests/V, flat from N=64 to N=512 (no superlinear trend).
- Scope note: `worker_source.rs`, `static/worker.js`, `generation_worker_policy.rs`,
  `generation_worker_requests.rs`, and the atomic statics in `wasm_frontend/src/lib.rs` are
  listed in the brief as the concurrency surface but are outside the adapters file set; they
  were not re-audited here beyond the inventory notes above.
