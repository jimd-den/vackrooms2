//! Menger galleries: recursive structure for the open expanse/vault fabric.
//!
//! Every expanse is overlaid by an infinite world-anchored lattice of
//! 21.6 u *plates* — three fabric wall cells, so every partition line lands
//! on the lattice walls already own. Each plate subdivides Menger-style
//! ([`generate_menger_bsp`]): the center cell at every level is carved as
//! open circulation (a 7.2 u central hall per plate, a 2.4 u court at the
//! heart of each subdivided ring cell),
//! and the remaining cells receive structure chosen by a per-plate 3D wave
//! function collapse: open floor, colonnade screens, or arcade courts whose
//! arches spring at the CAD standard ceiling height.
//!
//! Everything is a pure function of (seed, world position): plates are
//! solved per plate index and memoized, never per chunk, so overlapping
//! chunks reproduce identical galleries. Every module is passable by
//! construction — colonnades leave 0.8 u gaps (four fine voxels, wider
//! than the 0.7 u player), arcade bays are full doorway spans — so the
//! walkability contract survives without a connectivity search. Structure
//! deliberately ignores the Peripheral Shift: expanse structure never
//! drifts, matching the fabric doctrine.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use crate::domain::entities::anomaly::WorldBounds;
use crate::domain::entities::cad::CAD_STANDARD_CEILING;
use crate::use_cases::anomalies::determinism::{hash, unit};
use crate::use_cases::generate_chunk::LevelTuning;
use crate::use_cases::menger_bsp::generate_menger_bsp;
use crate::use_cases::region_plan::PLAN_WALL_T;
use crate::use_cases::wfc3d::{Axis6, WfcModuleSet, solve_wfc};

/// Plate side: three fabric wall cells (3 x 7.2).
const PLATE: f32 = 21.6;
/// Finest gallery cell (a plate is 9 x 9 of these).
const FINE: f32 = 2.4;
/// Cells per plate axis.
const CELLS: usize = 9;
/// Arcade arches spring at the referencable standard ceiling.
const ARCH_SPRING_UNITS: f32 = CAD_STANDARD_CEILING;
/// Colonnade post spacing along a cell edge.
const POST_PERIOD: f32 = 1.2;
/// Post/pier footprint side (one coarse voxel, like junction posts).
const POST_SIDE: f32 = 0.4;

const MODULE_OPEN: u8 = 0;
const MODULE_COLONNADE: u8 = 1;
const MODULE_ARCADE: u8 = 2;

/// What the gallery contributes to one sampled column.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub(super) struct ExpanseStructure {
    pub solid: bool,
    pub lintel_from_units: Option<f32>,
}

/// One solved plate: corridor mask plus a WFC module per fine cell.
struct PlateStructure {
    corridor: [bool; CELLS * CELLS],
    modules: [u8; CELLS * CELLS],
}

/// The gallery's contribution at world column (wx, wz). Only meaningful
/// inside expanse/vault fabric; the caller gates on the ceiling band.
pub(super) fn expanse_structure(
    seed: u32,
    tuning: &LevelTuning,
    wx: f32,
    wz: f32,
) -> ExpanseStructure {
    let plate = plate_for(seed, (wx / PLATE).floor() as i64, (wz / PLATE).floor() as i64);

    let (local_x, local_z) = (wx.rem_euclid(PLATE), wz.rem_euclid(PLATE));
    let cell_x = ((local_x / FINE).floor() as usize).min(CELLS - 1);
    let cell_z = ((local_z / FINE).floor() as usize).min(CELLS - 1);
    let cell = cell_z * CELLS + cell_x;

    // Circulation stays clean at every recursion level.
    if plate.corridor[cell] {
        return ExpanseStructure::default();
    }

    // Structure lives on each cell's own west/north boundary strips, the
    // same idiom as the fabric's walls, so shared edges are owned once.
    let (in_cell_x, in_cell_z) = (local_x.rem_euclid(FINE), local_z.rem_euclid(FINE));
    let on_west = in_cell_x < PLAN_WALL_T;
    let on_north = in_cell_z < PLAN_WALL_T;
    if !on_west && !on_north {
        return ExpanseStructure::default();
    }

    match plate.modules[cell] {
        MODULE_COLONNADE => ExpanseStructure {
            solid: colonnade_post(seed, tuning, wx, wz, on_west, on_north),
            lintel_from_units: None,
        },
        MODULE_ARCADE => {
            let lintel = (tuning.walls > 0.0).then_some(ARCH_SPRING_UNITS);
            ExpanseStructure {
                solid: arcade_pier(seed, tuning, wx, wz, on_west, on_north),
                lintel_from_units: lintel,
            }
        }
        _ => ExpanseStructure::default(),
    }
}

/// Colonnade screens: posts on the [`POST_PERIOD`] lattice along the strip,
/// with a deterministic per-post survival roll scaled by the pillars knob.
fn colonnade_post(
    seed: u32,
    tuning: &LevelTuning,
    wx: f32,
    wz: f32,
    on_west: bool,
    on_north: bool,
) -> bool {
    let on_post_lattice = |along: f32| along.rem_euclid(POST_PERIOD) < POST_SIDE;
    let sited = (on_west && on_post_lattice(wz))
        || (on_north && on_post_lattice(wx))
        || (on_west && on_north);
    if !sited {
        return false;
    }
    post_roll(seed, 0x9A11_E7C0, wx, wz) < (0.85 * tuning.pillars).min(1.0)
}

/// Arcade piers: one structural post at each cell corner carrying the arch
/// band overhead; bays between piers span a full doorway width.
fn arcade_pier(
    seed: u32,
    tuning: &LevelTuning,
    wx: f32,
    wz: f32,
    on_west: bool,
    on_north: bool,
) -> bool {
    let near_corner = |along: f32| along.rem_euclid(FINE) < POST_SIDE;
    let sited = (on_west && near_corner(wz)) || (on_north && near_corner(wx));
    if !sited {
        return false;
    }
    post_roll(seed, 0xA2C4_DE01, wx, wz) < (0.9 * tuning.pillars).min(1.0)
}

/// One roll per post site: quantized to the post lattice so every voxel
/// column of a post agrees about its existence.
fn post_roll(seed: u32, salt: u64, wx: f32, wz: f32) -> f32 {
    unit(hash(
        seed,
        salt,
        (wx / POST_SIDE).floor() as i64 >> 1,
        (wz / POST_SIDE).floor() as i64 >> 1,
    ))
}

// -- plate solving -----------------------------------------------------------

thread_local! {
    static PLATE_CACHE: RefCell<HashMap<(u32, i64, i64), Rc<PlateStructure>>> =
        RefCell::new(HashMap::new());
}

/// Bounded memo of solved plates: a chunk touches at most four.
fn plate_for(seed: u32, px: i64, pz: i64) -> Rc<PlateStructure> {
    PLATE_CACHE.with(|cache| {
        let mut cache = cache.borrow_mut();
        if cache.len() > 128 {
            cache.clear();
        }
        cache
            .entry((seed, px, pz))
            .or_insert_with(|| Rc::new(solve_plate(seed, px, pz)))
            .clone()
    })
}

fn solve_plate(seed: u32, px: i64, pz: i64) -> PlateStructure {
    let origin = (px as f32 * PLATE, pz as f32 * PLATE);
    let bounds = WorldBounds::new(origin.0, origin.1, origin.0 + PLATE, origin.1 + PLATE);

    // Menger partition: mark every finest cell covered by corridor leaves.
    let mut corridor = [false; CELLS * CELLS];
    generate_menger_bsp(bounds, 2, seed).visit(&mut |node| {
        if !node.is_corridor {
            return;
        }
        // Inputs are plate-relative; snap to the fine-cell lattice.
        let cell_span = |lo: f32, hi: f32| {
            let first = (lo / FINE).round() as isize;
            let count = ((hi - lo) / FINE).round() as isize;
            (first, count)
        };
        let (x0, xn) = cell_span(node.bounds.min_x - origin.0, node.bounds.max_x - origin.0);
        let (z0, zn) = cell_span(node.bounds.min_z - origin.1, node.bounds.max_z - origin.1);
        for dz in 0..zn {
            for dx in 0..xn {
                let (cx, cz) = ((x0 + dx) as usize, (z0 + dz) as usize);
                if cx < CELLS && cz < CELLS {
                    corridor[cz * CELLS + cx] = true;
                }
            }
        }
    });

    // WFC treatment field: arcades cluster into courts (they refuse to
    // touch bare open floor), colonnades are the universal transition.
    let mut set = WfcModuleSet::new(vec![1.3, 0.9, 0.8]);
    let lateral = [Axis6::PosX, Axis6::NegX, Axis6::PosZ, Axis6::NegZ];
    for dir in lateral {
        for (a, b) in [
            (MODULE_OPEN, MODULE_OPEN),
            (MODULE_OPEN, MODULE_COLONNADE),
            (MODULE_COLONNADE, MODULE_COLONNADE),
            (MODULE_COLONNADE, MODULE_ARCADE),
            (MODULE_ARCADE, MODULE_ARCADE),
        ] {
            set.allow(a as usize, dir, b as usize);
        }
    }
    let wfc_seed = hash(seed, 0x3E9C_57F0_0000_0001, px, pz);
    let modules = match solve_wfc(&set, (CELLS, 1, CELLS), wfc_seed) {
        Ok(solution) => {
            let mut modules = [MODULE_OPEN; CELLS * CELLS];
            modules.copy_from_slice(&solution.modules);
            modules
        }
        // Unreachable with this permissive catalog, but a deterministic
        // all-colonnade plate is a sane gallery if it ever happens.
        Err(_) => [MODULE_COLONNADE; CELLS * CELLS],
    };

    PlateStructure { corridor, modules }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn default_tuning() -> LevelTuning {
        LevelTuning::default()
    }

    #[test]
    fn structure_is_deterministic_and_chunk_independent() {
        // Same world point queried "from" different chunks (the function
        // only sees world coordinates) always answers identically.
        for &(wx, wz) in &[(3.1, 5.7), (-40.2, 88.8), (200.0, -13.3)] {
            let a = expanse_structure(42, &default_tuning(), wx, wz);
            let b = expanse_structure(42, &default_tuning(), wx, wz);
            assert_eq!(a, b);
        }
    }

    #[test]
    fn the_central_hall_of_every_plate_stays_open() {
        // The Menger cutout: the middle 7.2 x 7.2 block of each plate is
        // corridor and must contribute no structure anywhere inside it.
        let tuning = default_tuning();
        for plate in -2..2i64 {
            let base = plate as f32 * PLATE;
            let hall_min = base + PLATE / 3.0;
            for iz in 0..24 {
                for ix in 0..24 {
                    let wx = hall_min + 0.05 + ix as f32 * 0.3;
                    let wz = hall_min + 0.05 + iz as f32 * 0.3;
                    assert_eq!(
                        expanse_structure(42, &tuning, wx, wz),
                        ExpanseStructure::default(),
                        "central hall blocked at ({wx}, {wz})"
                    );
                }
            }
        }
    }

    #[test]
    fn zeroed_knobs_produce_an_empty_expanse() {
        let bare = LevelTuning {
            pillars: 0.0,
            walls: 0.0,
            ..LevelTuning::default()
        };
        for iz in 0..60 {
            for ix in 0..60 {
                let (wx, wz) = (ix as f32 * 0.4, iz as f32 * 0.4);
                assert_eq!(
                    expanse_structure(42, &bare, wx, wz),
                    ExpanseStructure::default(),
                    "structure survived zeroed knobs at ({wx}, {wz})"
                );
            }
        }
    }

    #[test]
    fn default_knobs_grow_posts_and_arches_somewhere() {
        let tuning = default_tuning();
        let mut posts = 0;
        let mut arches = 0;
        for iz in 0..180 {
            for ix in 0..180 {
                let s = expanse_structure(42, &tuning, ix as f32 * 0.4, iz as f32 * 0.4);
                posts += usize::from(s.solid);
                arches += usize::from(s.lintel_from_units.is_some());
            }
        }
        assert!(posts > 20, "only {posts} post columns in ~2 plates");
        assert!(arches > 20, "only {arches} arch columns in ~2 plates");
    }

    #[test]
    fn every_arch_springs_at_walkable_height() {
        let tuning = default_tuning();
        for iz in 0..120 {
            for ix in 0..120 {
                let s = expanse_structure(7, &tuning, ix as f32 * 0.6, iz as f32 * 0.6);
                if let Some(lintel) = s.lintel_from_units {
                    assert_eq!(lintel, ARCH_SPRING_UNITS);
                }
            }
        }
    }

    #[test]
    fn colonnade_gaps_admit_the_player() {
        // Along any structural strip, no solid run may exceed a post: the
        // 0.8 u gap between posts must always reappear within one period.
        let tuning = default_tuning();
        let mut longest_run = 0u32;
        let mut run = 0u32;
        for step in 0..2000 {
            let wz = step as f32 * 0.1;
            // A west strip line (x on a cell boundary).
            let s = expanse_structure(42, &tuning, FINE * 4.0 + 0.1, wz);
            if s.solid {
                run += 1;
                longest_run = longest_run.max(run);
            } else {
                run = 0;
            }
        }
        assert!(
            longest_run as f32 * 0.1 <= POST_SIDE + 0.15,
            "a colonnade fused into a wall: {longest_run} solid decimeters"
        );
    }
}
