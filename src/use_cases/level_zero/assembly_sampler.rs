//! Sampling one planned assembly: perimeter walls pierced by entrances,
//! sparse interior partitions, structural bays (plus a renovator's
//! contradictory grid), and fixtures on the room's ceiling modules.

use crate::domain::entities::architecture::{
    AssemblyInstance, CeilingLanguage, StructuralSystem, StructuralSystemInstance,
};
use crate::use_cases::generate_chunk::LevelTuning;

use super::fixture_plan::{FixtureOwner, fixture_at};
use super::{BackroomsLevel, ColumnPlan};

impl BackroomsLevel {
    /// Is (wx, wz) on a structural column of this system?
    pub(super) fn on_column(st: &StructuralSystemInstance, wx: f32, wz: f32) -> bool {
        if st.system == StructuralSystem::CoreAndShell {
            // Core-and-shell designers hide columns in walls; none inside.
            return false;
        }
        let mut mx = (wx - st.phase.0).rem_euclid(st.bay_x);
        let mz = (wz - st.phase.1).rem_euclid(st.bay_z);
        if st.system == StructuralSystem::OffsetGrid {
            let row = ((wz - st.phase.1) / st.bay_z).floor() as i64;
            if row.rem_euclid(2) == 1 {
                mx = (wx - st.phase.0 + st.bay_x * 0.5).rem_euclid(st.bay_x);
            }
        }
        mx < st.column_side && mz < st.column_side
    }

    /// Column plan for a point inside an assembly footprint or its
    /// surrounding wall band (`inside == false`).
    pub(super) fn assembly_column(
        a: &AssemblyInstance,
        renovator: Option<&StructuralSystemInstance>,
        tuning: &LevelTuning,
        seed: u32,
        wx: f32,
        wz: f32,
    ) -> ColumnPlan {
        let walls_on = tuning.walls > 0.0;
        let zone = a.ceiling.zone_at(wx, wz);
        let mut ceiling_units = zone.map_or(3.4, |c| c.height_units);
        // Coffered reads as a shading pattern now (see the splat shader),
        // not stepped geometry.
        if let Some(CeilingLanguage::ExposedSoffit) = zone.map(|c| c.language) {
            ceiling_units -= 0.2;
        }
        let mut plan = ColumnPlan::open(ceiling_units);
        // A planned room is a fitted-out room: base at the wall foot, real
        // ceiling grid overhead. An abandoned expansion was built and never
        // occupied, so nobody has replaced a tile in it — it ages the
        // ceiling directly rather than waiting on the institution field,
        // because abandonment is a fact about *this room*, not its district.
        plan.assembly = super::ceiling_system::fitted_stack(
            seed,
            wx,
            wz,
            ceiling_units,
            if a.corruption.abandoned { 0.9 } else { 0.15 },
        );

        // Hosts own wall geometry; openings cut only their referenced host.
        // This remains stable when a grammar moves, removes, or duplicates a
        // wall because the relationship is identity-based, not rediscovered
        // from coincident room bounds at every sample.
        let mut covering_hosts = 0usize;
        let mut cut_hosts = 0usize;
        let mut lintel_units = 0.0f32;
        let mut door_leaf = false;
        for host in a.hosts.iter().filter(|host| host.contains_plan(wx, wz)) {
            covering_hosts += 1;
            ceiling_units = ceiling_units.max(host.top_units);
            if let Some(opening) = a
                .openings
                .iter()
                .find(|opening| opening.host == host.id && opening.contains_plan(host, wx, wz))
            {
                cut_hosts += 1;
                lintel_units = lintel_units.max(opening.lintel_units.unwrap_or(0.0));
                door_leaf |= a.door_leaves.iter().any(|leaf| leaf.opening == opening.id);
            }
        }
        if covering_hosts > 0 {
            plan.ceiling_units = ceiling_units;
            if cut_hosts == covering_hosts {
                plan.lintel_from_units = walls_on
                    .then_some(lintel_units)
                    .filter(|height| *height > 0.0);
                plan.door_leaf = walls_on && door_leaf;
            } else {
                plan.solid = walls_on;
            }
            return plan;
        }

        // Structure: the original grid, plus the renovator's contradictory
        // grid where a renovation overlays the assembly.
        if tuning.pillars > 0.0 && !plan.solid {
            if Self::on_column(&a.structure, wx, wz) {
                plan.solid = true;
            }
            if let Some(r) = renovator
                && a.corruption.renovation_overlay
                && Self::on_column(r, wx, wz)
            {
                // Renovation columns intrude on the original bay rhythm;
                // the contradiction itself is the corruption. They stay
                // ordinary wall material — Level 0 has no red masonry.
                plan.solid = true;
            }
        }

        // Fixtures follow the assembly's ceiling zones and structural grid.
        if !plan.solid
            && tuning.lights > 0.0
            && let Some(zone) = a.ceiling.zone_at(wx, wz)
        {
            plan.fixture =
                fixture_at(seed, FixtureOwner::Assembly { assembly: a, zone }, wx, wz);
        }
        plan
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::entities::architecture::{
        AssemblyInstance, CeilingPlan, CeilingZone, CorruptionProfile, HostSegment, Polygon2,
        SpaceProgram, StructuralSystem,
    };

    fn zoned_assembly() -> AssemblyInstance {
        let footprint = Polygon2::rect(0.0, 0.0, 10.0, 4.0);
        AssemblyInstance {
            id: 1,
            program: SpaceProgram::OpenOffice,
            footprint: footprint.clone(),
            hosts: HostSegment::rectangular_shell(&footprint, 0.4, 5.0),
            openings: Vec::new(),
            door_leaves: Vec::new(),
            spaces: Vec::new(),
            structure: StructuralSystemInstance {
                system: StructuralSystem::CoreAndShell,
                bay_x: 4.0,
                bay_z: 4.0,
                phase: (0.0, 0.0),
                column_side: 0.4,
            },
            ceiling: CeilingPlan::banded(
                CeilingZone {
                    area: footprint,
                    language: CeilingLanguage::FlatTiles,
                    height_units: 3.2,
                },
                vec![CeilingZone {
                    area: Polygon2::rect(5.0, 0.0, 5.0, 4.0),
                    language: CeilingLanguage::FlatTiles,
                    height_units: 4.8,
                }],
            ),
            fixtures: Vec::new(),
            service_voids: Vec::new(),
            corruption: CorruptionProfile::default(),
        }
    }

    #[test]
    fn assembly_columns_use_the_spatially_containing_ceiling_zone() {
        let assembly = zoned_assembly();
        let tuning = LevelTuning::default();
        let seed = 42;
        let low = BackroomsLevel::assembly_column(&assembly, None, &tuning, seed, 2.0, 2.0);
        let high = BackroomsLevel::assembly_column(&assembly, None, &tuning, seed, 8.0, 2.0);
        assert_eq!(low.ceiling_units, 3.2);
        assert_eq!(high.ceiling_units, 4.8);
    }
}
