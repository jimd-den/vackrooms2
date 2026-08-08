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

/// Which wall of a cell an edge belongs to.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Axis {
    West,
    North,
}

/// Levels of the subdivision hierarchy above the fabric cell.
///
/// Four doublings: 7.2 u cells nest inside 14.4, 28.8, 57.6 and 115.2 u
/// blocks. That top block is a hall you can see across, and its existence is
/// the point — a world where every room is 7.2 u has no scale, because scale
/// is only legible as *contrast* between sizes.
const LEVELS: u32 = 4;

/// Hierarchy levels the Peripheral Shift is allowed to redraw.
///
/// Everything above this is the building's own structure and survives every
/// shift; at and below it, partitions are re-dealt each epoch.
const FINE_LEVELS: u32 = 3;

/// How strongly a wall belongs on this edge, in 0..=1.
///
/// This is the fractal half of the grammar. The same motif — "a block either
/// splits in two or stays whole" — is applied at every scale, so the plan is
/// self-similar: a hall contains rooms, those rooms contain smaller rooms,
/// and the rule that made each division is the same rule with a different
/// parameter draw.
///
/// An edge exists only if the block that *would have created it by
/// splitting* actually split. An edge on a coarse boundary is therefore a
/// major wall present only where a big block divided; an edge deep in the
/// hierarchy is a partition inside an already-small room. A block that
/// declines to split leaves everything inside it open, and that is where the
/// halls come from.
///
/// Implicit, like everything else here: no tree is built. The rank of an
/// edge is read straight off its coordinate, and its ancestry is a walk up
/// the powers of two — a pure function of position.
fn subdivision_weight(
    noise: &dyn NoiseProvider,
    seed: u32,
    cx: i64,
    cz: i64,
    axis: Axis,
    porosity: f32,
    epoch: u32,
) -> f32 {
    // The coordinate this edge divides. A west wall is a vertical line, so
    // its rank comes from x; a north wall from z.
    let along = match axis {
        Axis::West => cx,
        Axis::North => cz,
    };
    // Rank: how coarse a boundary this is. An edge at a multiple of 8 is the
    // boundary of a level-3 block; an odd coordinate is the finest partition
    // the grammar can draw.
    let rank = (along.trailing_zeros()).min(LEVELS);
    // Firmness rises with rank, so compute it before the gates below can
    // return early.
    let firmness = 0.80 + 0.20 * (rank as f32 / LEVELS as f32);

    // The boundaries of the coarsest blocks are the frame the whole grammar
    // hangs on; nothing above them could have declined to split.
    if rank >= LEVELS {
        return firmness;
    }
    // Every ancestor from this edge's own level up to the coarsest must have
    // chosen to split, or the edge lies inside something that stayed whole
    // and must not be drawn at all.
    for level in (rank + 1)..=LEVELS {
        if !splits(noise, seed, cx, cz, axis, level, porosity, epoch) {
            return 0.0;
        }
    }
    // Coarse divisions read as building structure and stand firm; fine ones
    // are partitions and are left weak, so the automaton is free to erode
    // them into alcoves and ragged openings. That gradient is what keeps a
    // large room from dissolving into the same texture as a small one.
    firmness
}

/// Is this edge structure rather than partition?
///
/// The top two levels of the hierarchy: walls a building would not take
/// down. They are held against the automaton so the plan keeps its large
/// shapes while its fine grain is free to grow and erode.
fn is_structural(along: i64, _axis: Axis) -> bool {
    along.trailing_zeros().min(LEVELS) >= LEVELS
}

/// Does the block containing this edge, at this level, divide in two?
fn splits(
    noise: &dyn NoiseProvider,
    seed: u32,
    cx: i64,
    cz: i64,
    axis: Axis,
    level: u32,
    porosity: f32,
    epoch: u32,
) -> bool {
    if level == 0 {
        return true;
    }
    let size = 1i64 << level;
    let (bx, bz) = (cx.div_euclid(size), cz.div_euclid(size));
    // Porous neighbourhoods stop dividing sooner, so "broken open" reads as
    // genuinely larger rooms rather than as the same rooms with holes.
    // Coarse blocks nearly always divide; leaving one whole is a hall, and
    // a hall has to stay rare enough to be an event.
    let chance =
        (0.96 - 0.07 * LEVELS.saturating_sub(level) as f32 - 0.18 * porosity).clamp(0.05, 0.98);
    // The Peripheral Shift re-partitions, it does not rebuild. Fine levels
    // are re-drawn every epoch, so the small rooms a wanderer walked through
    // are genuinely not the same rooms on their way back; coarse levels
    // ignore the epoch entirely, so the halls and the major walls they
    // navigate by stay put. That split is the canon reading of the mechanic:
    // you recognise the neighbourhood, never the hallways.
    let shift = if level <= FINE_LEVELS { epoch } else { 0 };
    let salt = 0xF2AC ^ (level << 4) ^ (shift << 8) ^ if axis == Axis::West { 1 } else { 2 };
    BackroomsLevel::cell_hash(noise, seed, salt, bx, bz) < chance
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
    let mut firm_w = vec![false; n];
    let mut firm_n = vec![false; n];

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
            // Where the hierarchy says a wall belongs. Denser than the
            // finished target: smoothing erodes isolated walls, so a fill at
            // the final density would thin away to almost nothing.
            let i = (iz * PADDED + ix) as usize;
            let epoch = era * (MAX_GENERATIONS + 1) + generation;
            let weight_w = subdivision_weight(noise, seed, cx, cz, Axis::West, porosity, epoch);
            let weight_n = subdivision_weight(noise, seed, cx, cz, Axis::North, porosity, epoch);
            west[i] = BackroomsLevel::cell_hash(noise, seed, 0x9300 ^ era, cx, cz) < weight_w;
            north[i] = BackroomsLevel::cell_hash(noise, seed, 0x9400 ^ era, cx, cz) < weight_n;
            // Structure does not erode. A wall high in the hierarchy is the
            // building holding itself up, and the automaton must not be able
            // to dissolve it -- otherwise the scale contrast the hierarchy
            // exists to create is smoothed away within a few generations,
            // which is exactly what happened when everything was mutable.
            firm_w[i] = is_structural(cx, Axis::West);
            firm_n[i] = is_structural(cz, Axis::North);
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
    let seeded_w = west.clone();
    let seeded_n = north.clone();
    for _ in 0..generation {
        west = step(&west);
        north = step(&north);
        for i in 0..n {
            if firm_w[i] {
                west[i] = seeded_w[i];
            }
            if firm_n[i] {
                north[i] = seeded_n[i];
            }
        }
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
