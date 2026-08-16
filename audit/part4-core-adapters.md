# Static Audit — Part 4: Core Adapters, Frameworks/Drivers, main.rs

**Scope:** `src/adapters/**` (blueprint_renderer, voxel_mapper, chunk_voxel_renderer,
brick_pool_gpu_serializer, octree_gpu_serializer, web_renderer, ascii_renderer,
json_presenter, material_palette, png_writer) + `src/frameworks_drivers/**`
(simple_noise, std_telemetry) + `src/main.rs` + `src/lib.rs` (~3.5k lines).

**Method:** pure static file reading (no compilation, no tests, no execution).
Complexity claims for out-of-scope callees were verified by reading the called
functions (`VoxelGrid::get/get_light/get_face_occlusion` are O(1) bounds-checked
array reads; `Polygon2::contains` is O(vertices) even-odd; `ProbeGrid::from_voxel_grid`
visits each voxel exactly once).

**Headline:** No data races, no shared mutable state, no atomics/locks/channels in
this area — the concurrency surface is exactly one `thread::spawn` per accepted
connection in `main.rs`. The two dominant issues are (1) a thread-per-connection
HTTP server with no bound, no read timeout, and no request cache, and (2) the
voxel mesher, which is O(n) but pays a large constant (per-probe recomputation of
face attributes plus two layers of dynamic dispatch) on the wasm hot path.

---

## 1. Big-O table of main algorithms

Variables: `V = W·H·D` voxels in a chunk grid (high spec ≈ 202×202×122 ≈ 5.0M);
`Q` = emitted quads; `N` = octree/brick-pool nodes; `P` = probe-grid cells;
`C` = corridors in a region plan, `L` = avg corridor path segments; `A` = assemblies,
`S`/`Z`/`E` = sub-spaces/ceiling zones/entrances per assembly; `W,D` = SVG cell
counts; `gw,gd,gh` = chunk-renderer grid dims; `n` = pixel count for PNG.

| Algorithm | Time | Space | Where |
|---|---|---|---|
| `map_voxel_region_with_policy` (6-direction greedy surface extraction) | O(V) amortized — 2 passes each over X/Y/Z slices, each cell probed amortized O(1) by `visited` mask | O(V) worst-case quads + O(max slice) visited | `voxel_mapper.rs:219`–326 |
| `greedy_mesh_2d` per slice | O(w·h) amortized (each marked cell probed ≤ a few times; failed row probes bounded by W per row) | O(w·h) visited + O(Q) | `voxel_mapper.rs:347`–409 |
| `face_key`/`attrs` per probe | O(1) but ~4 array lookups + palette lookup; **recomputed per probe per direction** (see §3.3) | — | `voxel_mapper.rs:231`–240, 328–344 |
| `ProbeGrid::from_voxel_grid` | O(V) (probe buckets partition the grid exactly) | O(P) | `probe_grid.rs:104` (called `web_renderer.rs:57`) |
| `OctreeGpuSerializer::serialize_to_gpu_data` | O(N) | O(N) | `octree_gpu_serializer.rs:46`–102 |
| `BrickPoolGpuSerializer::serialize_to_gpu_data` | O(N + V) | O(N + V) (clones arena) | `brick_pool_gpu_serializer.rs:80`–137 |
| `WebRendererAdapter::to_octree_binary` | O(N) | O(N) | `web_renderer.rs:146`–173 |
| `WebRendererAdapter::to_octree_json` | O(N) with per-texel `to_string` alloc; capacity underestimated (11 B/texel worst vs 8 B reserved) | O(N·11) | `web_renderer.rs:110`–132 |
| `JsonPresenter::render_voxels` | O(Q), one `format!` allocation per quad | O(Q·100+) | `json_presenter.rs:8`–37 |
| `to_low_spec_surface_binary` | O(V + Q + P) | O(V + Q + P) | `web_renderer.rs:43`–106 |
| `render_region_blueprint_svg` | O(A·bx·bz·V₁ + ΣC·L + gridlines) where bx,bz = bay counts, V₁ = footprint vertices (≈4–8) | O(output) | `blueprint_renderer.rs:23`–306 |
| `render_voxel_chunk_svg` | **O(W·D·(H + C·L + A·V₁))** — nested per-cell scans over corridors/assemblies (see §3.4) | O(output) | `blueprint_renderer.rs:316`–537 |
| `render_large_voxel_blueprint_svg` | O(total_w·total_d) | O(output) | `blueprint_renderer.rs:574`–802 |
| `render_chunk_voxel_blueprint_svg` | O(gw·gd·gh + C·L + A·(S+Z+E)) — column collapse is a full-height scan per cell (see §3.6) | O(output) (256 KB prealloc) | `chunk_voxel_renderer.rs:173`–573 |
| `AsciiRenderer::render` | O(W·D) | O(output) | `ascii_renderer.rs:13`–68 |
| `encode_rgb` (PNG) | O(n) but slow constant: bit-by-bit CRC (8 iterations/byte) + 2 integer modulos/byte in adler32 | O(n) | `png_writer.rs:16`–43, 76–108 |
| `SimpleNoiseProvider::evaluate_2d` | O(1), 4 lattice hashes per sample; no lattice caching (4× redundant hashing across adjacent samples) | O(1) | `simple_noise.rs:43`–69 |
| `StdTelemetry::now_micros` / `log` | O(1); `log` calls `SystemTime` twice | O(1) | `std_telemetry.rs:12`–25 |
| `main` /maze & /octree handlers | O(V) generation + lighting bake + O(V) meshing + O(Q) serialization **per request, no cache** | per-thread heap (≈5M-voxel grid) | `main.rs:81`–91, 128–139 |
| `serve_static` | O(file size); `fs::read` loads whole file | O(file size) | `main.rs:177`–196 |

---

## 2. Concurrency inventory (scope area)

| Shared state | Where | Synchronization | Assessment |
|---|---|---|---|
| Accept queue of `TcpListener` | `main.rs:15`, `22` | Kernel-serialized | Fine |
| `TcpStream` per connection | `main.rs:24`–27 | Moved (`move`) into the spawned thread; exclusively owned | Fine — no sharing, no race |
| `DEFAULT_MATERIAL_PALETTE` (ZST `DefaultMaterialPalette`) | `material_palette.rs:93` | Immutable `static`; zero-sized, stateless | Fine |
| `MATERIAL_VISUALS` const array | `material_palette.rs:21` | Immutable const | Fine |
| stdout (telemetry) | `std_telemetry.rs:24` (`println!`) | Per-call stdout lock | Lines cannot interleave mid-write; worst case is interleaved whole lines |
| `thread_local SHEET_CACHE` (`RefCell<HashMap<…, Rc<Sheet>>>`) — *out of scope file, on the call path of main.rs threads via `generate_chunk` → `level_zero`* | `use_cases/level_zero/fabric_ca.rs:84`–87 | `thread_local` + `RefCell`: no cross-thread sharing | Per-thread, **never evicted**; under thread-per-connection each thread is always cold and the cache dies with the thread (§3.2) |
| RNG / seeds | `main.rs:81`, `128` (seed `42`), `simple_noise.rs:16`–29 | None needed — per-request provider, deterministic hash | Fine; output is a pure function of (x, z, spec, seed) |

**Negative inventory:** no `Mutex`, `RwLock`, `Atomic*`, `mpsc`, `Arc<…>` shared
data, `static mut`, or `unsafe` in the audited area. **No data races, torn reads,
lost updates, or deadlocks were found in scope.** The only memory-ordering
consideration in the repo (wasm settings atomics) lives in `wasm_frontend` and is
out of scope.

### Worker lifecycle (the only threads in the repo — `main.rs`)

- **Spawn:** one `std::thread::spawn` per accepted connection (`main.rs:25`); the
  `JoinHandle` is dropped immediately (detached). **No bound on the number of
  concurrent threads** (no semaphore, no pool, no queue).
- **Lifetime:** thread lives exactly as long as one request; process exit kills
  in-flight requests mid-generation (acceptable for a demo, but no graceful drain).
- **Communication:** none between workers; the browser is the only peer, over the
  socket. No message ordering, backpressure, or lost-reply concerns *inside* the
  process — replies are the socket writes themselves.
- **Error handling:** `stream.read`/`write_all` errors are swallowed (`main.rs:38`,
  `91`, `157`, `196`); a client that disconnects mid-generation makes the worker
  finish a full O(V) generation + meshing before the write fails.
- **Re-entrancy / JS event loop:** not applicable natively; the wasm-side
  `requestAnimationFrame`/worker interplay is out of scope.
- **Determinism vs parallelism:** generation is pure per (x, z, spec, seed=42);
  concurrent workers for the *same* chunk produce identical bytes and race only on
  stdout telemetry — i.e. the design is safely parallelizable but currently wastes
  the parallelism on duplicate work (§3.1).

---

## 3. Findings

### 3.1 Unbounded thread-per-connection server; no bound, no backpressure
- **SEVERITY: High** — `src/main.rs:25`–27 (spawn), 36–160 (per-request work)
- Every connection gets a fresh OS thread that runs the *full* pipeline: region
  plan derivation, voxelization of a ≈202×202×122 grid, full lighting bake, greedy
  meshing, and JSON/binary serialization (high spec: `chunk_size 20 / voxel_scale
  0.1` → 200 + 2 padding cells/axis; `generate_chunk.rs:15`–32, 169–177). There is
  no cap on concurrent connections: N clients ⇒ N concurrent 5M-voxel generations
  plus N thread stacks; a client that connects and sends nothing pins a thread
  forever (`read` at `main.rs:38` has no timeout). This is a CPU/memory/thread
  exhaustion (DoS) vector, and on top of it, per-thread work duplicates what other
  threads may already be computing.
- **Fix:** replace with a bounded worker pool (e.g. `std::thread` × N cores +
  `mpsc` channel, or a semaphore around `thread::spawn`); add
  `stream.set_read_timeout(Some(...))` before reading; consider
  `TcpListener::set_nonblocking` + polling if you want to stay dependency-free.

### 3.2 No request cache — every request recomputes the same chunk from scratch
- **SEVERITY: High** — `src/main.rs:76`–84 (maze), 122–139 (octree)
- The chunk is a pure function of `(chunk_x, chunk_z, spec, seed=42, level)` —
  `SimpleNoiseProvider` is stateless and `execute` is deterministic — yet each
  request builds a new noise provider/generator and regenerates grid + lighting
  bake + mesh. A browser panning or zooming across chunk boundaries re-requests
  the same cells (the wasm front-end issues `/maze?x=…&z=…` per chunk), so the
  server redoes O(V) work per duplicate. The repo already contains the bulk-loading
  seam (`execute_from_plans`/`WorldBlock`, `generate_chunk.rs:363`) that main.rs
  does not use.
- **Fix:** memoize serialized payloads behind a bounded LRU:
  `Mutex<HashMap<(i32,i32,u8), Arc<Vec<u8>>>>` keyed by (chunk_x, chunk_z, spec),
  evicting by count/bytes; or precompute a region of chunks once with the WorldBlock
  path and slice it per request.

### 3.3 Voxel mesher: per-probe recomputation + double dynamic dispatch (wasm hot path)
- **SEVERITY: High** — `src/adapters/voxel_mapper.rs:231`–240 (`attrs` closure),
  `328`–344 (`face_key`), `351` (`get_face: &dyn Fn…`), `207` (`&dyn VoxelNeighborhood`),
  `64`–79 (`GridNeighborhood::voxel`)
- The algorithm is O(V) (good), but every probe — and each cell is probed several
  times by `greedy_mesh_2d` (width scan, height scan, seed) and again per direction
  (6 directions) — re-executes two `neighborhood.voxel()` calls (each a bounds-checked
  index computation) plus `face_key`, which re-reads `get_face_occlusion` and
  `get_light` and re-looks-up the palette color. On a 5M-voxel chunk this is
  ≈30M probes × ≈6 array ops ≈ 150M+ bounds-checked accesses, through two layers of
  trait-object dispatch (`&dyn Fn` + `&dyn VoxelNeighborhood`) that prevent inlining.
  `map_voxel_grid_with_padding` is the front-end's per-chunk mesher
  (`wasm_frontend/src/adapters/local_chunk_source.rs:245`), so this runs in the
  browser on every chunk load.
- **Fix:** (a) precompute per-axis face-key planes once — three O(slice) passes
  materializing `Option<FaceKey>` (or packed `(material,color,light,ao)` u64s) for
  the two directions of each axis, then let `greedy_mesh_2d` read flat arrays;
  (b) make `greedy_mesh_2d` generic over `F: Fn(usize,usize)->Option<FaceKey>` and
  the neighborhood generic/monomorphized so both dispatches inline; (c) precompute
  the solidity mask (`voxel != AIR`) once per axis so the neighbor test is a byte
  load instead of two bounds-checked lookups.

### 3.4 SVG chunk renderer: per-cell × per-corridor × per-segment distance scans
- **SEVERITY: Medium** — `src/adapters/blueprint_renderer.rs:376`–394 (corridor
  loop calling `point_to_polyline_distance`, itself O(L) at `539`–554), `400`–407
  (assembly `footprint.contains`, O(V₁) each), all inside the W×D cell loops at
  `364`–365; semantic result is used only at `518`–523 (air cells)
- `render_voxel_chunk_svg` is O(W·D·(H + C·L + A·V₁)): for a 200×200 high-spec grid
  every cell computes a distance to every corridor polyline and a containment test
  in every assembly — millions of `sqrt`-bearing distance evals per render — and it
  does so even for cells whose voxel class already decides the color (the semantic
  background is only drawn when `voxel_class` is `None`). Called from
  `wasm_frontend/src/lib.rs:751` for in-browser blueprints.
- **Fix:** classify voxels first and compute the semantic class lazily only for
  air cells (turns the common case into O(W·D·H)); when semantics are needed,
  rasterize corridor strokes/assembly footprints to a cell bitmap once per render
  (O(W·D + C·L + A·V₁)) instead of per-cell point queries, or use a segment
  bounding-box prefilter.

### 3.5 PNG encoder: bit-by-bit CRC and per-byte adler32 modulos
- **SEVERITY: Medium** — `src/adapters/png_writer.rs:76`–83 (`adler32`: two
  `% 65521` per byte), `92`–103 (`Crc::write`: 8-iteration inner loop per byte)
- Both checksums are O(n) with a slow constant and are purely serial. A 1M-pixel
  debug slice (the documented use, `png_writer.rs:4`–6; used by
  `examples/slice_view.rs` and `wasm_frontend/examples/surfel_view.rs`) costs tens
  of ms of integer-division and branchy bit loops, dominating the actual pixel copy
  (~ms). This is debug imagery, so Medium rather than High.
- **Fix:** defer the adler32 modulo until `a`/`b` would overflow (every 5552 bytes,
  the classic NMAX trick) — ~10–20× faster; replace the bit-at-a-time CRC with a
  table-driven (or slice-by-8) CRC-32 — ~8–16× faster; both are ~30 lines and stay
  dependency-free.

### 3.6 Column collapse scans full height with no early exit; visited buffer re-allocated per slice
- **SEVERITY: Low** — `src/adapters/chunk_voxel_renderer.rs:263`–294;
  `src/adapters/voxel_mapper.rs:354`
- `render_chunk_voxel_blueprint_svg` scans all `gh` layers per (vx,vz) column even
  though material priority 4 (wall/red-wall/tree, `chunk_voxel_renderer.rs:124`–132)
  is final — an early break (or a top-down scan that stops at the first
  non-transparent winner) cuts the loop to ~2 layers for typical rooms.
  `greedy_mesh_2d` allocates a fresh `vec![vec![false; h]; w]` per slice (2·(h+d+w)
  allocations per mesh, each O(slice)); a single reusable buffer sized
  max(w,h)² would remove the churn.
- **Fix:** early-exit column scan; hoist/reuse the visited buffer across slices.

### 3.7 HTTP request handling: single 1024-byte read, no timeout, silent truncation
- **SEVERITY: Medium** — `src/main.rs:37`–44
- One blocking `read` of at most 1024 bytes is treated as the whole request. TCP
  fragmentation can deliver a partial request line (spurious 404), and a query
  string longer than the buffer is silently truncated — the chunk is then generated
  at the wrong coordinates with no error. Combined with the absent read timeout,
  this is also the slowloris pinning vector for §3.1.
- **Fix:** loop until `\r\n\r\n` (with a hard cap, e.g. 8 KB), set a read timeout,
  and reject oversized request lines with 431.

### 3.8 JSON texel serialization: per-texel allocation + padding payload
- **SEVERITY: Low** — `src/adapters/web_renderer.rs:115` (capacity `len*8+128`
  underestimates the 11-byte worst case `4294967295,`), `124`–129 (per-texel
  `to_string`)
- `to_octree_json` builds a JSON array of up to ~10-digit u32s; the reserved
  capacity is ~27% short, forcing reallocations, and each texel allocates its own
  `String`. It also serializes the row-padding texels (`octree_gpu_serializer.rs:87`
  –93, up to 1023 dummy nodes), inflating the payload ~1 KB per chunk for nothing.
- **Fix:** `use std::fmt::Write` and `write!(json, "{val},")` into the preallocated
  buffer; skip padding texels for JSON (keep them only for the row-aligned binary
  path).

### 3.9 Thread-local sheet cache: unbounded and always cold under this server
- **SEVERITY: Low (cross-cutting)** — `src/use_cases/level_zero/fabric_ca.rs:84`
  –87 (out of scope file, on the call path of `main.rs:81` → level-0 generation)
- `SHEET_CACHE` is a per-thread `HashMap` with no eviction. Under the
  thread-per-connection server every request runs in a brand-new thread, so the
  cache is always empty (all the benefit lost) and grows until the thread exits;
  on the wasm main thread it grows forever. Either way it is a per-thread memory
  leak with no capacity bound.
- **Fix:** bound it (evict by count) and either share it (e.g.
  `Arc<Mutex<HashMap>>` with `entry()` — contention is negligible vs. chunk
  generation) or drop it in favor of a request-scoped memo.

### 3.10 Minor items (Low)
- `json_presenter.rs:24`–29: one `format!` allocation per quad — use `write!` into
  the preallocated `json` (`json_presenter.rs:9` already reserves `len*100`).
- `blueprint_renderer.rs:62`–68, 84–101, 110–114, 217–221, 243–247, 261–272,
  513–524, 734–738: `svg.push_str(&format!(…))` per element — `fmt::Write` avoids
  the temporary `String` per element (output-size-linear work either way).
- `blueprint_renderer.rs:196`–224: bay-loop columns re-test `footprint.contains`
  (O(V₁)) per column; a rectilinear footprint can be tested with 4 comparisons via
  `bounds()` plus an orientation check — negligible at current sizes, cheap to do.
- `octree_gpu_serializer.rs:48` hard-codes `1024` while `brick_pool_gpu_serializer.rs:17`
  exports `ATLAS_WIDTH` — the two will drift; reference the constant.
- `brick_pool_gpu_serializer.rs:121`: `pool.voxels.clone()` is O(V) — fine, but
  `Vec::with_capacity` + `extend_from_slice` avoids the temporary and is identical
  in spirit.
- `simple_noise.rs:60`–63: adjacent samples recompute shared lattice corners (each
  corner feeds up to 4 samples) — a per-row lattice strip cache removes ~4×
  redundant hashing; impact small (hash is ~8 ops).
- `std_telemetry.rs:19`–25: `log` calls `SystemTime::now` twice per line; harmless.
- `main.rs:172`–175: path-traversal guard is solid (`..` and `\` rejected); consider
  also rejecting `//` and empty segments as hardening. Binding `0.0.0.0` with no
  auth (`main.rs:15`) exposes the server to any local-network client; demo-acceptable.

---

## 4. Findings summary table

| # | Severity | File:line | Issue | Fix |
|---|---|---|---|---|
| 3.1 | High | `src/main.rs:25`–27, 38 | Unbounded thread-per-connection; no timeout; each thread runs full 5M-voxel generation + bake + serialize | Bounded worker pool (mpsc or semaphore); `set_read_timeout` |
| 3.2 | High | `src/main.rs:76`–84, 122–139 | No cache; identical chunks regenerated per request (pure function of x,z,spec,seed) | LRU `Mutex<HashMap<…, Arc<Vec<u8>>>>` keyed by (x,z,spec); or WorldBlock bulk path |
| 3.3 | High | `voxel_mapper.rs:231`–240, 328–344, 351, 207 | Mesher O(V) but per-probe recomputation of light/occlusion/color + double dyn dispatch on wasm hot path | Precompute per-axis face-key planes; generic (monomorphized) mesher; precomputed solidity mask |
| 3.4 | Medium | `blueprint_renderer.rs:364`–409, 518–523 | O(W·D·(C·L + A·V₁)) semantic distance scans per cell, even when unused | Lazy classification for air cells only; rasterize semantics to a cell bitmap |
| 3.5 | Medium | `png_writer.rs:76`–83, 92–103 | Bit-by-bit CRC + 2 modulos/byte adler32 → tens of ms per 1M-px slice | Adler NMAX deferral; table-driven CRC |
| 3.6 | Low | `chunk_voxel_renderer.rs:263`–294; `voxel_mapper.rs:354` | Full-height column scans without early exit; visited buffer re-allocated per slice | Early break at priority 4; reuse one visited buffer |
| 3.7 | Medium | `main.rs:37`–44 | Single 1024-byte read: fragmentation → 404, long query → silent wrong coords | Read until CRLFCRLF with cap; timeout; 431 on overflow |
| 3.8 | Low | `web_renderer.rs:115`, 124–129 | Per-texel allocs; capacity under-reserved; padding texels serialized | `fmt::Write`; exact capacity; trim padding for JSON |
| 3.9 | Low | `fabric_ca.rs:84`–87 (call path) | Per-thread sheet cache unbounded and always cold under thread-per-connection | Bound + share (mutex) or drop |
| 3.10 | Low | `json_presenter.rs:24`; `blueprint_renderer.rs` several; `octree_gpu_serializer.rs:48`; `simple_noise.rs:60`; `std_telemetry.rs:19` | Per-element `format!` allocs; duplicated `1024` constant; redundant lattice hashing; double `SystemTime` | `fmt::Write`; shared `ATLAS_WIDTH`; strip cache; single timestamp |

**Critical findings (data race / deadlock / correctness): none in scope.** The
audited area has no shared mutable state; the only concurrency primitive is the
detached per-connection thread, which is safe but unbounded (§3.1). The closest
thing to a correctness hazard is silent request truncation (§3.7).

---

## 5. Highest-leverage optimizations (ranked)

1. **Server-side chunk cache (main.rs)** — removes the entire per-request O(V)
   regeneration + lighting bake + meshing for repeated/panning requests; the
   response becomes a `memcpy` from an `Arc<Vec<u8>>`. Expected impact: one to two
   orders of magnitude on steady-state browser panning, and it also lets §3.1's
   bounded pool absorb bursts. (Combo with #2.)
2. **Bound concurrency: worker pool + read timeout (main.rs)** — eliminates the
   thread/CPU/memory exhaustion vector and the slowloris pin; makes per-thread
   overhead (cold thread-local caches, §3.9) disappear if workers are reused.
3. **Precompute face-key planes + monomorphize the mesher (voxel_mapper.rs)** —
   the front-end meshes every chunk through this path (`local_chunk_source.rs:245`);
   cutting per-probe recomputation and dynamic dispatch should give a 2–4× meshing
   speedup on 5M-voxel chunks and shrink per-frame stutter in wasm.
4. **Lazy + rasterized semantics in `render_voxel_chunk_svg` (blueprint_renderer.rs)**
   — eliminates the dominant cost of in-browser blueprint renders (millions of
   point-to-segment distances per render) by only classifying air cells and
   pre-rasterizing corridors/assemblies.
5. **Faster PNG checksums (png_writer.rs)** — adler deferral + table CRC make
   megapixel debug-slice export ~10× faster with no dependencies.
6. **Early-exit column collapse + reused visited buffer (chunk_voxel_renderer.rs,
   voxel_mapper.rs)** — small but free wins in two per-chunk loops; do these while
   touching the mesher for #3.

---

## 6. Notes on what was checked and found clean

- `lib.rs` is 4 lines of module declarations — nothing to audit.
- `material_palette.rs` is an O(1) const-array lookup (`material_visual`,
  `material_color_f32`) with a correct `UNKNOWN` fallback — no issues beyond style.
- `ascii_renderer.rs` is O(W·D) string building with a preallocated target — clean.
- Both GPU serializers are single-pass O(N)/O(N+V) with correct preallocation,
  row-aligned padding (verified against the documented encodings and the
  shader-walk test at `brick_pool_gpu_serializer.rs:173`–226) — the only note is
  the duplicated `1024` constant and the JSON padding payload (§3.8).
- `web_renderer.rs` binary paths use exact `Vec::with_capacity` and
  `extend_from_slice` — clean; the JSON path is the one with alloc churn (§3.8).
- `simple_noise.rs` and `std_telemetry.rs` are stateless, deterministic, and
  thread-safe by construction (no shared RNG, no `static mut`).
- Determinism: `main.rs:81`/`128` use a fixed seed `42`; identical (x,z,spec)
  requests produce byte-identical payloads — this is exactly what makes the §3.2
  cache safe and correct.
