//! Scored greedy room placement.
//!
//! The floor-plan literature's step 2 is: simulate placing each room at
//! every free position, score the placements, commit the best, repeat, in
//! order of program importance. This is that solver.
//!
//! What it replaces was a cursor walk: slide along the dominant corridor
//! leg, roll a hash, and place a suite if the roll passed and the spot
//! happened to be clear. Three consequences, all visible in a plan view --
//! a failed spot was skipped rather than improved on, only the dominant leg
//! was ever considered, and nothing compared two candidate spots against
//! each other. An 80 u region resolved to two rooms and 90% unplanned
//! fabric.
//!
//! Scoring, not solving: this is greedy best-first, not an optimizer. The
//! literature is explicit that greedy placement with good scoring functions
//! reproduces designer judgement well enough, and it stays fast and
//! debuggable, which an annealer would not.

use crate::domain::entities::architecture::{
    ArchitectGenome, AssemblyInstance, CirculationSpine, SpaceProgram,
};
use crate::domain::entities::position::Position;

use super::suites::place_suite;
use super::territories::Territory;

/// Programs in descending importance, with how many of each a region wants
/// at most. Importance order is load-bearing: the literature places the
/// large public rooms first because they are the ones a bad leftover shape
/// ruins, and lets small service rooms take what is left.
/// Interleaved rather than grouped: importance still decides who chooses
/// first, but taking all five open offices before any other program lets
/// the first entry eat the floor and returns a region of nothing but open
/// offices. One pass per round, largest programs earliest in the round.
const PROGRAM_BUDGET: [(SpaceProgram, usize); 7] = [
    (SpaceProgram::OpenOffice, 3),
    (SpaceProgram::ConferenceRoom, 2),
    (SpaceProgram::WaitingArea, 1),
    (SpaceProgram::BreakRoom, 1),
    (SpaceProgram::PrivateOffice, 3),
    (SpaceProgram::Storage, 2),
    (SpaceProgram::ServerRoom, 1),
];

/// Rounds the solver makes over the budget. Each round places at most one
/// of each program, so a floor gets a mix before it gets a repeat.
/// Two, not three. The corruption pass that runs after this one needs
/// somewhere to put duplicated suites, abandoned expansions and red rooms;
/// a solver that takes every good seat leaves it nothing, and a Backrooms
/// region without corruption is just an office. Density is traded against
/// that headroom deliberately.
const ROUNDS: usize = 2;

/// Candidate anchors are tried at this pitch along a leg. Finer than a
/// room is wide, so no viable seat falls between two samples.
const ANCHOR_PITCH: f32 = 2.0;

/// How much of a leg's ends stay clear of rooms, so corners and junctions
/// remain legible as circulation.
const LEG_END_MARGIN: f32 = 4.0;

/// Clear distance required between two placed rooms.
///
/// The suite builder only rejects overlap by 0.8 u, which was ample when a
/// region held two rooms far apart. Packed against each other, neighbours
/// interfere: a doorway samples through the strip between two shells, and a
/// ceiling fixture lands inside the next room's wall. Rooms in a real plan
/// either share a wall or stand clear of one another, and sharing is the
/// connectivity graph's job, not the packer's.
const ROOM_SEPARATION: f32 = 3.2;

/// A horizontal run of corridor that rooms can hang off.
#[derive(Clone, Copy, Debug)]
pub(super) struct Leg {
    pub x0: f32,
    pub x1: f32,
    pub z: f32,
    pub half_width: f32,
}

/// Extracts the corridor runs rooms may attach to.
///
/// Every spine contributes, not only the dominant one: a secondary hall
/// with nothing on it is a corridor to nowhere, which is a liminal
/// *feature* only when it is authored as one rather than left over.
pub(super) fn legs_of(spines: &[CirculationSpine], min_length: f32) -> Vec<Leg> {
    let mut legs = Vec::new();
    for spine in spines {
        for segment in spine.path.windows(2) {
            // Horizontal runs only. Vertical legs need the suite builder to
            // work on the other axis, which it does not yet -- see the
            // module note in `mod.rs`.
            if (segment[0].z - segment[1].z).abs() > 1e-4 {
                continue;
            }
            let (x0, x1) = (
                segment[0].x.min(segment[1].x),
                segment[0].x.max(segment[1].x),
            );
            if x1 - x0 < min_length {
                continue;
            }
            legs.push(Leg {
                x0,
                x1,
                z: segment[0].z,
                half_width: spine.width * 0.5,
            });
        }
    }
    legs
}

/// Places rooms by scored greedy selection, returning them in commit order.
///
/// `taken` is extended with every committed footprint, so callers that run
/// later passes (corruption, stairs) see the same occupancy this did.
#[allow(clippy::too_many_arguments)]
pub(super) fn lay_out_suites(
    seed_hash: &dyn Fn(i64) -> f32,
    genome: &ArchitectGenome,
    legs: &[Leg],
    territories: &[Territory],
    origin: Position,
    region_size: f32,
    wall_thickness: f32,
    spines: &[CirculationSpine],
    taken: &mut Vec<(f32, f32, f32, f32)>,
    next_id: &mut u32,
) -> Vec<AssemblyInstance> {
    let mut placed: Vec<AssemblyInstance> = Vec::new();
    if legs.is_empty() {
        return placed;
    }

    let mut used = [0usize; PROGRAM_BUDGET.len()];
    for round in 0..ROUNDS {
        for (slot, (program, budget)) in PROGRAM_BUDGET.iter().enumerate() {
            if used[slot] >= *budget {
                continue;
            }
            let salt = (slot * 64 + round) as i64;
            let aseed = seed_hash(0x5EA0 + salt);
            let threshold_seed = seed_hash(0x7A110 + salt);

            // Simulate every anchor on every leg, both sides, and keep the
            // best-scoring one that the builder actually accepts.
            let mut best: Option<(f32, AssemblyInstance)> = None;
            for leg in legs {
                let mut x = leg.x0 + LEG_END_MARGIN;
                while x <= leg.x1 - LEG_END_MARGIN {
                    for side in [1.0f32, -1.0] {
                        let Some(candidate) = place_suite(
                            *next_id,
                            *program,
                            genome,
                            aseed,
                            threshold_seed,
                            x,
                            leg.z,
                            leg.half_width + wall_thickness * 0.5,
                            side,
                            origin,
                            region_size,
                            taken,
                            spines,
                        ) else {
                            continue;
                        };
                        // The whole front wall must face corridor, not
                        // just the anchor. A room hung off the last few
                        // units of a leg overhangs its end, and the part
                        // that overhangs faces fabric -- so its entrance
                        // opens into solid wall. The old cursor walk never
                        // hit this only because it stopped a room's width
                        // short of the end.
                        let b = candidate.footprint.bounds();
                        if b.0 < leg.x0 + LEG_END_MARGIN || b.2 > leg.x1 - LEG_END_MARGIN {
                            continue;
                        }
                        if taken.iter().any(|t| {
                            b.0 < t.2 + ROOM_SEPARATION
                                && t.0 < b.2 + ROOM_SEPARATION
                                && b.1 < t.3 + ROOM_SEPARATION
                                && t.1 < b.3 + ROOM_SEPARATION
                        }) {
                            continue;
                        }
                        let score = score_placement(
                            candidate.footprint.bounds(),
                            *program,
                            territories,
                            &placed,
                        );
                        if best.as_ref().is_none_or(|(b, _)| score > *b) {
                            best = Some((score, candidate));
                        }
                    }
                    x += ANCHOR_PITCH;
                }
            }

            let Some((_, chosen)) = best else {
                // Nothing viable for this program in this round. Smaller
                // programs later in the round may still fit, so move on
                // rather than abandoning the pass.
                continue;
            };
            taken.push(chosen.footprint.bounds());
            placed.push(chosen);
            used[slot] += 1;
            *next_id += 1;
        }
    }

    placed
}

/// How good a seat is for a room, higher is better.
///
/// The terms are the ones the office-planning study derived from real
/// layouts: usable area, a habitable proportion, and separation from rooms
/// of the same kind. Corridor adjacency needs no term -- every candidate is
/// corridor-hosted by construction, which is what keeps the plan reachable.
fn score_placement(
    bounds: (f32, f32, f32, f32),
    program: SpaceProgram,
    territories: &[Territory],
    placed: &[AssemblyInstance],
) -> f32 {
    let (width, depth) = (bounds.2 - bounds.0, bounds.3 - bounds.1);
    let center = ((bounds.0 + bounds.2) * 0.5, (bounds.1 + bounds.3) * 0.5);

    // Area, against what this program wants. Both too small and too large
    // are penalized: a 400 m^2 private office is as wrong as a 6 m^2 one.
    let area = width * depth;
    let ideal = ideal_area(program);
    let area_score = 1.0 - ((area - ideal) / ideal).abs().min(1.0);

    // Proportion. Rooms read as rooms between about 1:1 and 1:1.8; beyond
    // that they read as corridors that failed to become corridors.
    let aspect = width.max(depth) / width.min(depth).max(0.01);
    let aspect_score = 1.0 - ((aspect - 1.4) / 1.4).abs().min(1.0);

    // Space used well: a seat inside a large free territory leaves that
    // territory still usable, while one that half-covers a small territory
    // strands the remainder as a sliver.
    let host = territories
        .iter()
        .filter(|t| {
            let (cx, cz) = t.center();
            (cx - center.0).abs() < t.width() * 0.5 + width * 0.5
                && (cz - center.1).abs() < t.depth() * 0.5 + depth * 0.5
        })
        .map(|t| t.area())
        .fold(0.0f32, f32::max);
    let fit_score = if host > 0.0 {
        (area / host).clamp(0.0, 1.0)
    } else {
        0.35
    };

    // Separation from the same program: five open offices in a row is a
    // tiled floorplan, which is the failure mode this whole pass exists to
    // avoid. Distance to the nearest same-program room, saturating at 24 u.
    let separation = placed
        .iter()
        .filter(|other| other.program == program)
        .map(|other| {
            let b = other.footprint.bounds();
            let c = ((b.0 + b.2) * 0.5, (b.1 + b.3) * 0.5);
            (c.0 - center.0).hypot(c.1 - center.1)
        })
        .fold(f32::INFINITY, f32::min);
    let separation_score = if separation.is_finite() {
        (separation / 24.0).clamp(0.0, 1.0)
    } else {
        1.0
    };

    0.30 * area_score + 0.25 * aspect_score + 0.20 * fit_score + 0.25 * separation_score
}

/// Floor area each program is planned around, m^2. Drawn from ordinary
/// commercial space standards rather than tuned: the point of the term is
/// to stop a server room from taking the biggest bay on the floor.
fn ideal_area(program: SpaceProgram) -> f32 {
    match program {
        SpaceProgram::OpenOffice => 220.0,
        SpaceProgram::ConferenceRoom => 90.0,
        SpaceProgram::WaitingArea => 70.0,
        SpaceProgram::BreakRoom => 80.0,
        SpaceProgram::PrivateOffice => 45.0,
        SpaceProgram::Storage => 55.0,
        SpaceProgram::ServerRoom => 60.0,
        _ => 100.0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spine(x0: f32, z0: f32, x1: f32, z1: f32, width: f32) -> CirculationSpine {
        CirculationSpine {
            id: 0,
            path: vec![Position::new(x0, z0), Position::new(x1, z1)],
            width,
            spine_kind: SpaceProgram::MainCorridor,
        }
    }

    #[test]
    fn legs_come_from_every_spine_not_only_the_dominant_one() {
        let mut secondary = spine(4.0, 60.0, 70.0, 60.0, 3.0);
        secondary.spine_kind = SpaceProgram::SecondaryHall;
        let spines = [spine(4.0, 20.0, 70.0, 20.0, 4.0), secondary];
        let legs = legs_of(&spines, 14.0);
        assert_eq!(legs.len(), 2, "both spines must offer legs");
    }

    #[test]
    fn short_legs_and_vertical_runs_are_not_offered() {
        let spines = [
            spine(0.0, 10.0, 6.0, 10.0, 4.0),  // too short
            spine(20.0, 0.0, 20.0, 70.0, 4.0), // vertical
        ];
        assert!(legs_of(&spines, 14.0).is_empty());
    }

    #[test]
    fn the_scorer_prefers_habitable_proportions_and_separation() {
        let square = ideal_area(SpaceProgram::ConferenceRoom).sqrt();
        let good = (0.0, 0.0, square, square);
        let thin = (0.0, 0.0, square * 3.0, square / 3.0);
        assert!(
            score_placement(good, SpaceProgram::ConferenceRoom, &[], &[])
                > score_placement(thin, SpaceProgram::ConferenceRoom, &[], &[]),
            "a 9:1 slot must not outscore a habitable room"
        );
    }

    #[test]
    fn the_scorer_penalizes_area_far_from_the_program_standard() {
        let ideal = ideal_area(SpaceProgram::PrivateOffice).sqrt();
        let right = (0.0, 0.0, ideal, ideal);
        let cavernous = (0.0, 0.0, ideal * 3.0, ideal * 3.0);
        assert!(
            score_placement(right, SpaceProgram::PrivateOffice, &[], &[])
                > score_placement(cavernous, SpaceProgram::PrivateOffice, &[], &[]),
            "a private office the size of a hall must score worse"
        );
    }
}
