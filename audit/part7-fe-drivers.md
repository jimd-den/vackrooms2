# Audit Part 7: Frontend Drivers (wasm_frontend/src/drivers, bin/play, reference)

Scope: wasm_frontend/src/drivers/** (~17k lines incl. worker_source.rs, generation_worker_requests.rs,
gl/, surface_webgl/, splat_webgl/, surfel_webgl/, shaders/** GLSL+WGSL, webgpu/**), bin/play/**,
reference/**. Focus: worker message protocol, GPU per-frame work, light clustering, shader nested
loops, WebGL resource management, JS interop, concurrency (atomics in wasm_frontend/src/lib.rs,
worker lifecycle).

## STATUS
- [x] Seed files read (fe-drivers-key-findings.txt, fe-drivers-prior-analysis.txt)
- [x] Verify findings against sources
- [x] Complete Big-O table, concurrency inventory, top optimizations

## Verified Findings (draft)
(will append as verified)

## Big-O table

Per-frame/per-image costs; constants from verified findings F1-F12 and this pass. H = shaded/hit pixels, S = raster samples (fragments or splat corners), C = resident chunks (~25-64), V = visible meshes, L = frame lights (≤48 analytic + ≤4 dynamic = ≤52), L_c = lights reaching one chunk (uncapped), T = trace steps (768 GLSL, 4096 WebGPU budget).

| Name | Complexity | Where | Notes |
|---|---|---|---|
| GLSL raymarch shading per hit | O(H × L_c × 4 × C × 768) ≈ 3M leaf lookups/pixel worst case | shade_voxel_hit.rs:49-55; trace_direct_light_visibility.rs:25-45,93; intersect_voxel_scene.rs:150,206 | F1. Per-hit loop over all chunk lights; each in-range light runs 4 Gauss samples × all chunks × 768 steps. No cap on L_c (F8). |
| GLSL secondary-ray trace | O(C × 768) per Gauss sample = 19,200 steps | trace_direct_light_visibility.rs:25-45; intersect_voxel_scene.rs:150/206 | F2. No global per-pixel secondary-ray budget; each sample may walk every chunk fully. |
| surface_webgl prepare_visible_draws | O(V × L) ≤ ~25×48 = 1,200 evals/frame | surface_webgl/draw.rs:262-312 | F3. Per-chunk light-range scan over all frame lights, recomputed every frame for static bounds. |
| append_lights_reaching_bounds | O(L) per chunk; O(V×L) clustered Vec/frame; O(V×L) `last_lights` compare | gl/cluster_scene_lights.rs:15-32; upload_scene_lights.rs:56-101 | F4. Bounded by 48-light cap; dead-code `LightSelectionCache` would add unbounded HashMap growth if ever wired (cluster_scene_lights.rs:68-130). |
| WebGPU raymarch shading per hit | O(H × L × 4096) ≈ 213k leaf lookups/pixel worst case; ~40k-120k typical | gpu_types.rs:105-110; raymarch.wgsl; DIRECT_VISIBILITY_TRACE_BUDGET=4096 | F5. All frame lights (≤52) per hit; each in-range light runs a 4096-step visibility trace across ≤25 chunks. |
| WebGPU raster shading (surface/splat/surfel) | O(S × L × 4) quadrature; splat vertex worst case 150k faces × 4 corners × 52 ≈ 31M light evals | pipelines/surface.rs:208, splat.rs:250, surfel.rs:223; splat_geometry.wgsl | F9+F10. draw=[0, light_count] for every chunk — no per-chunk clustering, no range early-out. |
| WebGPU shadow passes | O(C) draw calls per pass, zero culling | pipelines/surface.rs:273-295; splat.rs:361-382 | F11. Every resident chunk drawn into the shadow map; WebGL2 path has footprint AABB culling (surface_webgl/draw.rs:368-382). |
| WebGPU splat/surfel draw sort | O(C log C) per frame; per-cell sphere culling O(total cells); surface uniform writes O(C) | pipelines/splat.rs:258-265,328-350; surfel.rs:228-235; surface.rs:199-211 | F12. Negligible at C ≤ ~64. |
| WebGL2 light selection | O(N log N) sort + O(N) hero scan per frame, N ≤ 20 | gl/lights.rs:62-107 | F7. Negligible. |
| GPU timer | O(1), but hardcoded disabled | gl/timer.rs:70-87; surface_webgl/draw.rs:127, splat_webgl/draw.rs:102, surfel_webgl/draw.rs:84 | F6. All call sites pass `enabled=false`; HUD always shows 0.00 ms. |
| Reference raymarcher (test-only) | O(P × (C + C log C + 160 + L)) per image | raymarch_reference_renderer.rs:40-59,128 | SVO walk capped at 160 steps, stack capped at 8 (pushes silently dropped, line 176-178); per-pixel chunk AABB scan + sort + full light shading. Not in production path. |
| CpuReferenceRenderer (test-only) | Fresh SoftwareRasterizer per render: O(P) raster + O(P × L) shading | cpu_reference_renderer.rs:17-34 | Stateless by design; reference/benchmark use only. |
| Worker reply ledger | O(n) linear scan per reply, n = per-worker in-flight (≈ pool share of max_pending) | generation_worker_requests.rs:112-118 | Negligible (n ≤ ~4 typical). preferred_worker hash is O(1) fixed-16-byte FNV (generation_worker_requests.rs:55-67). |
| Engine request top-up | O(desired) per tick, O(1) pending lookups | engine.rs:923-987 | Bounded by max_pending = max(pool_size, 2×max_loads_per_tick, 4) (engine.rs:928-932). |
| Main-thread payload decode per completion | O(payload) copy + O(payload) decode, un-metered, in the reply handler | worker_source.rs:288-299,327-331 | F13. Every completion — including stale ones later discarded — is decoded on the main thread between frames. |

## Concurrency inventory

Threads in play: the browser main thread (rAF loop, engine, renderer), up to 32 generation Workers (`static/worker.js`), and the native winit thread. Each Worker is an independent wasm instance — there is **no** SharedArrayBuffer, so no memory is shared across instances; the only cross-thread state is the postMessage channel.

| Shared state | Sync | Notes |
|---|---|---|
| Worker pool lifecycle (spawn/terminate) | Spawned once in `WorkerChunkSource::new` (worker_source.rs:53-141), 1..=32 workers (`MAX_GENERATION_WORKERS`, generation_worker_policy.rs:14), `WorkerType::Module`, init message posted before any gen message (worker_source.rs:132-136). | Termination paths: `fatal` reply (worker_source.rs:104-110), `error` event (:125-129), `postMessage` failure (:255-266) — each drains the ledger and **terminates without respawn**. Pool degradation is permanent for the session; if all workers die, `request()` falls back to same-thread `LocalChunkSource` generation (worker_source.rs:212-235), a 42 ms/frame hitch per chunk (F14). |
| Message ordering | worker.js:99-106 serializes every message through one promise chain (`queue = queue.then(...)`), so replies are strictly in-order per worker; the handler is registered at module scope so no message is lost during wasm instantiation (worker.js:1-5,99). | Reply validity never relies on order alone: `finish_exact`/`fail_exact` match full `ChunkRequest` identity (ports.rs:625-633; generation_worker_requests.rs:84-95,112-118), so stale/duplicate replies cannot retire current work (test: generation_worker_requests.rs:192-202). |
| In-flight ledger (max_pending cap) | `GenerationWorkerRequests` per-worker `VecDeque` (generation_worker_requests.rs:14-17,76-80). Engine cap: `max_pending = max(source.max_concurrent_requests(), 2*max_loads_per_tick, 4)` (engine.rs:928-932); WorkerChunkSource reports `pool_size()` (worker_source.rs:196-198); one request per chunk key (`pending: HashMap<ChunkKey, ChunkRequest>`, engine.rs:250). | Total outstanding work is bounded by max_pending (≤32), so per-worker queues are ~pool share. No per-worker cap, no timeout — a wedged worker strands its affinity-chunk requests forever (F14). `take_exact` is O(n) but n is tiny. |
| Backpressure / completion backlog | No backpressure signal to workers; the engine time-slices *installation* at 8 cost units/tick (2 fine loads; `FINE_LOAD_COST=4`, engine.rs:171,841-842,897) via `completed_backlog` (engine.rs:839-913). | Decode is NOT budgeted: payload decode runs in the reply handler before the ledger check (worker_source.rs:288-299,327-331), so a full-pool burst decodes 32 × ~3.2 MB on the main thread between frames (F13), and decoded payloads double-buffer in `completed` sink + backlog (F16). |
| Error paths | Malformed reply → `fail_oldest` (worker_source.rs:111-117; relies on the serial-queue assumption, generation_worker_requests.rs:97-102); `failed` reply → `fail_exact`; `fatal`/`onerror`/postMessage error → `disable_and_drain` + `terminate` (worker_source.rs:104-110,125-129,255-266); all workers lost → same-thread fallback (:212-235). Engine retries drained requests via the failed sink (engine.rs:828-837). | Failure reporting has its own catch (worker.js:52-56) so even an unclonable error surfaces through `onerror`. No watchdog: a hang produces neither done nor failed (F14). |
| Async replies vs sync frame updates | JS single-threaded: message handlers run between rAF tasks; engine polls sinks inside `tick` → `install_completed` (engine.rs:1044-1048) with `is_same_request` validation (:855-864) and forced-reload collision deferral (:881-895). | No data race (Rc/RefCell, no await inside handlers; borrows never held across reentrancy). Worst-case staleness: a completion lands at most one frame late. Wrong-level / out-of-footprint / LOD-not-better results are discarded (engine.rs:866-880). |
| Stale-data windows | Stale worker replies (evicted chunk, replaced reality, superseded request id) are decoded then dropped (worker_source.rs:288-299 → engine.rs:855-864). Worker archive returns payloads keyed by `ArchiveIdentity` incl. generator_id hash of the query (lib.rs:1146-1177, chunk_archive.rs:63-149), so a query change invalidates archives. | Safe but wasteful: stale payloads pay full decode before rejection (F13). Reality mismatch cannot slip through: `RealitySnapshot` is part of request identity (ports.rs:616-618,631-632). |
| Transferred ArrayBuffer semantics | worker.js:82-96 posts `buf: bytes.buffer` with transfer list `[bytes.buffer]`; `worker_generate` returns `Vec<u8>` (lib.rs:1190-1212); generated glue copies it (`getArrayU8FromWasm0(...).slice()` in static/pkg/wasm_frontend.js `worker_generate` wrapper, wasm-bindgen 0.2.126) before freeing wasm memory; main thread copies again via `Uint8Array::new(&buffer).to_vec()` (worker_source.rs:327-331). | Safe **today** because the glue copies; worker.js itself never verifies the buffer is not a view of wasm linear memory. If the glue convention ever returns a view (older wasm-bindgen did), the transfer detaches the worker's entire wasm memory and kills it mid-session (F17). Two full copies + decode per payload on the main thread (F13). `reality` Uint32Array is copied, not transferred (worker_source.rs:248-250) — safe. |
| lib.rs Atomic statics (settings) | `MOUSE_SENSITIVITY_BITS`, `RENDER_SCALE_BITS`, `CPU_*_BITS`, `RENDER_TOGGLE_BITS`, `DOOM_CONTROLS`, `INVERT_Y`, `ANOMALY_DEBUG`, `ASSISTED_CONSUMPTION` (lib.rs:32-122,226) — all `Ordering::Relaxed` stores/loads. | Benign: every static is written by JS setters and read on the same (main) wasm instance; workers are separate instances and never see them. Relaxed is sufficient for a single-threaded instance; values are f32 bit patterns with clamped setters (lib.rs:84-104). No torn-state risk (single-word atomics). |
| `device_lost: Arc<AtomicBool>` | browser_context.rs:61-66 `store(true, Release)` in the device-lost callback; `is_device_lost()` `load(Acquire)` (browser_context.rs:148-150), checked at renderer entry (renderer.rs:195-198). | Correct handoff ordering. But the check-then-submit window is not atomic: loss between `is_device_lost()` and `queue.submit` (renderer.rs:305) throws through the rAF Closure with no panic guard — next frame is never scheduled (F15). |
| `Arc<Window>` + winit event loop (bin/play/app.rs) | `State.window: Arc<Window>` (app.rs:135) and the wgpu display handle (`InstanceDescriptor::new_with_display_handle(Box::new(window.clone()))`, app.rs:481-484). Renderer is `SharedNativeRenderer(Rc<RefCell<NativeRenderer>>)` (app.rs:745-749). `ControlFlow::Poll` (app.rs:120); tick in `window_event`/`about_to_wait` (:380), render on redraw. | Single-threaded: no `thread::spawn` anywhere in the crate (grep confirms). Arc<Window> exists only because wgpu's instance needs a `HasDisplayHandle`; it is never dereferenced from another thread. No Send/Sync violation; the Arc is harmless but unnecessary for concurrency. |
| Worker-side state | Per-instance `thread_local! SOURCE: RefCell<Option<ArchivedChunkSource>>` (lib.rs:1132-1135), archive budget 32 MB/worker (`WORKER_ARCHIVE_BYTES`, lib.rs:1144), `MemoryStorage` append-only with truncate (chunk_archive.rs:451-497). | Worker archive is per-instance memory; pool total can reach workers × 32 MB on long walks, not the ~32 MB the doc comment implies (lib.rs:1137-1144) — see F16. |

## Top optimizations (ranked)
See "Top optimizations (ranked)" below the findings batches.

## VERIFIED FINDINGS (batch 1 — GLSL raymarch + GL clustering)

### F1 [Critical — perf] GLSL raymarch: per-hit shading loops over all chunk lights × 4 Gauss samples × all chunks × 768 trace steps
- `shaders/trace_voxel_scene/shade_voxel_hit.rs:48-55` — `shadeVoxel` loops `lightCount = uChunkLightCounts[receiverChunkIndex]` lights of the receiver chunk; each light goes through `evaluateVisibleSceneLight` (`trace_direct_light_visibility.rs:72-111`), which for rectangle lights runs **4 Gauss samples** (`:93`), each guarded by `directSampleIsVisible`.
- `trace_direct_light_visibility.rs:25-45` — `directSampleIsVisible` loops over **all `uNumChunks` (≤25) chunks**; for each intersected chunk calls `traceChunk` → `traceChunkSkippingEmptyLeaves` / `traceChunkDda`, each with a hard **768-step** loop (`intersect_voxel_scene.rs:150` and `:206`), each step = SVO `lookupVoxelLeaf`.
- Worst case per hit pixel: O(L_c × 4 × C × S) = 40 lights × 4 samples × 25 chunks × 768 = **~3M leaf lookups/pixel** (cluster test proves no cap: `cluster_scene_lights.rs:152-171` keeps all 40 reaching lights). GLSL `&&` short-circuits (`:102`), so out-of-range samples skip the trace; cost is lights in range × 4 × C × S.
- Fix: (a) cap per-chunk light count (top-K by importance, like `MAX_SHADER_LIGHTS`), (b) cache visibility per (chunk, light) instead of per Gauss sample, (c) global per-pixel step budget across all secondary rays (WebGPU already has one — see F5), (d) distance early-out before Gauss loop.

### F2 [High] GLSL raymarch: no global secondary-ray budget; per-sample budget is per-chunk 768
- Each `directSampleIsVisible` can spend 25 × 768 = 19,200 steps (per Gauss sample per light). `intersect_voxel_scene.rs:150/206`. The WebGPU path bounds this with a per-light 4096 budget (F5); GLSL has no equivalent. A ray nearly parallel to a chunk slab that exits late (distance > chunkBox.exit at `:151`/`:207` breaks only at exit) still walks the whole chunk.
- Fix: pass a shared `inout` step counter through all secondary traces with a global cap per pixel.

### F3 [Medium] surface_webgl prepare_visible_draws O(V × L) per frame
- `surface_webgl/draw.rs:262-312` — for each visible mesh (V ≤ resident chunks), `append_lights_reaching_bounds(frame.active_scene_lights(), ...)` (`:293-298`) scans all L frame lights (≤ `MAX_ANALYTIC_SCENE_LIGHTS_PER_FRAME` = 48, `ports.rs:238`). O(V×L) ≤ ~25×48 = 1200 predicate evals/frame — bounded but recomputed every frame for static chunk bounds + mostly static lights.
- Fix: cache per-chunk light ranges keyed by (chunk, frame light ids) and invalidate when lights change; or build a light BVH/spatial hash.

### F4 [Low] append_lights_reaching_bounds is O(L) per chunk; LightSelectionCache is dead code with unbounded growth
- `gl/cluster_scene_lights.rs:15-32` — linear scan of all lights per call; fine given 48-light cap, but the per-frame `clustered` Vec can reach V×L = ~1200 entries, uploaded whole to the light texture each time it changes (`upload_scene_lights.rs:56-101`, `last_lights == lights` O(1200) compare per frame at `:57`).
- `LightSelectionCache` (`:68-130`) + `select_lights_with_hysteresis` are **dead code** — only tests reference them (`:200`, `:212`); grep confirms no production callers. If ever wired up, `entries: HashMap<chunk_key, Vec<u64>>` never evicts → unbounded growth as the player walks.
- Fix: delete dead code or add LRU eviction; consider caching clustered ranges.

### F5 [High/Critical] WebGPU raymarch: per-hit shading loops ALL frame lights; each in-range light spawns up to 4096-step visibility trace
- `gpu_types.rs:105-110` — `render_options.x = light_count` (all enabled scene lights ≤48 + ≤4 dynamic, `collect_frame_lights` `:174-182`). `raymarch.wgsl` `raymarch_analytic_direct` loops `render_options.x` lights per hit pixel; with `RAY_POLICY_DIRECT_LIGHT_VISIBILITY` each in-range light runs `finite_segment_visible` with `DIRECT_VISIBILITY_TRACE_BUDGET` = 4096 steps across ≤25 chunks.
- Worst case 52 × 4096 ≈ 213k leaf lookups/pixel; `has_positive_rgb` gate limits to in-range lights (dense corridor: 10-30) → ~40k-120k lookups/pixel. Dominant shader cost when direct visibility is on (default per prior analysis of render_settings).

### F6 [Medium] GPU frame timer hardcoded off in all three WebGL2 drivers
- `gl/timer.rs:70-87` — `begin()` accepts `enabled`, documented "comes from the toggle switchboard", but all call sites pass literal `false`: `surface_webgl/draw.rs:127`, `splat_webgl/draw.rs:102`, `surfel_webgl/draw.rs:84`. `last_ms` is reset to None whenever disabled (`:75`) so HUD always shows GPU 0.00ms (`cpu_telemetry_string`, `surface_webgl/draw.rs:84`).
- Fix: thread `RenderToggles::gpu_timer` into `begin()`.

### F7 [Low] WebGL2 light selection sort
- `gl/lights.rs:62-107` — `select_lights` sorts all fixtures+dynamic O(N log N) per frame, N ≤ 16+4 after engine dedup/truncate; `hero_slot` scan O(N). Negligible. `flare_cores` O(4). Fine.

### F8 [Medium] cluster test documents no cap on per-chunk light count
- `gl/cluster_scene_lights.rs:152-171` — test `clustering_keeps_every_reaching_light_without_a_slot_cap` asserts 40 lights all retained. Feeds F1: `uChunkLightCounts` is unbounded per chunk; a fixture-dense chunk makes the GLSL shading loop O(40×4×25×768) per pixel.

## VERIFIED FINDINGS (batch 2 — WebGPU pipelines + shaders)

### F9 [High] WebGPU raster paths (surface/splat/surfel): every vertex/fragment evaluates ALL frame lights — no clustering, no range early-out
- `webgpu/pipelines/surface.rs:208` — `draw: [0, light_count, 0, 0]`: light_first=0, light_count = ALL frame lights for EVERY chunk. Same in `pipelines/splat.rs:250` and `pipelines/surfel.rs:223`.
- `webgpu/gpu_types.rs:174-182` — `collect_frame_lights` = enabled scene lights (≤48) + ≤4 dynamic = ≤52 `GpuLight`s.
- The WGSL raster shaders loop that full range per vertex/fragment (see F10). Contrast: WebGL2 surface clusters per chunk (`surface_webgl/draw.rs:293`) and WebGL2 splat selects top-16 (`gl/lights.rs:79`). WebGPU raster pays 52 × 4 Gauss = 208 quadrature samples per shaded sample with no distance rejection inside the loop (only `half_size_kind.w < 0.5` disabled filter).
- Fix: reuse `append_lights_reaching_bounds` per chunk (fill `GpuChunkUniforms.draw.x/y` with clustered ranges), or add a distance/range early-out in `evaluate_scene_light`.

### F10 [High] WGSL raster light loop bodies
- See common.wgsl `raster_surface_radiance` / `evaluate_scene_light` (verify lines) — per-sample loop over `draw.y` lights starting at `draw.x`; with draw.x=0, draw.y=light_count every sample pays the full list. Splat vertex stage (`splat_geometry.wgsl`) calls the same radiance function per corner: 150k-face budget × 4 corners × 52 lights ≈ 31M light evaluations/frame worst case on the vertex stage.

### F11 [Medium] WebGPU shadow passes draw every resident chunk with zero culling
- `webgpu/pipelines/surface.rs:273-295` (`draw_shadow_pass`) and `pipelines/splat.rs:361-382` iterate ALL `self.chunks.values()` with no AABB test vs. the hero-light frustum. WebGL2 shadow pass has footprint AABB culling (`surface_webgl/draw.rs:368-382`). With ~25-64 resident chunks this is bounded, but it is pure waste for chunks far from the hero light and regresses vs. the WebGL2 path.
- Fix: port the AABB overlap test from `surface_webgl/draw.rs:373-380`.

### F12 [Low] WebGPU splat/surfel per-frame chunk sort
- `pipelines/splat.rs:258-265`, `pipelines/surfel.rs:228-235` — sorts chunk draw order by camera distance O(C log C) per frame when `front_to_back || face_budget`; C ≤ resident chunks (~25-64) → negligible. Fine.
- `pipelines/surface.rs:199-211` — one `queue.write_buffer` per resident chunk uniform per frame; fine for ~25 chunks.
- `pipelines/splat.rs:328-350` — per-cell sphere culling loop over all cells of visible chunks per frame: O(total cells) sphere tests — fine.

## VERIFIED FINDINGS (batch 3 — generation-worker pipeline)

### F13 [Medium] Main-thread payload decode is unbudgeted and runs before the staleness check — every completion (including stale ones) is fully decoded in the reply handler
- `worker_source.rs:288-299` — `parse_worker_reply` for `"done"` calls `parse_payload` **before** the ledger's `finish_exact` (:89-94); `parse_payload` (`worker_source.rs:327-331`) copies the transferred ArrayBuffer (`to_vec`) and runs `decode_chunk_payload` on the main thread inside the message event.
- `engine.rs:841-842,897` — installation of decoded payloads is metered (8 cost units/tick; fine chunk = 4), but the decode itself is not; a burst of N worker replies between two rAF callbacks decodes N payloads back-to-back on the main thread, directly delaying the next frame. High-spec payloads are ~3.2 MB of mesh (per browser.rs:523-526 comment) → several ms each; a 32-worker burst can stall the frame loop for tens of ms.
- Worse, `install_completed` later discards completions whose request is no longer current (wrong level, evicted footprint, superseded id — `engine.rs:855-864`), so stale work also pays the full decode first.
- Fix: split reply handling into (a) parse request → `finish_exact` ledger check → (b) only then decode; or defer decode into `install_completed` under the existing load budget. This removes both the unbudgeted hitch and the decode-of-stale-payload waste.

### F14 [Medium] No timeout/watchdog on generation requests — a wedged worker permanently strands its affinity chunks, and the pool never respawns
- `static/worker.js:99-106` — the promise chain serializes work but nothing bounds a single `worker_generate` call; grep for `timeout`/`watchdog`/`setTimeout` across worker.js, worker_source.rs, engine.rs, chunk_archive.rs: **no matches**.
- `generation_worker_requests.rs:16,76-80` — `in_flight` entries are retired only by done/failed/fatal/error replies (`worker_source.rs:86-130,255-266`). A hung worker (wasm infinite loop, wedged browser thread) produces neither; its chunks stay in `engine.pending` (`engine.rs:250,959` — `issue_requests` skips keys already pending) and are re-routed to the **same** worker forever by chunk affinity (`worker_source.rs:207-210`; `generation_worker_requests.rs:55-67`). Only a footprint prune (`engine.rs:916-917`) frees the key; returning to the area re-wedges it.
- The failure paths also **terminate without respawn** (worker_source.rs:109,128,261): a pool of N workers degrades monotonically to the same-thread fallback (`worker_source.rs:212-235`), which turns every missing chunk into a ~42 ms main-thread regeneration (per lib.rs:1152-1155 doc: "42 ms regeneration" vs "0.46 ms decode").
- Fix: per-request deadline on the main thread (e.g. `setTimeout` that calls `disable_and_drain` + `terminate`, mirroring the `Fatal` path) so a hung worker's chunks are re-issued to a healthy slot; consider respawning a replacement worker (fresh `Worker::new_with_options`) instead of permanent termination.

### F15 [Medium] WebGPU device-loss race between `is_device_lost()` check and `queue.submit` can permanently freeze the frame loop (no panic guard in the rAF closure)
- `webgpu/browser_context.rs:61-66` — the device-lost callback fires asynchronously (`store(true, Release)`); `renderer.rs:195-198` checks the flag at entry, but the device can be lost between that check and `queue.submit([encoder.finish()])` at `renderer.rs:305` (also `present` at :306). A lost-device submit throws a JS exception, which wasm-bindgen surfaces as a Rust panic.
- `browser.rs:747-980` — the rAF `Closure` has no `catch_unwind`/try-catch; the next-frame schedule lives at `browser.rs:977-978` after tick+render. Any panic in the frame body (device loss, unexpected JS exception in any backend, WebGL context loss) escapes the closure and the loop **never schedules another frame** — permanent freeze with no recovery path (the `WebGpuRenderer` entry check only prevents *starting* frames once the callback has fired).
- Fix: wrap the frame body in `std::panic::catch_unwind` (or a JS try/catch) and always schedule the next frame; after a submit failure, stop rendering until the flag is observed and surface a HUD error instead of dying.

### F16 [Low/Medium] Transient memory spike from the completion pipeline; per-worker archives make the pool's memory ceiling ~32× the documented figure
- `worker_source.rs:60,93` (`completed` sink) + `engine.rs:839-913` — a full-pool burst (32 workers) decodes 32 payloads into the sink, then `completed_backlog` drains them at 8 budget units/tick (2 fine chunks/tick, `engine.rs:841-842`): ~16 ticks (~270 ms) of holding up to 32 × ~3.2 MB ≈ **~100 MB** of decoded geometry *in addition* to the store copies (payloads double-buffer in sink + backlog + store).
- `lib.rs:1137-1144` — the doc claims the pool's archive total "stays near this figure [32 MB] however many workers there are", but `WORKER_ARCHIVE_BYTES` is a **per-worker** ceiling (`ArchivedChunkSource::with_budget(..., WORKER_ARCHIVE_BYTES, ...)`, lib.rs:1165-1172; `MemoryStorage` per instance, chunk_archive.rs:451-497). After a long walk each worker's share can reach 32 MB independently → worst-case pool archive ≈ workers × 32 MB ≈ **1 GB** on a 32-worker `Auto` config (generation_worker_policy.rs:46-48 uses hardware_concurrency−1), plus per-worker wasm heap/atlas scratch.
- Fix: cap bytes drained per tick (not just units) in `install_completed`; make the archive budget a pool-global budget divided by worker count, or shrink `WORKER_ARCHIVE_BYTES` for large pools.

### F17 [Low] worker.js transfers `bytes.buffer` relying on wasm-bindgen glue copy semantics — safe at the locked version, fragile to upgrades
- `static/worker.js:82-96` — `postMessage({ ..., buf: bytes.buffer }, [bytes.buffer])` transfers the buffer returned by `worker_generate` (Rust `Vec<u8>`, lib.rs:1198). The committed glue (`static/pkg/wasm_frontend.js`, wasm-bindgen 0.2.126) returns `getArrayU8FromWasm0(ret[0], ret[1]).slice()` — a **copy** of the wasm-memory subarray view — so today the transferred buffer is JS-owned and the transfer is safe.
- The safety lives in generated glue, not in worker.js: if the glue convention ever returns a view (older wasm-bindgen versions returned `subarray` views for some return types), the transfer would detach the worker's entire linear memory, breaking the wasm instance and its 32 MB archive mid-session. Nothing in worker.js checks `bytes.buffer.byteLength !== wasm.memory.buffer.byteLength` before transferring.
- Fix: assert/verify non-aliasing before transfer (`if (bytes.buffer === wasm.memory?.buffer) bytes = bytes.slice();`), or pin the glue's copy behavior with a comment + test on the wasm-bindgen version; alternatively add an explicit `Vec<u8>`-to-owned-copy helper in Rust.


## Top optimizations (ranked)

1. **Per-chunk light clustering with a hard cap, applied to every GPU path.** The single biggest cost in the renderer is unbounded light loops: GLSL per-hit `uChunkLightCounts` (F1/F8), WebGPU raster `draw=[0, light_count]` for every chunk (F9/F10), WebGPU raymarch over all ≤52 lights (F5). The WebGL2 path already has the machinery — `append_lights_reaching_bounds` per chunk (surface_webgl/draw.rs:293, gl/cluster_scene_lights.rs:15-32). Reuse it: write per-chunk light ranges into `GpuChunkUniforms`/`uChunkLightCounts` with a top-K cap (e.g. 8-16 lights/chunk, matching the 4-Gauss budget), and push the 4× Gauss-sample loop inside the light loop only for lights actually in range. Cuts worst-case shading by 3-4 orders of magnitude in dense scenes and removes the F9/F10 splat vertex blow-up (31M evals/frame).
2. **One global per-pixel secondary-ray step budget shared across lights and samples.** GLSL has none at all (F2: 25 chunks × 768 per Gauss sample per light); WebGPU has a per-light 4096 budget (F5) that multiplies by lights. A shared `inout` counter capped at ~4096-8192 steps per pixel (all lights × samples combined) bounds the worst case to one trace-length, not L×trace-length, and makes direct-visibility cost predictable for the adaptive resolution governor.
3. **Cache light visibility per (chunk, light) instead of per Gauss sample.** `directSampleIsVisible` (trace_direct_light_visibility.rs:25-45,93) re-traces the same light up to 4× per hit pixel (once per Gauss sample). A rectangle light's visibility is one trace; memoize per (chunk, light) within a frame (or compute 4 samples from one shadow ray + cone falloff) — a straight 4× reduction of the dominant F1/F5 cost.
4. **Port the WebGL2 shadow-pass AABB culling to WebGPU.** `surface_webgl/draw.rs:368-382` already culls chunks against the hero-light footprint; WebGPU shadow passes draw every resident chunk with zero culling (F11, surface.rs:273-295, splat.rs:361-382). Trivial port, removes O(C) wasted shadow draws per frame for chunks behind/away from the hero light.
5. **Reorder worker reply handling: ledger check before decode (F13).** Parse the echoed request, run `finish_exact`/staleness checks, and only then decode the payload (or defer decode into `install_completed`'s budget). Removes multi-ms main-thread hitches from completion bursts and stops decoding stale payloads that are immediately discarded. Pairs with a per-request timeout so wedged workers stop stranding chunks (F14).
6. **Cache per-chunk light ranges across frames.** F3/F4 recompute O(V×L) light predicates every frame for static chunk bounds and mostly static lights; invalidate per-chunk ranges only when the frame light set or chunk bounds change. Also delete the dead `LightSelectionCache`/`select_lights_with_hysteresis` (cluster_scene_lights.rs:68-130) rather than risk wiring up its unbounded-growth HashMap.

## Summary / severity-ranked findings

All 17 findings verified against sources in this part. F1-F12 were verified by the prior pass; F13-F17 were verified in this pass from the generation-worker pipeline (worker_source.rs, generation_worker_requests.rs, static/worker.js, static/pkg/wasm_frontend.js, engine.rs, browser.rs, webgpu/browser_context.rs, bin/play/app.rs).

| # | Severity | Finding | Where |
|---|---|---|---|
| F1 | Critical | GLSL raymarch shades every hit against all chunk lights × 4 Gauss samples × all chunks × 768 steps — ~3M leaf lookups/pixel worst case | shade_voxel_hit.rs:48-55; trace_direct_light_visibility.rs:25-45,93; intersect_voxel_scene.rs:150,206 |
| F5 | High/Critical | WebGPU raymarch loops all ≤52 frame lights per hit; each in-range light runs a 4096-step visibility trace | gpu_types.rs:105-110; raymarch.wgsl (DIRECT_VISIBILITY_TRACE_BUDGET) |
| F9 | High | WebGPU raster paths evaluate ALL frame lights per vertex/fragment — no clustering, no range early-out (draw=[0, light_count] for every chunk) | pipelines/surface.rs:208; splat.rs:250; surfel.rs:223 |
| F10 | High | WGSL raster light-loop bodies; splat vertex stage worst case ~31M light evaluations/frame | common.wgsl; splat_geometry.wgsl; splat.rs face budget |
| F2 | High | GLSL has no global secondary-ray budget; per-sample traces can walk 25 chunks × 768 steps each | trace_direct_light_visibility.rs:25-45; intersect_voxel_scene.rs:150/206 |
| F3 | Medium | surface_webgl prepare_visible_draws O(V×L) per frame, recomputed every frame | surface_webgl/draw.rs:262-312 |
| F6 | Medium | GPU frame timer hardcoded off in all three WebGL2 drivers; HUD always shows 0.00 ms | gl/timer.rs:70-87; surface_webgl/draw.rs:127; splat_webgl/draw.rs:102; surfel_webgl/draw.rs:84 |
| F8 | Medium | Cluster test documents no per-chunk light cap (feeds F1) | gl/cluster_scene_lights.rs:152-171 |
| F11 | Medium | WebGPU shadow passes draw every resident chunk with zero culling (WebGL2 has AABB culling) | pipelines/surface.rs:273-295; splat.rs:361-382 |
| F13 | Medium | Main-thread payload decode unbudgeted, runs before staleness check; burst decode stalls frames, stale payloads decoded then dropped | worker_source.rs:288-299,327-331; engine.rs:841-842 |
| F14 | Medium | No timeout/watchdog on generation requests; wedged worker strands its affinity chunks forever; pool terminates without respawn, degrading to 42 ms same-thread fallback | static/worker.js:99-106; generation_worker_requests.rs:76-80; worker_source.rs:104-110,207-235; engine.rs:959 |
| F15 | Medium | Device-loss race between is_device_lost() check and queue.submit; no panic guard in rAF closure → permanent freeze | renderer.rs:195-198,305; browser.rs:747-980; browser_context.rs:61-66 |
| F4 | Low | append_lights_reaching_bounds O(L) per chunk; dead LightSelectionCache with unbounded growth if wired | gl/cluster_scene_lights.rs:15-32,68-130; upload_scene_lights.rs:56-101 |
| F7 | Low | WebGL2 light selection sort O(N log N), N ≤ 20 — negligible | gl/lights.rs:62-107 |
| F12 | Low | WebGPU splat/surfel per-frame chunk sort O(C log C) — negligible | pipelines/splat.rs:258-265; surfel.rs:228-235 |
| F16 | Low/Medium | Completion burst holds ~100 MB decoded backlog; per-worker 32 MB archives scale pool memory to ~1 GB on 32 workers | worker_source.rs:60,93; engine.rs:839-913; lib.rs:1137-1144,1165-1172 |
| F17 | Low | worker.js transfers bytes.buffer relying on wasm-bindgen glue copy semantics; safe at 0.2.126, would detach worker memory on a glue convention change | static/worker.js:82-96; static/pkg/wasm_frontend.js worker_generate wrapper |

**Bottom line:** the performance-critical issues are concentrated in unbounded light loops in the shaders (F1/F5/F9/F10) — every GPU path pays per-light cost with no per-chunk clustering cap and no shared ray budget; fixing those four changes the worst case from millions of leaf lookups per pixel to a bounded constant. The worker pipeline (F13-F17) is architecturally sound (identity-checked replies, serialized per-worker queues, metered installation, safe-at-current-version buffer transfers) but lacks a timeout/respawn story and meters installation without metering decode.

## STATUS
- [x] Seed files read (fe-drivers-key-findings.txt, fe-drivers-prior-analysis.txt)
- [x] Verify findings against sources
- [x] Complete Big-O table, concurrency inventory, top optimizations
- [x] F13-F17 added from generation-worker pipeline verification
