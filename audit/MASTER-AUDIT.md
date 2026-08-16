# Vackrooms — Algorithm & Multithreading Audit (Static, Big-O)
**Scope:** entire repo (~80k lines): core `src/` (31.2k), `wasm_frontend/src` (46k), `wasm_raycaster`, tests, benches, examples, scripts, `static/worker.js`.
**Method:** pure static reading — **nothing compiled, built, run, or tested**. Eight partition audits (each with Big-O table, concurrency inventory, severity-ranked findings, file:line evidence) plus independent pattern cross-checks by the coordinator.
**Deliverables:** this master report + per-partition reports in `audit/partN-*.md`.

---

## 0. Executive summary

**Concurrency: this is a single-threaded codebase by construction.** Exactly one `std::thread::spawn` exists (src/main.rs — a per-connection dev-server thread), zero Mutex/RwLock/channel/rayon in production deps, no `static mut`, no shared RNG. The real concurrency surface is (a) browser **Web Workers** for chunk generation, (b) ~14 **atomic settings statics** crossing the JS↔wasm boundary, (c) JS event-loop re-entrancy (rAF vs. worker messages). Consequently: **no data races, deadlocks, or lock contention were found anywhere**. The atomics are all `Relaxed` on a single thread — correct today, but with documented latent hazards if threading is ever added (read-modify-write toggles, multi-atomic snapshot torn reads).

**Algorithms: the core generation pipeline is mostly linear** (O(V) per chunk), but there are seven high-leverage complexity problems:

1. **[CRITICAL — correctness] Cross-level archive staleness**: `RecordKey` has no `level` field while the archive identity (checked only at open) does — after a runtime level switch the worker serves the *previous level's geometry* for same coordinates (part 5, verified).
2. **[High] Per-frame all-lights shading everywhere**: GLSL/WGSL shaders and the CPU splatter loop over *all* scene lights per hit/splat, with per-light visibility marches of up to 32,768 steps × all resident chunks (parts 6/7).
3. **[High] Level-0 generation recomputes cell-constant invariants per column** (~4×10⁷ wasted ops/chunk) and regenerates entire assembly/corridor fixture layouts per column sample (part 2).
4. **[High] Per-chunk region-window re-planning**: `RecursiveLevelWindow::around_chunk` re-derives the full region window (≈2,200 noise evals + layout solver each) on every chunk with no cache (part 3).
5. **[High] Per-frame O(M·N) linear scans** in the streaming keep/evict filter (≈3.6M tuple compares/frame at R=16) plus a per-frame O(L log L) light sort with allocations (part 5).
6. **[High] Unbounded thread-per-connection dev server** that regenerates 5M-voxel chunks with no cache (fixed seed ⇒ identical output) and no read timeout (parts 4/8).
7. **[High] WFC O(N²) entropy scan** — latent (N=81 today) but the classic hotspot (part 3).

**Worker pipeline is sound on ordering** (exact request-echo matching, `max_pending` cap, per-worker serial promise chain) but **weak on failure recovery and burst handling**: no respawn/backoff, no request timeout/watchdog (a wedged worker permanently strands its affinity chunks), rejected-init cached forever, and worker replies are decoded on the main thread *before* the staleness check — unbudgeted burst decodes stall frames and stale payloads are decoded then dropped (parts 5/7/8).

---

## 1. Global concurrency inventory

| Shared state | Where | Synchronization | Assessment |
|---|---|---|---|
| Settings bitfields (14 atomics) | wasm_frontend/src/lib.rs (DOOM_CONTROLS:32 … RENDER_TOGGLE_BITS:191) | AtomicU32/Bool, all Relaxed; single-threaded wasm event loop | Correct today; A2/A3 latent hazards if threads ever appear (non-atomic RMW toggle, mixed-generation snapshots) |
| `device_lost` | webgpu/browser_context.rs:61–70 | Arc<AtomicBool>, Release/Acquire | Correct |
| Generation requests | worker_source.rs ↔ static/worker.js ↔ generation_worker_requests.rs | message passing; engine total-pending cap (engine.rs:928–932); exact-request echo matching (generation_worker_requests.rs:84–118) | Ordering safe; per-worker queue unbounded (no worker-side NAK/backpressure); F9 O(q²) drain (q≤32, Low) |
| Worker shard archive | archived_chunk_source.rs (per worker) | single-threaded per worker; budget-bounded (clear-on-overflow) | **Level-ambiguity bug (Critical)**; no cross-worker sharing |
| Worker pool ledger | generation_worker_policy.rs / worker_source.rs | single-threaded main thread | Disabled/failed workers never respawned (Medium) |
| HTTP server | src/main.rs:25 | thread-per-connection, zero shared state | Unbounded threads; no cache; no timeout (High) |
| RNG | everywhere | `StdRng::seed_from_u64`, deterministic dual streams | No sharing ⇒ deterministic by construction (verified) |
| Recursion | red_rooms, SVO, CSG BSP | flat addressing / bounded depth (≤16 in adapters; BSP worst-case P depth) | No stack-overflow risk found in hot paths |

## 2. Master findings — severity-ranked

### Critical (correctness)
| # | Finding | Evidence | Fix |
|---|---|---|---|
| C1 | **Cross-level archive staleness**: RecordKey {chunk, lod, artifacts, reality} omits `level`; ArchiveIdentity.level checked only at `open()`. Level switch at runtime ⇒ same-coordinate chunks fetch the previous level's geometry (hit path ignores level; the level-stamped request even passes the engine's completion check). | chunk_archive.rs:150–169, 259–279; archived_chunk_source.rs:99–105, 146–193; engine.rs:377–405, 797, 866; lib.rs:1114–1178 | Add `level` to RecordKey + record header, bump ARCHIVE_FORMAT_VERSION; or key archive per level |

### High (performance / resource)
| # | Finding | Evidence | Fix |
|---|---|---|---|
| H1 | **Per-frame all-lights shading** (all renderers): GLSL raymarch per-hit loops all chunk lights × 4 Gauss samples, each visibility eval marching up to 4096/32,768 steps × 25 chunks; WebGPU raster/splat/surfel pass `light_count` = all frame lights per vertex/fragment; CPU splatter Full-shadow does per-splat SVO occlusion without budget | part7 F1/F2/F5/F9/F10; part6 C2; shaders/trace_voxel_scene/shade_voxel_hit.rs; webgpu/pipelines/*.rs | Cluster lights per chunk (exists for WebGL2) + port to WebGPU; cap secondary-ray budget globally (Hero mode already has one); decouple light list from shadow pass |
| H2 | **Cell-constant invariant recomputation**: fabric invariants (band, ceiling, room_at, thickness) recomputed per column (40k columns/chunk ⇒ ~6×10⁵ evals); assembly fixture layouts regenerated per column sample (4×10⁷ ops/chunk); corridor fixtures regenerated per column | part2 F1–F4, F7–F10 | Memoize per (cell anchor); make FixtureLayout a true flyweight; two-pass generation |
| H3 | **Per-chunk region-window re-planning**: RecursiveLevelWindow::around_chunk per chunk (≈2,200 noise evals + layout solver per region plan); red-room window rebuilt per chunk | level_zero/generate.rs:321–339; recursive_level.rs:50–77 | Block-level planning + memoized recursive window |
| H4 | **Per-frame O(M·N) streaming scans**: keep is Vec; evicted filter, retain_keys (×2 passes), forced_reloads.retain, pending.retain ⇒ ≈3.6M tuple compares/frame at R=16, even idle | engine.rs:1019–1032, 916–917; streaming.rs:309–314 | HashSet<ChunkKey> keep |
| H5 | **Unbudgeted sync forced-reload loop**: RedRoom commit force-rebuilds every resident chunk; sync path loads all in one tick (0.4–1 s freeze on no-worker path) | engine.rs:1065–1090, 660–670 | Budget the forced loop per tick |
| H6 | **Thread-per-connection server + no cache**: unbounded threads; each request regenerates full 5M-voxel chunk (fixed seed 42) with no LRU; no read timeout; up to ~118 MB bake transients per request ⇒ OOM/DoS | src/main.rs:25–160; part4 §3.1–3.2, part8 F7 | Bounded pool + read timeout + LRU of serialized payloads |
| H7 | **WFC O(N²) entropy scan**: lowest_entropy_cell scans all N cells per collapse | wfc3d.rs:144–180 (N=81 today) | Heap/bucket of entropies (classic WFC optimization) |
| H8 | **Per-frame light selection**: O(L) HashMap dedup + O(L log L) sort + allocs every frame (L≈4k) | prepare_frame_lighting.rs:18–48; engine.rs:485–490 | Cache dedup; O(L) top-48 heap selection |
| H9 | **Hero light-visibility cache never invalidated on atlas edits**: stale shadows up to 4 frames after every chunk edit; hash collisions serve wrong receiver visibility | resolve_hero_light_visibility.rs:17–98; rasterizer.rs:590–601 | atlas_epoch folded into query key |
| H10 | **Flashlight per-splat occlusion ray, no cache** | flashlight.rs:158–204; shade_visible_splat.rs:97–109 | Cache per chunk+light; budget |
| H11 | **GLSL raymarch per-hit all-lights shading**: every hit × all chunk lights × 4 Gauss samples × all chunks × 768 steps ≈ 3M leaf lookups/pixel worst case (no per-chunk light cap) | shade_voxel_hit.rs:48–55; trace_direct_light_visibility.rs:25–45, 93; intersect_voxel_scene.rs:150, 206 | Per-chunk light cap + global ray budget (see H1) |
| H12 | **WebGPU raymarch**: loops all ≤52 frame lights per hit, each in-range light runs a 4096-step visibility trace | gpu_types.rs:105–110; raymarch.wgsl | Cluster lights; budget secondary rays |
| H13 | **WebGPU raster paths pass draw=[0, light_count] for EVERY chunk** — no clustering/range early-out; splat vertex stage ≈31M light evals/frame worst case | surface.rs:208; splat.rs:250; surfel.rs:223; splat_geometry.wgsl | Per-chunk light ranges (WebGL2 pattern) | | flashlight.rs:158–204; shade_visible_splat.rs:97–109 | Cache per chunk+light; budget |

### Medium (selection)
| # | Finding | Evidence | Fix |
|---|---|---|---|
| M1 | Worker never respawned/re-enabled after failure; `wasmReady` caches rejected init (retry storm) | worker_source.rs:104–130; static/worker.js:21–24 | Respawn with backoff; clear rejected promise |
| M2 | 32 workers × 32 MB archive budget ≈ 1 GB/tab worst case; budget doesn't shrink with pool | lib.rs:1110; generation_worker_policy.rs:20, 39–41 | Scale budget with pool size |
| M3 | Macro-field noise amplification: ~121 anchors × 18 noise evals per region query, no memoization | macro_planner.rs:278–324 | Memoize per region |
| M4 | Authoring bake re-walks full history O(E × volume) | authoring.rs:186–215 | Incremental bake |
| M5 | Lighting-bake transients 3×V×u32 ≈ 118 MB at max budget | propagate_through_air.rs:37 | Stream/blocked bake |
| M6 | SVG renderer O(W·D·(C·L + A·V₁)) distance/containment per cell | blueprint_renderer.rs:376–407, 539–554 | Rasterize corridor/assembly strokes once per render; lazy semantics for air cells |
| M7 | Voxel mesher: per-probe recompute + double trait-object dispatch in wasm hot path (~150M bounds-checked accesses / 5M-voxel chunk) | voxel_mapper.rs:231–240, 328–344, 351 | Precompute per-axis face-key planes; monomorphize |
| M8 | WebGPU shadow passes draw every resident chunk, no frustum/AABB culling (WebGL2 has it) | webgpu/pipelines/surface.rs:273–295; splat.rs:361–382 | Port AABB test |
| M9 | GLSL raymarch: no global secondary-ray budget; per-sample budget per-chunk 768–1536 steps | part7 F2 | Global budget accumulator |
| M10 | Deferred shading double-scans every splat footprint (probe + write pass) | rasterizer.rs:442–459; write_depth_tested_splats.rs:180–201, 245–312 | Fuse passes |
| M11 | CPU surfel: ~26 MB/frame allocations at 1080p | cpu_surfel.rs:198–200, 282 | Arena reuse |
| M12 | Cache keys without noise-provider identity ⇒ same-thread provider collision can serve wrong sheets | fabric_ca.rs:84–87, 267–277; menger_expanse.rs:220–237 | Provider id in key (latent today, flaky in tests) |
| M13 | Per-column fresh Vec + 3–5 linear anomaly scans + full assemblies scan | compose_column.rs:62, 113–234 | Single pass; fixed arrays; spatial buckets |
| M14 | SVG/chunk renderer scans full height without early exit; visited buffer re-allocated per slice | chunk_voxel_renderer.rs:263–294; voxel_mapper.rs:354 | Early break; reusable buffer |
| M15 | PNG: bit-by-bit CRC + per-byte adler32 modulos | png_writer.rs:76–103 | Table CRC; NMAX adler trick |
| M17 | **Worker replies decoded on main thread BEFORE staleness check, unbudgeted**: burst decodes stall frames; stale payloads decoded then dropped | worker_source.rs:288–299, 327–331 (vs metered install engine.rs:841) | Move staleness check before decode; budget/batch decodes |
| M18 | **No timeout/watchdog on generation requests**: wedged worker permanently strands its affinity chunks (pending blocks re-issue, affinity re-routes to same worker); pool terminates without respawn ⇒ 42 ms same-thread fallback | worker_source.rs:207–235; generation_worker_requests.rs:55–80; engine.rs:959 | Request timeout + worker watchdog + respawn with backoff |
| M19 | **WebGPU device-loss race**: check `is_device_lost()` then `queue.submit` can race the loss event and permanently freeze the rAF loop (no panic guard) | browser.rs:747–980 | Re-check after submit; handle loss in rAF catch |
| M20 | **Completion-burst ~100 MB backlog**; per-worker 32 MB archives scale pool to ~1 GB on 32 workers | part7 F16 | Meter completions per frame; shrink budget with pool |
| M21 | **GLSL raymarch has no per-chunk light cap** (chunks can exceed cluster-test cap) | part7 F8 | Cap lights/chunk in clustering |
| M22 | **GPU frame timer hardcoded off** in all three WebGL2 drivers | part7 F6 | Expose via settings |
| M16 | Test sweeps are O(n²) plan evals; `ascii_map.txt` cwd side effect | level_zero/tests.rs:1073–1147, 617–652 | Reduce radii; reuse window; gate dump |

### Low (selection, see part reports)
- compress_svdag 128-byte intern keys + all-8-slot recursion (compress_svdag.rs:107–115); build_octree full-cube recursion w/o interior pruning; layout.rs taken-scan O(C×|taken|); frame_quality_controller per-frame re-sort; O(q²) request-drain (q≤32); transferred-buffer `to_vec()` copy per chunk (worker_source.rs:329); `set_render_toggle` non-atomic RMW; mixed-generation settings snapshot; raycaster scalar byte stores (SIMD-blocked); worker.js unbounded queue absent engine cap; Rc<Sheet> caches block future rayon parallelization (F12); benches: duplicate arm, missing black_box, sample_size(20) noise; build-wasm hashes tree twice; MAX_GENERATION_WORKERS duplicated JS/Rust; worker.js `bytes.buffer` transfer relies on wasm-bindgen glue copy semantics (verified safe at 0.2.126, fragile to upgrades — F17); path-tracer bake O(V·R·S) ≈ 500M iterations (one-time); SVO set_recursive never clears mask on air write; SVO collapse leaves dead arena nodes (unbounded growth under edit); BSP worst-case O(P²) + depth P; RealitySnapshot clone per gate; generate_samples discards 100 region plans; collision_diagnostics regenerates identical chunk 3×.

### Verified NOT findings (clean)
- Lighting bake propagation O(V), each air cell enqueued ≤ once; legacy_blueprint O(V) deterministic dual RNG; BlockCache bounded LRU; plan purity (all planners pure functions of (seed, pos)) with cross-query consistency tests; worker promise-chain serialization prevents stale-result races; red_rooms "recursion" is flat addressing; `collect_emissive_lights` O(V) amortized (≤2.5 tests/cell — prior quadratic suspicion disproven by simulation); recursion depths bounded (≤16); all 30 atomics correct as written.

## 3. Master Big-O table (key algorithms)

| Algorithm | Complexity | Where | Notes |
|---|---|---|---|
| Chunk generation pipeline (dense) | O(V) + invariants | generate_chunk.rs; level_zero | V≈5M voxels; constant factor dominated by H2 |
| WFC solve | O(N²) | wfc3d.rs:144–180 | N=81 today; classic hotspot |
| SVO/SVDAG build | O(V) build; O(8^depth) worst recursion | build_octree.rs, compress_svdag.rs | intern map keys 128 B |
| SVO ray march (CPU splatter) | O(C·B) per ray; B≤32,768 steps | raycast.rs:127–146, 444–492 | per-chunk slab + budget |
| Splat shading (Full shadows) | O(S·L·G·C·B) worst | shade_visible_splat.rs; evaluate_fixture_irradiance.rs | unbudgeted except Hero |
| Shader shading (GLSL/WGSL) | O(pixels · L · steps · C) | shade_voxel_hit.rs; raster_surface_radiance | H1 |
| Streaming keep/evict | O(M·N) per frame | engine.rs:1019–1032 | M≈N≈1089 ⇒ 3.6M compares |
| Light selection | O(L log L) per frame | prepare_frame_lighting.rs | L≈4k |
| Region plan derivation | ≈2,200 noise evals + layout solver per region | region_plan/**; world_topology | re-derived per chunk (H3) |
| Region layout (suites) | O(C·(T+S+E·S+P)), C≈rounds×program×legs×anchors×2 | layout.rs:221–228 | bounded, once per region |
| Assembly fixture layout | O(zone-bay² × 40) per query | fixture_plan.rs:71–171 | regenerated per column (H2) |
| wall_host_near raster | 4,489 probes × full column plan | provisions.rs:122–182 | High const factor (part2 F3) |
| Hero light visibility | 65,536-entry direct map, retrace 4 frames | resolve_hero_light_visibility.rs | stale on atlas edits (H9) |
| Greedy mesher | O(V) ×6 dirs, double dispatch | voxel_mapper.rs | M7 |
| PNG encode | O(pixels) slow constant | png_writer.rs | M15 |
| Maze gen (Growing Tree) | O(cells) amortized | generate_maze.rs | per-cell Vec alloc (F7 micro) |
| CSG BSP | O(P²) worst, depth P | csg/bsp.rs | fragment blow-up (part1 F-2) |
| Path-tracer bake | O(V·R·S) ≈ 1,920 probes/solid voxel | path_tracer.rs | one-time bake |
| Raycaster (legacy) | O(W·H·steps) per column | wasm_raycaster/lib.rs | SIMD-blocked stores (part8 F6) |

## 4. Top cross-cutting recommendations (ranked)

1. **Fix C1** (RecordKey level) — one-key correctness bug; blocks safe level switching.
2. **Kill the all-lights loops** (H1): bake/cluster per-chunk light lists, port WebGL2 clustering to WebGPU, add a global shadow-ray budget (pattern exists in Hero mode).
3. **Memoize generation invariants** (H2+H3): cell-anchor memo tables + true fixture flyweights + cached region windows. Expected: 10–100× on per-chunk generation constant factors.
4. **O(1) keep-set streaming** (H4) + cache light selection (H8): removes ~4M compares + sort/alloc churn per frame.
5. **Harden the worker pipeline** (M1/M2 + part8 F1/F2/F11): respawn w/ backoff, worker-side NAK/backpressure, scale archive budget with pool, single source of truth for MAX_GENERATION_WORKERS.
6. **Bound the dev server** (H6): pool + timeout + LRU; also fixes silent 1024-byte read truncation.
7. **Parallelization path**: production deps have zero rayon; core generation is embarrassingly parallel across chunks but Rc-caches (F12) block it — thread-local caches + per-chunk RNG seeds get there.

## 5. Part report index
| Report | Coverage | Status |
|---|---|---|
| part1-core-domain.md | domain entities (SVO/SVDAG, CSG, maze, path tracer, probes) | ✅ |
| part2-core-gen0.md | level_zero + level_one generation (fabric CA, menger, provisions, fixtures) | ✅ |
| part3-core-gen-rest.md | WFC, region plan, red rooms, anomalies, lighting bake, chunk pipeline | ✅ |
| part4-core-adapters.md | adapters (renderers, serializers, noise), main.rs server thread | ✅ |
| part5-fe-application.md | engine loop, streaming, archive, worker policy, all 30 atomics | ✅ |
| part6-fe-adapters.md | cpu_splatter, raycast, surfels, caches | ✅ |
| part7-fe-drivers.md | drivers, shaders, worker protocol, bin/play | ✅ (17 findings F1–F17) |
| part8-peripherals.md | raycaster, benches, examples, scripts, worker.js | ✅ |

*Generated by static analysis only — no compilation or tests were performed.*
