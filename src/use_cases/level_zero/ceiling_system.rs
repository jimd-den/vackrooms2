//! The suspended ceiling as a *system*, not a height.
//!
//! A dropped ceiling in a real fit-out is a coordination module: aluminium
//! T-bar runners on a fixed tile grid, acoustic tiles dropped into it, and a
//! plenum above carrying duct, pipe and structure. Every drawing set draws it
//! separately — the reflected ceiling plan — because the ceiling grid is what
//! lights, diffusers and sprinklers align to.
//!
//! Level 0 needs this for two reasons beyond realism. The grid gives the
//! ceiling a *module*, which is what makes a room read as built rather than
//! extruded; and the missing tile is the level's signature ceiling motif and
//! its architectural seam to Level 1 — the pipes glimpsed through a gap here
//! are the same pipes Level 1 exposes outright.
//!
//! Everything is a pure function of world coordinates, so two chunks
//! sampling the same ceiling agree without coordinating — the same contract
//! the rest of Level 0 keeps.

use crate::domain::entities::voxel_grid::{VOXEL_DUCT, VOXEL_PIPE};
use crate::use_cases::level_zero::GRID_HEIGHT_UNITS;
use crate::use_cases::level_zero::column_plan::AssemblyStack;
use crate::use_cases::level_zero::fixture_plan::fixture_hash;

/// Ceiling tile module, world units. A 600x1200 mm lay-in tile — the
/// near-universal commercial size, and the one the ISO 2848 300/600
/// preferred dimensions land on.
pub(crate) const TILE_X: f32 = 1.2;
pub(crate) const TILE_Z: f32 = 0.6;

/// Width of the exposed T-bar between tiles. Real runners are 15--24 mm;
/// this is one voxel at Level 0's 0.2 u scale, which is the finest a runner
/// can be drawn and still exist. Drawn wider than reality on purpose: a
/// grid line thinner than a voxel is not a grid, it is nothing.
const RUNNER_T: f32 = 0.2;

/// Base course height at the foot of a partition. A real vinyl base is
/// ~100 mm; at a 0.2 u voxel this quantizes to a single course either way,
/// so it is authored at the voxel it will actually occupy rather than at a
/// dimension the grid cannot express.
pub(crate) const BASEBOARD_UNITS: f32 = 0.2;

/// Depth of the plenum between finished ceiling and slab.
const PLENUM_DEPTH: f32 = 0.8;

/// Share of tiles missing from an intact ceiling. Deliberately small: the
/// motif works because it is an exception you notice, and a ceiling with
/// every fourth tile gone reads as demolition rather than neglect.
const MISSING_SHARE_INTACT: f32 = 0.02;

/// Share of tiles missing where the institution has aged out completely.
/// `institution_age` already drives wallpaper and carpet decay; the ceiling
/// ages with them rather than on a schedule of its own.
const MISSING_SHARE_AGED: f32 = 0.10;

/// Which tile of the ceiling grid a world point falls in.
fn tile_of(wx: f32, wz: f32) -> (i64, i64) {
    ((wx / TILE_X).floor() as i64, (wz / TILE_Z).floor() as i64)
}

/// Is this column on a grid runner rather than inside a tile?
///
/// Runners are the tile boundaries themselves, so this is a pure function of
/// position on the world lattice — no per-room origin, which means the grid
/// runs continuously across a whole floor exactly as a real one does, rather
/// than restarting at every room and betraying the room's extent.
fn on_runner(wx: f32, wz: f32) -> bool {
    let fx = wx.rem_euclid(TILE_X);
    let fz = wz.rem_euclid(TILE_Z);
    fx < RUNNER_T || fz < RUNNER_T
}

/// Plenum depth available above a finished ceiling at `ceiling_units`.
///
/// Zero when the room is too tall to have one. This is not a fudge: a vault
/// whose ceiling reaches for the slab genuinely has no plenum, and the
/// missing-tile motif must not fire there, because there would be nothing
/// above the tile to reveal. The world's total height budget decides.
pub(crate) fn plenum_depth_at(ceiling_units: f32) -> f32 {
    // Room for the plenum void *and* the slab course that closes it.
    let available = GRID_HEIGHT_UNITS - ceiling_units - RUNNER_T;
    if available < PLENUM_DEPTH {
        0.0
    } else {
        PLENUM_DEPTH
    }
}

/// What runs through the plenum over one tile, if anything.
///
/// Services are sparse and run in the plenum's own right, not per-tile
/// confetti: the hash is taken on the tile's *duct run* — a band of tiles
/// sharing a z row — so a glimpse through one missing tile shows a duct
/// going somewhere rather than a disconnected fragment.
fn plenum_content_at(seed: u32, tile: (i64, i64)) -> Option<u8> {
    let run = fixture_hash(seed, 0x0DC7_0001, tile.1, 0);
    if run < 0.30 {
        Some(VOXEL_DUCT)
    } else if run < 0.55 {
        Some(VOXEL_PIPE)
    } else {
        None
    }
}

/// The ceiling/base assembly for one fitted-out column.
///
/// `age` is `institution_age` at this column (0 = maintained, 1 = long
/// abandoned); it decides how much of the ceiling has come down.
pub(crate) fn fitted_stack(
    seed: u32,
    wx: f32,
    wz: f32,
    ceiling_units: f32,
    age: f32,
) -> AssemblyStack {
    let plenum_units = plenum_depth_at(ceiling_units);
    let tile = tile_of(wx, wz);
    let share =
        MISSING_SHARE_INTACT + (MISSING_SHARE_AGED - MISSING_SHARE_INTACT) * age.clamp(0.0, 1.0);
    // A runner is steel: it stays up when the tile it carries falls out, so
    // only tiles go missing. This is also what keeps the hole reading as a
    // *tile-shaped* gap rather than an amorphous void.
    let tile_missing = plenum_units > 0.0
        && !on_runner(wx, wz)
        && fixture_hash(seed, 0x0007_171E, tile.0, tile.1) < share;

    AssemblyStack {
        baseboard_units: BASEBOARD_UNITS,
        ceiling_grid: on_runner(wx, wz),
        tile_missing,
        plenum_units,
        plenum_content: tile_missing
            .then(|| plenum_content_at(seed, tile))
            .flatten(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runners_form_a_continuous_grid_on_the_tile_module() {
        // Exactly on a tile boundary is a runner; the middle of a tile is not.
        assert!(on_runner(0.0, 0.0));
        assert!(on_runner(TILE_X, 5.0 * TILE_Z));
        assert!(!on_runner(TILE_X * 0.5, TILE_Z * 0.5));
    }

    #[test]
    fn the_grid_is_world_aligned_not_room_aligned() {
        // Two points a whole tile apart must agree: a grid that restarted
        // per room would betray the room's extent as a seam in the ceiling.
        for k in 0..8 {
            let x = 3.7 + k as f32 * TILE_X;
            assert_eq!(
                on_runner(3.7, 2.9),
                on_runner(x, 2.9 + k as f32 * TILE_Z),
                "grid phase drifted after {k} tiles"
            );
        }
    }

    #[test]
    fn tall_rooms_have_no_plenum_and_therefore_never_lose_a_tile() {
        // A vault reaching for the slab has nothing above the tile.
        let tall = GRID_HEIGHT_UNITS - 0.1;
        assert_eq!(plenum_depth_at(tall), 0.0);
        for k in 0..200 {
            let stack = fitted_stack(42, k as f32 * 0.37, k as f32 * 0.21, tall, 1.0);
            assert!(
                !stack.tile_missing,
                "a room with no plenum opened a hole into nothing"
            );
        }
    }

    #[test]
    fn ordinary_rooms_do_get_a_plenum() {
        assert!(plenum_depth_at(3.4) > 0.0);
    }

    #[test]
    fn missing_tiles_are_rare_when_maintained_and_commoner_when_aged() {
        let count = |age: f32| {
            let mut n = 0;
            let mut total = 0;
            for iz in 0..120 {
                for ix in 0..120 {
                    let (wx, wz) = (ix as f32 * TILE_X + 0.6, iz as f32 * TILE_Z + 0.3);
                    total += 1;
                    if fitted_stack(42, wx, wz, 3.4, age).tile_missing {
                        n += 1;
                    }
                }
            }
            n as f32 / total as f32
        };
        let intact = count(0.0);
        let aged = count(1.0);
        assert!(
            intact < 0.05,
            "a maintained ceiling should be nearly whole, lost {:.1}%",
            intact * 100.0
        );
        assert!(
            aged > intact,
            "an aged ceiling should lose more tiles than a maintained one ({aged:.3} vs {intact:.3})"
        );
    }

    #[test]
    fn a_missing_tile_always_resolves_its_plenum_and_a_runner_never_goes_missing() {
        for k in 0..4000 {
            let (wx, wz) = (k as f32 * 0.13, k as f32 * 0.29);
            let stack = fitted_stack(7, wx, wz, 3.2, 0.8);
            if stack.tile_missing {
                assert!(stack.plenum_units > 0.0);
                assert!(!stack.ceiling_grid, "a steel runner fell out of the grid");
            }
        }
    }
}
