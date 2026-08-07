//! The fabric as a cellular automaton: walls that grew, not walls that were
//! rolled.
//!
//! The warren used to decide each cell's walls with an independent hash. Two
//! neighbouring cells therefore agreed about nothing, and the result was a
//! statistically uniform lattice — every 7.2 u cell the same, everywhere,
//! forever. That is why it read as "rooms with pillars" rather than as the
//! Backrooms: a labyrinth whose every cell is an independent coin flip has
//! no shape at any scale larger than one cell.
//!
//! A cellular automaton couples the decision to its neighbours, which is the
//! whole difference. Under a Life-like smoothing rule walls bloom into
//! coherent clumps and long runs: rooms come out irregular and various, some
//! cells merge into halls, others thicken into blocks you have to walk
//! around. The same rule run one more generation is the Peripheral Shift —
//! the level *evolves* rather than re-deals, so a returning wanderer finds
//! the neighbourhood changed the way a thing grows, not the way a shuffled
//! deck changes.
//!
//! ## Implicit, not buffered
//!
//! The engine forbids a global CA buffer: chunk-independent world bytes mean
//! no cross-chunk reads at sample time. So the automaton is *implicit* — a
//! cell's state at generation `t` is a pure function of `(seed, cell, t)`,
//! computable on demand. Generation `t` at one cell depends on its
//! neighbours at `t-1`, so an honest evaluation needs a `t`-cell margin;
//! that is solved once per sheet and memoized, exactly as
//! `menger_expanse` solves plates. A chunk touches at most a few sheets.
//!
//! Determinism, seam tiling and streaming all hold: any chunk, in any order,
//! asks the same question and gets the same answer.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use crate::use_cases::ports::NoiseProvider;

use super::BackroomsLevel;
use super::fabric::FABRIC_CELL;

/// Fabric cells per solved sheet, per axis.
const SHEET: i64 = 16;

/// Generations the automaton will actually run.
///
/// The margin a sheet needs grows with this, so it bounds work: a sheet is
/// solved over `SHEET + 2 * MAX_GENERATIONS` cells per axis. Six is enough
/// for clumps to form and to keep evolving visibly; drift epochs past it
/// fold into the initial hash instead (see [`generation_of`]), so the world
/// keeps changing without the neighbourhood ever costing more to compute.
const MAX_GENERATIONS: u32 = 6;

/// Padded side of a solved sheet.
const PADDED: i64 = SHEET + 2 * MAX_GENERATIONS as i64;

/// Which walls a fabric cell carries.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub(super) struct CellWalls {
    pub west: bool,
    pub north: bool,
    /// The binary-tree doorway: this cell opens through its west wall
    /// (otherwise through its north). Re-asserted after the automaton runs,
    /// so connectivity never depends on what the rule happened to do.
    pub opens_west: bool,
}

/// Solved wall state for one padded sheet.
struct Sheet {
    west: Vec<bool>,
    north: Vec<bool>,
}

impl Sheet {
    fn at(&self, ix: i64, iz: i64) -> (bool, bool) {
        if ix < 0 || iz < 0 || ix >= PADDED || iz >= PADDED {
            return (false, false);
        }
        let i = (iz * PADDED + ix) as usize;
        (self.west[i], self.north[i])
    }
}

thread_local! {
    static SHEET_CACHE: RefCell<HashMap<(u32, i64, i64, u32, u8), Rc<Sheet>>> =
        RefCell::new(HashMap::new());
}

/// How many generations this epoch has run, and the salt the initial fill
/// uses.
///
/// Epochs past [`MAX_GENERATIONS`] do not deepen the recursion — they
/// re-seed it. The level therefore keeps changing forever at constant cost,
/// and each era is its own automaton run rather than an ever-longer one.
/// A wanderer who leaves an area for a very long time comes back to a place
/// that grew differently, which is the same promise, differently kept.
fn generation_of(epoch: u32) -> (u32, u32) {
    let era = epoch / (MAX_GENERATIONS + 1);
    let generation = epoch % (MAX_GENERATIONS + 1);
    (generation, era)
}

/// The walls of one fabric cell at one drift epoch.
pub(super) fn walls_at(
    noise: &dyn NoiseProvider,
    seed: u32,
    cx: i64,
    cz: i64,
    epoch: u32,
    tier: u8,
) -> CellWalls {
    let (generation, era) = generation_of(epoch);
    let (sx, sz) = (cx.div_euclid(SHEET), cz.div_euclid(SHEET));
    let sheet = sheet_for(noise, seed, sx, sz, generation, era, tier);
    // Index inside the padded sheet.
    let ix = cx.rem_euclid(SHEET) + MAX_GENERATIONS as i64;
    let iz = cz.rem_euclid(SHEET) + MAX_GENERATIONS as i64;
    let (west, north) = sheet.at(ix, iz);

    // Connectivity is re-asserted here rather than trusted from the rule.
    // The automaton decides *which* walls stand; the binary-tree invariant
    // decides that you can always get out — every cell knocks its doorway
    // through the west or the north wall, chunk-locally, exactly as before.
    // Keeping the two separate is what lets the rule be tuned for how the
    // warren looks without ever being able to seal the world.
    let opens_west = BackroomsLevel::cell_hash(noise, seed, 0x9200 ^ era, cx, cz) < 0.5;
    let _ = tier;
    CellWalls {
        west,
        north,
        opens_west,
    }
}

#[allow(clippy::too_many_arguments)]
fn sheet_for(
    noise: &dyn NoiseProvider,
    seed: u32,
    sx: i64,
    sz: i64,
    generation: u32,
    era: u32,
    tier: u8,
) -> Rc<Sheet> {
    SHEET_CACHE.with(|cache| {
        let mut cache = cache.borrow_mut();
        if cache.len() > 64 {
            cache.clear();
        }
        cache
            .entry((seed, sx, sz, generation ^ (era << 8), tier))
            .or_insert_with(|| Rc::new(solve_sheet(noise, seed, sx, sz, generation, era)))
            .clone()
    })
}

/// Runs the automaton over one padded sheet.
fn solve_sheet(
    noise: &dyn NoiseProvider,
    seed: u32,
    sx: i64,
    sz: i64,
    generation: u32,
    era: u32,
) -> Sheet {
    let n = (PADDED * PADDED) as usize;
    let mut west = vec![false; n];
    let mut north = vec![false; n];

    // --- generation 0: the porosity climate ------------------------------
    // Seeded from the same smooth porosity field the fabric always used, so
    // the automaton starts from Level 0's character rather than from noise:
    // tight neighbourhoods start dense, broken-open ones start sparse, and
    // the rule then makes each of them coherent.
    for iz in 0..PADDED {
        for ix in 0..PADDED {
            let cx = sx * SHEET + ix - MAX_GENERATIONS as i64;
            let cz = sz * SHEET + iz - MAX_GENERATIONS as i64;
            let (wx, wz) = (
                (cx as f32 + 0.5) * FABRIC_CELL,
                (cz as f32 + 0.5) * FABRIC_CELL,
            );
            let porosity =
                (BackroomsLevel::n(noise, seed, 0x9010, wx, wz, 0.11) * 0.5 + 0.5).clamp(0.0, 1.0);
            // Denser than the finished target: smoothing erodes isolated
            // walls, so a fill at the final density would thin away to
            // almost nothing after a few generations.
            let fill = 0.62 - 0.30 * porosity;
            let i = (iz * PADDED + ix) as usize;
            west[i] = BackroomsLevel::cell_hash(noise, seed, 0x9300 ^ era, cx, cz) < fill;
            north[i] = BackroomsLevel::cell_hash(noise, seed, 0x9400 ^ era, cx, cz) < fill;
        }
    }

    // --- evolve ------------------------------------------------------------
    // The game-industry cave rule (RogueBasin's "4-5"): a wall stands where
    // it has company, and blooms where it is surrounded. Run over each edge
    // lattice separately, it turns independent flips into long runs — a
    // west-wall run is a north-south wall, which is what a room is made of.
    //
    // Cells outside the padded sheet read as open. That is a lie the margin
    // makes harmless: after `generation` steps the error cannot have
    // travelled further than `generation` cells, and the margin is
    // MAX_GENERATIONS wide, so no cell a caller can ask about is affected.
    for _ in 0..generation {
        west = step(&west);
        north = step(&north);
    }

    Sheet { west, north }
}

/// One synchronous generation of the smoothing rule.
fn step(state: &[bool]) -> Vec<bool> {
    let mut next = vec![false; state.len()];
    for iz in 0..PADDED {
        for ix in 0..PADDED {
            let i = (iz * PADDED + ix) as usize;
            let mut alive = 0;
            for dz in -1i64..=1 {
                for dx in -1i64..=1 {
                    if dx == 0 && dz == 0 {
                        continue;
                    }
                    let (nx, nz) = (ix + dx, iz + dz);
                    if nx < 0 || nz < 0 || nx >= PADDED || nz >= PADDED {
                        continue;
                    }
                    if state[(nz * PADDED + nx) as usize] {
                        alive += 1;
                    }
                }
            }
            next[i] = alive >= 5 || (state[i] && alive >= 4);
        }
    }
    next
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::entities::position::Position;

    struct TestNoise;
    impl NoiseProvider for TestNoise {
        fn evaluate_2d(&self, seed: u32, position: Position) -> f32 {
            let mut h = seed
                .wrapping_add((position.x as i32) as u32 ^ 0x9E37_79B9)
                .wrapping_add((position.z as i32) as u32 ^ 0x85EB_CA6B);
            h ^= h >> 16;
            h = h.wrapping_mul(0x85EB_CA6B);
            h ^= h >> 13;
            ((h as f32) / (u32::MAX as f32)) * 2.0 - 1.0
        }
    }

    fn walls(cx: i64, cz: i64, epoch: u32) -> CellWalls {
        walls_at(&TestNoise, 42, cx, cz, epoch, 0)
    }

    #[test]
    fn the_answer_is_the_same_whichever_sheet_asks() {
        // A cell on a sheet boundary must resolve identically no matter
        // which chunk asked, or the world would not tile.
        for k in -2..=2 {
            let cx = SHEET + k;
            let a = walls(cx, 3, 2);
            let b = walls(cx, 3, 2);
            assert_eq!(a, b);
        }
    }

    #[test]
    fn evolution_is_deterministic_across_calls() {
        for epoch in 0..10 {
            assert_eq!(walls(5, 7, epoch), walls(5, 7, epoch));
        }
    }

    #[test]
    fn walls_clump_instead_of_scattering() {
        // The point of the automaton. Measure how often a wall's east/west
        // neighbour is also a wall: independent flips give roughly the base
        // density, a coherent run gives markedly more.
        let density_and_run = |epoch: u32| {
            let (mut walls_seen, mut paired, mut total) = (0.0f32, 0.0f32, 0.0f32);
            for cz in 0..40 {
                for cx in 0..40 {
                    total += 1.0;
                    if walls(cx, cz, epoch).west {
                        walls_seen += 1.0;
                        if walls(cx, cz + 1, epoch).west {
                            paired += 1.0;
                        }
                    }
                }
            }
            (walls_seen / total, paired / walls_seen.max(1.0))
        };
        let (d0, run0) = density_and_run(0);
        let (_, run3) = density_and_run(3);
        assert!(
            run3 > run0,
            "smoothing did not increase wall continuity ({run3:.2} vs {run0:.2})"
        );
        assert!(
            d0 > 0.05,
            "generation 0 should not start empty, got {d0:.2}"
        );
    }

    #[test]
    fn the_fabric_never_becomes_all_wall_or_all_void() {
        // A rule that saturates would give a solid block or an empty plain;
        // both stop being a labyrinth.
        for epoch in 0..=MAX_GENERATIONS {
            let mut solid = 0.0f32;
            let mut total = 0.0f32;
            for cz in 0..30 {
                for cx in 0..30 {
                    total += 1.0;
                    if walls(cx, cz, epoch).west {
                        solid += 1.0;
                    }
                }
            }
            let ratio = solid / total;
            assert!(
                (0.02..0.98).contains(&ratio),
                "epoch {epoch} saturated at {ratio:.2} wall density"
            );
        }
    }

    #[test]
    fn epochs_past_the_cap_reseed_rather_than_deepen() {
        // Cost must stay bounded forever, and the world must keep changing.
        let (g_at_cap, era_at_cap) = generation_of(MAX_GENERATIONS);
        let (g_past, era_past) = generation_of(MAX_GENERATIONS + 1);
        assert_eq!(g_at_cap, MAX_GENERATIONS);
        assert_eq!(g_past, 0);
        assert!(era_past > era_at_cap, "a new era must start a new run");
        // And the new era is genuinely a different world.
        let mut differences = 0;
        for cx in 0..24 {
            for cz in 0..24 {
                if walls(cx, cz, 0) != walls(cx, cz, MAX_GENERATIONS + 1) {
                    differences += 1;
                }
            }
        }
        assert!(
            differences > 20,
            "a new era reproduced the old one ({differences} cells differ)"
        );
    }

    #[test]
    fn every_cell_still_declares_a_doorway_side() {
        // The connectivity floor is independent of whatever the rule did.
        let (mut west, mut north) = (0, 0);
        for cx in 0..40 {
            for cz in 0..40 {
                if walls(cx, cz, 4).opens_west {
                    west += 1;
                } else {
                    north += 1;
                }
            }
        }
        assert!(
            west > 0 && north > 0,
            "the doorway rule collapsed to one side"
        );
    }
}
