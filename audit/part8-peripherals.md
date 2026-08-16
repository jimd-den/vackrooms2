# Audit Part 8 — Peripherals: wasm_raycaster, tests, benches, examples, build scripts, worker.js, index.html

**Repo:** /home/dbslim/vackrooms
**Date:** static audit (file reading only — nothing was compiled, built, run, or tested)
**Auditor scope:** `wasm_raycaster/src/lib.rs` (256 lines), `tests/` (2 files), `benches/generation.rs`,
`examples/` (6 files), `scripts/*.mjs` (5 files), `static/worker.js`, `static/index.html` embedded
module script (lines 1125–1901). Cross-referenced for protocol/concurrency claims:
`wasm_frontend/src/drivers/worker_source.rs`, `wasm_frontend/src/drivers/generation_worker_requests.rs`,
`wasm_frontend/src/application/generation_worker_policy.rs`, `wasm_frontend/src/application/engine.rs`,
`wasm_frontend/src/lib.rs` (worker entry + settings statics), `src/main.rs`, generated glue
`static/pkg/wasm_frontend.js`.

## 0. Headline verdict

**No Critical findings** (no correctness bugs, data races, or deadlocks found in scope). The worker
subsystem is unusually well-engineered: exact-identity request ledgers, in-order serial worker queues,
bounded in-flight windows, stale-reply rejection, and a main-thread fallback. The `Vec<u8>` returned by
`worker_generate` is **copied** by the generated glue (`static/pkg/wasm_frontend.js`:
`getArrayU8FromWasm0(...).slice()`), so transferring `bytes.buffer` in worker.js does **not** detach the
worker's wasm memory — verified against the shipped glue, not assumed.

All findings are **Medium/Low**: mostly lifecycle robustness (no worker respawn, no wasm-init retry),
redundant recomputation in dev tooling, and micro-optimizations in a legacy reference renderer.

---

## 1. Algorithm audit

### 1.1 wasm_raycaster/src/lib.rs — CPU raycaster

**`render` (lib.rs:81–255).** Per frame, with `Sx` = screen width, `Sy` = screen height,
`K = max_steps = 200` (line 148), `V = W·H·D` = voxel volume:

| phase | lines | cost |
|---|---|---|
| background clear | 83–88 | `O(Sx·Sy)` (4 byte-stores per pixel, `step_by(4)`) |
| DDA per column | 102–168 | `O(Sx·K)` worst; each step is O(1) `get_voxel` (bounds-checked index, lines 53–65) |
| wall/floor/ceiling fills per column | 229–252 | `O(Sx·Sy)` total (the three loops partition each column: wall span + floor span + ceiling span) |

**Total: O(Sx·Sy + Sx·K) time, O(Sx·Sy) space** (frame buffer allocated once in `new`, line 28).
This is the textbook DDA raycaster complexity — **no accidental O(n²)**, no `contains()`/linear scans
in loops, no per-frame allocation, no string building. `get_voxel`/`get_light` are O(1) with explicit
bounds checks (lines 53–79). `set_grid` is O(V) copy per call (lines 45–46), the only per-update cost,
and it is amortized over many frames.

Notes:
- **Hot-loop vectorization (Medium).** The clear loop (83–88) uses `step_by(4)` on `u8`, which prevents
  LLVM from vectorizing (non-contiguous stores); the wall/floor/ceiling fill loops write 4 scalar bytes
  per pixel. Writing a `u32` per pixel (or `chunks_exact_mut(4)`) would let the compiler emit SIMD
  stores and cut instruction count ~4× on the dominant O(Sx·Sy) term. This is a legacy reference
  renderer (workspace comment, Cargo.toml:16–17), so impact is bounded.
- **Fixed 2-D slice (Low, by design).** The ray marches only in X/Z at `map_y = py.floor()` (lines
  147, 162); pitch is faked by shifting the drawn span (line 178). Geometry above/below the camera row
  can never occlude, so ceiling/floor overhangs render through. `max_steps = 200` caps the ray at 40 m
  at `voxel_scale = 0.2` — a hard draw-distance limit. Acceptable for the documented "legacy
  experiment" status; if ever revived, switch to a 3-D DDA.
- **No overflow/UB hazards.** `screen_height / perp_wall_dist` can be `inf` (player inside a wall);
  the `as isize` cast saturates (Rust float→int casts saturate), and `draw_start`/`draw_end` are
  clamped before use (lines 180–191). `(-0.0*0.05).exp() = 1.0` is fine. `frame_buffer.len()` is always
  divisible by 4 (width·height·4). Safe.
- **Micro:** `set_grid` unconditionally reallocates both `Vec`s (45–46) even when dims are unchanged —
  could reuse buffers. Trivial.

### 1.2 tests/ — collision & surface diagnostics

- `grid_wall_boxes_at_spawn_x` (collision_diagnostics.rs:25–51): full-grid triple scan
  **O(W·H·D)** plus one full `GenerateChunkArchitectureUseCase::execute` per call; `floor_boxes`
  (53–77): O(W·D); `player_overlaps` (80–100): O(1). No algorithmic issues.
- **Redundant recomputation (Medium, test-time only):** tests 1, 2, and 4 (lines 106, 139, 196) each
  call `grid_wall_boxes_at_spawn_x(0.0, 0.0)` / `floor_boxes(0.0, 0.0)` — three independent
  regenerations of the *identical* (0,0) seed-42 chunk, each rebuilding the full architectural chunk
  and re-scanning every voxel. Tests run in parallel, so this also multiplies peak memory. Fix: one
  `OnceLock`/lazy fixture shared by the three tests.
- Misleading name: `grid_wall_boxes_at_spawn_x` claims "x≈5" but scans the whole grid (25–27). Low.
- `surface_rendering_diagnostics.rs`: grids are tiny (≤ 20×3×3); each scenario is O(W·H·D) to build +
  O(Q) quad filtering. Scenario 6 (206–234) asserts exact budget numbers (16/34) — brittle but
  deterministic. No complexity concerns.

### 1.3 benches/generation.rs — methodology review

**What it measures:** (a) `octree_build`: `BuildOctreeUseCase::execute` on synthetic hollow-room vs
checkerboard grids at edges 16/32/48/64/96 — the classic "cost scales with surface vs 8^depth" bracket;
(b) `octree_direct_vs_dense`: same grids, dense octree vs `build_octree_direct`+`GridSampler`;
(c) `svdag_compress`: SVDAG dedup on the same grids + one real Level-0 chunk; (d) `region_plan`:
`generate_region_plan` with a pillar/wall/atria density sweep as a proxy for assembly count
(`place_suite`'s documented quadratic candidate search).

**Does it use rayon?** No. `rayon` appears only in `wasm_frontend/Cargo.toml:95` under
`[dev-dependencies]` and only `wasm_frontend/examples/path_trace.rs:29` imports it. The root bench is
pure criterion. Correct — criterion runs each group's iterations sequentially in-process and rayon
inside a timed closure would corrupt the measurement.

**Validity — mostly good, with caveats:**
1. **Inputs built outside the timed loop** (hollow_room/checkerboard at lines 75/82/113–114, SVO at
   151–152, real chunk at 158–159) — correct; the measured closure runs identical work per iteration.
2. **Stateless generator** — `SimpleNoiseProvider` is a pure hash/noise struct (simple_noise.rs), so
   `region_plan` iterations are deterministic and identical. Good.
3. **No `black_box` (Low).** `execute`/`compress_svdag`/`generate_region_plan` return values are
   dropped inside `b.iter`. Cross-crate, without LTO (no `[profile.release]` in the workspace
   Cargo.toml), these calls cannot be eliminated today, so the numbers are valid — but criterion
   hygiene says wrap outputs in `criterion::black_box` so a future LTO/refactor can't silently turn
   the bench into a no-op.
4. **`sample_size(20)` with default 5 s measurement (Low).** The comment (201–204) explains the cap,
   but with the default `measurement_time` of 5 s, criterion budgets 250 ms per sample; the slowest
   case (checkerboard 96³, depth-7 → ~2.1M leaf cells visited per iteration) can take longer than
   that per iteration, collapsing to ~1 iteration/sample → noisy 20-sample estimates. If the suite
   runtime allows it, prefer `measurement_time(Duration::from_secs(1))` per slow group rather than
   shrinking the sample count, or keep sample_size(20) and accept wider confidence intervals.
5. **Redundant arm (Low).** `octree_direct_vs_dense`'s `{label}/dense` arm (117–124) is the same
   measurement as `octree_build` (77–79) — identical inputs, identical closure. Kept presumably for
   side-by-side presentation, but it doubles the wall-clock of the suite.
6. **Doc/measurement mismatch (Low).** The module docstring (22–24) says `svdag_compress` reports "the
   GPU upload size win" via printed node counts, but the stats are printed **only** for
   `real_level_zero_chunk` (161–166); for `hollow_room_64`/`checkerboard_64` the returned `SvdagStats`
   is dropped inside the timed closure and never printed. Print the per-iteration stats (or a
   once-per-benchmark `eprintln`) for the synthetic grids too, or the upload-size claim is untestable.
7. **What it does *not* measure:** allocations/memory (no criterion memory profiling), the worker
   encode→transfer→decode round-trip, and `place_suite` directly (acknowledged proxy, 25–27). For a
   Pi-target GPU-upload claim, memory-per-chunk is arguably the more important number than time.

### 1.4 examples/ — six files

- **generate_samples.rs (92 lines) — wasted expensive work (Medium).** Lines 36–38 call
  `generate_region_plan(...)` **per seed and immediately discard the result** (`let _plan`). Region
  planning is one of the repo's flagged expensive stages (bench docstring calls `place_suite`
  quadratic in assemblies-per-region, benches/generation.rs:25–27). Over 100 seeds this is 100
  full region plans computed and thrown away. Fix: delete the call. The rest is O(seeds · steps² ·
  chunk_w · chunk_d · height) ≈ 100 seeds × 100 chunks × 2500 columns × ~15 y-lookups ≈ 375M voxel
  reads plus 100 SVG renders of 250k-cell grids — inherent to the tool, but the per-column y-scan
  (lines 46–55) could be folded into a single pass building per-column wall/light flags while the
  grid is generated (the generator already visits every voxel once). Note the seeds are
  embarrassingly parallel — `rayon` is available in dev-deps and this is a dev tool; a `.par_iter()`
  over seeds would give near-linear speedup on multi-core machines (Medium for a batch tool that
  prints "Generating 100 SVGs…").
- **slice_view.rs (150 lines).** One chunk at a time (16×16=256 chunks at default SPAN=160) — the
  right streaming approach. Per column: two O(1) `pixel_span` calls (97–106, guarantees ≥1 px),
  O(1) material lookups, and the paint loops paint each pixel of the image exactly once
  (O((span·scale)²) total, at scale 5 px/unit and 0.2-unit voxels the inner square is 1×1).
  Complexity is optimal: O(chunks·columns + pixels). No findings beyond style.
- **section_view.rs (142 lines).** SVG assembled with `svg.push_str(&format!(...))` per rect
  (116–126): amortized O(1) per append, O(output) total; `format!` per rect allocates but W·H ≤
  50×~20 rects. The `String::from_iter` row build (100–103) is fine. No issues.
- **dump_blueprints.rs (120 lines).** Two distinct plans (REGION_SIZE=80.0 at region_plan/mod.rs:30 vs
  the 100.0 square at line 80) — **not** a duplicate computation. O(100) bounds. Hard-coded absolute
  artifact path `/home/dbslim/.gemini/...` (lines 65, 94) is a portability smell, not a perf issue.
- **dump_plan.rs (23 lines).** Two plans, trivial. Fine.
- **plan_view.rs (127 lines).** `chunk_grid` is O(per_side²) = 100 bounds; per region one plan + ASCII
  + SVG; `rasterize` spawns one `magick` (best-effort). Fine.

### 1.5 scripts/*.mjs — build-tool complexity

- **wasm-source-hash.mjs (40 lines).** `filesBelow` (17–25) is a directory walk with `sort()` for
  determinism and one `readFile` fully into memory per file — O(total bytes), sequential, ~one file in
  memory at a time. Recursion depth = directory depth (≤ ~6 here) — no stack risk. `INPUTS` (8–14)
  correctly covers only the wasm contract (worker.js yes; index.html/scripts deliberately not — a page
  change must not orphan archives). No string concat in loops (uses `hash.update`, correct). Fine.
- **build-wasm.mjs (93 lines).** `wasmSourceHash` is computed **twice** — once for the build id (line
  20) and once for the stamp (line 63) — each re-reading and re-hashing every source file. For this
  repo ~1–2 MB total, so ~2× that cost; Low. The staging-dir → atomic `rename` swap (75–86) with
  rollback is correct; `cp` per entry is O(pkg size). `spawn`/exit handling is clean. No O(n²).
- **build-single-file.mjs (113 lines).** The interesting one for "string concat in loops": there are
  none — four `mustReplace` calls and two `jsString` transforms, all O(file size) single passes
  (JSON.stringify + one `.replace(/</g,'\\u003c')`), one final multi-MB `writeFile`. The runtime
  `.replaceAll('__GLUE_URL__', ...)` is O(worker size) once at page load. `mustReplace` failing loud
  on pattern drift (27–38) is exactly right for a bundle script. Two robustness notes (Low):
  (a) the `count` check then `source.replace(pattern, replacement)` replaces only the **first**
  occurrence — correct only because every caller passes count=1; if a future call passes count=2 the
  second match silently survives; (b) correctness depends on `__GLUE_URL__`/`__WASM_URL__` never
  appearing inside glue/worker sources — worth a comment (it is partially documented at 70–78).
  Base64 payload cannot contain `</script>` (alphabet has no `<`), so the text/plain embedding is
  safe; `jsString` escapes `<` for the JS embeds — verified safe.
- **check-wasm-fresh.mjs (23 lines).** O(stamp) read + O(source tree) hash; the error messages are
  actionable. Fine.
- **check-workflow-contract.mjs (80 lines).** Three `.includes` substring checks — O(file size), and
  fragile by design (whitelist contract). `logTelemetry` with `JSON.stringify` — O(args). Fine.

### 1.6 static/index.html — embedded module script (1125–1901)

No hot loops. Startup is a bounded sequence of O(1)/O(knobs) preference sanitization (1172–1267),
DOM population (1359–1417), listener registration (1518–1610), and one `init()` (1894). `buildWorldQuery`
(1661–1703) and the setup-screen builders (1762–1812) are O(constants) (≤ 13 toggles, 9 knobs, 5
presets). Seed rolling uses `crypto.getRandomValues` (1317, 1845) — no Math.random determinism trap.
`history.replaceState` URL rewriting happens at most twice (1281, 1320). **The only duplicated
constant is `MAX_GENERATION_WORKERS = 32` (line 1182), mirrored in
`generation_worker_policy.rs:20`** — consistent today, drift-prone tomorrow (Low).

---

## 2. Concurrency audit

### 2.1 Worker protocol cross-check — static/worker.js ↔ worker_source.rs

Hand-verified every field in both directions (worker.js:7–17 documents the protocol; worker_source.rs
sends at 132–136 and 241–250; parses at 288–331):

| message | sender fields | receiver | verified |
|---|---|---|---|
| init | `type, query, seed` (worker_source.rs:133–135) | worker.js:61–63 → `worker_init(query, seed>>>0)` (lib.rs:1114) | ✓ |
| gen | `type, requestId, ox, oz, level, lod, artifacts, reality` (241–250) | worker.js:69–77 → `worker_generate(...)` (lib.rs:1156) | ✓ |
| done | `type, requestId, ox, oz, level, lod, artifacts, reality, ms, buf` (82–96) | `parse_request` fields 311–323 + `parse_payload` buf 327–331 | ✓ all present |
| failed | `type, requestId, ox, oz, level, lod, artifacts, reality, message` (38–48) | `parse_request` 300–302 | ✓ |
| fatal | `type, message` (50) | `WorkerReply::Fatal` 303 | ✓ |

- **Transfer safety: verified against shipped glue.** `worker_generate` returns `Vec<u8>`; the
  generated wrapper in `static/pkg/wasm_frontend.js` does `getArrayU8FromWasm0(ret[0], ret[1]).slice()`
  then `__wbindgen_free` — a **fresh copy**, not a view. So `postMessage(..., [bytes.buffer])`
  (worker.js:82–96) detaches a copy, never the worker's linear memory. **Fragile by contract though
  (Low):** if anyone changes the return type to `&[u8]`/`Box<[u8]>`, wasm-bindgen switches to a memory
  *view* and the first transfer silently detaches the worker's heap, killing every subsequent
  `worker_generate` on that worker. Add a comment in worker.js or a CI grep for the return type.
- **init is never re-sent** (one-shot, worker_source.rs:132–136) and `worker_init` is only valid once
  per worker instance (lib.rs:1130–1143). World changes are full page reloads — consistent with the
  URL-carried world identity (index.html:1271–1325). OK.

### 2.2 Message ordering, backpressure, lifecycle

- **Ordering is sound.** `onmessage` is registered synchronously at module scope (worker.js:99), so no
  message is lost during wasm instantiation. The promise chain (103–105) serializes processing and
  **per-item catch** prevents a failed request from poisoning later ones (documented at 101–102). The
  browser delivers messages in order and the chain preserves that order, so the reply stream is FIFO —
  which is exactly what `fail_oldest` (generation_worker_requests.rs:100–102) relies on. Verified
  consistent.
- **Backpressure is engine-side only (Low).** worker.js has no queue bound: every delivered message
  appends one `.then()` to the chain. In practice the engine caps in-flight at
  `max(max_concurrent_requests, max_loads_per_tick*2, 4)` (engine.rs:928–932) and `pending.len()`
  gates issuance (944, 955, 967), so a worker's queue is ≤ pool_size ≈ in-flight — bounded. But
  nothing in worker.js or the ledger *enforces* a per-worker cap (`assign` pushes unconditionally,
  generation_worker_requests.rs:76–80); if a future engine change over-issues, the worker's message
  queue and promise chain grow without limit (each queued message also pins its `reality` copy).
  Defense in depth: reject/NAK when the chain exceeds N (e.g., 64) instead of queueing.
- **Worker lifecycle: spawn is fine, respawn is missing (Medium).** Spawn path is correct:
  module-type worker, relative URL resolution for subpath hosting (worker_source.rs:74–80), `init`
  posted before any `gen`, closures kept alive by `_message_handlers` (41–42), deterministic
  disable-and-drain on: fatal reply (104–110), `onerror` (125–129), and synchronous
  `post_message` failure (255–266). `next_accepting` skips disabled workers (70–74) and an exhausted
  pool falls back to same-thread generation (212–234). **However, a disabled worker is never
  replaced or re-enabled.** Over a long session (transient OOM, a single wasm trap, a browser worker
  eviction), workers ratchet downward until the whole pool is on the main-thread fallback and frame
  times degrade silently. Fix: respawn a replacement worker (with backoff) on error/fatal, re-sending
  the init message; the ledger already supports re-enabling (`accepting_work = true`).
- **`ensureWasm` failure is cached forever (Medium).** worker.js:21–24: if `init()` rejects once
  (network failure fetching the wasm module), `wasmReady` stays a rejected promise; every queued and
  future `gen` message then rejects through the same promise and posts a `"failed"` reply each
  (33–48), while the main thread re-issues those chunks — a self-limiting but noisy retry storm, and
  the worker never retries instantiation. Fix: on rejection set `wasmReady = null` and rethrow; after
  N consecutive failures post `{type:"fatal"}` so the driver terminates and (with the respawn fix)
  replaces the worker.
- **Stale replies / dropped-worker data: handled correctly.** `finish_exact`/`fail_exact` match full
  request identity (request_id + coords + lod + artifacts + reality,
  generation_worker_requests.rs:84–95, tests 193–253), so a late callback after `disable_and_drain`
  finds nothing and is dropped; the engine independently re-validates against `pending`
  (`is_same_request`, engine.rs:858–864) and reality (`is_in_reality`, engine.rs:870–877). No lost
  replies, no double-install.
- **Malformed replies** map to `fail_oldest` (worker_source.rs:111–117) — correct under FIFO.
  `exact_u32`/`finite_f32` (333–342) reject NaN/negative/overflowing values; `decode_chunk_payload`
  returns `None` on any truncation (chunk_codec.rs doc). Defense is thorough.
- **Race between async worker replies and synchronous frame updates:** none possible — all shared
  state (`completed`, `failed`, `requests`) is `Rc<RefCell<...>>` touched only from the single
  main-thread event loop, with short borrows; `poll_completed`/`poll_failed_requests` `mem::take`
  (269–275) so the sinks are bounded. The wasm module inside each worker is single-threaded with a
  `thread_local! SOURCE` (lib.rs:1098–1101) — no cross-thread sharing of wasm state at all.
- **Determinism:** main thread and workers derive the world from the same URL query + rolled seed
  (index.html:1316–1325 stamps `seed`+`auto_seed` into the URL *before* `init()`; worker_init receives
  `query`+`seed`, lib.rs:1114–1115). `preferred_worker` uses a fixed FNV-style hash
  (generation_worker_requests.rs:59–66) — stable routing per chunk, deterministic across runs and
  tested (128–142). RNG is seeded per worker (`StdRng::seed_from_u64`, root Cargo.toml:22–25 comment)
  — no shared RNG, no `static mut`, no global mutable state in production code.

### 2.3 Settings atomics, device_lost, and the native thread

- **Settings statics (wasm_frontend/src/lib.rs:32–122):** `AtomicBool`/`AtomicU32` with `Relaxed`
  ordering everywhere (60–272). This is correct *for today's topology*: all of these are written by
  wasm-bindgen setters on the main thread and read by the same thread's render loop; there is no
  second Rust thread touching them. Relaxed is the cheapest valid choice. If workers ever read
  settings directly (they don't — config crosses via the query string), a Release/Acquire pair or a
  versioned snapshot would be required; document that invariant next to the statics.
- **`device_lost` (wasm_frontend/src/drivers/webgpu/browser_context.rs:20, 61–65, 148–149):**
  `Arc<AtomicBool>` set in the WebGPU lost-device callback (`store(true, Release)`) and polled by the
  renderer (`load(Acquire)`) — correct pattern, no torn reads, no lost update (single writer).
- **src/main.rs:25 — thread-per-connection (Medium for a dev server):** `thread::spawn` per accepted
  connection with no pool and no limit; under a scan or a burst of keep-alive connections this spawns
  unbounded threads (fd/stack exhaustion). Each `/maze` and `/octree` request also **regenerates the
  same chunk from scratch** (fixed seed 42, query coords) — identical JSON/binary recomputed per
  request with no cache (lines 60–84, 88–112). For a local dev tool this is tolerable, but a
  ​small LRU (last-N chunk responses) plus a bounded thread pool (or a single-threaded `for` loop with
  a connection queue) would make it robust. Not part of the wasm product path.

### 2.4 Re-entrancy (JS event loop / wasm_bindgen_futures / rAF)

No `wasm_bindgen_futures::spawn_local` or `requestAnimationFrame` usage exists in the audited files
(the module script is plain synchronous DOM + one `await`-free `init().then` at index.html:1894, and
`beginSurvey` awaits `engineReady` at 1858). The worker replies arrive as `message` events interleaved
with the render loop's rAF callbacks; all shared state mutations are short, non-reentrant RefCell
borrows in the same task. No re-entrancy hazards found. (The `beginSurvey` guard `entering` at
1830–1831 correctly prevents double-entry; `engineReady` promise guards init-ordering at 1858.)

---

## 3. Findings table

| # | Severity | file:line(s) | issue | fix |
|---|---|---|---|---|
| F1 | Medium | static/worker.js:21–24 | `wasmReady` caches a rejected `init()` promise forever; every subsequent message posts a `"failed"` reply (retry storm) and instantiation is never retried | on rejection: `wasmReady = null`, rethrow; after N consecutive failures post `{type:"fatal"}` |
| F2 | Medium | wasm_frontend/src/drivers/worker_source.rs:104–130 | disabled/failed workers are never respawned or re-enabled; long sessions ratchet to main-thread fallback with silent frame-time degradation | respawn replacement worker with backoff on error/fatal; re-send `init`; re-enable ledger slot |
| F3 | Medium | examples/generate_samples.rs:36–38 | `generate_region_plan` runs per seed and the result is discarded (`let _plan`) — 100 wasted region plans (an expensive, quadratic-in-assemblies stage) | delete the call |
| F4 | Medium | tests/collision_diagnostics.rs:106, 139, 196 | tests 1/2/4 regenerate the identical (0,0) seed-42 chunk three times (full architectural generation + full-grid scans each) | share one `OnceLock`-built fixture |
| F5 | Medium | wasm_frontend/src/lib.rs:1110 + generation_worker_policy.rs:20, 39–41 | `Auto` on a 64-thread machine spawns 32 workers, each a full wasm module instance with up to a 32 MB archive budget — worst case ≈1 GB/tab; per-worker budget doesn't shrink as pool grows | scale `WORKER_ARCHIVE_BYTES` down with pool size (e.g., 32 MB / max(1, n/4)) |
| F6 | Medium | wasm_raycaster/src/lib.rs:83–88, 229–252 | clear loop uses `step_by(4)` byte stores (blocks SIMD); fills write 4 scalar bytes/pixel | write one `u32` per pixel (or `chunks_exact_mut(4)`) so LLVM vectorizes; also hoist `half_fov.tan()` (line 99) out of the column loop |
| F7 | Medium | src/main.rs:25, 60–112 | native dev server: thread-per-connection (unbounded) and chunk regeneration with no cache on every `/maze`/`/octree` request (fixed seed 42 → identical output) | bounded thread pool + small LRU of generated chunk responses |
| F8 | Medium | examples/generate_samples.rs:27–68 | 100 seeds are embarrassingly parallel but run sequentially (~hundreds of millions of voxel reads + 100 SVG renders) | `rayon::par_iter` over seeds (rayon already in dev-deps) |
| F9 | Low | generation_worker_requests.rs:112–118 | `take_exact` is a linear `position()` scan + O(n) `VecDeque::remove` per reply — O(q) per reply, O(q²) per drain; q is bounded by max_pending ≤ 32 today | keep as-is, or switch to `HashMap<u32, ChunkRequest>` keyed by request_id if q ever grows |
| F10 | Low | static/worker.js:82–96 + static/pkg/wasm_frontend.js (`getArrayU8FromWasm0(...).slice()`) | transfer safety of `buf` depends on wasm-bindgen copying `Vec<u8>` returns; a return-type change to `&[u8]`/`Box<[u8]>` would silently detach the worker's wasm memory | comment the invariant in worker.js; add a CI check that `worker_generate` returns owned `Vec<u8>` |
| F11 | Low | static/worker.js:99–106 | worker message queue/promise chain is unbounded if the engine ever over-issues (only engine-side `max_pending` caps it today, engine.rs:928–932) | explicit per-worker cap + NAK/backpressure in worker.js |
| F12 | Low | benches/generation.rs:77–79, 117–124 | `octree_direct_vs_dense/{dense}` duplicates `octree_build` exactly; doubles suite wall-clock | drop the duplicate arm (keep the direct comparison only) |
| F13 | Low | benches/generation.rs:153–155, 161–166 | docstring claims SVDAG node-count "GPU upload size win" is reported, but stats are printed only for the real chunk, not hollow/checkerboard grids | print `SvdagStats` per synthetic grid once outside the timed loop |
| F14 | Low | benches/generation.rs:78, 134, 154, 191 | no `criterion::black_box` on dropped return values; safe cross-crate today, fragile under future LTO | wrap results in `black_box` |
| F15 | Low | benches/generation.rs:199–205 | `sample_size(20)` + default 5 s measurement → slowest cases get ~1 iteration/sample (noisy); comment says cap is for suite runtime | prefer shorter `measurement_time` per slow group, or accept and document wider CIs |
| F16 | Low | worker_source.rs:327–331 | transferred payload ArrayBuffer is copied into wasm heap (`to_vec()`) before decode — one O(payload) main-thread copy per chunk | decode over a `Uint8Array` view (or document the copy as accepted overhead) |
| F17 | Low | scripts/build-wasm.mjs:20, 63 | `wasmSourceHash` runs twice per build, re-reading/hashing the whole source tree each time | compute once, reuse for build id and stamp |
| F18 | Low | static/index.html:1182 vs wasm_frontend/src/application/generation_worker_policy.rs:20 | `MAX_GENERATION_WORKERS = 32` duplicated across JS and Rust; drift silently changes worker caps | single source of truth (inject the Rust constant at build time) |
| F19 | Low | wasm_raycaster/src/lib.rs:45–46 | `set_grid` reallocates both voxel buffers on every call even when dims are unchanged | reuse buffers when `(w,h,d)` match |
| F20 | Low | wasm_raycaster/src/lib.rs:147–148, 162, 178 | 2-D DDA at fixed `map_y` + faked pitch: no vertical occlusion; `max_steps=200` hard-caps reach at 40 m (0.2 scale) | document as legacy limits (already "kept for reference"); if revived, use 3-D DDA |
| F21 | Low | tests/collision_diagnostics.rs:25–27 | `grid_wall_boxes_at_spawn_x` name says "x≈5" but scans the entire grid | rename (`grid_wall_boxes`) |
| F22 | Low | static/worker.js:79–81 | `console.debug` per generated chunk — log volume at 32 workers | keep behind a debug flag or throttle |
| F23 | Low | scripts/build-single-file.mjs:27–38 | `mustReplace` checks count but `replace` only replaces the first occurrence — correct only because every call passes count=1 | assert `count === 1` or loop `replace` count times |
| F24 | Low | examples/dump_blueprints.rs:65, 94 | hard-coded absolute artifact dir `/home/dbslim/.gemini/...` | read from env/CLI arg |

---

## 4. Big-O table (audited algorithms)

| Algorithm | Complexity (time / space) | Variables | Location |
|---|---|---|---|
| `CPURaycaster::render` full frame | O(Sx·Sy + Sx·K) / O(Sx·Sy) | Sx,Sy = screen w/h; K = 200 max steps | wasm_raycaster/src/lib.rs:81–255 |
| DDA ray march (per column) | O(K) worst, O(1) per step | K ≤ 200 | lib.rs:151–168 |
| `get_voxel` / `get_light` | O(1) | — | lib.rs:53–79 |
| `set_grid` | O(W·H·D) | voxel volume | lib.rs:34–47 |
| `grid_wall_boxes_at_spawn_x` | O(W·H·D) + chunk generation | voxel volume | tests/collision_diagnostics.rs:25–51 |
| `floor_boxes` | O(W·D) | | tests/collision_diagnostics.rs:53–77 |
| `player_overlaps` | O(1) | | tests:80–100 |
| surface diagnostics scenarios | O(W·H·D) build + O(Q) filter | Q = quads | tests/surface_rendering_diagnostics.rs |
| `hollow_room`/`checkerboard` grid builders | O(E³) | E = edge (≤ 96) | benches/generation.rs:42–67 |
| bench: dense octree build (measured) | O(8^D) worst / O(V) best | D = depth ≤ 7 | benches/generation.rs:69–95 (callee not audited) |
| bench: `compress_svdag` (measured) | O(N) dedup (claimed) | N = nodes | benches/generation.rs:142–176 |
| bench: `generate_region_plan` density sweep (measured) | O(A²) candidate search (documented) | A = assemblies | benches/generation.rs:178–197 |
| `generate_samples` batch | O(seeds·steps²·cw·cd·H) ≈ 375M voxel reads @ defaults | 100 seeds × 100 chunks × 2500 cols × H | examples/generate_samples.rs:27–68 |
| `slice_view` | O(chunks²·cols + (span·scale)²) | 256 chunks, 800² px @ defaults | examples/slice_view.rs:42–75 |
| `section_view` SVG build | O(W·H) appends, O(output) | | examples/section_view.rs:110–126 |
| `plan_view` chunk_grid | O((REGION/chunk)²) = 100 | | examples/plan_view.rs:97–112 |
| `wasmSourceHash` | O(total source bytes), sequential | | scripts/wasm-source-hash.mjs:17–37 |
| `build-wasm` staging/swap | O(pkg bytes) + 2× source hash | | scripts/build-wasm.mjs:57–91 |
| `build-single-file` | O(index+glue+worker+wasm bytes) | | scripts/build-single-file.mjs:44–106 |
| `check-workflow-contract` | O(yml size) ×3 includes | | scripts/check-workflow-contract.mjs:38–62 |
| worker `processMessage` | O(1) envelope + O(payload) encode (in wasm) | | static/worker.js:59–97 |
| main-thread `parse_payload` | O(payload) copy + O(payload) decode | | worker_source.rs:327–331 |
| ledger `take_exact` | O(q) scan + O(q) remove | q ≤ max_pending ≤ 32 | generation_worker_requests.rs:112–118 |

No accidental O(n²)/O(n³), no `contains()`/`indexOf` inside loops, no per-frame re-sorting, no string
concat in hot loops, no unbounded Vec growth in production paths (all sinks are drained or capped).

---

## 5. Concurrency inventory

| Shared state | Location | Synchronization | Assessment |
|---|---|---|---|
| `queue` promise chain + `wasmReady` | static/worker.js:20–26, 103–105 | JS single-thread, chain serialization | Correct ordering; rejected-init cached forever (F1) |
| worker wasm `thread_local SOURCE` (`RefCell<Option<ArchivedChunkSource>>`) | wasm_frontend/src/lib.rs:1098–1101 | none needed (one JS thread per worker) | Sound |
| `completed`/`failed` `Rc<RefCell<Vec>>` sinks | worker_source.rs:38–39, 60–61 | main-thread event loop, short borrows, `mem::take` on poll (269–275) | Sound, bounded |
| `GenerationWorkerRequests` ledger (`Vec<WorkerRequests>` of `VecDeque`) | generation_worker_requests.rs:14–24 | main-thread only | Sound; O(q) removes (F9) |
| `Worker` objects + `Closure` handlers | worker_source.rs:37, 41–43, 86–130 | kept alive by struct fields; `terminate()` on fatal/error/send-failure | Sound; no respawn (F2) |
| settings statics `AtomicBool`/`AtomicU32` (Relaxed) | wasm_frontend/src/lib.rs:32–122, 60–272 | Relaxed load/store; single-threaded wasm today | Adequate; document invariant |
| `device_lost: Arc<AtomicBool>` | browser_context.rs:20, 61–65, 148–149 | Release store / Acquire load | Correct |
| native HTTP server threads | src/main.rs:25 | none shared; per-connection thread | No races; unbounded spawn (F7) |
| wasm memory of each worker (transferred `buf`) | worker.js:82–96 + glue `.slice()` copy | transfer of a copy | Safe today; fragile to return-type change (F10) |
| world identity (query+seed) main↔workers | index.html:1316–1325; worker_source.rs:132–136; lib.rs:1114–1115 | URL-carried, set before `init()` | Deterministic; no drift |

No data races, deadlocks, livelocks, or priority inversion found. The design (exact-match ledger +
FIFO serial worker + engine-side stale/reality re-validation at engine.rs:858–877) is the correct
shape for a wasm worker pool.

---

## 6. Highest-leverage optimizations (ranked by expected impact)

1. **Fix `generate_samples` dead region-plan + parallelize seeds** (examples/generate_samples.rs:36–38,
   27–68). Deleting the discarded per-seed `generate_region_plan` removes 100 expensive runs, and
   `rayon::par_iter()` over seeds turns a minutes-long batch job into a seconds-long one on any
   multi-core machine (rayon already in dev-deps). Impact: ~4–8× wall-clock on the batch tool, plus
   removed wasted work.
2. **Worker respawn + wasm-init retry** (worker_source.rs:104–130; worker.js:21–24). Without it, a
   single transient worker failure permanently degrades the pool and eventually migrates generation
   to the main thread (frame-time spikes, stutter). Respawn with backoff and re-send `init`; reset
   `wasmReady` on rejection. Impact: long-session stability of the product's core streaming path.
3. **Scale the per-worker archive budget with pool size** (lib.rs:1110). 32 workers × (wasm instance
   + up to 32 MB archive) can approach ~1 GB/tab on high-core machines; budget `32 MB / max(1, n/4)`
   keeps total near the design figure. Impact: worst-case memory footprint on 16–64-thread machines.
4. **SIMD-friendly pixel stores in the raycaster** (lib.rs:83–88, 229–252). Write `u32` per pixel
   instead of 4 scalar `u8` stores; the clear loop's `step_by(4)` currently blocks auto-vectorization
   of the dominant O(Sx·Sy) term. Impact: 2–4× on this renderer's frame cost (legacy reference only).
5. **Deduplicate/robustify the bench suite** (benches/generation.rs:117–124, 153–155, 199–205): drop
   the duplicated `dense` arm, print SVDAG stats for synthetic grids (the docstring promises them),
   add `black_box`, and prefer shorter `measurement_time` over `sample_size(20)`. Impact: halved
   suite runtime, trustworthy comparisons, and honest reporting of the Pi GPU-upload claim.
6. **Cache chunk generation in the native dev server** (src/main.rs:60–112) + bound threads. A tiny
   LRU of the last N chunk responses (fixed seed 42) turns repeated `/maze`/`/octree` probes into
   O(1) hits and removes the unbounded thread-per-connection risk. Impact: dev-server responsiveness
   under bursty browser refreshes.
