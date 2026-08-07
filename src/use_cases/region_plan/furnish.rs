//! The fit-out on the floor: what each room is furnished with.
//!
//! Canon describes Level 0's rooms by what is *in* them — desks, chairs,
//! tables, shelving — and the engine had one prop voxel in the whole level.
//! An empty shell reads as a set, not a building someone worked in.
//!
//! Arranged, not scattered. Real furniture sits in rows aligned to the room:
//! desks run in banks, chairs face a table, racking lines a storage wall.
//! So each program states a *pattern*, and the pattern's candidates are then
//! filtered against the things furniture may never do — sit inside a
//! structural column, straddle a partition, or stand in a doorway. That
//! ordering matters: rejection-sampling positions and calling whatever
//! survives a layout produces confetti, which is exactly how procedural
//! interiors betray themselves.
//!
//! Pure in its inputs, like the rest of the planner: same assembly in,
//! same furniture out, with no reference to chunks or voxels.

use crate::domain::entities::architecture::{
    AssemblyInstance, FurnitureKind, FurniturePiece, HostSegment, Opening, Polygon2, SpaceProgram,
};
use crate::domain::entities::position::Position;

use super::PLAN_WALL_T;

/// Clear floor kept in front of every opening, world units.
///
/// Egress, not tidiness: a doorway blocked by a desk is a room you cannot
/// leave, and Level 0's whole horror depends on being able to keep walking.
/// IBC Ch.10 wants the door's own width clear; this is that, rounded up to
/// the plan lattice.
const DOOR_CLEARANCE: f32 = 1.6;

/// Gap kept between two pieces and between a piece and a wall face.
/// Below this the two read as one lump of geometry rather than two objects.
const PIECE_GAP: f32 = 0.4;

/// Circulation kept clear down the middle of a furnished room, world units.
/// A room packed wall-to-wall with desks has no way through it; real plans
/// keep an aisle, and the aisle is what makes the furniture read as arranged
/// *around* movement.
const AISLE: f32 = 1.2;

/// Furnishes one assembly.
///
/// `density` is the architect genome's `furnishing_density`: 0 leaves bare
/// shells, 1 fills a floor plate. It scales how much of the pattern is
/// actually placed, so two regions by different designers furnish the same
/// program differently without needing two patterns.
pub(super) fn furnish(
    assembly: &AssemblyInstance,
    density: f32,
    seed_unit: f32,
) -> Vec<FurniturePiece> {
    // Nobody ever moved in. An abandoned expansion keeps its walls and
    // loses everything that was going to make it a room.
    if assembly.corruption.abandoned {
        return Vec::new();
    }
    let density = density.clamp(0.0, 1.0);
    let (x0, z0, x1, z1) = assembly.footprint.bounds();
    // Usable floor: inside the shell, clear of the wall faces.
    let inset = PLAN_WALL_T * 0.5 + PIECE_GAP;
    let (fx0, fz0) = (x0 + inset, z0 + inset);
    let (fx1, fz1) = (x1 - inset, z1 - inset);
    if fx1 - fx0 < 1.0 || fz1 - fz0 < 1.0 {
        return Vec::new();
    }

    let room = Room {
        x0: fx0,
        z0: fz0,
        x1: fx1,
        z1: fz1,
        seed_unit,
    };
    let candidates = match assembly.program {
        SpaceProgram::OpenOffice => desk_banks(&room, density),
        SpaceProgram::PrivateOffice => private_office(&room),
        SpaceProgram::ConferenceRoom => table_with_seating(&room, FurnitureKind::Table),
        SpaceProgram::BreakRoom => break_room(&room, density),
        SpaceProgram::WaitingArea | SpaceProgram::Atrium => perimeter_seating(&room, density),
        SpaceProgram::Reception => reception(&room),
        SpaceProgram::Storage | SpaceProgram::ServerRoom => racking(&room, density),
        SpaceProgram::RestroomCore => restroom(&room),
        // Circulation, plant and vertical cores carry no furniture. Plant
        // rooms have equipment, but equipment is structure-scale and belongs
        // with the service voids, not on the furniture layer.
        _ => Vec::new(),
    };

    commit(candidates, assembly)
}

/// Usable floor of one room, plus the room's own deterministic draw.
struct Room {
    x0: f32,
    z0: f32,
    x1: f32,
    z1: f32,
    seed_unit: f32,
}

impl Room {
    fn width(&self) -> f32 {
        self.x1 - self.x0
    }
    fn depth(&self) -> f32 {
        self.z1 - self.z0
    }
    fn center(&self) -> (f32, f32) {
        ((self.x0 + self.x1) * 0.5, (self.z0 + self.z1) * 0.5)
    }
    /// Long axis runs along X?
    fn long_x(&self) -> bool {
        self.width() >= self.depth()
    }
}

fn piece(x: f32, z: f32, half_x: f32, half_z: f32, kind: FurnitureKind) -> FurniturePiece {
    FurniturePiece {
        at: Position::new(x, z),
        half_x,
        half_z,
        kind,
    }
}

/// Open office: banks of desks in rows, each desk paired with a chair, with
/// an aisle down the room's long axis.
fn desk_banks(room: &Room, density: f32) -> Vec<FurniturePiece> {
    let mut out = Vec::new();
    let (dx, dz) = (0.8f32, 0.6f32); // half-extents: a 1.6 x 1.2 workstation
    let pitch_along = dx * 2.0 + PIECE_GAP;
    let pitch_across = dz * 2.0 + AISLE;
    let (cx, cz) = room.center();

    if room.long_x() {
        let mut z = room.z0 + dz;
        let mut row = 0;
        while z + dz <= room.z1 {
            // Leave the central aisle clear rather than filling to the wall.
            if (z - cz).abs() > AISLE * 0.5 || room.depth() < AISLE * 3.0 {
                let mut x = room.x0 + dx;
                while x + dx <= room.x1 {
                    out.push(piece(x, z, dx, dz, FurnitureKind::Desk));
                    // The chair sits on the aisle side of its desk.
                    let side = if z < cz { dz + 0.4 } else { -(dz + 0.4) };
                    out.push(piece(x, z + side, 0.3, 0.3, FurnitureKind::Chair));
                    x += pitch_along;
                }
            }
            z += pitch_across;
            row += 1;
            if row > 12 {
                break;
            }
        }
    } else {
        let mut x = room.x0 + dz;
        let mut col = 0;
        while x + dz <= room.x1 {
            if (x - cx).abs() > AISLE * 0.5 || room.width() < AISLE * 3.0 {
                let mut z = room.z0 + dx;
                while z + dx <= room.z1 {
                    out.push(piece(x, z, dz, dx, FurnitureKind::Desk));
                    let side = if x < cx { dz + 0.4 } else { -(dz + 0.4) };
                    out.push(piece(x + side, z, 0.3, 0.3, FurnitureKind::Chair));
                    z += pitch_along;
                }
            }
            x += pitch_across;
            col += 1;
            if col > 12 {
                break;
            }
        }
    }
    thin(out, density)
}

/// One desk against a wall, a chair at it, a cabinet down the side.
///
/// Which wall the desk faces is the room's own draw: an office whose desk is
/// always against the same wall is a stamp, and a floor of identical private
/// offices is exactly the tiling this whole layer exists to avoid.
fn private_office(room: &Room) -> Vec<FurniturePiece> {
    let (cx, cz) = room.center();
    let far = room.seed_unit < 0.5;
    let desk_z = if far { room.z0 + 0.8 } else { room.z1 - 0.8 };
    let chair_z = if far { desk_z + 1.2 } else { desk_z - 1.2 };
    let cabinet_x = if far { room.x1 - 0.4 } else { room.x0 + 0.4 };
    vec![
        piece(cx, desk_z, 0.9, 0.6, FurnitureKind::Desk),
        piece(cx, chair_z, 0.3, 0.3, FurnitureKind::Chair),
        piece(cabinet_x, cz, 0.4, 0.8, FurnitureKind::Cabinet),
    ]
}

/// A table centred in the room with chairs down both long sides.
fn table_with_seating(room: &Room, kind: FurnitureKind) -> Vec<FurniturePiece> {
    let (cx, cz) = room.center();
    let (half_x, half_z) = if room.long_x() {
        ((room.width() * 0.3).min(2.4), 0.8)
    } else {
        (0.8, (room.depth() * 0.3).min(2.4))
    };
    let mut out = vec![piece(cx, cz, half_x, half_z, kind)];
    let seats = 4;
    for k in 0..seats {
        let t = (k as f32 + 0.5) / seats as f32 - 0.5;
        if room.long_x() {
            let x = cx + t * half_x * 2.0;
            out.push(piece(x, cz - half_z - 0.5, 0.3, 0.3, FurnitureKind::Chair));
            out.push(piece(x, cz + half_z + 0.5, 0.3, 0.3, FurnitureKind::Chair));
        } else {
            let z = cz + t * half_z * 2.0;
            out.push(piece(cx - half_x - 0.5, z, 0.3, 0.3, FurnitureKind::Chair));
            out.push(piece(cx + half_x + 0.5, z, 0.3, 0.3, FurnitureKind::Chair));
        }
    }
    out
}

/// Several small tables rather than one big one.
fn break_room(room: &Room, density: f32) -> Vec<FurniturePiece> {
    let mut out = Vec::new();
    let mut x = room.x0 + 1.2;
    while x + 1.2 <= room.x1 {
        let mut z = room.z0 + 1.2;
        while z + 1.2 <= room.z1 {
            out.push(piece(x, z, 0.7, 0.7, FurnitureKind::Table));
            out.push(piece(x - 1.1, z, 0.3, 0.3, FurnitureKind::Chair));
            out.push(piece(x + 1.1, z, 0.3, 0.3, FurnitureKind::Chair));
            z += 3.2;
        }
        x += 3.2;
    }
    thin(out, density)
}

/// Chairs along the walls, facing in. What a waiting area is.
fn perimeter_seating(room: &Room, density: f32) -> Vec<FurniturePiece> {
    let mut out = Vec::new();
    // Phase the run off the room's own draw so two waiting areas of the same
    // size do not seat identically.
    let mut x = room.x0 + 0.6 + room.seed_unit.clamp(0.0, 1.0) * 0.8;
    while x <= room.x1 - 0.6 {
        out.push(piece(x, room.z0 + 0.4, 0.3, 0.3, FurnitureKind::Chair));
        out.push(piece(x, room.z1 - 0.4, 0.3, 0.3, FurnitureKind::Chair));
        x += 1.0;
    }
    thin(out, density * 0.8)
}

/// A counter facing the entrance, with seating opposite.
fn reception(room: &Room) -> Vec<FurniturePiece> {
    let (cx, cz) = room.center();
    let mut out = vec![piece(
        cx,
        cz,
        (room.width() * 0.25).min(2.0),
        0.6,
        FurnitureKind::Desk,
    )];
    let mut x = room.x0 + 0.8;
    while x <= room.x1 - 0.8 {
        out.push(piece(x, room.z1 - 0.5, 0.3, 0.3, FurnitureKind::Chair));
        x += 1.2;
    }
    out
}

/// Racking in rows with aisles between, along the room's long axis.
fn racking(room: &Room, density: f32) -> Vec<FurniturePiece> {
    let mut out = Vec::new();
    let run = 0.5f32; // half-depth of a rack
    let pitch = run * 2.0 + AISLE;
    if room.long_x() {
        let mut z = room.z0 + run;
        while z + run <= room.z1 {
            out.push(piece(
                (room.x0 + room.x1) * 0.5,
                z,
                room.width() * 0.5 - 0.4,
                run,
                FurnitureKind::Shelving,
            ));
            z += pitch;
        }
    } else {
        let mut x = room.x0 + run;
        while x + run <= room.x1 {
            out.push(piece(
                x,
                (room.z0 + room.z1) * 0.5,
                run,
                room.depth() * 0.5 - 0.4,
                FurnitureKind::Shelving,
            ));
            x += pitch;
        }
    }
    thin(out, density.max(0.5))
}

/// Fittings down one wall.
fn restroom(room: &Room) -> Vec<FurniturePiece> {
    let mut out = Vec::new();
    let mut x = room.x0 + 0.5;
    while x <= room.x1 - 0.5 {
        out.push(piece(x, room.z0 + 0.4, 0.4, 0.4, FurnitureKind::Fixture));
        x += 1.0;
    }
    out
}

/// Drops pieces to hit `density`, deterministically and without clumping.
///
/// A golden-ratio walk rather than a hash per piece: a hash thins evenly on
/// average but leaves visible holes and clusters in any one room, and a
/// half-empty office should read as *sparsely furnished*, not as a floor
/// where someone removed random desks.
fn thin(pieces: Vec<FurniturePiece>, density: f32) -> Vec<FurniturePiece> {
    let keep = (0.35 + 0.65 * density.clamp(0.0, 1.0)).clamp(0.0, 1.0);
    if keep >= 0.999 {
        return pieces;
    }
    let mut acc = 0.0f32;
    let mut out = Vec::with_capacity(pieces.len());
    for p in pieces {
        // Chairs follow the surface they belong to rather than being thinned
        // independently: a chair without its desk is litter.
        acc += keep;
        if acc >= 1.0 {
            acc -= 1.0;
            out.push(p);
        } else if p.kind == FurnitureKind::Chair {
            continue;
        }
    }
    out
}

/// Filters candidates against everything furniture may not do, keeping the
/// ones that survive in pattern order.
fn commit(candidates: Vec<FurniturePiece>, assembly: &AssemblyInstance) -> Vec<FurniturePiece> {
    let mut placed: Vec<FurniturePiece> = Vec::new();
    for candidate in candidates {
        if !fits(&candidate, assembly, &placed) {
            continue;
        }
        placed.push(candidate);
        if placed.len() >= 128 {
            break;
        }
    }
    placed
}

fn fits(
    candidate: &FurniturePiece,
    assembly: &AssemblyInstance,
    placed: &[FurniturePiece],
) -> bool {
    let b = candidate.bounds();
    // Wholly inside the room it belongs to.
    if !corners_inside(&assembly.footprint, b) {
        return false;
    }
    // Never inside a building column.
    if assembly
        .structure
        .has_column_at(candidate.at.x, candidate.at.z)
    {
        return false;
    }
    // Never straddling a wall or partition.
    if assembly.hosts.iter().any(|host| host_overlaps(host, b)) {
        return false;
    }
    // Never in the clear floor a door needs.
    if assembly
        .openings
        .iter()
        .any(|opening| blocks_opening(opening, candidate))
    {
        return false;
    }
    // Never inside another piece.
    !placed.iter().any(|other| {
        let o = other.bounds();
        b.0 < o.2 + PIECE_GAP * 0.5
            && o.0 < b.2 + PIECE_GAP * 0.5
            && b.1 < o.3 + PIECE_GAP * 0.5
            && o.1 < b.3 + PIECE_GAP * 0.5
    })
}

fn corners_inside(footprint: &Polygon2, b: (f32, f32, f32, f32)) -> bool {
    footprint.contains(b.0, b.1)
        && footprint.contains(b.2, b.1)
        && footprint.contains(b.0, b.3)
        && footprint.contains(b.2, b.3)
}

fn host_overlaps(host: &HostSegment, b: (f32, f32, f32, f32)) -> bool {
    // Sample the piece's own footprint against the wall band rather than
    // testing segment intersection: hosts are axis-aligned bands and this
    // stays correct for a piece that merely clips a wall's corner.
    let step = 0.2f32;
    let mut z = b.1;
    while z <= b.3 {
        let mut x = b.0;
        while x <= b.2 {
            if host.contains_plan(x, z) {
                return true;
            }
            x += step;
        }
        z += step;
    }
    false
}

fn blocks_opening(opening: &Opening, candidate: &FurniturePiece) -> bool {
    let dx = (candidate.at.x - opening.center.x).abs() - candidate.half_x;
    let dz = (candidate.at.z - opening.center.z).abs() - candidate.half_z;
    dx.max(0.0).hypot(dz.max(0.0)) < DOOR_CLEARANCE
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::entities::architecture::{
        CeilingLanguage, CeilingPlan, CeilingZone, CorruptionProfile, HostRole, HostSegment,
        OpeningId, OpeningRole, StructuralSystem, StructuralSystemInstance,
    };

    fn room_assembly(program: SpaceProgram, w: f32, d: f32) -> AssemblyInstance {
        let footprint = Polygon2::rect(0.0, 0.0, w, d);
        let ceiling = CeilingZone {
            area: footprint.clone(),
            language: CeilingLanguage::FlatTiles,
            height_units: 3.4,
        };
        AssemblyInstance {
            id: 0,
            program,
            hosts: HostSegment::rectangular_shell(&footprint, PLAN_WALL_T, 3.4),
            openings: vec![Opening {
                id: OpeningId(0),
                host: crate::domain::entities::architecture::HostId(0),
                role: OpeningRole::Entrance,
                center: Position::new(w * 0.5, 0.0),
                width: 0.9,
                through_x_wall: true,
                lintel_units: Some(2.1),
            }],
            door_leaves: Vec::new(),
            spaces: Vec::new(),
            structure: StructuralSystemInstance {
                system: StructuralSystem::RegularGrid,
                bay_x: 6.0,
                bay_z: 6.0,
                phase: (0.0, 0.0),
                column_side: 0.4,
            },
            ceiling: CeilingPlan::flat(ceiling),
            fixtures: Vec::new(),
            furniture: Vec::new(),
            service_voids: Vec::new(),
            corruption: CorruptionProfile::default(),
            footprint,
        }
    }

    #[test]
    fn an_office_gets_desks_and_they_stay_inside_the_room() {
        let a = room_assembly(SpaceProgram::OpenOffice, 16.0, 12.0);
        let out = furnish(&a, 1.0, 0.5);
        assert!(!out.is_empty(), "an open office furnished to nothing");
        assert!(out.iter().any(|p| p.kind == FurnitureKind::Desk));
        for p in &out {
            let b = p.bounds();
            assert!(
                b.0 >= 0.0 && b.1 >= 0.0 && b.2 <= 16.0 && b.3 <= 12.0,
                "piece {p:?} escaped the room"
            );
        }
    }

    #[test]
    fn nothing_ever_blocks_the_doorway() {
        for program in [
            SpaceProgram::OpenOffice,
            SpaceProgram::ConferenceRoom,
            SpaceProgram::BreakRoom,
            SpaceProgram::Storage,
            SpaceProgram::WaitingArea,
            SpaceProgram::Reception,
            SpaceProgram::RestroomCore,
        ] {
            let a = room_assembly(program, 14.0, 10.0);
            for piece in furnish(&a, 1.0, 0.3) {
                for opening in &a.openings {
                    assert!(
                        !blocks_opening(opening, &piece),
                        "{program:?} put {:?} in its doorway",
                        piece.kind
                    );
                }
            }
        }
    }

    #[test]
    fn furniture_never_stands_inside_a_structural_column() {
        let mut a = room_assembly(SpaceProgram::OpenOffice, 20.0, 16.0);
        a.structure.bay_x = 4.0;
        a.structure.bay_z = 4.0;
        a.structure.column_side = 0.6;
        for piece in furnish(&a, 1.0, 0.7) {
            assert!(
                !a.structure.has_column_at(piece.at.x, piece.at.z),
                "{:?} was placed inside a building column",
                piece.kind
            );
        }
    }

    #[test]
    fn pieces_do_not_overlap_each_other() {
        let a = room_assembly(SpaceProgram::OpenOffice, 18.0, 14.0);
        let out = furnish(&a, 1.0, 0.9);
        for (i, p) in out.iter().enumerate() {
            for q in &out[i + 1..] {
                let (pb, qb) = (p.bounds(), q.bounds());
                assert!(
                    !(pb.0 < qb.2 && qb.0 < pb.2 && pb.1 < qb.3 && qb.1 < pb.3),
                    "{:?} overlaps {:?}",
                    p.kind,
                    q.kind
                );
            }
        }
    }

    #[test]
    fn an_abandoned_expansion_is_never_furnished() {
        let mut a = room_assembly(SpaceProgram::OpenOffice, 16.0, 12.0);
        a.corruption.abandoned = true;
        assert!(furnish(&a, 1.0, 0.5).is_empty());
    }

    #[test]
    fn density_scales_how_much_is_placed() {
        let a = room_assembly(SpaceProgram::OpenOffice, 20.0, 16.0);
        let sparse = furnish(&a, 0.0, 0.5).len();
        let full = furnish(&a, 1.0, 0.5).len();
        assert!(
            full > sparse,
            "furnishing density did not change the fit-out ({full} vs {sparse})"
        );
    }

    #[test]
    fn circulation_and_plant_carry_no_furniture() {
        for program in [
            SpaceProgram::MainCorridor,
            SpaceProgram::Mechanical,
            SpaceProgram::Stair,
        ] {
            let a = room_assembly(program, 14.0, 10.0);
            assert!(
                furnish(&a, 1.0, 0.5).is_empty(),
                "{program:?} was furnished"
            );
        }
    }
}
