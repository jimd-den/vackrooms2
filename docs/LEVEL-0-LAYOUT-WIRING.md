# Level 0 — Wiring the Layout Solver (dead code → the three-layer engine)

- **Date:** 2026-08-06
- **Status:** implementation plan / spec
- **Depends on:** `docs/LEVEL-0-ARCHITECTURE.md` (design thesis: fit-out + implicit fractal/CA), the traversability research in that doc's §5.2/§7 and the semi-lattice/isovist design from the follow-up.
- **Scope:** activate the compiled-but-unwired room-placement solver (`region_plan/layout.rs`, `territories.rs`) inside `generate_region_plan`, then extend it into the three-layer architectural engine (substrate → grammar → perceptual) for Level 0.

---

## 1. Goal

Turn `generate_region_plan` from a **cursor walk** into a **scored architectural composition pass** that is the *grammar layer* of the Level 0 engine:

- **Substrate** (unchanged): implicit fractal + implicit CA — the Menger/wfc expanse and the `RealitySnapshot`-driven fabric drift.
- **Grammar layer** (this work): typed program placement with architectural scoring terms — the solver.
- **Perceptual layer** (next): isovist scoring, rhythm-of-walk targets, designed confusion.

All three must keep the engine's hard invariants: determinism, chunk-independence, seamless tiling, and **traversability**.

---

## 2. Current state

### The dead solver (fully built, tested, unwired)

- `region_plan/territories.rs` — `free_territories(origin_x, origin_z, region_size, corridors, taken, edge_margin, wall_thickness) -> Vec<Territory>`: largest-free-rectangle decomposition of the region floor after corridors and committed footprints are removed (`territories.rs:82-149`). O(n²) largest-rectangle-in-histogram (`territories.rs:155-198`).
- `region_plan/layout.rs` — `lay_out_suites(seed_hash, genome, legs, territories, origin, region_size, wall_thickness, spines, taken, next_id) -> Vec<AssemblyInstance>`: scored greedy placement over `PROGRAM_BUDGET` (`layout.rs:36-53`, importance-ordered: Atrium/OpenOffice/Reception first, Restroom/Storage/Server/Mechanical last), `ROUNDS = 2` (`layout.rs:62`), every anchor on every leg both sides (`layout.rs:156-238`), scoring = `0.30·area + 0.25·aspect + 0.20·fit + 0.25·separation` (`layout.rs:305`). Room ideal areas from `ideal_area` (`layout.rs:311-326`).
- The module's own docs claim it "lift[s] a region from 2 rooms to 6 with a real program mix" (`region_plan/mod.rs:101-106`).

### The live cursor walk it replaces

`generate_region_plan` (`region_plan/mod.rs:123-276`) places suites with the old cursor walk (`mod.rs:156-211`): slide along dominant-spine legs, hash-roll a 62% chance, `place_suite` at the cursor if clear. Consequences (per `layout.rs:7-13`): a failed spot is skipped, only the dominant leg is considered, nothing compares two candidates — an 80 u region resolves to ~2 rooms and ~90% unplanned fabric.

### The wiring leaves three failing tests

`region_plan/mod.rs:100-110` documents: wiring the solver leaves three failures (down from five), kept compiled so diagnosis runs green:

| Test (`level_zero/tests.rs`) | Invariant it asserts | Failure class |
|---|---|---|
| `rare_doorways_still_have_lintels` (`:732`) | Every narrow doorway (`width ≤ DOOR_WIDTH + 0.01`) is not blocked in voxel space, has `lintel_from_units == Some(DOOR_HEIGHT)`, and has a sampled `door_leaf`. | **Traversability** (graph says open, voxels block) |
| `red_rooms_are_lit_red_but_never_built_red` (`:1077`) | A red-room assembly's lit fixture plans a crimson light; `VOXEL_RED_WALL` is never voxelized anywhere (red is presentation via light only). | **Content identity** |
| `peripheral_shift_rearranges_fabric_but_never_the_plan` (`:1692`) | Drift epochs change fabric columns but never corridor interiors, assembly footprints (+wall band), anomaly footprints (+3.2 u keep-out), or near-spawn. | **Epoch stability** |

---

## 3. The wiring step (Phase A)

Replace the cursor loop in `generate_region_plan` (`mod.rs:156-211`) with the solver, preserving the exact post-pass ordering: corridors → **suites** → corruption → stairs → anomalies (`mod.rs:150-266`).

```rust
// (mod.rs, replacing lines 156-211)
let mut assemblies: Vec<AssemblyInstance> = Vec::new();
let mut taken: Vec<(f32, f32, f32, f32)> = Vec::new();
let mut id = 0u32;

let legs = layout::legs_of(&corridors, 14.0);            // ALL spines, not just main
let territories = free_territories(
    region_origin.x, region_origin.z, region_size,
    &corridors, &taken, EDGE_MARGIN, PLAN_WALL_T,
);
let seed_hash = |k: i64| hash01(seed, &[0xA0 + k, rx, rz]);

let placed = layout::lay_out_suites(
    &seed_hash, &dominant, &legs, &territories,
    region_origin, region_size, PLAN_WALL_T,
    &corridors, &mut taken, &mut id,
);
assemblies.extend(placed);
```

Preconditions to land first (kept in the same change, since they are what the three tests exercise):

1. **Remove `#[allow(dead_code)]`** on `mod layout; mod territories;` (`region_plan/mod.rs:107-110`) and make `free_territories` `pub(super)`-reachable from `generate_region_plan` (it is `pub`, `territories.rs:82` — fine).
2. **Keep `ROUNDS = 2`** (not 3) — the corruption pass needs leftover space for duplicated suites, abandoned expansions, and red rooms (`layout.rs:55-62`). If corruption starves, red-room promotion collapses (failure 2).
3. **Feed the solver the *secondary* spines' legs too** (`legs_of` already does, `layout.rs:96-122`) so secondary halls become "corridors to nowhere" only when *authored* that way, not left over.

---

## 4. The three failing invariants — fix strategies

### 4.1 `rare_doorways_still_have_lintels` — traversability

**What breaks:** the solver packs rooms with `ROOM_SEPARATION = 3.2` (`layout.rs:80`); the suite builder only rejects overlap by 0.8 u, so a packed neighbor can (a) drop a partition/soffit across a doorway's center column (making `column.solid == true`), or (b) strip the lintel (`lintel_from_units` lost) or the door leaf. This is the "graph says open, voxels block" class — the exact bug fixed once already by commit `7ce789f` "stop partitions sealing doorways," now re-triggered by denser packing.

**The traversability contract (from the research):**

1. **The doorway is a frozen tree-edge.** The binary-tree fabric doorway rule (`fabric.rs:243-251`) is the spanning-tree backbone. Add the same invariant at assembly level: **an assembly entrance may never be covered by another room or by a partition** — the solver's `lay_out_suites` already enforces that every entrance lands in a corridor wall band (`layout.rs:191-204`); extend the rejection to "no *other* assembly's footprint may intersect any committed entrance's 0.4 u column box," checked in `score_placement`/commit.
2. **The lintel survives:** keep the circulation-priority rule that only corridor-hosted entrances are carved full height (they lose the lintel and stop reading as doors, `layout.rs:193-195`). After the solver commits, add a **doorway-preservation pass** over `taken` + `assemblies` that re-probes every narrow entrance center column via `BackroomsLevel::plan_column` and repairs: raise a lintel at `DOOR_HEIGHT` if the column went full-height, or reject the offending footprint (reroll that seat) if it went solid.
3. **Verification at nonzero epochs + strain tiers** (research: your one flood-fill test runs at epoch 0 where `shift_bias == 0`). Add the passable-column predicate: `solid below + headroom ≥ DOOR_HEIGHT + gap + width ≥ CAD_DOOR_WIDTH_ADA_MIN (0.815)` — not the single-air-voxel check.

**Acceptance:** the test passes AND a new `passable_columns_at_epoch_n_and_strain` test asserts the same narrow-doorway invariants under a drifted `RealitySnapshot` and strain ≥ 1.

### 4.2 `red_rooms_are_lit_red_but_never_built_red` — content identity

**What breaks:** red rooms are promoted from real assemblies by the corruption pass (`corruption.rs:190-273`). With the solver choosing a different (denser, program-typed) assembly set, promotion may (a) stop finding a promotable occupied assembly within 81 regions, or (b) produce a red room whose fixture loses its `red_room` flag / `lit` status.

**Fix:**
- Red rooms must be **solver-aware**: keep the invariant "red room = occupied, non-abandoned assembly" (`tests.rs:1098`), and give the corruption pass first claim on a dedicated seat — e.g. reserve one `AssemblyInstance` slot for promotion (a `SpaceProgram::RedRoomSeed` in the budget or a post-solver reservation), so promotion no longer depends on what the solver happened to leave.
- The solver must not abandon or relocate a promoted assembly after corruption runs — ordering already guarantees corruption runs before anomalies; make sure the solver's `taken` is the same `taken` corruption mutates (it is, shared through the pipeline).
- `VOXEL_RED_WALL` must stay **never voxelized** (`tests.rs:1124`): red is light-only presentation. Guard with a test, not a hope.

**Acceptance:** the test passes; red-room density (one per macro cell, Matern-separated) is unchanged from the cursor-walk era.

### 4.3 `peripheral_shift_rearranges_fabric_but_never_the_plan` — epoch stability

**What breaks:** the drift test (`tests.rs:1692-1766`) asserts columns in *planned* space never change across epochs: corridor interiors, assembly footprints expanded by `PLAN_WALL_T + 0.05`, anomaly footprints expanded by `3.2 + 0.1`, near-spawn (`26 u`), and outside-drift fabric. The wiring must keep the plan a **pure function of `(seed, region)` with zero epoch dependence**.

**Fix:**
- The solver is already pure (all randomness flows through `seed_hash` of `(seed, region)`) — this is the key property that must survive. Audit `place_suite` for any hidden state: it must not read `RealitySnapshot`, `time`, or a mutable cache.
- **Do not place assemblies into the drift boundary band.** The test's boundary logic (`tests.rs:1754-1758`) is sensitive to footprint margins; the solver's `ROOM_SEPARATION = 3.2` strips between assemblies land in space the test may classify as "unplanned fabric" that *must* drift — so keep solver footprints out of the corridor-adjacent and drift-edge margins that `compose_column` treats as fabric. Practically: keep the `EDGE_MARGIN = 3.2` for assembly footprints (already the solver's territory margin), and add a **drift-band keep-out** on the solver's candidate rejection when the region border is a drift-cell boundary.
- The corruption duplicate/renovation overlays (translated suites) must also stay epoch-stable: translation offsets derive from `(seed, region)` hashes only.

**Acceptance:** the test passes; additionally a `plan_is_epoch_stable` test asserts `generate_region_plan(seed, origin) == generate_region_plan(seed, origin)` under any `RealitySnapshot`.

---

## 5. Extending the solver into the grammar layer (Phase B)

The solver's `score_placement` (`layout.rs:249-306`) is the seam where the *art-and-design* scoring terms land. Keep the existing four terms (area/aspect/fit/separation) and add, all deterministic and all below the JND for bulk variation:

1. **Alexander-pattern terms** (the compositional checklist):
   - *Hierarchy of Open Space* — score for nested scale: a large room should not sit beside another large room; prefer big/small alternation.
   - *Intimacy Gradient* — reward rooms whose *graph depth* from the main spine increases with privacy class (service deep, public shallow). Read from the corridor graph.
   - *Flow Through Rooms* — prefer doors on opposing walls for through-rooms; this is a `place_suite` constraint, not a score.
   - *Alcoves* — after commit, allow small back-wall recesses (cheap voxel nooks) as a `furnish`-level detail pass.
   - *Ceiling Height Variety* — tie `CeilingZone` to program (office 3.0–3.2, meeting 3.4, atrium/atria 4.5–5.4) instead of per-genome roll; the cheapest rhythm variable.
   - *Light on Two Sides* — deliberately *violated* for the liminal read (constant top light), but the term exists so the violation is a flag, not an accident.
2. **Parametricist correlation** — replace independent hash rolls with **correlated fields**: room width ↔ ceiling height ↔ fixture pitch all sample the *same* pink-noise field (see §6), so the space reads as designed rather than noise.
3. **Space-Syntax integration bias** — compute integration over the corridor graph (BFS mean-depth), and bias program placement toward integrated lines (movement economy): public rooms on high-integration legs, service on low.
4. **Semi-lattice thickening** — keep the binary-tree backbone frozen (§4.1); add a `loop_density` knob that adds extra doorways/loops (the existing porosity dropouts + second doorways in `fabric.rs`) so the connectivity graph thickens toward a semi-lattice the deeper/worse the region — "you keep coming back" without literal loops.

---

## 6. Subtle-randomness toolchain (Phase B, deterministic + streaming-safe)

All seed-hashable per chunk — no global RNG state (matches the streaming contract):

- **Pink/brown noise fields** for *proportions* (corridor widths slowly widening/narrowing, ceiling drift). Implement as correlated octave value-noise over the existing `NoiseProvider` port.
- **Blue noise / Poisson-disc** for *placed details* (stains, wall discoloration, alcove positions, ceiling-tile damage). Bridson-style rejection on a hash grid, or the golden-ratio/Halton low-discrepancy sequence, hash-injected per chunk so each chunk sees a different subsequence.
- **JND (Weber) amplitude clamps**: bulk variation below the just-noticeable difference (reads as material imperfection); spend the perceptual budget on **1–2 salient features per view** (one alcove, one ceiling anomaly, one landmark column).

---

## 7. The interior composition layer (Phase B·2 — detailed rooms + artistic architecture)

The goal of the whole engine is **architectural variety, subtle randomness, and designed confusion** — and that lives in the *room*, not just the layout. The distinction that matters: a **shell** is bounded space; a **room** is a composition — an interior with a hierarchy, a rhythm, a focal element, and a material language. Level 0's uncanny comes from rooms that are *crafted* but *empty*: detail everywhere, people nowhere. This phase runs **after** the solver places the assembly and **before** voxelization, and it is where "more detailed rooms + more artistic architecture" actually ships.

### 7.1 The room-detail vocabulary (mapped to the engine)

| Layer | The detail | Engine hook |
|---|---|---|
| **Floor** | carpet seams + tile grid on a module, blue-noise stains/damp, threshold transitions (carpet→vinyl at entries) | extend `cellular_decay.rs` output with a *seam grid* pass + blue-noise stain placement (§6) |
| **Wall** | baseboard, chair rail, wallpaper seams/patterns, scuffs, outlets/switches, **door casing**, room-number plaques, corner guards | a `wall_trim` pass along partition runs — materials-level, cheap |
| **Ceiling** | T-bar grid (0.6 m module), coffer/soffit language (soffit designers already exist, `suites.rs:56-114`), **missing tiles → plenum glimpses**, sprinklers/vents/grilles | the ceiling *system* from `docs/LEVEL-0-ARCHITECTURE.md` §4.2 — the single biggest "detail" upgrade |
| **Articulation** | **alcoves/recesses** (Alexander #179), interior columns (structural bays *inside* a room), **partitions-within-rooms** (half-walls, cubicle partitions), sealed-over window niches, built-in cabinets | extend `AssemblyInstance` with an `interior: Vec<InteriorElement>` list |
| **Furniture/props** | desks, chairs, cabinets, filing, tables, vending, dead plants, signage, boxes | `furnish` pass — `ArchitectGenome.furnishing_density` exists and is unused (`architecture.rs:107-119`) |
| **Light as art** | fixture language per room (grid/strip/pendant/sconce), **pools of light** (Tapestry of Light and Dark), deliberate light-on-two-sides violation, exit/emergency light | extend `fixture_plan.rs` with per-room light-composition rules |

### 7.2 The artistic composition principles

These are the rules that turn detail into *architecture* (and they are all deterministic and integer-addressable):

1. **The room volume, not the floor plan** (Alexander #191, *The Shape of Indoor Space*). Correlate ceiling height to room *proportion* per `SpaceProgram`: a 4×4 at 3.2 m is a box, at 4.5 m a *hall*, at 2.5 m a *compression*. This is the cheapest rhythm variable and the strongest interior signature.
2. **Hierarchy: one focal element per room** ("The Fire", #181) — a landmark column, a focal wall, or a light pool. A room with a center reads as *designed*; one without reads as a corridor-fragment. This is the "1–2 salient features per view" JND budget (§6) applied per room.
3. **Rhythm at human proportions.** Repeated elements (column bays, alcoves, fixture pitch) on the coordinated 4·ISO-M module — the artistic step is that bays *read as bays* (visible rhythm), not random columns.
4. **Compression → expansion at the entrance** (threshold room, #112). Every room gets a pinch at the doorway opening into the space — the canon's door-to-corridor reveal, and the strongest "whoa" move per room.
5. **Ornament via rewrite rules** (Hansmeyer). 2–4 subdivision steps of a base tile — cornices, column capitals, coffered ceilings, paneled doors — *crafted* detail at constant cost. This is fractal applied *inside* the room: the self-similar detail layer.
6. **Correlated material language** (parametricism). Room width ↔ ceiling height ↔ fixture pitch ↔ finish must sample the *same* correlated field (§6), or the room reads as noise. The genomes already define the languages; the missing piece is correlation.

### 7.3 The `compose_interior` pass

A new `use_cases/interior.rs` module: `compose_interior(seed, program, footprint, genome) -> InteriorPlan`, producing the `interior` element list on the `AssemblyInstance`:

- Reuses the exact `fixture_plan.rs` pattern — candidates → reject over doorways/clearance → commit — generalized from fixtures to interior elements (alcoves, interior columns, partitions, ceiling articulation, light pools, furniture clusters).
- Every element must clear the **passable-column predicate** (§4.1): furniture and partitions are the #1 way the solver's rooms re-block doorways. The doorway-preservation pass runs *after* interior composition.
- The CA gets its second job: **stains/decay/disarray as a cellular automaton over the interior**, so every furnished room is *disturbed* differently — the "someone was here, no one is here" dread (`cellular_decay.rs` extended to interior elements).

### 7.4 Integration with the other layers

- **Substrate**: the room detail is where the fractal shows *inside* the room (ornament subdivision) and the CA shows *as decay* (not just fabric drift).
- **Grammar layer**: the solver scores the room's *seat*; `compose_interior` scores/realizes the room's *interior*. The Alexander terms (§5.1) flow straight into per-room composition.
- **Perceptual layer** (§8): isovist scoring *inside* rooms — alcoves change the isovist, rhythm targets apply per room, and the JND budget (one alcove / one light pool / one landmark per view) is enforced here, not hoped for.

---

## 8. Perceptual layer (Phase C — after B)

- **Isovist scorer**: DDA visibility flood through the SVO (`octree_gpu_serializer`/`raymarch` traversal) computing area, radial variance, occlusivity per vantage. Deterministic, per-chunk, on demand. Uses: (a) accept/reject rooms against openness targets in `score_placement`, (b) score the walk-path isovist *series* (mean/variance/rate-of-change = rhythm), (c) deliberately break occlusion at thresholds.
- **Confusion metrics with target bands**: mean space-syntax integration; "same-hallway-twice" collision count via variation-salts per junction (identical topologies → different realized widths/textures/light).
- **Non-Euclidean via topology**: wrapping repetition (chunk-coordinate modulo), "Tardis" wrapped interiors, exponentially-branching corridor semi-lattices — all data-level, no renderer work.

---

## 9. Implementation checklist (ordered)

**Phase A — wire it green**
- [ ] A1: Remove `#[allow(dead_code)]` on `mod layout; mod territories;` (`region_plan/mod.rs:107-110`).
- [ ] A2: Replace the cursor loop (`mod.rs:156-211`) with `legs_of` + `free_territories` + `lay_out_suites` (§3 snippet), keeping post-pass order.
- [ ] A3: Doorway-preservation: reject footprints covering committed entrances; re-probe narrow entrance columns after commit; repair lintel or reroll seat (§4.1).
- [ ] A4: Red-room reservation: give corruption a promotable seat independent of solver leftovers (§4.2).
- [ ] A5: Drift-band keep-out on solver candidates; audit `place_suite` for epoch-purity (§4.3).
- [ ] A6: Run `cargo test` — the three tests green, plus the full `region_plan` + `level_zero` suites.

**Phase B — grammar layer**
- [ ] B1: Extend `score_placement` with Alexander terms + integration bias (§5.1–5.3), each behind a `LevelTuning` weight.
- [ ] B2: Correlated pink-noise fields for proportions; blue-noise/JND detail placement (§6).
- [ ] B3: `loop_density` semi-lattice thickening in `fabric.rs` (§5.4).
- [ ] B4: Golden-bytes + property tests: determinism, seam tiling, walkability at epoch 0/N and strain tiers.

**Phase B·2 — interior composition (detailed rooms + artistic architecture)**
- [ ] B5: `InteriorPlan` + `InteriorElement` on `AssemblyInstance` — alcoves, interior columns, partitions-within-rooms, ceiling articulation, light pools, furniture clusters (new `use_cases/interior.rs`).
- [ ] B6: `compose_interior(seed, program, footprint, genome) -> InteriorPlan` reusing the `fixture_plan.rs` rejection pattern (candidates → reject over doorways/clearance → commit); runs after the solver, before voxelization; all elements clear the passable-column predicate (§7.3).
- [ ] B7: Room-volume rules: ceiling height correlated to room aspect per `SpaceProgram` (the Shape of Indoor Space term, §7.2.1).
- [ ] B8: Focal-element rule — exactly one landmark per room ("The Fire", §7.2.2): landmark column, focal wall, or light pool.
- [ ] B9: Trim/ceiling-system detail: `wall_trim` (baseboard, door casing, plaques), ceiling grid + missing-tile plenum glimpses (§7.1).
- [ ] B10: Furniture driven by `ArchitectGenome.furnishing_density`; interior stains/decay as a second CA over the interior (§7.3).
- [ ] B11: Doorway-preservation re-run after interior composition; interior determinism + non-blocking tests.

**Phase C — perceptual layer**
- [ ] C1: Isovist scorer module (`use_cases/isovist.rs`) over the SVO.
- [ ] C2: Rhythm-of-walk scoring on the corridor graph; confusion target bands.
- [ ] C3: Variation salts + wrapping/Tardis topologies.

---

## 10. Test plan & world-byte impact

- **World bytes change** with A2 (the solver's placement differs from the cursor walk). Keep `GeneratedChunk` wire version (`chunk_codec.rs` magic `0x564B_4307`) and pin new goldens. This is the *deliberate, separately-tested change* the architecture doc already anticipates. B·2 adds interior elements to the plan → another goldens bump.
- **New tests:** `passable_columns_at_epoch_n_and_strain` (§4.1), `plan_is_epoch_stable` (§4.3), red-room density invariant (§4.2), interior determinism + **interior-elements-never-block-doorways** (§7.3), `furnishing_density` steering (zero → empty rooms, max → furnished), plus the existing determinism/seam tests extended to the solver + interior output.
- **Streaming:** solver and `compose_interior` are pure `(seed, region)`; no shared mutable state; `taken`/`next_id` are local to the call — verify no ordering dependence when two chunks query the same region.

---

## 11. Risks

- **Density vs. corruption headroom**: raising `ROUNDS` to 3 or loosening `ROOM_SEPARATION` starves the corruption pass (failure 2 returns). Keep the documented trade (`layout.rs:55-62`).
- **Epoch-stability regressions**: any future pass reading `RealitySnapshot` at plan time re-breaks `peripheral_shift_rearranges_fabric_but_never_the_plan`. The `plan_is_epoch_stable` test is the guard. Interior elements must be epoch-stable too (they are — pure `(seed, program, footprint)`).
- **Interior vs. traversability**: furniture/partitions are the #1 way packed rooms re-block doorways — the doorway-preservation pass and the passable-column tests must run after `compose_interior`, not before (§7.3, B11).
- **World-byte scale**: interior composition multiplies per-room element counts. Keep the element list compact and JND-budgeted (1–2 salient features per view, §7.2.2); expand `chunk_codec` if a wire version needs it.
- **Perceptual-layer cost**: isovist scoring is O(view) per vantage — bound it (coarse lattice of sample points, memoize per `(cell, epoch)` like `menger_expanse.rs:164`) before it hits the per-frame path.
