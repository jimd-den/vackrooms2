# Static Audit — Part 2: Core Generation, Level 0 (procedural level generation) & Level 1

**Scope audited:** `src/use_cases/level_zero/**` (18 files, incl. `tests.rs`) and
`src/use_cases/level_one/**` (3 files) — 8,502 lines, read in full.
**Method:** pure file reading (ripgrep + Python). Nothing was compiled, built, or run.
**Focus:** procedural level generation algorithms (fabric CA, menger expanse, motif, provisions,
compose_column, voxelize, ceiling system, fixture planning, assembly sampling) and the
concurrency surface of this area (thread-local caches; the rest of the repo's threading is in
`src/main.rs` / `wasm_frontend` and out of this scope).

Cross-referenced (read-only, for complexity ground truth): `src/use_cases/wfc3d.rs`,
`src/use_cases/menger_bsp.rs`, `src/domain/entities/architecture.rs`,
`src/domain/entities/fixture.rs`, `src/domain/entities/anomaly/{phenomena,geometry,reality}.rs`,
`src/use_cases/infinite_level.rs`, `src/use_cases/world_topology.rs`,
`src/frameworks_drivers/simple_noise.rs`, `src/use_cases/generate_chunk.rs`.

---

## 0. Executive summary

Level 0/1 generation is *architecturally* sound: everything is a pure function of
`(seed, world position, reality)`, chunk-tiling is enforced by construction, and the two
memo caches (`SHEET_CACHE`, `PLATE_CACHE`) are thread-confined and bounded. **No Critical
findings** (no data races, deadlocks, or definite correctness bugs in this area).

The problems are **performance architecture**, all in the same shape: **cell-constant /
owner-constant quantities are recomputed per voxel column**, and the flyweight fixture
"layouts" are *regenerated from scratch for every column sample* instead of being computed
once and queried. At the high-spec chunk size (20 u chunk, 0.1 u voxel ⇒ **200×200 = 40,000
column plans per chunk**), the per-column constant factor (~30–45 noise evaluations + full
fixture-layout regeneration) dominates: a single large assembly inside a chunk costs
~10⁷–10⁸ wasted layout-build operations per chunk, and the fabric ceiling system re-evaluates
each 7.2 u cell's room hierarchy ~5,000× per chunk (a 20 u chunk holds ~8 fabric cells).

One latent correctness bug (not exploitable today): the two thread-local caches key on
`seed` but **not on the noise provider**, so two providers sharing a seed in one thread
(very possible in tests / future A/B configs) silently serve each other's solved sheets/plates.

One design inconsistency: `fabric_ceiling_height` samples porosity **per column** and feeds it
into `room_at`, so the "room" (and thus ceiling height) is not strictly cell-constant, which
contradicts the module's documented invariant and the voxelizer's "steps only where a wall
carries them" assumption (`voxelize.rs:84–110`).

---

## 1. Big-O table of the main algorithms

Notation: chunk = W×D columns (low-spec 50×50, high-spec 200×200), H = height voxels (29);
region plan holds C corridors, each spine of S segments and length L (u); A assemblies;
M anomalies; per assembly: Z ceiling zones, P host segments, K openings, polygon vertices V;
N = `RealitySnapshot` drift/consumed entry counts; `plan_eval` = one full
`plan_column_in_reality` call (~30–45 noise evaluations, see §3).

| Algorithm | Time complexity | Space | Where |
|---|---|---|---|
| `fabric_ceiling_band` | O(1) (2 noise evals) | O(1) | fabric.rs:75–96 |
| `fabric_ceiling_height` | O(1) but ~12–15 noise evals, **per column** (see finding F4) | O(1) | fabric.rs:113–155 |
| `room_at` (room hierarchy) | O(LEVELS·2) = O(1), 4 levels × 2 `splits` | O(1) | fabric_ca.rs:306–324 |
| `splits` / `subdivision_weight` | O(LEVELS) cell-hash evals, O(1) | O(1) | fabric_ca.rs:417–458, 470–500 |
| `walls_at` | O(1) amortized (sheet cache hit); cold miss = `solve_sheet` | O(1) | fabric_ca.rs:104–133 |
| `solve_sheet` (CA sheet) | O(PADDED²·(2·LEVELS + 9·GEN)) = O(784·~20 + 784·8·6) ≈ 1.5×10⁴–7.5×10⁴ ops per (sheet, era, generation, tier) | O(PADDED²) = 2×784 bool | fabric_ca.rs:503–577 |
| `step` (CA generation) | O(PADDED²·9) with per-cell bounds checks | O(PADDED²) | fabric_ca.rs:580–604 |
| `segment_stands` / `wall_stands` | O(1) with up to **7** `walls_at` lookups + noise evals; corner columns call it 4× (≈28 lookups) | O(1) | fabric_ca.rs:147–179, 208–255; fabric.rs:346–349 |
| `generate_menger_bsp` | O(8^depth), depth ≤ 2 ⇒ ≤ ~73 nodes | O(nodes) | menger_bsp.rs:75–114 (out of scope, referenced) |
| `solve_plate` (WFC 9×9×1) | `solve_wfc` ≈ O(cells²·modules + worklist) ≈ 81²·3 ≈ 2×10⁴; **cached per (seed, px, pz)** | O(81) | menger_expanse.rs:239–295; wfc3d.rs:129–156 |
| `expanse_structure` (per column) | O(1) amortized (plate cache) + pier hashes | O(1) | menger_expanse.rs:86–132 |
| `fitted_stack` (ceiling system) | O(1) (2–3 hashes) | O(1) | ceiling_system.rs:109–136 |
| `generate_assembly_fixture_layouts` | O((zone_area/bay²)·(4V + K + voids)) ≈ O(50–300) **per call** | O(layouts) | fixture_plan.rs:71–171 |
| `generate_corridor_fixture_layouts` | O(L/4) **per call** | O(L/4) | fixture_plan.rs:174–264 |
| `generate_fabric_cell_fixture_layouts` | O((7.2/3.6)²) ≈ 4–9 layouts | O(1) | fixture_plan.rs:267–344 |
| `fixture_at` / `corridor_fixture_at` | **regenerates the full owner layout set per column** (see F1, F2) | O(layouts) | fixture_plan.rs:347–375, 378–390 |
| `assembly_column` | O(P·K + Z·V + assembly-layout regen) per column | O(1) | assembly_sampler.rs:21–111 |
| `plan_column_in_reality` (fabric) | O(C·S + ~30–45 noise evals + sheet lookups) | O(1) | fabric.rs:197–542 |
| `plan_column_in_reality` (compose) | O(C·(S + L/4) + 3–5·M + A·(1 + P·K) + fabric path) | O(1)+1 small Vec | compose_column.rs:43–291 |
| **Per-chunk generation** | **O(W·D·(C·L/4 + A·zone_area/bay²·(4V+K) + M + ~40 noise))** | O(W·D·H) grid + O(W·D) ColumnField | generate.rs:294–429 |
| `voxelize_columns` | O(W·D·H) writes (floor/solid/cap/plenum/fixture) | O(1) extra | voxelize.rs:41–154 |
| `ColumnField::sample` | O(W·D) | O(W·D) | column_field.rs:17–33 |
| `decide_supply_items` | O(((chunk/40)+2)²·(log N + plan_eval)) ≈ 1–4 plan evals | O(items) | provisions.rs:220–279 |
| `wall_host_near` (door seating) | **O((REACH/STEP)²·plan_eval) ≈ 4,489 plan evals, worst ~500k** (see F3) | O(1) | provisions.rs:122–182, 187–208 |
| `stamp_level_zero_provisions` | O(regions_in_chunk·(door_chance·wall_host_near) + supplies) | O(1) | provisions.rs:283–375 |
| `decide_traversal_gates_and_hazards` | O(plans·M·(gates + hazard lattice cells)) | O(gates+hazards) | generate.rs:55–126 |
| `decide_runtime_lights` | O(Σ assemblies·fixtures·(M scan + plan_eval)) — only fixtures within chunk+15 u | O(lights) | generate.rs:135–263 |
| Level 1 `plan_column` (+4 sectors) | O(1), ~6–10 hashes | O(1) | level_one/generate.rs:89–105 |
| Level 1 `supplies_in_bounds` | O(((chunk/24)+2)²·6·plan_eval), re-evaluated per overlapping chunk (F11) | O(items) | level_one/generate.rs:383–462 |
| `stamp_supply_marker` / `stamp_level_door` / `carve_door_threshold` | O(stamped voxels), constant | O(1) | level_one/generate.rs:473–571; provisions.rs:387–421 |

**Dominant terms.** For one chunk the hot cost is
`W·D × (C·L/4 + A·zone_area/bay²·(4V+K) + M + ~40 noise evals)`. At high spec (W=D=200)
this is ≈ 40,000 × (≈50–400 ops) ≈ **2×10⁶–2×10⁷ ops minimum, before fixture regeneration**;
with one assembly zone filling the chunk (e.g. 20×20 u ⇒ ~25 bay candidates × ~40 ops each ≈ 10³ ops/column
for 40,000 columns) a single chunk can exceed **4×10⁷ ops** in the fixture builder alone.
The rare door-seating search (`wall_host_near`) is the single most expensive *individual*
operation: ~4,489 full column plans minimum per door (≈2×10⁵ noise evals), worst case
~10× that with `centre_across` + seated/front/back probes.

---

## 2. Concurrency inventory (this area)

The area contains **no native threads, no atomics, no mutexes, no `static mut`, no `unsafe`**
(grep-verified). The entire concurrency surface is two `thread_local!` memo caches plus a
local RNG. Everything else is immutable input (`&RealitySnapshot`, `&RegionPlan`,
`&dyn NoiseProvider`) per call.

| Shared state | Type / sync | Bounded? | Race risk | Where |
|---|---|---|---|---|
| `SHEET_CACHE` | `thread_local! RefCell<HashMap<(u32,i64,i64,u32,u8), Rc<Sheet>>>` — thread-confined, no atomics needed | Yes: `clear()` when `len() > 64` (fabric_ca.rs:269–271) | None today (single-threaded generator; JS worker is single-threaded). `Rc` is `!Send/!Sync` ⇒ **blocks rayon column/chunk parallelism** (rayon exists only in dev-dependencies today). | fabric_ca.rs:84–87, 258–277 |
| `PLATE_CACHE` | `thread_local! RefCell<HashMap<(u32,i64,i64), Rc<PlateStructure>>>` — same model | Yes: `clear()` when `len() > 128` (menger_expanse.rs:229–231) | None today; same `!Send/!Sync` constraint; **key omits the noise provider** (latent wrong-world bug, F6). | menger_expanse.rs:220–237 |
| WFC RNG | `StdRng::seed_from_u64(seed)` local to `solve_wfc` | n/a | None — deterministic per plate; no cross-thread RNG sharing anywhere in scope. | wfc3d.rs:136 |
| `RealitySnapshot` | immutable `&` borrows per chunk; internally sorted Vecs (binary search) | bounded by event history | None — no mutation during generation (`with_*` builders clone). | reality.rs:288–360 |
| Generator types | zero-sized (`BackroomsLevel`, `HabitableLevel`) | n/a | None | level_zero/mod.rs:62; level_one/mod.rs:39 |

**Worker lifecycle / message ordering / backpressure:** none of that surface exists in this
area — chunk generation is synchronous, pure, and order-independent; the wasm worker
(`worker_source.rs`, `static/worker.js`) is out of scope. The caches are correct under any
chunk streaming order because they memoize pure functions.

**Re-entrancy:** `SHEET_CACHE`/`PLATE_CACHE` borrows are held across `solve_sheet` /
`solve_plate` (`or_insert_with`). Neither solver re-enters its own cache today (verified:
`solve_sheet` → `subdivision_weight`/`splits`/`cell_hash` → noise only; `solve_plate` →
BSP/WFC only). **But** a user-supplied `NoiseProvider::evaluate_2d` that calls back into
`walls_at`/`expanse_structure` (or panics) would hit a `RefCell` double-borrow panic —
single-threaded panic, not a race. Low risk.

**Determinism vs parallelism:** all decisions are fixed-point/bit-mix hashes plus
deterministic f32 arithmetic with no cross-thread reductions; parallelizing over columns or
chunks would produce bit-identical output. The only non-`Send` values are the cached
`Rc<Sheet>/Rc<PlateStructure>`, which stay thread-local under rayon — so parallelizing
requires only `NoiseProvider: Send + Sync` bounds (F12 / Opt 3).

**Memory-ordering notes:** none needed — no shared atomic state, so no Relaxed/Acquire/
Release concerns exist in this area. The `AtomicU32`/`AtomicBool` settings in
`wasm_frontend/src/lib.rs` are out of scope.

---

## 3. Findings table

Severity guide: **Critical** = correctness bug / data race / deadlock (none found here);
**High** = real performance regression / accidental quadratic behavior in a hot path;
**Medium** = latent correctness risk or notable redundancy; **Low** = style / micro.

| # | Severity | File:line | Issue | Fix |
|---|---|---|---|---|
| F1 | **High** | fixture_plan.rs:71–171, 347–375 | `fixture_at(FixtureOwner::Assembly{..})` **regenerates the entire assembly fixture-layout set on every column sample** (`generate_assembly_fixture_layouts` sweeps the whole zone bay grid: 4 `Polygon2::contains` + service-void scan + opening scan + `on_column` per candidate). A 20×20 u zone fills a whole high-spec chunk: 40,000 columns × ~25 bay candidates × ~40 ops ≈ 4×10⁷ wasted layout-build ops per chunk (a 40×30 u assembly is several chunks). The module doc claims the Flyweight pattern ("canonical FixtureLayout"), but the flyweight is rebuilt per query. | Compute layouts **once per (assembly, zone)** per chunk (memo keyed by `assembly.id` + zone identity) and point-query the cached set; or precompute per world-cell bins. Pure function ⇒ cache is trivially safe. |
| F2 | **High** | fixture_plan.rs:174–264, 378–390 | `corridor_fixture_at` regenerates every 4 u module along the **whole spine** for each column inside the corridor (`compose_column.rs:75–79`), and `plan_column_in_reality` calls it per corridor. O(W·D·C·L/4) with `sqrt` per segment per module. | Memo the layout list per `(spine.id, ceiling_h)` (ceiling is zone-quantized, so the set is stable); `ceiling_h` is already `corridor_ceiling.max(3.0)` — quantize and key on it. |
| F3 | **High** | provisions.rs:122–182, 187–208 | `wall_host_near` brute-force rasterizes a 5 u radius at 0.15 u pitch (**67×67 = 4,489 probes**), each probe = a **full `plan_column_in_reality`** (`is_solid`, ~30–45 noise evals), plus up to 2×5 seated probes, 2 open-floor probes, and two `centre_across` walks (≤24 probes each) per solid hit ⇒ worst ~5×10⁵ column plans ≈ 2×10⁷ noise evals per door. Comment admits it is "empirical rather than lattice-based" (lines 115–120), but walls here come from only three known lattices (fabric 7.2 u + 0.4 u band, corridor spines, assembly hosts). | Replace the raster with a lattice-aware candidate search: generate candidate wall-line points from the fabric cell lattice lines + spine segments + assembly hosts within REACH, test a handful of points per candidate, then `centre_across` only the survivors. Turns ~4,489 plan evals into ~O(10–50). |
| F4 | **Medium** | fabric.rs:113–155, 139–142; fabric_ca.rs:306–324 | Cell-constant invariants are recomputed per column: `fabric_ceiling_band` (anchor noise), `fabric_ceiling_height`'s `detail` (anchor), `room_at` (≤8 `splits` hashes), `wall_thickness`, corridor ceilings — all constant per 7.2 u/12 u cell but evaluated for every one of the 40,000 columns of a high-spec chunk (~8 cells ⇒ ~5,000× recomputation per cell, ~15 noise evals each ≈ 6×10⁵ evals/chunk). | Per-chunk memo keyed by `(cx, cz)`: band, base height, `detail`, room level/span, porosity-at-anchor, wall thickness; two-pass generation (first pass fills the cell memo, second samples columns). This removes the largest constant-factor term. |
| F5 | **Medium** | fabric.rs:141–142, 369–370; fabric_ca.rs:489–490 | `fabric_ceiling_height` samples porosity **per column** (`n(0x9010, wx, wz, 0.11)`) and feeds it into `room_at`, which decides the room level via `splits` thresholds that move with porosity. A block whose hash sits near the threshold can flip **mid-cell**, so ceiling height can step inside a room — violating the documented invariant ("heights vary per fabric *cell*… never per column", fabric.rs:98–112) and the voxelizer's assumption that steps only occur where a wall/lintel column exists to carry them (voxelize.rs:84–110, 95). The constancy test (tests.rs:617–652) samples only 3 points per cell and can miss it. | Sample porosity at the cell anchor (cell center) for `room_at`/`splits` (the wall/door section may keep per-column porosity for doorway framing). Also cheaper: one anchor sample instead of per column. |
| F6 | **Medium** | fabric_ca.rs:84–87, 267–277; menger_expanse.rs:220–237 | Cache keys are `(seed, sx, sz, …)` with **no noise-provider identity**. Two providers with the same seed in one thread (tests.rs uses `TestNoise`, runtime uses `SimpleNoiseProvider`; future A/B or fallback providers) silently share solved sheets/plates ⇒ wrong walls/galleries, non-deterministic across configurations. Latent in the binary (one provider per process), but the test harness runs `TestNoise(seed 42)` (fabric_ca tests) and `SimpleNoiseProvider(seed 42)` (provisions tests) on a shared thread pool, so a same-thread collision — and flaky wrong-world results — is already possible. | Include a provider identity in the key (type-name hash, `TypeId`, or an `&dyn Any` address — better: a provider-supplied id/version), or own the cache inside the provider. |
| F7 | **Low** | fabric.rs:256, 498–499, 539 | `institution_age_at` (a 3-octave field ⇒ 3 noise evals) is sampled **3× per fabric column** (cell anchor for env profile, light age, ceiling age) plus once in `compose_column.rs:99`; at 40,000 columns that's ~1.2×10⁵ octave evals for one value. | Sample once per column (and once per cell for the anchor value) and pass through; or memo per cell anchor. |
| F8 | **Low** | compose_column.rs:62, 113–169, 180–234 | Per column: a fresh `Vec` allocation for `gap_probes`, plus 3–5 separate linear scans of `plan.anomalies` (blackout find, archway anchor, `near_anchor`, hostile `filter().max_by`, red-room find) and a full `plan.assemblies` scan with per-assembly bounds checks ⇒ O(W·D·(M + A)). | Combine anomaly scans into one pass over a single indexed structure; replace `gap_probes: Vec` with a fixed array (`[Option<(…)>; 3]` — corridors per region are ≤3); spatial-bucket assemblies (e.g. 40 u grid hash) so only nearby assemblies are tested. |
| F9 | **Low** | fabric.rs:141 vs 369–370 | The same porosity field (`0x9010`, same point) is sampled twice per column — once in `fabric_ceiling_height`, once in the wall/door section. | Compute once in `column_plan_in_reality` and pass down. |
| F10 | **Low** | fabric_ca.rs:208–255 + fabric.rs:346–349 | `wall_stands` performs up to 7 `segment_stands` (each a HashMap lookup + `cell_hash` noise); corner columns call `wall_stands` up to 4× (short-circuited) ⇒ up to ~28 cache lookups + noise evals per corner column. `step` (fabric_ca.rs:580–604) also does bounds-checked 3×3 neighbor scans with 2 Vec allocations per generation. | Batch: fetch the neighbor strip once from the sheet and evaluate all `raw` probes against it; pad the sheet with a dead border in `step` to drop the `nx<0…` checks. |
| F11 | **Low** | level_one/generate.rs:383–462 | `supply_for_cell` runs up to **6 full `plan_column` evaluations** per 24 u supply cell and is re-run by **every chunk that overlaps the cell** (up to 4×, plus the 0.5 u margin). Deterministic but wasted; multiplies with F4/F7 constant factors. | Memo `supply_for_cell` per `(seed, cx, cz)` (chunk-local or thread-local with the same bounded-cache pattern); or evaluate only the owning chunk's cells. |
| F12 | **Low** | fabric_ca.rs:33–35, 84–87; menger_expanse.rs:22–24 | `Rc<Sheet>` / `Rc<PlateStructure>` make the caches `!Send/!Sync`. Today generation is single-threaded (rayon is dev-only), so this is a portability/parallelism blocker rather than a bug: any attempt to parallelize chunks/columns with rayon fails to compile. | Keep caches thread-local (correct under rayon) and only require `NoiseProvider: Send + Sync`; or switch `Rc` → `Arc` only if a cache must be shared. Thread-local `Rc` is the right call — document it. |
| F13 | **Low** | provisions.rs:220–279; level_one/generate.rs:83–85 | Supply ids are `roll | 1` / `hash | 1` — forced odd, but the hash itself already has full entropy; no uniqueness *guarantee* across cells (two cells could theoretically collide; `decide_supply_items` also recomputes `id` from `roll` while `supply_consumed` checks use it). Probability negligible; consistency is fine. | Optional: fold `(cx, cz)` into the id explicitly to make uniqueness structural rather than probabilistic. |
| F14 | **Low** | tests.rs:1073–1147, 617–652, 2343–2370 | Test sweeps are large O(n²) plan-eval loops (e.g. 80×80 cells × ~35 samples/cell with full plan generation ≈ 2×10⁵+ plan evals; `every_fabric_cell_opens_west_or_north` alone re-derives a region window per cell) and `test_print_ascii_map` writes `./ascii_map.txt` (cwd-dependent side effect). Test-only, but they make the suite slow and the map dump is a hidden artifact. | Reduce sweep radii, reuse one `InfiniteRegionWindow` per sweep, and gate the map dump behind an env var or remove it. |

**No Critical findings.** Verified absent: data races (no shared mutable state), deadlocks
(no locks), unbounded growth (both caches `clear()` at 64/128 entries; all other Vecs are
bounded by lattice area), recursion-depth risks (Menger BSP depth ≤ 2 ⇒ ≤73 nodes;
`MengerNode::visit` recursion depth ≤ 2; WFC is iterative worklist), and re-sorting in loops
(none). `contains()` on Vec is used only for tiny fixed sets (`voxelize.rs:198`,
`tests.rs`); the hot `Vec` scans are the layout regeneration (F1/F2) and anomaly/assembly
scans (F8), both linear per column with the column loop as the outer multiplier.

---

## 4. Concurrency summary for the area

1. **No shared mutable state crosses threads** in `level_zero`/`level_one`. The two
   `thread_local! RefCell<HashMap>` caches are thread-confined by construction — no torn
   reads, no lost updates, no memory-ordering requirements. Relaxed/Acquire-Release/SeqCst
   questions do not arise here (the repo's atomics live in `wasm_frontend/src/lib.rs`).
2. **Worker lifecycle / message ordering**: not present in this area. Generation is
   synchronous per chunk and order-independent; memo caches are pure and keyed by all inputs
   (except the provider-identity gap, F6).
3. **Re-entrancy**: single `RefCell` borrow per lookup, held across solver calls that do
   not re-enter the cache; a hostile/re-entrant `NoiseProvider` could panic (double borrow),
   not corrupt.
4. **Determinism**: all randomness is per-call hashing (`hash`/`unit`/`cell_hash`) or a
   locally seeded `StdRng` inside `solve_wfc`; no global RNG, no `static mut`, no
   `thread_local` mutable *decisions* (only memoization). Parallel regeneration of any chunk
   set is bit-identical.
5. **Parallelism opportunity**: the per-column sampling is embarrassingly parallel and pure;
   only the `dyn NoiseProvider` bound and the `Rc` caches (thread-local, fine) stand between
   this code and a rayon `par_iter` over columns/chunks (Opt 3).

---

## 5. Highest-leverage optimizations (ranked)

1. **Chunk-level memo of cell-constant invariants (F4, F5, F7, F9).** Two-pass chunk
   generation: first pass fills a per-chunk `HashMap<(cx,cz), CellMemo>` (ceiling band,
   base height, detail, room level/span, anchor porosity, wall thickness, corridor zone
   ceilings, fixture layout sets), second pass samples columns from the memo.
   **Expected impact: 2–4× on whole-chunk generation time at high spec** (removes
   ~60–70% of ~1.2–1.6M noise evals per chunk), and it *fixes* the mid-cell step
   inconsistency (F5) as a side effect.
2. **Memoize fixture layout sets per owner (F1, F2).** Build `generate_assembly_fixture_
   layouts` / `generate_corridor_fixture_layouts` / `generate_fabric_cell_fixture_layouts`
   once per `(owner, zone/ceiling)` and point-query. **Expected impact: 30–50% further**
   on chunks containing large assemblies or long corridors; removes the only
   super-linear-per-column terms (A·area/bay² and C·L/4).
3. **Parallelize column sampling with rayon (F12).** Add `Send + Sync` bounds to
   `NoiseProvider` (trivial for `SimpleNoiseProvider`), keep the thread-local caches, and
   `par_iter` over chunk rows or columns (or over chunks in a world-block pass).
   **Expected impact: ~N-core speedup** (generation is pure and deterministic; no shared
   state to protect).
4. **Lattice-aware door seating (F3).** Replace the 67×67 raster in `wall_host_near` with
   candidate points from the three known wall lattices. **Expected impact: 10²–10³× on the
   door-seating path** (from ~4.5k–500k column plans to ~O(50) per door); doors are rare
   per chunk but the worst case currently costs tens of milliseconds of single-thread CPU.
5. **One-pass anomaly/assembly spatial indexing (F8).** Bucket assemblies and anomalies by
   a 40 u world grid at plan build time and scan only overlapping buckets per column.
   **Expected impact: 10–30% on chunks in assembly/anomaly-dense regions** and removes the
   per-column `Vec` allocation.
6. **Trim per-column constant (F7, F9, F10).** One `institution_age_at` sample per column,
   one porosity sample per column, and batched sheet-neighbor reads in `wall_stands`.
   **Expected impact: 10–20% total** — cheap, low-risk, and it also reduces the
   `RefCell`/hashmap pressure on the caches.

---

## 6. Scope coverage notes

* All 18 `level_zero` files and 3 `level_one` files were read in full, including both test
  files (2,426 + 243 lines).
* Algorithm audit covered: fabric CA (implicit, sheet-solved, memoized), menger expanse
  (plate-solved WFC + BSP), motif/brutalist fields, provisions (supplies + door seating +
  carving), `compose_column` priority order, `voxelize_columns` cap/plenum logic, ceiling
  system, fixture planning (assembly/corridor/fabric-cell), assembly sampling, Level 1's
  four sectors, supply stamping, and the door/supply exports.
* Concurrency audit covered the only shared state in scope: `SHEET_CACHE`,
  `PLATE_CACHE`, local WFC RNG, and the immutable `RealitySnapshot`/plan inputs.
