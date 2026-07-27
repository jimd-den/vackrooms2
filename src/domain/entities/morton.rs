//! 3D Morton (Z-order) curve keys.
//!
//! Interleaving the bits of (x, y, z) into one 64-bit key makes points that
//! are close in 3D space land close in 1D memory, so arrays sorted by Morton
//! key stay cache-coherent under spatial traversal. Coordinates up to 21 bits
//! each (0..=0x1F_FFFF) fit one `u64` exactly.

/// Highest coordinate representable in a 64-bit 3D Morton key.
pub const MORTON_MAX_COORD: u32 = (1 << 21) - 1;

/// Spreads the low 21 bits of `v` so consecutive input bits land three
/// output bits apart (the classic magic-mask expansion).
const fn expand_bits(mut v: u64) -> u64 {
    v &= 0x001f_ffff;
    v = (v | (v << 32)) & 0x001f_0000_0000_ffff;
    v = (v | (v << 16)) & 0x001f_0000_ff00_00ff;
    v = (v | (v << 8)) & 0x100f_00f0_0f00_f00f;
    v = (v | (v << 4)) & 0x10c3_0c30_c30c_30c3;
    v = (v | (v << 2)) & 0x1249_2492_4924_9249;
    v
}

/// Reverses [`expand_bits`]: collects every third bit back into the low 21.
const fn compact_bits(mut v: u64) -> u64 {
    v &= 0x1249_2492_4924_9249;
    v = (v | (v >> 2)) & 0x10c3_0c30_c30c_30c3;
    v = (v | (v >> 4)) & 0x100f_00f0_0f00_f00f;
    v = (v | (v >> 8)) & 0x001f_0000_ff00_00ff;
    v = (v | (v >> 16)) & 0x001f_0000_0000_ffff;
    v = (v | (v >> 32)) & 0x001f_ffff;
    v
}

/// Interleaves (x, y, z) into a 64-bit Morton key. Coordinates above
/// [`MORTON_MAX_COORD`] are masked to their low 21 bits.
pub const fn morton_encode_3d(x: u32, y: u32, z: u32) -> u64 {
    expand_bits(x as u64) | (expand_bits(y as u64) << 1) | (expand_bits(z as u64) << 2)
}

/// Recovers (x, y, z) from a Morton key produced by [`morton_encode_3d`].
pub const fn morton_decode_3d(key: u64) -> (u32, u32, u32) {
    (
        compact_bits(key) as u32,
        compact_bits(key >> 1) as u32,
        compact_bits(key >> 2) as u32,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_decode_round_trips_across_the_full_coordinate_range() {
        for &c in &[0, 1, 2, 7, 255, 1023, 0x1_0000, MORTON_MAX_COORD] {
            for &(x, y, z) in &[(c, 0, 0), (0, c, 0), (0, 0, c), (c, c / 2, c / 3)] {
                assert_eq!(morton_decode_3d(morton_encode_3d(x, y, z)), (x, y, z));
            }
        }
    }

    #[test]
    fn axis_bits_interleave_in_xyz_order() {
        // x owns bit 0, y bit 1, z bit 2 of every triple.
        assert_eq!(morton_encode_3d(1, 0, 0), 0b001);
        assert_eq!(morton_encode_3d(0, 1, 0), 0b010);
        assert_eq!(morton_encode_3d(0, 0, 1), 0b100);
        assert_eq!(morton_encode_3d(3, 0, 0), 0b001001);
    }

    #[test]
    fn keys_of_one_octant_stay_contiguous() {
        // Every point of the 2x2x2 block at origin sorts before any point of
        // the next block along x — the property that makes Morton order a
        // depth-first octree walk.
        let inside_max = (0..8)
            .map(|i| morton_encode_3d(i & 1, (i >> 1) & 1, (i >> 2) & 1))
            .max()
            .unwrap();
        assert!(inside_max < morton_encode_3d(2, 0, 0));
    }
}
