# Report: The robust high-performance path for `cpu_splat` — multithreaded wasm baseline, WebGPU as optional accelerator

- **Date:** 2026-08-14 (rev. 2 — supersedes the rev. 1 WebGPU-hybrid-only draft)
- **Scope:** `wasm_frontend/src/adapters/cpu_splatter/`, its browser wiring (`drivers/browser.rs`, `drivers/webgpu/pipelines/cpu_present.rs`), and the deployment target (GitHub Pages, `static/`).
- **Method:** source audit with `file:line` citations + live platform facts (MDN browser-compat-data, fetched 2026-08-14).
- **Question, restated:** "I want the *most robust* thing: a multithreadable wasm build, and I can't rely on WebGPU because base Firefox doesn't have it."

## Verdict (TL;DR)

The fastest **robust** design is a **capability ladder with a threaded-CPU core at the bottom**:

| Tier | Renderer | Threads | Headers/APIs needed | Works in base Firefox? |
|---|---|---|---|---|
| **L0** | CPU splat + SIMD, single thread | 0 | `+simd128` only | ✅ everywhere |
| **L1** | CPU splat, worker pool via `postMessage` + transferables | N | none (no SAB!) | ✅ everywhere |
| **L2** | CPU splat, worker pool via SharedArrayBuffer | N | COOP/COEP (SW-injected on Pages) | ✅ with isolation headers |
| **L3** | WebGPU hybrid (CPU emits splats, GPU rasterizes) | N + GPU | `navigator.gpu` probe | ❌ on Linux / Intel-macOS / Android Firefox (no WebGPU at all) |

The evergreen path is **L1 → L2**: the same tile-parallel CPU raster core, upgraded to
shared memory when `crossOriginIsolated` becomes true. WebGPU (L3) is a **progressive
enhancement behind a hard probe**, never a requirement. The GPU is still used in every
tier — the browser GPU-composites `ImageBitmap`s on the present path — but nothing
breaks when it can't do more.

---

## 1. The compatibility reality (why "robust" must mean CPU-first)

### 1.1 Base Firefox and WebGPU — the facts (MDN browser-compat-data, `api/GPU.json`, live)

Firefox's `navigator.gpu` support is **partial and platform-gated**:

- **Windows**: since Firefox 141 (bug 1972486).
- **macOS (Apple silicon only)**: Firefox 145 (macOS Tahoe) / 147 (older macOS) (bugs 1992212, 1993341).
- **macOS on Intel CPUs: not supported** (bug 2004105).
- **Linux: not supported** (bug 2006676). **Firefox Android: not supported.**
- No WebGPU in service-worker contexts (bug 1942431).
- Chrome: 113+ (Linux only on Intel Gen12+ GPUs since 144); Safari: 26+.

Consequence: on the development machine here (Linux), **base Firefox has no WebGPU
whatsoever** — `navigator.gpu` is simply undefined. Any renderer that requires WebGPU is
broken by default on a large share of the audience. WebGL2, by contrast, works in every
base Firefox build.

### 1.2 wasm threads — what actually gates them

`wasm32-unknown-unknown` + `atomics` needs **SharedArrayBuffer**, which browsers only
allow when the document is cross-origin-isolated: `Cross-Origin-Opener-Policy:
same-origin` **and** `Cross-Origin-Embedder-Policy: require-corp`
(`crossOriginIsolated === true`). Verified in this repo today:

- Build is single-threaded `wasm32-unknown-unknown` (`.github/workflows/pages.yml:29`, `scripts/build-wasm.mjs:49`).
- **No COOP/COEP headers anywhere; no service worker** (grep of `static/`, `scripts/` → zero hits; `static/` ships only `index.html`, `debug.html`, `pkg/`, `worker.js`).

And the deployment host matters: **GitHub Pages cannot set response headers** (no
`_headers` support). The canonical workaround — used by dozens of wasm apps on Pages —
is a **service worker that injects COOP/COEP into the navigation response**
(the `coi-serviceworker` pattern). It works on Pages (HTTPS is guaranteed), with two
caveats: a first-visit reload dance (register SW → reload once → `crossOriginIsolated`
is true), and COEP `require-corp` means cross-origin subresources must carry CORP/CORS
(all of this app's assets are same-origin, so it's a non-issue here).

**The key insight: threads do *not* require SAB.** A worker pool where each worker owns
a copy of the world state and returns rendered tile bands via `postMessage` + transferable
buffers runs in **base Firefox today, with zero headers**. SAB is an *optimization* of
that design (share the atlas + framebuffer instead of copying), not a prerequisite.

### 1.3 The present path that always works

`OffscreenCanvas.transferToImageBitmap()` (Firefox 105+, Chrome 69+, Safari 16.4+) →
`transferFromImageBitmap()` on the visible canvas (Firefox 50+, Chrome 66+, Safari 11.1+)
is universal on modern browsers and **GPU-composited by the browser**. So even a
pure-CPU renderer "uses the GPU" for presentation, with zero-copy ownership handoff and
no main-thread pixel pushing. Canvas2D `putImageData` remains only a last-resort fallback.

---

## 2. Codebase readiness for the threaded core

The CPU splatter is unusually well positioned for this:

1. **Platform-free core.** `adapters/cpu_splatter/mod.rs` is explicitly "platform-free
   (no web-sys)". It can be driven verbatim from worker threads; only the *driver*
   (`cpu_present.rs`, `browser.rs`) is DOM-facing.
2. **Read-only world state after upload.** The SVO atlas is assembled once
   (`upload_atlas` / `update_atlas_rows.rs`) and chunks change rarely — ideal for a
   SAB-shared read-only atlas (L2) or a cheap per-worker copy (L1).
3. **Payload plumbing already exists.** Chunk payloads are already serialized for
   `postMessage` transport (`drivers/worker_source.rs`, `adapters/chunk_codec.rs`) — the
   same codec syncs render workers in L1.
4. **Atomic settings globals already exist** (`lib.rs:28-56`: `AtomicBool`/`AtomicU32`
   settings) — the pattern extends naturally to shared frame state and telemetry.
5. **Determinism is an asset, not an obstacle.** 881 lines of CPU-splatter tests
   (`adapters/cpu_splatter/tests.rs`), golden renders (`tests/render_goldens.rs`), and a
   headless GPU harness (`tests/vulkan_renderers.rs`). The threaded design below keeps
   output **bit-identical** to the single-threaded reference by construction.

The one structural change needed: `DepthTestedSplatTarget` (`write_depth_tested_splats.rs:58`)
owns `Vec<u8>`/`Vec<f32>` buffers; the driver must be able to hand it shared/borrowed
backing memory (SAB slice or owned per-worker buffer) — a refactor, not a redesign.

---

## 3. The robust target: tile-parallel CPU splatting (L0 → L2)

The current frame is single-threaded and *fill-bound*: up to **16M scalar pixel writes**
per frame (`frame_work_budget.rs:20`) against a full-res `f32` depth buffer plus 8 px
coarse hierarchical-Z tiles (`write_depth_tested_splats.rs:11,58`). The threaded design
keeps every algorithm, but partitions the work so no thread contends on another's pixels.

### 3.1 Two-phase, deterministically partitioned pipeline

**Phase 1 — emit (parallel over chunks).** Chunks are independent: traversal
(`traverse_voxel_scene/`), detail/LOD selection, per-chunk light clustering
(`select_lights_for_chunk.rs`), and per-splat shading (`shading/shade_visible_splat.rs`)
all depend only on the chunk, the camera, and the frame's light list. Each thread claims
chunks from an atomic counter (load balancing), traverses, and appends packed splat
records to that chunk's own stream — so **a chunk's output is identical regardless of
which thread ran it**. Per-chunk scratch (`chunk_light_scratch`) becomes per-thread
scratch; the hero-visibility cache (`resolve_hero_light_visibility.rs`) becomes
per-thread or `Mutex`-guarded (it is a pure cache; results are deterministic either way).

**Binning (cheap, replaces the per-pixel loop).** Each emitted splat is appended to the
per-tile lists it covers. Cost is ∝ footprint ÷ tile-area — *strictly less* than the
per-pixel writes it replaces (one append per covered 32–64 px tile, not per pixel), and
bounded by the same 16M-write budget.

**Phase 2 — raster (parallel over tiles).** Each thread claims tiles from an atomic
counter. A tile's work is its splat list, depth-tested against that tile's private slice
of the depth buffer (the existing fine-depth + coarse-entry + filled-counter fields map
1:1 from `DepthTestedSplatTarget`; now cache-resident and contention-free). Each tile's
list is ordered canonically — chunk draw order × in-chunk emission order — so output is
**bit-identical to today's front-to-back reference**, including the coplanar
`equal_depth_wins` tie rule (`write_depth_tested_splats.rs:31-49`).

**Budget & determinism contract.** The work budget (`frame_work_budget.rs`) is consumed
in *canonical order*: each chunk receives a deterministic allowance computed from the
same per-target density functions, and the global telemetry counters become atomics. A
frame's output depends only on its inputs, never on thread scheduling. Goldens pin L0;
L1/L2 must reproduce L0 output exactly (same partition, same order, same budget gates).

**SIMD.** Build with `+simd128` (all modern engines; no isolation needed): 4-wide `f32`
depth compares on the reciprocal-depth buffer, wide `u8` color stores. This is pure
headroom on the phase-2 loop — a ~2–4× win on the inner loop before threads are even
involved. The threads build additionally uses `+atomics,+bulk-memory,+mutable-globals`.

### 3.2 L1: workers without SAB (base Firefox today)

- N render workers; each holds a copy of the atlas (synced on chunk change via the
  existing `postMessage` payload codec — chunk payloads already serialize cleanly).
- Workers rasterize their assigned tiles into their own framebuffer band → `OffscreenCanvas`
  → `transferToImageBitmap()` → `postMessage` the bitmap to the main thread →
  `transferFromImageBitmap()` on the visible canvas. Zero-copy handoff, GPU-composited.
- Cost: N × atlas memory (tens of MB at 25 chunks — acceptable); main thread never
  renders, so rAF jank from rendering disappears.

### 3.3 L2: SAB upgrade (when `crossOriginIsolated`)

- Same raster core; now the atlas lives once in a SAB read-only region, the framebuffer
  + depth buffer once in shared memory, and threads claim work via atomics
  (wasm-bindgen-rayon-style pool).
- Enabled purely by deployment: add the header-injecting service worker to `static/`,
  reload-once detection, and `crossOriginIsolated` feature-detect. **No renderer code
  change** beyond backing the buffers with shared memory.

---

## 4. L3: WebGPU as a probed accelerator (not a requirement)

When (and only when) `navigator.gpu` exists **and** adapter/device creation succeeds —
Windows Firefox 141+, Apple-silicon macOS 145+, Chrome, Safari 26+ — reuse the rev. 1
hybrid design as the top tier:

- CPU Phase 1 emits packed 16-byte splat records (the `GpuPackedFace`/`GpuPackedSurfel`
  pattern, `gpu_types.rs:241-246`) into **retained per-chunk buffers**
  (`SplatPipeline::upload()` pattern, `splat.rs`) — re-uploaded only when a chunk changes.
- GPU expands quads in the vertex stage (proven in `splat.rs`/`supply_labels.rs`),
  hardware depth test replaces `DepthTestedSplatTarget`, and the ported `shading/`
  modules run in the fragment shader (15-step irradiance quantization already exists in
  `splat.wgsl:32-126`; fog/tonemap/dither already exist in `common.wgsl:430-452`).
- Shadow rays reuse the raymarch's `DIRECT_VISIBILITY_TRACE_BUDGET` primitive
  (`raymarch.wgsl:29`) and the existing `HeroShadowMap` — exactly as `SplatPipeline` does.

The probe must be **hard**: `requestAdapter`/`requestDevice` inside `try/catch` with a
timeout, and any failure (absent API, null adapter, device lost at init) falls back to
L2/L1 with no user-visible difference. Base-Firefox-on-Linux simply never enters this
path. The frozen WebGL2 stack (`splat_webgl/`) remains the mid-tier for browsers with
WebGL2-only (works in every base Firefox), and the rev. 1 hybrid can be ported to it
later if it ever earns maintenance again.

---

## 5. Roadmap

| Phase | Work | Payoff | Risk | Gate |
|---|---|---|---|---|
| **P0** | `+simd128` build; vectorize the phase-2 pixel loop; keep everything else | 2–4× inner loop, zero compat risk | low | `benches/rendering.rs` before/after |
| **P1** | Tile-parallel core, single thread (pure Rust) | the whole design validated natively | medium (determinism) | goldens + 881 tests pass bit-identical |
| **P2** | L1: worker pool via `postMessage` + band bitmaps | threads in **base Firefox**, no headers | medium (atlas sync) | in-browser fps on Linux Firefox |
| **P3** | L2: SAB + SW header injection on Pages | shared-memory threads, main-thread fully free | low (deployment only) | `crossOriginIsolated` true on Pages |
| **P4** | L3: WebGPU probe + hybrid accelerator | GPU raster when present | medium (golden parity) | `vulkan_renderers.rs` CPU-vs-GPU diff |

Order matters: P1 keeps determinism provable before any threading; P2 gives the
robustness win (base Firefox threads) before the isolation dance of P3; P4 is pure upside
on top.

---

## 6. Key facts & citations

- Firefox WebGPU platform gates: MDN browser-compat-data `api/GPU.json` (live): FF 141 Windows; FF 145/147 Apple-silicon macOS; **no Linux** (bug 2006676), **no Intel macOS** (bug 2004105), **no Android**, no SW contexts (bug 1942431); Chrome 113+/144 Linux Gen12+; Safari 26+.
- SAB requires COOP/COEP → `crossOriginIsolated`; GitHub Pages cannot set headers → SW header injection (coi-serviceworker pattern) + one reload.
- `OffscreenCanvas.transferToImageBitmap`: FF 105+, Chrome 69+, Safari 16.4+; `transferFromImageBitmap`: FF 50+, Chrome 66+, Safari 11.1+ (MDN compat, live).
- Current build is single-threaded `wasm32-unknown-unknown` (`pages.yml:29`); no SW/COOP/COEP in `static/` (grep-verified).
- CPU splatter is platform-free (`cpu_splatter/mod.rs`), fill-bound (16M pixel-write ceiling, `frame_work_budget.rs:20`), with a hierarchical-Z target that maps 1:1 to per-tile slices (`write_depth_tested_splats.rs:11,58`).
- Chunk payloads already serialize for `postMessage` (`chunk_codec.rs`, `worker_source.rs`); atomic settings globals already exist (`lib.rs:28-56`).
