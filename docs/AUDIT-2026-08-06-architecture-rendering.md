# Vackrooms — Architecture & Rendering Audit

- **Date:** 2026-08-06
- **Scope:** Clean Architecture layering (root `vackrooms` crate and `wasm_frontend` crate), rendering methods across WebGPU / WebGL2 / CPU, build health, dead code, test coverage, and doc drift.
- **Method:** static source audit with `file:line` citations, cross-layer import greps, and `cargo check --workspace --all-targets` / `cargo clippy --workspace --all-targets`.
- **Baseline:** ~66k lines of Rust. 4 workspace members: `vackrooms` (root), `wasm_frontend`, `wasm_raycaster` (legacy, dead), plus benches.

## Verdict

The Clean Architecture skeleton is **genuine, not decorative**: two crates with real dependency inversion, mechanical platform isolation, and deterministic generation — and almost no boundary violations. The main problems are **doc drift** (ARCHITECTURE.md now contradicts the actual rendering strategy) and a **frozen-but-shipped second GPU stack** that duplicates ~30% of the active one. Nothing is broken; the risk is maintenance drag and misdirection.

---

## 1. Clean Architecture — root crate (`src/`)

### Strengths

- Layers map cleanly: `domain/entities`, `domain/use_cases`, `use_cases`, `adapters`, `frameworks_drivers`, wired through `src/lib.rs` and `src/main.rs` (a pure `std::net` static HTTP server, `src/main.rs:3`). No dependency anywhere on `wasm_frontend`.
- DI works on the main path: `main.rs:76-84` injects `SimpleNoiseProvider` / `StdTelemetry` into use cases that only hold traits (`generate_chunk.rs:298-310`); the whole level pipeline threads `&dyn NoiseProvider`.
- Determinism is well engineered: `rand` with `default-features = false` (`Cargo.toml:18-21`), seeded `StdRng` everywhere, stable hash primitives (`anomalies/determinism.rs:15-43`), row-major plan iteration (`infinite_level.rs:19-28`), no `thread_rng` / `SystemTime` / `Instant` leaks into generation.

### Violations

1. **The one production dependency-rule violation:** `src/domain/entities/surface_quality_profile.rs:20` — a *domain entity* embeds an *adapter* type, `use crate::adapters::voxel_mapper::SurfaceMeshingPolicy`, and uses it in every factory (lines 38, 58, 86, 100). The arrow points outward. Easy fix: move `SurfaceMeshingPolicy` into the domain layer or `use_cases/ports.rs`.
2. **Test-only crossings** (17 sites, all inside `#[cfg(test)]`): use_cases reaching into `frameworks_drivers` / `adapters` for `SimpleNoiseProvider` / `DEFAULT_MATERIAL_PALETTE` instead of testing through ports. Purist-only; low priority. Sites include `grassland_level.rs:123`, `generate_chunk.rs:725,906`, `infinite_level.rs:178`, `world_topology.rs:434`, `compress_svdag.rs:224`, `anomalies/geometry.rs:535`, `anomalies/mod.rs:104-153`, `level_zero/generate.rs:407`, `level_zero/provisions.rs:426`, `build_octree.rs:211`, `csg_voxelizer.rs:88`, `build_octree_direct.rs:192`, `compress_svdag.rs:156`.
3. **`src/domain/use_cases/` is a misnomer:** it holds one correctly-placed kernel (`csg/`, dependency-free) plus legacy, framework-coupled algorithms — `path_tracer.rs` constructs `StdRng::seed_from_u64(42)` at line 108 (dead), `level_builder.rs` (dead), `generate_maze.rs` (dead, superseded by `region_plan` + `level_zero`).
4. **No RNG port.** Only Noise / MaterialPalette / Telemetry are ported (`use_cases/ports.rs`); `wfc3d.rs:136` and `legacy_blueprint.rs:285,292` instantiate concrete `StdRng` in production use-case code.
5. **Hidden global state:** `level_zero/menger_expanse.rs:164` uses a `thread_local! RefCell<HashMap>` memo. Deterministic output, but a purity smell in the application layer.
6. **Oversized files:** `generate_chunk.rs` (952 LOC: config + orchestrator + ~555 LOC tests), `level_zero/tests.rs` (2,341 LOC). `legacy_blueprint.rs`, `reality.rs`, `architecture.rs` are large but cohesive.
7. **Empty dead dirs** `src/entities/` and `src/interface_adapters/` exist but are never declared.

### Dead code (root crate)

`cellular_decay.rs`, `csg_voxelizer.rs`, `domain/use_cases/{path_tracer,level_builder,generate_maze}.rs`, `adapters/ascii_renderer.rs`, and `vertical_circulation.rs` (test-only consumer). All compile because `lib.rs` over-exposes internals as `pub mod`.

---

## 2. Clean Architecture — `wasm_frontend` crate

### Strengths

- **Imports are clean in the forbidden directions:** zero `application → drivers/adapters` and zero `adapters → drivers` matches.
- **Application layer is 100% platform-free:** no `web_sys` / `js_sys` / `JsValue` / `Closure` anywhere in `application/` (verified).
- **Ports are real:** `RendererPort` (`ports.rs:465-521`) and `ChunkSourcePort` (`ports.rs:610-682`), each with multiple implementors — `LocalChunkSource` (sync, `adapters/local_chunk_source.rs:177`) vs `WorkerChunkSource` (async pool embedding the local one, `drivers/worker_source.rs:163`). The composition root, not the engine, picks between them (`browser.rs:491-530`); the engine just asks `is_async()`.
- **The Engine is a big but disciplined god-object:** ~1,300 non-test lines, 35 fields, but every subsystem lives in a dedicated file (`player.rs`, `body.rs`, `survival_inventory.rs`, `flares.rs`, `collision.rs`, `streaming.rs`, `atlas.rs`, `perf_governor.rs`, `navigation.rs`, `thermal.rs`, `prepare_frame_lighting.rs`) and `impl Engine` is split across `presenter.rs` / `route.rs` / `noclip.rs` / `peripheral_shift.rs`. Next extraction candidate: the streaming block (`engine.rs:988-1259`).

### Caveats

- Adapters → application is heavy (54 imports), dominated by `cpu_splatter` pulling `ports` / `rendering` / `render_settings` / `atlas`. Consistent with the layer model, but adapters couple to application *internals*, not just the ports.
- `input.rs:39-43,99-108` smuggles `cfg(wasm32)` into an adapter via crate-root atomics (`lib.rs:28-56`). Minor, but native unit tests always exercise the non-wasm branch.
- The docstring claiming a `TelemetryPort` in this crate is wrong — it lives in the core crate (`console_telemetry.rs:12`).
- One truly dead module: `drivers/fs_artifact_sink.rs` (unreferenced, and it is the *only* drivers→reference outward edge).
- `bin/play` (native winit/Vulkan viewer) correctly reuses the application layer + WebGPU pipelines; only the outermost renderer facade (`app.rs:405-410`, `NativeRenderer`) and HUD are duplicated (`app.rs:17-20`).

---

## 3. Rendering methods

### Two complete, independently maintained GPU stacks

| Stack | LOC (est.) | Status | Wired |
|---|---|---|---|
| **WebGPU** (`drivers/webgpu/` + WGSL) | ~3.6k Rust + ~1.9k WGSL | **Active production** — all recent work (brick pool, reality-unfold, splat shading; commits since Jul 27) | `browser.rs:300-304` (auto backend), `bin/play` |
| **WebGL2** (`surface_webgl/`, `splat_webgl/`, `webgl/`, `gl/` + GLSL) | ~2.7k Rust + ~2k GLSL | **Frozen compatibility port** — compiles, ships, nearly zero new dev | `browser.rs:307-343` (fallback) |
| CPU splatter (`adapters/cpu_splatter/`) | ~30 files | First-class selectable strategy + reference | all shells, benchmarks, goldens |
| `wasm_raycaster` | legacy | Dead weight — compiled in CI, consumed by nothing | — |

**The auto backend already prefers WebGPU** (`browser.rs:289-296`, verified): preflight success → WebGPU, else WebGL2. So the docs are inverted: `ARCHITECTURE.md:13` claims "WebGL2 as primary production API", but the actual primary is WebGPU. `browser.rs:45-48`'s "WebGL2 is the boot default" comment is likewise stale.

### Techniques by strategy (WebGPU, the live stack)

- **Surface** (`pipelines/surface.rs`, `surface.wgsl`): indexed greedy mesh with **true GPU vertex pulling** — zero vertex-buffer layouts, storage buffer indexed by `vertex_index` (`surface.rs:81`, `surface.wgsl:15-16`); CPU never materializes expanded geometry.
- **Splat** (`pipelines/splat.rs`, `splat_geometry.wgsl`): 16-byte `GpuPackedFace` records, **quad expansion** in the vertex stage from a 4-vertex TriangleStrip (`splat.rs:343-355`); Minecraft-grammar per-face lighting with 15-step irradiance quantization (`splat.wgsl:32-126`); reality-unfold staggered materialization (`splat_geometry.wgsl:49-100`).
- **Raymarch** (`pipelines/raymarch.rs`, `raymarch.wgsl`): fullscreen triangle; 4-`u32` SVO or bricked hierarchy traversed in-fragment, exact fine-cell DDA, finite-segment direct-visibility rays with hard budget 4096.
- **CPU** (`cpu_present.rs`): the platform-free `SoftwareRasterizer` renders into RGBA, `queue.write_texture` upload, fullscreen triangle present (`cpu_present.wgsl:13-25`).

The CPU splatter is a **serious reference renderer** — hierarchical-Z coarse buffer, near-to-far splatting with deferred hidden-splat shading, MIP-splat LOD, crowding-based AO, 65k-entry hero-visibility hash cache — not a toy fallback.

### Documented claims that don't match code

1. "WebGL2 primary" — false; WebGPU is primary (above).
2. "Surface samples a 3D probe volume (L_baked)" (`ARCHITECTURE.md:17`) — WebGPU surface uses a per-vertex **scalar** baked fill (`surface.wgsl:26`, `common.wgsl:379-384`); the 3D `ProbeGrid` is real (`surface_mesh.rs:57-62`) but only the *WebGL2* path samples it as `sampler3D`, and baked lighting is **off by default** (`render_settings.rs:112`).
3. "CPU reference path shares the WebGPU present lifecycle" (`ARCHITECTURE.md:62`) — true only on the WebGPU backend; the WebGL2 branch's CPU path is a Canvas2D blit (`cpu_canvas.rs:24-29`).
4. `gl/light_volume.rs:3-6` "baked volumes removed from generation" — stale; probe bytes are still generated and transported (`chunk_codec.rs:70-75`).
5. Surface "one/four taps" (`ARCHITECTURE.md:78`) — WebGL2 surface is hardcoded 128² / 4-tap (`surface_webgl/mod.rs:162`, `hero_shadow_visibility.rs:21-25`).

### Duplicated logic across stacks (the real cost)

- Visibility math — `gl/visibility.rs:15-136` vs `webgpu/pipelines/visibility.rs:5-95` (near-identical, ~130 lines).
- Hero-light shadow maps — WebGL2 `DEPTH_COMPONENT16` + manual GLSL PCF vs WebGPU `Depth32Float` + comparison sampler.
- Material-aging noise patterns — GLSL `chunks::MATERIAL_PATTERN_GLSL` / `NOISE_GLSL` vs WGSL `common.wgsl:66-170`.
- Fog + tone-mapping + dither present — GLSL `present_surface_frame.rs` / `present_voxel_frame.rs` vs WGSL `common.wgsl:430-452`.
- Splat shading grammar — WebGL2 splat GLSL vs WebGPU `splat.wgsl`.
- Light upload / clustering — WebGL2 `SceneLightTexture` RGBA32F (`gl/upload_scene_lights.rs`) + per-chunk clustered ranges (`gl/cluster_scene_lights.rs`) vs WebGPU `GpuLight` storage array.
- Supply labels — both expand quads in the vertex stage.
- **Most expensive:** WebGL2 splat materializes a full indexed greedy mesh purely as a shadow caster (`splat_webgl/resources.rs:180-211`), which the WebGPU splat avoids entirely via face expansion.

---

## 4. Build health, tests, dead code

- **Compiles clean:** `cargo check --workspace --all-targets` → 0 errors, ~21 warning sites. `cargo clippy --workspace --all-targets` → 0 errors, ~146 warnings (top: `collapsible_if`×14, deprecated `criterion::black_box`×9, `manual_div_ceil`×7). No clippy in CI (`test.yml`).
- **Tests:** ~649 `#[test]` (276 root, 351 wasm_frontend, 22 integration). Strong reference/golden coverage: CPU-splatter & raymarch native reference renderers with golden PNGs (`tests/render_goldens.rs`), naga shader validation (`tests/shader_validation.rs`), Vulkan/Lavapipe headless contracts for all four strategies (`tests/vulkan_renderers.rs`), 1:1 `lookup_leaf` brick-vs-plain SVO equivalence (`tests/brick_atlas_reference.rs`).
- **Dead weight:** `wasm_raycaster` (2 warnings, nothing consumes it), `drivers/fs_artifact_sink.rs`, `adapters/ascii_renderer.rs`, `use_cases/cellular_decay.rs`, `use_cases/csg_voxelizer.rs`, `domain/use_cases/{path_tracer,level_builder,generate_maze}.rs`.
- **repomix-output.md:** 2.7 MB, 327 files, ~1 day stale, packed with `removeComments: true` — stripping this heavily-documented repo's prose for the AI-consumption artifact.

---

## 5. Prioritized recommendations

1. **Fix the one real violation** — move `SurfaceMeshingPolicy` out of `adapters/voxel_mapper.rs` so `surface_quality_profile.rs:20` points inward.
2. **Fix the docs** — rewrite `ARCHITECTURE.md` Rendering family + module tables to state WebGPU is primary and WebGL2 is the compatibility port (or make the code match the doc). Update stale comments at `browser.rs:45-48` and `gl/light_volume.rs:3-6`.
3. **Decide the WebGL2 stack's fate** — it is 2.7k lines of frozen-but-shipped code duplicating the active stack, including one wasted geometry materialization. Either budget active maintenance or cut it (the wasm bundle ships both).
4. **Delete the proven-dead code** and add clippy to CI; stop compiling `wasm_raycaster` or remove it.
5. **Add an RNG port** to `ports.rs` so `StdRng` stops leaking into use cases; retire `domain/use_cases/` legacy modules.
6. **Replace the `thread_local` memo** in `menger_expanse.rs:164` with explicit threading.

---

## Appendix — evidence index

- Dependency-rule violation: `src/domain/entities/surface_quality_profile.rs:20,38,58,86,100`
- Ports: `src/use_cases/ports.rs`; DI root: `src/main.rs:76-84`
- Determinism: `Cargo.toml:18-21`; `anomalies/determinism.rs:15-43`; `infinite_level.rs:19-28,147`
- `thread_local` memo: `src/use_cases/level_zero/menger_expanse.rs:164-181`
- wasm_frontend ports: `wasm_frontend/src/application/ports.rs:465-521,610-682`
- Chunk source choice: `wasm_frontend/src/drivers/browser.rs:491-530`
- RendererKind / profile: `wasm_frontend/src/drivers/webgpu/config.rs:18-28,473-488`
- Strategy dispatch: `wasm_frontend/src/drivers/webgpu/renderer.rs:247-294`
- Backend selection: `wasm_frontend/src/drivers/browser.rs:282-344`
- CPU present: `wasm_frontend/src/drivers/webgpu/pipelines/cpu_present.rs:149-181`
- Golden tests: `wasm_frontend/tests/render_goldens.rs`, `wasm_frontend/tests/golden/`
- CI: `.github/workflows/test.yml`
