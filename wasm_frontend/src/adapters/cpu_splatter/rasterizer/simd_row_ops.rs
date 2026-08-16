//! Four-wide depth-test primitives for the splat fill loop.
//!
//! Only the *decision* is vectorized. The candidate reciprocal depth is still
//! evaluated one pixel at a time with `mul_add`, because wasm has no FMA at
//! any width and `f32x4_mul` + `f32x4_add` would not reproduce libm `fmaf`'s
//! rounding. Likewise the write budget, the colour store, and the coarse
//! coverage bookkeeping stay scalar, so a splat torn by an exhausted budget
//! tears at exactly the pixel it does today.
//!
//! What is left to vectorize is the part that is pure comparison: load four
//! stored depths, compare four candidates against them, get four verdicts.
//! Both implementations below evaluate the identical per-lane expression, so
//! the host build (scalar) and the browser build (SIMD) agree bit for bit.

/// How many pixels one depth-test batch covers.
pub(super) const LANES: usize = 4;

/// Per-lane verdict of the fine depth test.
///
/// `valid` is separate from the comparison because a lane whose plane
/// evaluation was rejected (non-finite, or behind the camera) must never
/// write — and cannot simply be given a sentinel depth, since a zero sentinel
/// would tie with an uncovered pixel and an emitter's `equal_depth_wins`
/// would then let it through.
pub(super) fn depth_pass_mask(
    stored: [f32; LANES],
    candidate: [f32; LANES],
    valid: [bool; LANES],
    equal_depth_wins: bool,
) -> [bool; LANES] {
    let mut compared = compare_lanes(stored, candidate, equal_depth_wins);
    for lane in 0..LANES {
        compared[lane] &= valid[lane];
    }
    compared
}

#[cfg(all(target_arch = "wasm32", target_feature = "simd128"))]
fn compare_lanes(
    stored: [f32; LANES],
    candidate: [f32; LANES],
    equal_depth_wins: bool,
) -> [bool; LANES] {
    use core::arch::wasm32::{
        f32x4, f32x4_eq, f32x4_gt, i32x4_extract_lane, v128_any_true, v128_or,
    };

    let stored = f32x4(stored[0], stored[1], stored[2], stored[3]);
    let candidate = f32x4(candidate[0], candidate[1], candidate[2], candidate[3]);
    // `f32x4_gt`/`f32x4_eq` are IEEE-754 comparisons, identical to the scalar
    // `>` and `==` below on every lane including NaN (both yield false).
    let nearer = f32x4_gt(candidate, stored);
    let mask = if equal_depth_wins {
        v128_or(nearer, f32x4_eq(candidate, stored))
    } else {
        nearer
    };
    if !v128_any_true(mask) {
        return [false; LANES];
    }
    [
        i32x4_extract_lane::<0>(mask) != 0,
        i32x4_extract_lane::<1>(mask) != 0,
        i32x4_extract_lane::<2>(mask) != 0,
        i32x4_extract_lane::<3>(mask) != 0,
    ]
}

#[cfg(not(all(target_arch = "wasm32", target_feature = "simd128")))]
fn compare_lanes(
    stored: [f32; LANES],
    candidate: [f32; LANES],
    equal_depth_wins: bool,
) -> [bool; LANES] {
    let mut mask = [false; LANES];
    for lane in 0..LANES {
        mask[lane] = candidate[lane] > stored[lane]
            || (equal_depth_wins && candidate[lane] == stored[lane]);
    }
    mask
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The reference the SIMD arm has to reproduce: exactly the expression
    /// the scalar fill loop used before batching existed.
    fn reference(
        stored: [f32; LANES],
        candidate: [f32; LANES],
        valid: [bool; LANES],
        equal_depth_wins: bool,
    ) -> [bool; LANES] {
        let mut mask = [false; LANES];
        for lane in 0..LANES {
            mask[lane] = valid[lane]
                && (candidate[lane] > stored[lane]
                    || (equal_depth_wins && candidate[lane] == stored[lane]));
        }
        mask
    }

    #[test]
    fn matches_the_scalar_reference_across_awkward_lane_patterns() {
        let cases: [([f32; 4], [f32; 4], [bool; 4]); 6] = [
            // Uncovered pixels, an exact coplanar tie, a nearer and a farther.
            ([0.0, 0.5, 0.5, 0.5], [0.25, 0.5, 0.75, 0.25], [true; 4]),
            // Every lane rejected by the plane evaluation.
            ([0.0; 4], [1.0; 4], [false; 4]),
            // Mixed validity, so `valid` cannot be folded into the compare.
            ([0.0; 4], [1.0; 4], [true, false, true, false]),
            // NaN must lose both comparisons in every lane.
            (
                [f32::NAN, 0.5, f32::NAN, 0.5],
                [0.5, f32::NAN, f32::NAN, 0.5],
                [true; 4],
            ),
            // Infinities and signed zero.
            (
                [f32::INFINITY, -0.0, 0.0, f32::NEG_INFINITY],
                [f32::INFINITY, 0.0, -0.0, 1.0],
                [true; 4],
            ),
            // Adjacent floats, where an approximate compare would diverge.
            (
                [1.0, 1.0, 1.0 + f32::EPSILON, 1.0],
                [1.0 + f32::EPSILON, 1.0 - f32::EPSILON, 1.0, 1.0],
                [true; 4],
            ),
        ];

        for (stored, candidate, valid) in cases {
            for equal_depth_wins in [false, true] {
                assert_eq!(
                    depth_pass_mask(stored, candidate, valid, equal_depth_wins),
                    reference(stored, candidate, valid, equal_depth_wins),
                    "stored={stored:?} candidate={candidate:?} valid={valid:?} \
                     equal_depth_wins={equal_depth_wins}"
                );
            }
        }
    }

    #[test]
    fn an_emitter_wins_only_an_exact_tie() {
        let stored = [0.5; LANES];
        let candidate = [0.5, 0.5 + f32::EPSILON, 0.5 - f32::EPSILON, 0.5];
        assert_eq!(
            depth_pass_mask(stored, candidate, [true; LANES], false),
            [false, true, false, false]
        );
        assert_eq!(
            depth_pass_mask(stored, candidate, [true; LANES], true),
            [true, true, false, true]
        );
    }
}
