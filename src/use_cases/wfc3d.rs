//! 3D Wave Function Collapse over a 6-connected voxel-module lattice.
//!
//! Every cell starts in superposition of all modules. Each iteration picks
//! the uncollapsed cell with minimal weighted Shannon entropy, collapses it
//! to one module by weighted draw, and propagates the six-axis adjacency
//! constraints until the wave is quiet. A cell emptied of candidates is a
//! contradiction, reported with its position rather than papered over.
//!
//! Module sets are capped at 64 so a superposition is one `u64` bitmask:
//! support unions and constraint intersections are single bitwise ops.

use rand::rngs::StdRng;
use rand::{RngExt, SeedableRng};

/// The six lattice directions, ordered so `opposite()` is a bit flip.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Axis6 {
    PosX = 0,
    NegX = 1,
    PosY = 2,
    NegY = 3,
    PosZ = 4,
    NegZ = 5,
}

impl Axis6 {
    pub const ALL: [Axis6; 6] = [
        Axis6::PosX,
        Axis6::NegX,
        Axis6::PosY,
        Axis6::NegY,
        Axis6::PosZ,
        Axis6::NegZ,
    ];

    pub fn opposite(self) -> Axis6 {
        Axis6::ALL[self as usize ^ 1]
    }

    fn offset(self) -> (i64, i64, i64) {
        match self {
            Axis6::PosX => (1, 0, 0),
            Axis6::NegX => (-1, 0, 0),
            Axis6::PosY => (0, 1, 0),
            Axis6::NegY => (0, -1, 0),
            Axis6::PosZ => (0, 0, 1),
            Axis6::NegZ => (0, 0, -1),
        }
    }
}

/// A weighted module catalog plus its directional adjacency rules.
pub struct WfcModuleSet {
    weights: Vec<f32>,
    /// `allowed[dir][module]`: bitmask of modules permitted as the neighbor
    /// of `module` in direction `dir`.
    allowed: [Vec<u64>; 6],
}

impl WfcModuleSet {
    /// `weights[m]` biases how often module `m` is drawn on collapse.
    pub fn new(weights: Vec<f32>) -> Self {
        assert!(
            weights.len() <= 64,
            "a superposition is one u64: at most 64 modules"
        );
        assert!(weights.iter().all(|&w| w > 0.0), "weights must be positive");
        let empty = vec![0u64; weights.len()];
        Self {
            weights,
            allowed: std::array::from_fn(|_| empty.clone()),
        }
    }

    pub fn module_count(&self) -> usize {
        self.weights.len()
    }

    /// Declares "`b` may sit in direction `dir` of `a`" and its mirror rule,
    /// so adjacency stays symmetric by construction.
    pub fn allow(&mut self, a: usize, dir: Axis6, b: usize) {
        self.allowed[dir as usize][a] |= 1 << b;
        self.allowed[dir.opposite() as usize][b] |= 1 << a;
    }

    /// Union of what every candidate in `mask` tolerates in `dir`.
    fn support(&self, mask: u64, dir: Axis6) -> u64 {
        let mut union = 0;
        let mut rest = mask;
        while rest != 0 {
            let module = rest.trailing_zeros() as usize;
            union |= self.allowed[dir as usize][module];
            rest &= rest - 1;
        }
        union
    }

    fn full_mask(&self) -> u64 {
        if self.module_count() == 64 {
            u64::MAX
        } else {
            (1 << self.module_count()) - 1
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WfcContradiction {
    pub position: (usize, usize, usize),
}

/// A solved lattice: `modules[index_of(x, y, z)]` is the collapsed module.
pub struct WfcSolution {
    pub dims: (usize, usize, usize),
    pub modules: Vec<u8>,
}

impl WfcSolution {
    pub fn module_at(&self, x: usize, y: usize, z: usize) -> u8 {
        self.modules[index_of(self.dims, x, y, z)]
    }
}

fn index_of(dims: (usize, usize, usize), x: usize, y: usize, z: usize) -> usize {
    (y * dims.2 + z) * dims.0 + x
}

/// Runs the collapse to completion. Deterministic per (set, dims, seed).
pub fn solve_wfc(
    set: &WfcModuleSet,
    dims: (usize, usize, usize),
    seed: u64,
) -> Result<WfcSolution, WfcContradiction> {
    let (w, h, d) = dims;
    let mut cells = vec![set.full_mask(); w * h * d];
    let mut rng = StdRng::seed_from_u64(seed);

    // Settle the initial wave first: a catalog whose full superposition
    // already lacks support somewhere must surface as a contradiction even
    // if no collapse would ever visit that cell.
    let every_cell: Vec<usize> = (0..cells.len()).collect();
    propagate(set, dims, &mut cells, every_cell)?;

    while let Some(cell) = lowest_entropy_cell(set, &cells) {
        collapse(set, &mut cells[cell], &mut rng);
        propagate(set, dims, &mut cells, vec![cell])?;
    }

    Ok(WfcSolution {
        dims,
        modules: cells
            .iter()
            .map(|mask| mask.trailing_zeros() as u8)
            .collect(),
    })
}

/// The uncollapsed cell with minimal weighted Shannon entropy
/// `H = ln W - (Σ w·ln w)/W`; first occurrence wins ties for determinism.
fn lowest_entropy_cell(set: &WfcModuleSet, cells: &[u64]) -> Option<usize> {
    let mut best: Option<(f32, usize)> = None;
    for (index, &mask) in cells.iter().enumerate() {
        if mask.count_ones() <= 1 {
            continue;
        }
        let (mut total, mut weighted_log) = (0.0f32, 0.0f32);
        let mut rest = mask;
        while rest != 0 {
            let weight = set.weights[rest.trailing_zeros() as usize];
            total += weight;
            weighted_log += weight * weight.ln();
            rest &= rest - 1;
        }
        let entropy = total.ln() - weighted_log / total;
        if best.is_none_or(|(lowest, _)| entropy < lowest) {
            best = Some((entropy, index));
        }
    }
    best.map(|(_, index)| index)
}

/// Collapses one superposition to a single module by weighted draw.
fn collapse(set: &WfcModuleSet, mask: &mut u64, rng: &mut StdRng) {
    let total: f32 = candidates(*mask).map(|m| set.weights[m]).sum();
    let mut roll = rng.random::<f32>() * total;
    for module in candidates(*mask) {
        roll -= set.weights[module];
        if roll <= 0.0 {
            *mask = 1 << module;
            return;
        }
    }
    // Float round-off fell past the last candidate: take it.
    let last = 63 - mask.leading_zeros();
    *mask = 1 << last;
}

fn candidates(mask: u64) -> impl Iterator<Item = usize> {
    (0..64).filter(move |m| mask & (1 << m) != 0)
}

/// Worklist constraint propagation: each changed cell re-restricts its six
/// neighbors until the wave settles or a cell runs out of candidates.
fn propagate(
    set: &WfcModuleSet,
    dims: (usize, usize, usize),
    cells: &mut [u64],
    mut work: Vec<usize>,
) -> Result<(), WfcContradiction> {
    let (w, h, d) = dims;
    while let Some(cell) = work.pop() {
        let x = cell % w;
        let z = (cell / w) % d;
        let y = cell / (w * d);
        for dir in Axis6::ALL {
            let (dx, dy, dz) = dir.offset();
            let (nx, ny, nz) = (x as i64 + dx, y as i64 + dy, z as i64 + dz);
            if nx < 0 || ny < 0 || nz < 0 || nx >= w as i64 || ny >= h as i64 || nz >= d as i64 {
                continue;
            }
            let neighbor = index_of(dims, nx as usize, ny as usize, nz as usize);
            let restricted = cells[neighbor] & set.support(cells[cell], dir);
            if restricted == cells[neighbor] {
                continue;
            }
            if restricted == 0 {
                return Err(WfcContradiction {
                    position: (nx as usize, ny as usize, nz as usize),
                });
            }
            cells[neighbor] = restricted;
            work.push(neighbor);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const GROUND: usize = 0;
    const HORIZON: usize = 1;
    const SKY: usize = 2;

    /// Strata world: solid ground below one horizon layer below sky.
    fn strata_set() -> WfcModuleSet {
        let mut set = WfcModuleSet::new(vec![1.0, 0.4, 1.0]);
        for m in [GROUND, HORIZON, SKY] {
            // Lateral bands: like sits beside like.
            for dir in [Axis6::PosX, Axis6::NegX, Axis6::PosZ, Axis6::NegZ] {
                set.allow(m, dir, m);
            }
        }
        set.allow(GROUND, Axis6::PosY, GROUND);
        set.allow(GROUND, Axis6::PosY, HORIZON);
        set.allow(HORIZON, Axis6::PosY, SKY);
        set.allow(SKY, Axis6::PosY, SKY);
        set
    }

    fn assert_adjacency_holds(set: &WfcModuleSet, solution: &WfcSolution) {
        let (w, h, d) = solution.dims;
        for y in 0..h {
            for z in 0..d {
                for x in 0..w {
                    let module = solution.module_at(x, y, z) as usize;
                    if x + 1 < w {
                        let east = solution.module_at(x + 1, y, z);
                        assert!(
                            set.allowed[Axis6::PosX as usize][module] & (1 << east) != 0,
                            "illegal +X pair {module}->{east} at ({x},{y},{z})"
                        );
                    }
                    if y + 1 < h {
                        let above = solution.module_at(x, y + 1, z);
                        assert!(
                            set.allowed[Axis6::PosY as usize][module] & (1 << above) != 0,
                            "illegal +Y pair {module}->{above} at ({x},{y},{z})"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn strata_world_solves_into_ground_horizon_sky_columns() {
        let set = strata_set();
        let solution = solve_wfc(&set, (5, 6, 5), 42).expect("strata set is solvable");
        assert_adjacency_holds(&set, &solution);
        for z in 0..5 {
            for x in 0..5 {
                let column: Vec<u8> = (0..6).map(|y| solution.module_at(x, y, z)).collect();
                let horizon_count = column.iter().filter(|&&m| m == HORIZON as u8).count();
                assert!(horizon_count <= 1, "at most one horizon per column");
                let mut sorted = column.clone();
                sorted.sort_unstable();
                assert_eq!(sorted, column, "strata must be ordered bottom-up");
            }
        }
    }

    #[test]
    fn solutions_are_deterministic_per_seed_and_vary_across_seeds() {
        let set = strata_set();
        let a = solve_wfc(&set, (4, 5, 4), 7).unwrap().modules;
        let b = solve_wfc(&set, (4, 5, 4), 7).unwrap().modules;
        assert_eq!(a, b);
        let c = solve_wfc(&set, (4, 5, 4), 8).unwrap().modules;
        assert_ne!(a, c, "different seeds should find different strata");
    }

    #[test]
    fn impossible_neighborhoods_report_a_contradiction() {
        // One module that tolerates no lateral neighbor at all: any grid
        // wider than one cell is unsolvable.
        let mut set = WfcModuleSet::new(vec![1.0]);
        set.allow(0, Axis6::PosY, 0);
        let result = solve_wfc(&set, (2, 1, 1), 1);
        assert!(matches!(result, Err(WfcContradiction { .. })));
    }

    #[test]
    fn weighting_biases_the_draw() {
        // Two freely-mixing modules, one weighted 9:1: the heavy module
        // must dominate a large solve.
        let mut set = WfcModuleSet::new(vec![9.0, 1.0]);
        for dir in Axis6::ALL {
            for a in 0..2 {
                for b in 0..2 {
                    set.allow(a, dir, b);
                }
            }
        }
        let solution = solve_wfc(&set, (8, 8, 8), 3).unwrap();
        let heavy = solution.modules.iter().filter(|&&m| m == 0).count();
        assert!(
            heavy > solution.modules.len() / 2,
            "9:1 weight produced only {heavy}/{} heavy cells",
            solution.modules.len()
        );
    }
}
