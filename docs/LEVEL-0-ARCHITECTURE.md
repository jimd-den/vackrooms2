# Level 0 — "Architecturally Perfect" Design Report

- **Date:** 2026-08-06
- **Scope:** A blueprint-science / architectural-science / CAD-BIM-grounded design and gap report for a canon-faithful, *breathing* infinite Backrooms Level 0, driven by **fractal math** and **Conway's Game-of-Life (cellular-automata) math**.
- **Goal:** infinite, unique, billions-of-permutations architecture that reads as *real building* (fit-out + structure), evolves over time ("breathing"), and is exactly the "Yellow Hell" of the Wikidot canon.

---

## 1. Executive summary — the design thesis

Level 0 should be read as **one infinite commercial building seen through two of its layers**:

- **Level 0** = the *tenant fit-out*: carpet tile over concrete, painted gypsum partitions, dropped acoustical ceiling, recessed fluorescent troffers, door frames, baseboards, exit signs. Everything finished and code-compliant.
- **Level 1** = the *raw shell* of the same building: exposed concrete slab, massive columns, pipes and ducts hanging where the plenum used to be (parking-garage anatomy).
- The flickering-wall exit = **no-clipping through the finish into the guts**.

Two mathematical engines generate the level:

1. **Fractal self-similarity** is the *spatial* grammar. The Backrooms' horror is that the layout is the same *pattern* at every scale — a corridor contains rooms that contain corridors. This is fractal architecture (Goldberger 1996; Ostwald & Vaughan, *The Fractal Dimension of Architecture* 2016; *Fractal Cities*, Batty & Longley 1994) and it is already half-built in the engine: the Menger sponge is literally the 3D fractal of wall-and-void.
2. **Cellular automata (Game of Life)** is the *temporal* grammar. The **Peripheral Shift** — the level warping when unobserved — should be a cellular-automaton evolution of the wall/door lattice, not a random re-deal. Walls bloom, settle, narrow, and seal in coherent organic clumps. The level *breathes*.

The binding trick that makes both work inside the engine's determinism contract is the **implicit CA / implicit IFS**: evolve the plan as pure functions of `(seed, world-coordinate, generation)` — computable on demand per chunk, exactly like the existing noise fields and region plans. No global CA buffer, no cross-chunk reads at stream time, deterministic world bytes, infinite streaming.

---

## 2. Research base (with sources)

### 2.1 Blueprint science — construction documentation

- **Layer standards.** The US National CAD Standard (NCS, NIBS; from the 1990 AIA CAD Layer Guidelines) encodes *building systems as drawing layers*: discipline prefix + major group + minor group — `A-WALL-FULL`, `A-DOOR`, `A-CEIL`, `M-DUCT`, `E-LITE`. A building is literally a *stack of system layers*: architecture, structure, mechanical, electrical, plumbing, civil. (Wikipedia: CAD standards; nationalcadstandard.org; ISO 13567, BS 1192.)
- **Wall assemblies.** At 1:20 a plan draws the *material layers* of a wall (stud + gypsum + finish); at 1:100 it draws overall thickness. In BIM this is `IfcMaterialLayerSet` — a wall **is** a list of layers with thicknesses. (Wikipedia: Architectural drawing, Floor plan; ISO 16739 IFC.)
- **Reflected Ceiling Plan (RCP).** The ceiling is its own drawing set: fixtures, ducts, diffusers, sprinklers, ceiling grid — drawn as a mirror reflection. The ceiling grid *is* the coordination module for lights and devices. (Wikipedia: Floor plan §Reflected ceiling plan; Dropped ceiling.)
- **Schedules.** Fixture/door/window schedules are bills-of-material derived from the model (→ per-tile instance property tables in a voxel engine).
- **Design-intent phases.** SD shows major divisions + approximate areas; CD shows wall types, doors, hardware (→ the plan→voxel "assembly detail" depth ladder).

### 2.2 Architectural science

- **Modular coordination.** ISO 2848: basic module **M = 100 mm** (imperial 4 in); preferred dimensions are multiples of 300/600 mm; the grid coordinates structure, partition, and fixture. (Wikipedia: ISO 2848.) The engine's 0.4 u construction lattice = 400 mm = 4·M — already grid-coordinated by construction.
- **Space Syntax.** Hillier & Hanson, *The Social Logic of Space* (1984): circulation is a graph; **integration** (fewest turns to reach all space) predicts movement, **choice** predicts flow. This is the canonical "which corridors get the most life" metric — and the design handle for placing program on the most integrated lines. (Wikipedia: Space syntax.)
- **Egress / life safety (IBC 2021, Ch. 10).** Means of egress = exit access → exit → exit discharge. Anchors: egress ceiling ≥ 7'6"; **doors ≥ 32 in clear**; **dead-end corridors ≤ 20 ft** (50 ft sprinklered); two exits separated ≥ ½ the diagonal of the served area; **exit signs within 100 ft** of any exit-access point. (up.codes IBC Ch.10.) The uncanny engine: Level 0 should be *egress-compliant on the surface with no exit* — exit-sign theater.
- **Anthropometrics / ADA.** Corridors ≥ 36 in clear; doorways ≥ 32 in; 60 in turning circles. The engine's `cad.rs` already carries these (IBC/ADA-rounded): `CAD_DOOR_WIDTH 0.9`, `CAD_DOOR_WIDTH_ADA_MIN 0.815`, `CAD_CORRIDOR_WIDTH_EGRESS_MIN 1.12`, `CAD_CEILING_MIN_HABITABLE 2.29`. (access-board.gov/ada.)
- **Liminal / Backrooms as architecture.** The original 2024-traced source photo is a 2002 furniture-store back room ("partitions and fake inner walls," mono-yellow, fluorescent hum) — *back-of-house empty retail* is the archetype. Diel & Lewis (2022), *Structural deviations drive an uncanny valley of physical places* (J. Env. Psych. 82:101844): **small deviations from familiar architectural patterns read as eerie** — the scientific license for "almost-normal" architecture. Wiggins, *New Media & Society* 2025 (DOI 10.1177/14614448241238395) analyzes the Backrooms as digital urban legend. (Wikipedia: The Backrooms, Liminal space.)

### 2.3 CAD / BIM software data models

- **Revit.** Parametric families/types/instances; walls/floors/ceilings are *system families* whose geometry is parametric and whose **layer structure** (finish–core–finish) is editable. Rooms/areas carry program; levels/stories and grids are first-class. (Wikipedia: Autodesk Revit.)
- **IFC (ISO 16739).** The formal building schema: `IfcSite → IfcBuilding → IfcBuildingStorey → IfcSpace`; physical `IfcWall/IfcDoor/IfcWindow/IfcStair`; distribution systems (HVAC/electrical) with ports; `IfcGridPlacement` for column grids; `IfcMaterialLayerSet` for assemblies. **This is the exact "systems + assemblies + program + grids" model a voxel engine should mirror per-voxel.**
- **CityEngine CGA.** The procedural grammar: `split`, `extrude`, `comp`, `offset`, `repeat`, `roofGable`; pipeline street→lot→massing→facade. CGA is geometry-first and has *no* room/adjacency/program model — the known limitation. (CityEngine; Müller et al., *Procedural Modeling of Buildings*, SIGGRAPH 2006, DOI 10.1145/1179352.1141931.)
- **Room graphs.** House-GAN (arXiv:2003.06988) consumes a room-adjacency graph and emits axis-aligned room boxes — the "program → floorplan" formulation. Hua 2017 (CGF 36(8)) embeds connectivity + spatial hierarchy in a bi-directional grammar.

### 2.4 Fractal & cellular-automata architecture

- **Fractal architecture.** Goldberger, *Fractals and the birth of Gothic* (Molecular Psychiatry 1(2), 1996): self-similar design spanning scales. Ostwald & Vaughan, *The Fractal Dimension of Architecture* (Birkhäuser 2016): box-counting fractal dimension computed from 85 real house plans — **measurable self-similar complexity in floor plans**. *Fractal Cities* (Batty & Longley 1994): bottom-up self-similar city growth. Real precedent: Sudano-Sahelian vernacular architecture uses literal fractal scaling. (Wikipedia: Ary Goldberger, The Fractal Dimension of Architecture, Michael Batty, Architecture of Africa.)
- **Menger sponge.** 3D Sierpinski carpet: 20^n cubes of side (1/3)^n; **Hausdorff dimension log 20 / log 3 ≈ 2.727**; volume→0, surface→∞; buildable at scale (MegaMenger). The canonical "wall/void fractal," and already the engine's expanse generator. (Wikipedia: Menger sponge.)
- **Cellular automata for layout.** The game-industry "4–5 rule" cave generator: random fill → Life-like CA smoothing → flood-fill connectivity repair (RogueBasin, Brogue, rot.js). CA is a first-class *layout* generator, not just a filter. Herr & Kvan (CAADRIA 2005), *Using cellular automata to generate high-density building form*; Paul Coates et al.: CA as bottom-up "architectonic rule" design.
- **Time-stepped CA urbanism.** Batty, *Cities and Complexity* (MIT Press 2005): CA + agents + fractals evolve urban layouts over time; Batty, Xie & Sun (1999, DOI 10.1016/S0198-9715(99)00017-9): a GIS CA that *grows/changes layout per generation* — the academic precedent for "the level updates over time."
- **Game of Life itself.** B3/S23 Life-like CA — the reference automaton family (Wikipedia: Conway's Game of Life). Note: vanilla Life destroys connectivity and explodes/entropies; a *connectivity-safe* rule or a repair pass is mandatory (see §5.2).

---

## 3. What the engine already has (asset inventory)

| Capability | Where | Status |
|---|---|---|
| IBC/ADA CAD vocabulary | `domain/entities/cad.rs` (doors 0.9/0.815, egress 1.12, ceiling 2.29, stair riser/tread, handrail, wall 0.2) | ✅ but Level 0 uses historical dims — rebase is a deliberate, separately-tested change (`cad.rs:9-11`) |
| Modular grid | 0.4 u construction lattice (`region_plan/mod.rs:44`), 400 mm = 4·ISO M; corridor widths on 0.8 u | ✅ |
| Structural systems + bay grids | `architecture.rs:42-46,374` (`DeepSpansWithBeams`, `CoreAndShell`, `StructuralSystemInstance`) | ✅ |
| Ceiling zones/bands | `fabric.rs:75-131`, `suites.rs:56-114` (compression/regular/expanse/vault) | ⚠️ heights yes, ceiling *system* no |
| Fixture grid by bay phase | `fixture_plan.rs:71-264` | ✅ geometry-driven placement |
| Fixture maintenance (districts, flicker) | `fixture_maintenance.rs:51-115` | ✅ |
| Connectivity by construction | portals (`world_topology.rs:72-92`), spine (`circulation.rs:50-74`), binary-tree doorway rule (`fabric.rs:243-251`) | ✅ |
| Typed office program + ideal areas | `region_plan/layout.rs` (OpenOffice 220, ConferenceRoom 90, PrivateOffice 45, RestroomCore 40, Mechanical 50…) | ✅ wired — scored greedy placement drives `generate_region_plan` |
| Free-space decomposition | `region_plan/territories.rs` (maximal free rectangles, largest first) | ✅ feeds the placement scorer |
| **Fractal generation** | `menger_bsp.rs` (Menger 3×3 carve-center recursion), `menger_expanse.rs` (plates + WFC module pick) | ✅ expanse only |
| **Cellular automata** | `suites.rs:467-503` (partition-retention CA), `cellular_decay.rs` (Moore-neighborhood decay CA: carpet saturates 3, wall ages 2, crumbles 4) | ✅ post-passes only |
| The "breathing" state machine | `RealitySnapshot` (RTY v5, drift epochs per 40 u cell, strain tiers); blackout rear-shift cadence (`engine/peripheral_shift.rs`) | ✅ but drift is re-hash, not CA |
| Anomalies as assembly failures | `anomalies/geometry.rs` (Pillar/Blackout/Pit/Arch), red rooms, arches w/ CultureSeam/ScaleBreach | ✅ |
| Material decay | `cellular_decay.rs` | ✅ |
| Almond water / supplies / doors-to-Level-1 | `provisions.rs` | ✅ |

---

## 4. Gap analysis — what's missing (mapped to code)

### 4.1 The system/assembly layer model (blueprint science: NCS layers, IFC material layers)

**Missing:** the engine has *materials* (wall, carpet, ceiling) but not *assemblies* — a wall is a single voxel, not `stud + gypsum + finish`; the ceiling is a height, not `structure + plenum + T-bar grid + tile + troffer + sprinkler`.

**The move (fits your architecture perfectly):** extend the 2.5D `ColumnPlan` into an **assembly stack** — per column: `finish/floor` → `slab` → `structure` → `plenum` → `ceiling grid/tile` → `fixture` → `sprinkler/grille` → `wall assembly (stud+gypsum+baseboard)`. This is *literally* an `IfcMaterialLayerSet` per column, and it is the thing that makes Level 0 read as a *building* rather than a maze. It also creates the Level 1 bridge: Level 1 = the same stack *minus* the ceiling system and finishes (structure + plenum exposed).

### 4.2 The ceiling system & RCP (reflected ceiling plan)

**Missing:** no ceiling *grid* (T-bar), no missing tiles exposing the plenum, no troffers/diffusers/sprinklers as coordinated elements, no RCP semantics.

**The move:** add a ceiling sub-module on the 0.6 m-equivalent grid (reads on the 0.4 lattice as 1.6 u / 4 tiles per 6.4 u bay), fixtures placed *on the ceiling grid* (extends `fixture_plan.rs`), plus **missing-tile events** — the signature motif: occasional voids in the ceiling revealing ducts and pipes above, which are the *same* pipes Level 1 exposes. The plenum is the architectural seam between the levels.

### 4.3 Egress theater (IBC Ch. 10)

**Missing:** exit signs, emergency lights, fire-rated doors, door swing direction, dead-end control. Level 0 should be **surface-egress-compliant with no exit** — the deepest canon horror.

**The move:** a life-safety pass along the spine and fabric corridors: exit signs above thresholds (`E-LITE`), emergency fixtures, double-leaf egress doors at assembly fronts, "exit" signage everywhere pointing into the void. Optionally use IBC dead-end limits (≤20 ft) as a *measure* that the corruption/CA pass deliberately violates by design.

### 4.4 Circulation as Space Syntax graph

**Missing:** connectivity is guaranteed by construction, but nothing measures *integration/depth* — so program isn't placed by "which corridor has the most life," and "maze-ness" isn't tunable.

**The move:** compute an integration measure over the region plan's corridor graph (cheap, deterministic) and use it to (a) bias `SpaceProgram` placement toward integrated lines (movement economy) and (b) expose a `maze_factor` knob = inverse of integration, so the Corruption/CA passes can be tuned to produce "unfathomable" vs "merely labyrinthine."

### 4.5 Program wiring + room adjacency

**Done (2026-08-06).** The cursor walk is gone; `generate_region_plan` now runs free-space decomposition (`territories.rs`) and scored greedy placement (`layout.rs`) over every corridor leg, not just the dominant one. Measured over 81 regions at seed 42, a region went from **1.73 to 4.49 assemblies, with no region left empty** (one was, before the fixes below), and the draw is genuinely typed rather than a near-uniform open-office roll.

Four defects surfaced only once the solver was actually driving the plan, each fixed at its cause rather than at the call site:

- **Room size was drawn once per program-round**, so every candidate for a program was the same room in a different spot — a region whose corridor legs were all short rejected that program everywhere (1 region in 81 generated *no* assemblies at all, and it was the one the red-room macro event targeted). Size is now part of the placement, advanced per anchor by the golden-ratio conjugate. This also un-deadens the scorer's area term, which previously could never discriminate.
- **Leg containment double-counted the end margin**, demanding a room fit inside `leg_length − 8 u`; a genome drawing wide rooms then fit nowhere. Containment is now against the leg's own span, which is what "the front wall faces corridor" actually requires.
- **Entrance validation used `any`**, so a door correctly banded against its own corridor could sit squarely inside a *perpendicular* one — where `compose_column` takes the circulation branch first, carves the column full height, and the doorway loses its lintel. Now shared as `suites::entrance_faces_route`, which requires banding against some corridor *and* clearance from every corridor.
- **Duplicated suites had nowhere to go.** Against a laid-out floor, every offset in the old six-entry ladder landed on a neighbour, and duplication — the most recognizably Backrooms corruption there is — silently stopped happening in all 36 test regions. Duplicates now sweep the corridor for the gap the solver actually left, and may cross to a parallel run; they also keep the same clearance as any other room, instead of the looser 0.4 u that let a neighbour's fixture sample inside an abandoned shell.

**Open — the program mix is skewed, and it is a tuning call, not a bug.** Across those 81 regions the draw is Atrium 72, OpenOffice 74, ConferenceRoom 66, Reception 60 — but Mechanical 1, RestroomCore 2, PrivateOffice 3, Storage 3, ServerRoom 3. Two consequences worth deciding on deliberately:

- Only ~4–5 rooms fit per region, because rooms may only hang off corridor legs and legs are short. `PROGRAM_BUDGET` is ordered strictly large-public-first, so the big rooms take every seat and the service programs — tried last — find nothing left. That directly contradicts the budget's own stated intent ("a floor plan that never shows its plumbing or its plant reads as a set rather than a building").
- An Atrium in 89% of regions reads wrong: an atrium is a special space, not a per-region fixture.

The fix is a distribution decision (weight the budget, rotate which programs get first pick per region, or give service rooms their own smaller-footprint seats away from corridor frontage), so it is left for authorship rather than guessed at here.

**Also still open:** a room-adjacency graph (House-GAN style) so program adjacencies are first-class, and per-program fixture/lighting language (RestroomCore → tile + mirror; Mechanical → louver + conduit; BreakRoom → tables). Placement is solved; *furnishing* it is §4.6.

### 4.6 Furniture / props

**Missing:** ~one prop voxel total (`VOXEL_CRATE`). Canon everywhere describes objects (desks, computers, hospital beds, paintings, crates).

**The move:** a `furnish` pass per `SpaceProgram` reusing `fixture_plan.rs`'s rejection pattern (largest-first, clearance, reachability, retry).

### 4.7 Facade / exterior envelope

**Missing:** no windows, no exterior expression. Note: canon Level 0 is *windowless*, so the "envelope" here is the **back-of-house wall**: blank, outlet-scarred, baseboarded, with the occasional sealed-over window or door frame. Model as wall-assembly detail, not glazing.

### 4.8 Verticality (canon stairs)

**Missing:** canon Level 0 has *stairs*; the engine's stairs are landmarks. `WorldNode::elevation` + `vertical_circulation.rs` are reservation-only.

**The move:** real stair *cores* that read as multi-level structure (even if travel is deferred): egress stair towers at the ends of spines, `E-LITE` above, tread/riser from `cad.rs` (`CAD_STAIR_RISER_MAX 0.178`, `CAD_STAIR_TREAD_MIN 0.279`).

---

## 5. The generative design — fractal + Game of Life that breathes

### 5.1 The spatial grammar: implicit fractal self-similarity

The Backrooms' signature is **self-similarity**: the same "yellow back-room" motif at every scale — a corridor is made of rooms that contain corridors. That is fractal architecture, and the engine already has the seed (`menger_bsp.rs`).

**Proposal — an implicit IFS (iterated function system) over the plan hierarchy.** Define a single recursive motif `M(seed, world-coords, scale, params)`:

- At each scale level (160 u macro cell → 80 u region → assembly → 7.2 u fabric cell → 0.4 u construction), `M` returns the layout as a *scaled variant of the same grammar*: walls on a module, a doorway rule, a ceiling band, a lighting row, a decay profile.
- The recursive *parameters* (doorway side, porosity, ceiling height, structural system, lighting language, corruption) are perturbed deterministically per level — so the whole is self-similar (same grammar) yet every branch differs (different parameter draw). This is exactly the "sameness that blends into one big yellow blur" with billions of variations.
- The Menger sponge generalizes from the expanse to the whole: level *n*'s walls contain level *n+1*'s pattern. Fractal dimension of the generated plan becomes a *tunable* (measure with box-counting per Ostwald & Vaughan).

**Permutation space (billions):** each branch is `f(seed, rule_id, scale, params)`. With ~8 branching decisions per level and 5 levels, a region already has ~10^4 variants; × rule space (a dozen CA rules, §5.2) × generation space (CA epochs advance the world over time) → astronomically beyond billions, all deterministic.

### 5.2 The temporal grammar: Game of Life as the Peripheral Shift

The canon's **Peripheral Shift** ("the layout warps, stretches, or rearranges itself whenever not directly observed") is *by definition* a cellular automaton — a local, synchronous rule over the wall/door lattice. Today the engine re-salts by *hash* (`fabric.rs:256-349`), which teleports rooms. Make it *evolve*:

**Proposal — an implicit cellular automaton over the 7.2 u fabric lattice (and the 40 u drift cells).**

- State per fabric cell: `{wall_w, wall_n, doorway_w, doorway_n, porosity, ceiling_band, light_state}` — a *local, sync-read* CA, exactly like `cellular_decay.rs` but over the plan lattice instead of the voxel grid.
- Each drift epoch = one CA generation. A Life-like rule (e.g. a "wall-settle" rule in the B/S family, tuned) makes walls **bloom into coherent clumps, open and close doorways in waves, narrow and widen corridors, seal dead-ends then re-open neighbors** — organic breathing instead of random re-deal.
- **Connectivity safety:** vanilla B3/S23 destroys connectivity. Two options, both keep the engine's guarantee: (1) a *connectivity-safe* rule family (like the existing binary-tree doorway rule, but CA-selected), or (2) the CA runs, then the existing "guaranteed doorway" invariant re-asserts connectivity chunk-locally (`fabric.rs:243-251`) — CA decides *which* walls shift, the invariant decides the floor. Prefer (2): it reuses proven machinery.
- **Strain/delirium as rule-pressure:** canon's mismanagement (strain tiers) already feeds drift cadence (`peripheral_shift.rs`); extend it to *rule* selection — under strain the CA rule shifts toward sealing (fewer doorways), exactly the canon's "bricked-over guaranteed doorways."

### 5.3 The binding trick: implicit evolution (determinism + chunk-independence)

The engine forbids CA over voxel neighborhoods during sampling because chunk-independent world bytes require no cross-chunk reads (`cellular_decay.rs:6-9`). The solution is to make the CA **implicit**:

- A fabric cell's state at generation `t` is `CA(seed, cell, t)` — a *pure function*, precomputable on demand from world coordinates, like the existing noise fields and region plans.
- `RealitySnapshot` advances `t` locally (drift on abandon; blackout rear-shift in real time, already implemented in `engine/peripheral_shift.rs`).
- Any chunk, in any streaming order, samples `CA(seed, cell, t)` for its cells and never needs a global buffer. Determinism, seam tiling, streaming, and infinite permutation all hold.

This is the single most important design decision in the report: **the level doesn't store its own history — it computes it.**

### 5.4 Billions of permutations, made concrete

| Lever | How | Scale |
|---|---|---|
| Seed space | u32 seeds | 4.3 billion base worlds |
| Fractal rule + scale params | per-level IFS parameter draw | ×10^4+ per region |
| CA rule space | rule id + generation `t` | ×(dozens of rules)^epochs |
| Strain/delirium | rule pressure | multiplies again |
| Anomaly/red-room placement | Matern hard-core + promotion | orthogonal |

Composition (seed × rule × epoch) is far beyond 10^9 distinct per-region layouts, all reproducible and streamable.

### 5.5 The assembly/system layer model (from §4.1) — the "blueprint" feel

Every column carries its system stack (finish/slab/structure/plenum/ceiling/fixture/wall-assembly). The visual result: a cross-section of Level 0 looks like a construction detail sheet; the RCP (ceiling grid + troffers + sprinklers + missing tiles) looks like a reflected ceiling plan. That is "architecturally perfect" — it reads as *drawn* architecture, not a maze.

### 5.6 Egress theater + Space Syntax (from §4.3–4.4)

Program sits on the most integrated lines (integration from the corridor graph); exit signs, emergency lights, fire doors, and double egress doors make the *surface* code-compliant; the CA + corruption pass then *violate* dead-end limits by design. Surface order, deeper chaos — the Diel & Lewis uncanny valley in practice.

---

## 6. Implementation roadmap

**Phase 0 — Groundwork (world-bytes change, golden tests)**
1. Rebase Level 0 onto `cad.rs` dimensions (doors 0.9×2.1, egress corridors ≥1.12, standard ceiling 3.0) as a deliberately versioned world-byte change with golden-bytes tests (`cad.rs:9-11` already calls for this).

**Phase 1 — The assembly stack (blueprint feel)**
2. Extend `ColumnPlan` into the system/assembly stack (finish → slab → structure → plenum → ceiling grid → fixture → wall assembly). Keep serialization-compatible or versioned.
3. Ceiling sub-module: T-bar grid, troffers on grid (extend `fixture_plan.rs`), sprinklers/grilles, and **missing-tile plenum glimpses** that reveal the ducts/pipes shared with Level 1.

**Phase 2 — The implicit fractal + CA engine (the centerpiece)**
4. Implement the implicit IFS: `M(seed, region, scale, params)` = recursive self-similar motif across macro-cell → region → assembly → fabric → construction.
5. Implement the implicit CA: fabric-lattice state as `CA(seed, cell, t)`; replace the re-hash drift in `fabric.rs` with CA evolution; wire `t` to `RealitySnapshot` epochs and blackout rear-shift; keep the binary-tree connectivity invariant as the repair floor.
6. Add connectivity-safe rule families + strain-based rule pressure.

**Phase 3 — Program, circulation, egress, furniture**
7. ~~Wire the typed program (target areas from `region_plan/layout.rs`; finish + activate the solver in `layout.rs`/`territories.rs`).~~ **Done** — see §4.5.
8. Add Space Syntax integration over the corridor graph; bias program placement to integrated lines; expose `maze_factor`.
9. Egress theater: exit signs, emergency fixtures, fire doors, double egress doors.
10. `furnish` pass per `SpaceProgram` (desks, chairs, tables, restroom fixtures) via the `fixture_plan.rs` rejection pattern.
11. Stair cores that read as vertical structure (tread/riser from `cad.rs`).

**Phase 4 — Level 1 as the revealed shell**
12. Express Level 1 as the same building *minus* ceiling system and finishes: exposed structure, plenum pipes/ducts, parking-scale columns. Sectors = structural-system states (Aquila = raw bay; Gild = back-of-house storage; Gothic = masonry arches; Construction = fit-out caught mid-construction).

---

## 7. Risks & open questions

- **World-byte stability.** Phases 0–2 change every generated byte. Mitigate: versioned `GeneratedChunk` wire format (already versioned, `chunk_codec.rs` magic 0x564B_4307), golden-bytes tests, separate feature branch.
- **CA cost.** Implicit CA state must be cheap per cell — keep state tiny (a few bits per cell) and memoize (like `menger_expanse.rs:164`). Recompute-on-demand across epochs must be amortized, not per-column brute force.
- **Connectivity under CA.** The binary-tree invariant must hold across rule families; verify with a flood-fill reachability test in the generation test suite (currently no BFS closure check exists).
- **Fractal sameness vs. variation.** Too much self-similarity → monotony; too little → not Backrooms. The parameter-perturbation distribution and the `maze_factor`/integration knobs are the tuning surface.
- **Perf (per-epoch drift rebuild).** CA-driven drift must stay within the existing drift budget; the blackout rear-shift cadence already bounds in-place rebuilds (`engine/peripheral_shift.rs`).

---

## 8. Sources

Blueprint/CAD: NCS (nationalcadstandard.org), Wikipedia *CAD standards, Architectural drawing, Floor plan, Autodesk Revit, CityEngine, L-system*; IFC ISO 16739. Building science: ISO 2848; IBC 2021 Ch.10 (up.codes); ADA 2010 (access-board.gov/ada); Wikipedia *Space syntax, Dropped ceiling, Concrete block*. Liminal: Wikipedia *The Backrooms, Liminal space*; Diel & Lewis 2022 (DOI 10.1016/j.jenvp.2022.101844); Wiggins 2025 (DOI 10.1177/14614448241238395). Fractal/CA: Goldberger 1996; Ostwald & Vaughan 2016; Batty & Longley 1994; Batty 2005; Batty/Xie/Sun 1999 (DOI 10.1016/S0198-9715(99)00017-9); Herr & Kvan CAADRIA 2005; RogueBasin CA cave gen; Wikipedia *Menger sponge, Conway's Game of Life, Michael Batty, Ary Goldberger, The Fractal Dimension of Architecture*. Procedural: Müller et al. SIGGRAPH 2006 (DOI 10.1145/1179352.1141931); House-GAN (arXiv:2003.06988); Hua 2017 (CGF 36(8)). Backrooms lore: backrooms-wiki.wikidot.com (Levels 0, 1, 34, 90).
