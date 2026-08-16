# Audit Part 5: wasm_frontend/src/application/** + wasm_frontend/src/lib.rs
## Static audit: algorithms (Big-O), concurrency, worker lifecycle, atomics

**Status: COMPLETE — all sections verified against source, pure file reading only (nothing compiled/run).**
**Method: pure file reading only, nothing compiled/run.**

---

## 1. Big-O table (verified against source)

| Algorithm / site | Complexity | Where | Notes |
|---|---|---|---|
| `desired_origins_in_view` | O(N log N) time, O(N) alloc per call | streaming.rs | N=(2R+1)^2, called per frame |
| `ChunkStore::retain_keys` | O(N·M) | streaming.rs | keep is Vec; linear contains |
| `stream_chunks` keep/evicted filters | O(M·N) per frame | engine.rs | keep.contains in loops |
| `select_scene_lights` | O(L)+O(L log L) + allocs per frame | prepare_frame_lighting.rs | called engine.rs |
| `flares.extinguish_buried` | O(F·B) per changed tick | flares.rs | F<=12, B=all boxes |
| `RecordKey` archive fetch | O(1) lookup but level-ambiguous key | chunk_archive.rs | CRITICAL cross-level |
| worker generation | O(1) queue ops, promise-chain serialized | worker.js / worker_source.rs | bounded by max_pending |
| lib.rs atomics | O(1) per frame | lib.rs | Relaxed, single-threaded |

## 2. Concurrency inventory — see §10 (complete)

---

## 3. Findings — engine.rs (verified line numbers)

### E1 [High] Per-frame O(M·N) linear scans in `stream_chunks` (keep is a Vec)
- engine.rs:1019-1022 (`keep: Vec<_>` from desired_visual), 1025-1030 (`evicted` filter `!keep.contains(k)` over all resident chunks), 1031 (`store.retain_keys(&keep)`), 1032 (`forced_reloads.retain(|key| keep.contains(key))`), 916-917 (`pending.retain(|key, request| keep.contains(key) ...)`).
- With surface-renderer visual radius R=16 → N≈1089 desired and M≈1089 resident: ~3 passes × ~1.2M tuple comparisons per frame ≈ 3.6M compares/frame (~1-3 ms) every frame, even when nothing changed. `retain_keys` (streaming.rs:309-314) does `keep.contains` linear scans again inside (order.retain + chunks.retain).
- Fix: build `keep: HashSet<ChunkKey>` once (streaming.rs:309 take `&HashSet<ChunkKey>` or a slice sorted for binary search); all contains become O(1). Cost drops to O(N+M).

### E2 [Medium] `desired_origins_in_view` O(N log N) + allocation every frame (streaming.rs:170-203, called engine.rs:1007-1018)
- N=(2R+1)²≈1089 for surface renderers; the full Vec<(f32,f32)> is rebuilt and sorted nearest-first per frame regardless of movement. ~11k compares + Vec alloc per frame — small but fully redundant when the player hasn't crossed a chunk boundary or changed yaw enough to change the cone result.
- Fix: cache the previous result keyed by (player chunk, yaw bin); recompute only when the key changes (or when store changed). Low effort, removes per-frame alloc + sort.

### E3 [Medium] `select_scene_lights` per-frame O(L) HashMap dedup + O(L log L) sort + allocations (engine.rs:485-490; prepare_frame_lighting.rs, verified below)
- Called every tick with ALL lights of ALL resident chunks (L up to ~4k for a 1089-chunk footprint at ~4 lights/chunk). Dedup set is stable between chunk-set changes; ranking keys (intensity) are static pre-flicker — flicker is applied after selection (engine.rs:494-497). Only camera distance changes per frame.
- Fix: cache the deduped light Vec (invalidate on store change via `changed` flag) and, per frame, run a bounded top-48 selection (heap/quickselect, O(L)) instead of a full sort.

### E4 [Medium/High] Unbudgeted synchronous forced-reload loop (engine.rs:1065-1090)
- Sync path (`LocalChunkSource`, no workers): forced reloads (RedRoom commit → force_rebuild on EVERY resident chunk, engine.rs:660-670; also pickups 712; blackout rear-shift) load ALL of them in one tick with no budget metering; each `load_with_artifacts` is a blocking full generate (~42 ms fine / ~6.5 ms coarse in wasm per header comment in archived_chunk_source.rs:8-11). 9-25 forced chunks → 0.4-1.0 s freeze.
- Fix: budget the forced loop like phase 1/2 (deduct from `budget`, defer the rest into next tick), or make RedRoom commits staged over ticks.

### E5 [Low] Per-frame gate sort/dedup and supply-item scans
- engine.rs:633-635 gates collected+sort+dedup EVERY frame (O(G log G), G≈resident gates, could be ~hundreds in a 1089-chunk footprint); 686-695 same for grabbed supplies; 737-761 `collect_supply_sprites` O(S log S) per frame with Vec alloc. S and G are usually small (dozens), so Low — but all three are recomputed from scratch per frame; cache by store generation counter.

### E6 [Info/OK] Good: event-driven collision rebuild
- `world.rebuild` only on `changed` (engine.rs:1190/1288), install budget metering (841-851, 913), monotonic LOD refinement, request identity (request_id + reality) stale-completion rejection (858-867), forced-reload player-safety check (881-895), `switch_level` clears pending/backlog/forced + rejects wrong-level completions (377-405, 866). These are well-designed; no finding.

### E7 [Low] `pending.retain` drops in-flight worker work after eviction (engine.rs:916-917)
- When a chunk leaves the footprint, its pending entry is dropped but the worker still processes the request; if the same chunk is re-requested later, two in-flight generations exist for the same chunk (second hits the worker archive → cheap decode, so bounded waste). Not a bug — documented design; note as Low.

### E8 [Info] switch_level retains `[]` (engine.rs:379) — O(M) fine; does NOT reset `reality` — feeds the Critical cross-level finding (see F1 below).

---

## 4. Findings — streaming.rs (verified)

### S1 [High] `ChunkStore::retain_keys` O(N·M) (streaming.rs:309-314)
- `self.order.retain(|k| keep.contains(k))` + `self.chunks.retain(|k,_| keep.contains(k))` — keep is `&[ChunkKey]`, so each retain is O(N·M). Called from engine.rs:1031 every frame with the full keep set.
- Fix: parameterize over `&HashSet<ChunkKey>` (or sort keep once and binary-search). See E1.

### S2 [Medium] `desired_origins_in_view` sort per call (streaming.rs:195-201)
- Sorting N≈1089 tuples by squared distance per frame; fine in isolation but per-frame redundant. See E2.

### S3 [Info] `chunk_key` mm quantization (streaming.rs:22-24) — stable for integer-multiple origins; no collision issue found.

---

## 5. Findings — CRITICAL: cross-level archive staleness (RecordKey without level)

### F1 [CRITICAL] RecordKey omits `level` → worker archive serves wrong level's geometry after a runtime level switch
Data flow (all verified in source):
1. `RecordKey` (chunk_archive.rs:150-169) = { chunk, lod, artifacts, reality } — NO level. Record header (chunk_archive.rs:57-59) likewise has no level byte.
2. `ArchiveIdentity` DOES include level (chunk_archive.rs:63-78, `pub level: u32` at 65; written at 124), but identity is checked ONLY at `ChunkArchive::open` (chunk_archive.rs:259-279: `if existing == Some(identity)` else reset). No per-fetch identity check.
3. `ArchivedChunkSource::with_budget` builds identity from `config.level` (archived_chunk_source.rs:99-105) — the BOOT level from the URL query, captured once at `worker_init` (lib.rs:1114-1144; `generator_setup_from_query(query, ...)`).
4. `worker_generate` (lib.rs:1156-1178) receives `level` from the message and passes it to `load_with_artifacts` → `fetch` (archived_chunk_source.rs:146-193): `RecordKey::new(chunk_key(origin_x, origin_z), lod, artifacts.bits(), reality)` at 155-160. On a HIT (166-170) the payload is returned with NO level check; `level` only flows into generation on a miss (177-179).
5. The engine switches level at runtime via `switch_level` (engine.rs:377-405) — `self.level = target` (378) — and does NOT reset `reality` (no reality mutation anywhere in 377-405). Requests are stamped with `level: self.level` (engine.rs:797 in make_request_for) and issued to the SAME worker pool (source created once at boot; switch_level never re-creates workers).
6. `RealitySnapshot` carries no level (verified in prior analysis; engine.rs:263 type only).

Exploit scenario (door Level 0 → Level 1): player spawns at (5,5) in chunk (0,0) at level 0 → chunk (0,0) lod1 is generated and ARCHIVED in the worker's shard. Player walks to the door at (8,5) (still chunk (0,0)), transits → `switch_level(1, Some([3,1.7,3]))` (engine.rs:724-727 via update_provisions). Streamer re-requests chunk (0,0) at level 1 → same RecordKey (same chunk key, same lod, same artifacts, same reality hash — no gate crossing happened) → archive HIT → the LEVEL-0 record is decoded and installed (engine.rs:866 level check only rejects completions whose request level differs from the engine's CURRENT level — the request WAS stamped level 1, so it passes!). The arrival chunk renders as Level 0's interior; the Level-1 world never loads for that chunk until the archive clears (budget overflow, archived_chunk_source.rs:186-189) or reality changes (gate crossing).
Same for noclip (engine/noclip.rs) to level 34 and back.

Fix (any one): (a) add `level: u32` to RecordKey and the record header (bump ARCHIVE_FORMAT_VERSION, chunk_archive.rs:54) — cleanest, keys stay unambiguous; (b) fold level into the reality hash / generator_id per request — fragile; (c) make fetch reject hits whose level != identity.level by keying the archive per level. Recommended (a): `RecordKey { chunk, lod, level, artifacts, reality }`.

### F2 [High] Consequence amplifier: `switch_level` does not clear worker archives
- engine.rs:377-405 clears store/pending/backlog/forced/pool but has no path to invalidate worker-side archives (they live in the worker threads' thread_local SOURCE, lib.rs:1098-1101). Even with F1 fixed by identity-level archives, stale level-0 records remain reachable per level — but with F1(a) they'd be keyed correctly and harmless (just stale until budget clear). Note: this is by design for same-level revisits; only the missing level in the key is a bug.

---

## 6. Findings — worker pool lifecycle & concurrency (generation_worker_requests.rs, worker_source.rs, worker.js, lib.rs)

### W1 [Low] `take_exact` linear scan + VecDeque::remove per worker reply (generation_worker_requests.rs:112-118)
- `queue.iter().position(...)` + `queue.remove(position)` is O(q) time + O(q) shift per completion/failure, q = per-worker in-flight queue depth. Bounded by max_pending/pool_size (engine.rs:928-932; worker_source.rs:196-198), so q ≤ ~36 worst case (single worker) — trivial in practice. Note only; no fix needed unless pool_size drops to 1 with a wide footprint.

### W2 [Info/OK] Worker queue serialization, ordering, backpressure
- worker.js:26 (`queue = Promise.resolve()`), 99-105 (onmessage chains each message; catch isolates failures) — per-worker serial FIFO, bounded by engine's max_pending (engine.rs:928-932); no unbounded queue growth on the worker side.
- Init-before-gen ordering: worker_source.rs:132-136 posts `init` before any `gen`; browser FIFO per port guarantees order; `ensureWasm()` (worker.js:21-24) awaits wasm init in the chain. Verified safe.
- Engine-side pending dropped on eviction (engine.rs:916-917) while the worker ledger keeps the in-flight entry (worker_source.rs:252-254) — late completions are dropped by `finish_exact`/`is_same_request` (generation_worker_requests.rs:84-86; ports.rs is_same_request includes request_id). No lost replies; possible duplicate generation if re-requested before the old reply lands — bounded waste, archive makes it cheap. Note only.

### W3 [Info/OK] Failure paths
- Fatal/onerror → `disable_and_drain` + terminate (worker_source.rs:104-110, 125-129); drained requests are retried by the engine via poll_failed (engine.rs:828-837). Exhausted pool → synchronous fallback with one-time warning (worker_source.rs:211-235) — can hitch the frame but only after total pool failure. 
- Deterministic panic in generation → `failed` → engine re-issues same request next tick → same panic → persistent churn at one failing chunk per tick, each tick paying a full failing generation (~42 ms worker-side; main thread unaffected since it's off-thread). Livelock-ish but bounded; no memory growth. Medium severity robustness note (worker.js:33-57 reportFailure path; engine.rs:828-837 retry).
- `reported_emergency_fallback` and closures kept alive in `_message_handlers` (worker_source.rs:42-43, 150-151) — no use-after-free; workers never dropped mid-session. No Drop impl terminates workers — irrelevant in wasm page lifetime. OK.

### W4 [Info] Affinity routing determinism
- `preferred_worker` (generation_worker_requests.rs:55-67) FNV over chunk key — deterministic, balanced (tests 146-167); `next_accepting` wraps and skips disabled (70-74). Consistent across main/worker lifetime → per-worker archives hit. Good design; no finding.

### W5 [Low] Async-reply vs sync-frame consistency
- All worker callbacks and `Engine::tick` run on the same JS thread; `RefCell` borrows (worker_source.rs:38-40, 89-118) cannot interleave with tick. Message events dispatch only between tasks, so `install_completed` sees a consistent snapshot per tick. `Relaxed` atomics (lib.rs) never cross threads (workers get config via message, not globals). No torn reads, no lost updates, no deadlock potential. Note: `set_render_toggle` read-modify-write (lib.rs:200-202) is non-atomic but single-threaded — safe, though worth a comment.

---

## 7. Findings — lib.rs Atomic statics (all 30 audited)

Statics (lib.rs): DOOM_CONTROLS:32, INVERT_Y:36, MOUSE_SENSITIVITY_BITS:40, RENDER_SCALE_BITS:45, ANOMALY_DEBUG:49, ASSISTED_CONSUMPTION:55, CPU_SCALE_BITS:110, CPU_LOD_CUTOFF_BITS:112, CPU_MAX_SPLAT_RADIUS_BITS:114, CPU_MAX_VIRTUAL_DEPTH:116, CPU_MIN_MIP_OCCUPANCY_BITS:118, CPU_MAX_DRAW_DISTANCE_BITS:120, CPU_SHADOWS:122, RENDER_TOGGLE_BITS:191.

### A1 [Info] All Relaxed, all single-threaded — correct as written
- Every static is written only from wasm-bindgen setters (main thread) and read only in main-thread frame code (get_cpu_settings lib.rs:233-254, get_render_toggles 216-224; splatter/driver reads). Workers never read these globals (worker_init gets config via the query message). On the wasm32 single-threaded event loop, Relaxed + AtomicU32/Bool is sufficient; no torn reads (AtomicU32 load of a bit pattern is atomic), no lost updates (settler and tick cannot interleave).
- Torn float encoding: floats are stored as validated bit patterns (e.g., set_mouse_sensitivity clamps 0.1–5.0 at lib.rs:84-91; store_cpu_settings validates via `.validated()` at 264-273; set_render_scale clamps at 97-104). `f32::from_bits` of any u32 is defined (possibly NaN), but setters guarantee finite values. No issue.

### A2 [Low] `set_render_toggle` non-atomic read-modify-write (lib.rs:199-203)
- get→set→store is not atomic; two JS callers racing would lose an update. Impossible on one thread today; if a future worker or SharedArrayBuffer path reads/writes these, upgrade to fetch_add/cmpxchg. Document only.

### A3 [Info] CPU_* atomics mirrored snapshot pattern (lib.rs:106-122, 233-254, 264-273)
- 7 values stored independently → `get_cpu_settings` may observe a mixed-generation snapshot if a setter runs between loads. Single-threaded today → harmless; if threads ever appear, store a generation counter or a single packed u64. Document only.

---

## 8. Findings — flares, collision, peripheral shift, presenter, main.rs

### FL1 [Medium/High] `FlareField::extinguish_buried` O(F·B) on every changed streaming tick (flares.rs:99-108; called engine.rs:1191, 1289)
- For each of up to FLARE_CAP=12 flares, `boxes.iter().any(...)` scans ALL collision boxes of ALL resident chunks (B can be tens of thousands for a 1089-chunk surface footprint with ~20-100 boxes/chunk → up to ~1.2M AABB point tests per changed tick; during initial fill and every chunk-boundary crossing). Changed ticks are frequent while moving (eviction/load at boundaries, refinement ticks).
- Fix: reuse the existing spatial hash — `CollisionWorld` already builds `occupied_cells` (collision.rs:67, 101-108); probe only cells overlapping the flare's 1×1 neighborhood (point query) instead of the whole `boxes()` slice, or pass `&CollisionWorld` and query cells around each flare position. Drops O(F·B) to O(F·cells).

### FL2 [Info] Flare cap maintenance O(F) (flares.rs:85-87 `self.active.remove(0)` is O(F) shift; F≤12) — negligible.

### C1 [Info] CollisionWorld queries are well-indexed (collision.rs:82-184)
- `collides` (engine hot path, 2× per frame via player.rs:133, 138) probes only cells overlapped by the player AABB + the small `fallback` list for oversized boxes (collision.rs:165-184). `count_centers_within_xz` (thermal, per frame) is cell-indexed (138-163). Rebuild is event-driven (engine.rs:1190/1288) — good design; no finding.

### C2 [Info] `CollisionWorld::rebuild` worst case O(B·W) where W = cells per box (collision.rs:89-108)
- A 200×200 floor slab spans 100×100 cells = 10k > MAX_CELLS_PER_BOX=256 → fallback (collision.rs:94-99), so wide region slabs are handled. Normal wall boxes are a few cells. Bounded; fine.

### PS1 [Low] Tier-1 drift marking scans ~O(81) cells/frame (peripheral_shift.rs:144-155) and abandoned-cell pass iterates `drift_near` per frame (157-165) — both bounded by the near radius (visual_radius+1 chunks → (2·4+1)² ≈ 81 cells at R=16/40u cells). Blackout tier-2 scan (191-207) similarly bounded; `targets` filter O(M·C) only when a shift fires (~every 0.75 s in blackout). Acceptable; no change needed.

### P1 [Info] Presenter/debug aggregation only on demand
- `anomaly_debug_text` sorts gates+pits (presenter.rs:74-115) but is invoked only for the debug overlay, not per frame. `stats()` is per-frame O(M) — fine.

### M1 [Low] `src/main.rs` thread-per-connection HTTP server (main.rs:22-33)
- `thread::spawn` per accepted connection, no pool, no limit; `/maze` and `/octree` also run a full chunk generation (~42 ms) inside the connection thread. Under load (or an attacker), unbounded thread count → resource exhaustion; also each generation re-creates `SimpleNoiseProvider`/use-case objects. Dev-only server → Low/Medium. Fix: thread pool (e.g., `std::thread::scope` with N workers or a crate) + shared cached generator.

---

## 9. Completed Big-O table (all verified file:line)

| # | Algorithm / site | Complexity | Variables | Where | When |
|---|---|---|---|---|---|
| 1 | `desired_origins_in_view` (build + sort) | O(N log N) time, O(N) alloc | N=(2R+1)² ≈1089 at R=16 | streaming.rs:170-203; called engine.rs:1014-1018 | every frame |
| 2 | `stream_chunks` keep/evicted/retain filters | O(M·N) each (×3 passes) | M=resident≈N≈1089 | engine.rs:1019-1032, 916-917; streaming.rs:309-314 | every frame |
| 3 | `select_scene_lights` dedup+rank | O(L) + O(L log L) + allocs | L=all resident lights (~4k worst) | prepare_frame_lighting.rs:18-48; engine.rs:485-490 | every frame |
| 4 | `fixture_flicker_gain` loop | O(L) | L=truncated scene lights | engine.rs:494-497 | every frame |
| 5 | `thermal::ambient_celsius` | O(L) | L=full ranked list | thermal.rs:40-62; engine.rs:502-508 | every frame |
| 6 | `count_centers_within_xz` | O(cells × per-cell) | bounded 6u radius | collision.rs:138-163 | every frame |
| 7 | `world.collides` (player step) | O(cells × per-cell) | 2 calls/frame | collision.rs:165-184; player.rs:133,138 | every frame |
| 8 | `FlareField::extinguish_buried` | O(F·B) | F≤12, B=all resident boxes (10⁴-10⁵) | flares.rs:99-108; engine.rs:1191/1289 | changed ticks only |
| 9 | `update_anomalies` gates sort/dedup | O(G log G) per frame + O(P) alloc | G=resident gates, P=pits | engine.rs:613, 633-635 | every frame (L0) |
| 10 | `update_provisions` grabbed sort/dedup | O(S log S) per frame | S=supplies in reach | engine.rs:686-695 | every frame |
| 11 | `collect_supply_sprites` | O(S log S) + alloc | S=all resident supplies | engine.rs:733-762 | every frame |
| 12 | `ChunkStore::retain_keys` | O(N·M) | N,M≈1089 | streaming.rs:309-314 | every frame |
| 13 | `take_exact` (worker ledger) | O(q) per reply | q=per-worker in-flight ≤36 | generation_worker_requests.rs:112-118 | per worker reply |
| 14 | `preferred_worker` | O(1) (FNV-16 bytes) | — | generation_worker_requests.rs:55-67 | per request |
| 15 | Archive fetch / put | O(1) HashMap index; replay O(records) at open | — | chunk_archive.rs:259-325, 355 | open + per chunk |
| 16 | Drift cell marking + blackout scan | O(cells)=O(81) | cells=(2·⌈170/40⌉+1)² | peripheral_shift.rs:144-207 | every frame / 0.75 s |
| 17 | `AtlasPool::assign/release/offset_of` | O(1) (HashMap index + free stack) | — | atlas.rs:137-167 | changed ticks |
| 18 | Sync forced-reload loop | O(F) blocking loads, unbudgeted | F=forced set (≤resident) | engine.rs:1065-1090 | RedRoom/pickup/blackout events |
| 19 | main.rs HTTP server | thread per connection | unbounded | src/main.rs:22-33 | per request |

## 10. Concurrency inventory

Shared mutable state (all single-threaded JS main thread unless noted):

| State | Owner | Sync | Notes |
|---|---|---|---|
| `completed` / `failed` Vecs | worker_source.rs:38-39 | `Rc<RefCell<Vec<_>>>`, drained by engine poll (269-275) | no cross-thread access; event-loop serialized |
| `GenerationWorkerRequests` ledger | worker_source.rs:40 | `Rc<RefCell<>>`; borrow_mut in handlers + request() (89-118, 211, 252-259) | no re-entrancy possible (JS single thread, handlers cannot fire mid-tick) |
| Engine `pending` / `completed_backlog` / `forced_reloads` | engine.rs:250-268 | plain fields, tick-local | — |
| 14 Atomic statics | lib.rs:32-191 | Ordering::Relaxed, wasm32-only | workers NEVER read them; single-threaded → correct; see A1-A3 |
| Worker-side `SOURCE` thread_local | lib.rs:1098-1101 | per-worker thread local, RefCell | only touched by worker_init/worker_generate on the worker thread |
| Worker.js promise chain | worker.js:26, 99-105 | serializes per-worker message processing | bounded by engine max_pending (engine.rs:928-932) |

Locks/atomics: none contended — zero cross-thread shared state by design (config crosses via structured-clone messages). Deadlock risk: none (no locks). Torn reads: none (AtomicU32 loads atomic; bit patterns validated at store). Lost updates: `set_render_toggle` RMW (lib.rs:200-202) non-atomic but single-threaded.

Worker lifecycle: spawned once at boot (worker_source.rs:53-155), init posted before gens (132-136, FIFO), fatal/error → disable_and_drain + terminate (104-110, 125-129), exhausted pool → synchronous fallback (211-235), no Drop/terminate on teardown (page lifetime — fine). Message ordering: per-port FIFO + serialized promise chain. Backpressure: engine-side `max_pending` cap (928-932) bounds worker queues; `completed_backlog` meters installs (841-851). Lost replies: none — identity-checked (ports.rs:625-633); stale completions dropped (engine.rs:858-867, 916-917).

Determinism: worker affinity hash deterministic (generation_worker_requests.rs:55-67); engine RNG (noclip) is a single xorshift64 in `Engine.rng` (engine.rs:288, 361; noclip.rs:11-16) — no static mut, no thread_local RNG; flicker is deterministic in (id, time) (engine.rs:202-220). One shared RNG means noclip rolls and any future consumer interleave — fine today.

## 11. Ranked optimizations (highest leverage first)

1. **[Critical correctness] Add `level` to `RecordKey` + record header** (chunk_archive.rs:150-169, 57-59; archived_chunk_source.rs:155-160; bump ARCHIVE_FORMAT_VERSION at chunk_archive.rs:54). Wrong-level worlds are served from worker archives after any runtime level switch (door: engine.rs:724-727 → switch_level:377-405; noclip: noclip.rs:46-56). Fix cost ~20 lines; also add a per-fetch identity/level guard as defense-in-depth.
2. **[High perf] Make the per-frame `keep` set a `HashSet`** (engine.rs:1019-1022; streaming.rs:309-314 `retain_keys(&HashSet)`). Kills the 3× O(M·N) linear scans (~3.6M compares/frame) — the biggest per-frame CPU waste in the hot loop.
3. **[High perf] Cache the deduped scene-light list + bounded top-48 selection** (prepare_frame_lighting.rs:18-48; engine.rs:485-490). Dedup set is stable between chunk-set changes and pre-flicker ranking keys are static; per frame do O(L) heap selection instead of O(L log L) sort + HashMap rebuild + allocs.
4. **[High perf] Budget the sync forced-reload loop** (engine.rs:1065-1090) — spread forced rebuilds (RedRoom commit can force ALL resident chunks, engine.rs:660-670) across ticks under the same per-tick budget as phases 1/2. Prevents 0.4-1 s freezes on the no-worker path.
5. **[Medium perf] Spatial-hash `extinguish_buried`** (flares.rs:99-108) via `CollisionWorld.occupied_cells` (collision.rs:67) — removes O(F·B) bursts on every changed streaming tick.
6. **[Medium perf] Skip per-frame recompute when nothing changed**: `stream_chunks` desired-origins sort (streaming.rs:195-201) and the anomaly/provision gather-sort-dedup chains (engine.rs:633-635, 686-695, 733-762) can be cached behind a store-generation counter (increment on insert/retain; engine.rs store ops).

## 12. Scope coverage

- wasm_frontend/src/application/**: all 19 files + engine/ (4 files) + rendering/ (2 files) read; findings cited by file:line. atlas.rs, body.rs, camera.rs, mod.rs (rendering), navigation.rs, perf_governor.rs, ports.rs, quality.rs, render_settings.rs, survival_inventory.rs, thermal.rs: no significant findings (documented in-line where relevant).
- wasm_frontend/src/lib.rs: all 14 Atomic statics audited (A1-A3), worker_entry (F1), bootstrap config flow.
- Cross-scope reads needed to confirm F1: adapters/archived_chunk_source.rs (146-193, 99-105), drivers/generation_worker_requests.rs, drivers/worker_source.rs, static/worker.js, src/main.rs (M1).
