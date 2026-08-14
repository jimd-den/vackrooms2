//! The byte layout one surfel takes on its way to WebGL, and the attribute
//! names the vertex shader reads it back through.
//!
//! This is deliberately separate from [`crate::drivers::surfel_webgl`], which
//! is `wasm32`-only and therefore untestable under `cargo test`. The layout is
//! the part that can be silently wrong -- a shifted offset or a flipped byte
//! order yields discs at plausible-looking wrong positions rather than an
//! error -- so it lives where the test runner can reach it, following
//! `generation_worker_requests`.

use crate::adapters::surfel_cloud::PackedSurfel;

/// Bytes per surfel uploaded to WebGL.
///
/// `PackedSurfel` is 12 bytes. The upload pads to 16 to match
/// `GpuPackedSurfel`'s WebGPU footprint, so the two backends describe one
/// record rather than two, and so the stride stays a power of two.
pub const SURFEL_STRIDE: i32 = 16;

/// `aSurfelPos` — disc centre, chunk-local fixed point at 1/1024 u.
/// Three `UNSIGNED_SHORT`, unnormalized: the shader applies the scale.
pub const ATTRIB_POS: &str = "aSurfelPos";
pub const OFFSET_POS: i32 = 0;

/// `aSurfelMeta` — radius, normal_axis, material, baked_light.
/// Four `UNSIGNED_BYTE`, unnormalized: radius decodes by `RADIUS_QUANT` and
/// the other three are indices, none of which survive a 0..1 remap.
pub const ATTRIB_META: &str = "aSurfelMeta";
pub const OFFSET_META: i32 = 6;

/// `aSurfelFlags` — ao, flags (bit 0: emissive). Two `UNSIGNED_BYTE`.
pub const ATTRIB_FLAGS: &str = "aSurfelFlags";
pub const OFFSET_FLAGS: i32 = 10;

/// Serializes a cloud explicitly, so the GPU layout is stable across Rust
/// compiler versions and host alignment rules. `pack_instances` in the splat
/// driver does the same thing for the same reason.
pub fn pack_surfels(surfels: &[PackedSurfel]) -> Vec<u8> {
    let mut packed = Vec::with_capacity(surfels.len() * SURFEL_STRIDE as usize);
    for surfel in surfels {
        packed.extend_from_slice(&surfel.position[0].to_le_bytes());
        packed.extend_from_slice(&surfel.position[1].to_le_bytes());
        packed.extend_from_slice(&surfel.position[2].to_le_bytes());
        packed.push(surfel.radius);
        packed.push(surfel.normal_axis);
        packed.push(surfel.material);
        packed.push(surfel.baked_light);
        packed.push(surfel.ao);
        packed.push(surfel.flags);
        packed.extend_from_slice(&[0, 0, 0, 0]);
    }
    packed
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> PackedSurfel {
        PackedSurfel {
            position: [0x0102, 0x0304, 0x0506],
            radius: 23,
            normal_axis: 4,
            material: 7,
            baked_light: 11,
            ao: 1,
            flags: 1,
        }
    }

    /// Every attribute must land where its offset says it does. A shifted
    /// offset draws discs at wrong-but-plausible positions instead of
    /// failing, which is the whole reason this test exists.
    #[test]
    fn each_attribute_reads_the_field_it_names() {
        let bytes = pack_surfels(&[sample()]);
        assert_eq!(bytes.len(), SURFEL_STRIDE as usize);

        let pos = OFFSET_POS as usize;
        assert_eq!(u16::from_le_bytes([bytes[pos], bytes[pos + 1]]), 0x0102);
        assert_eq!(u16::from_le_bytes([bytes[pos + 2], bytes[pos + 3]]), 0x0304);
        assert_eq!(u16::from_le_bytes([bytes[pos + 4], bytes[pos + 5]]), 0x0506);

        let meta = OFFSET_META as usize;
        assert_eq!(&bytes[meta..meta + 4], &[23, 4, 7, 11]);

        let flags = OFFSET_FLAGS as usize;
        assert_eq!(&bytes[flags..flags + 2], &[1, 1]);
    }

    /// The three attributes must not overlap, and must fit inside the stride
    /// they are pointed at with.
    #[test]
    fn the_attributes_tile_the_stride_without_overlapping() {
        assert_eq!(OFFSET_POS + 6, OFFSET_META);
        assert_eq!(OFFSET_META + 4, OFFSET_FLAGS);
        assert!(OFFSET_FLAGS + 2 <= SURFEL_STRIDE);
    }

    /// The padding is padding. If a future field claims those bytes, the
    /// shader has to learn about them in the same commit.
    #[test]
    fn the_tail_of_the_stride_is_zero_padding() {
        let bytes = pack_surfels(&[sample()]);
        assert_eq!(&bytes[12..SURFEL_STRIDE as usize], &[0, 0, 0, 0]);
    }

    /// Stride times count, exactly -- the draw call passes an instance count
    /// and trusts the buffer to be that long.
    #[test]
    fn a_cloud_packs_to_exactly_one_stride_per_surfel() {
        let cloud = vec![sample(); 5];
        assert_eq!(
            pack_surfels(&cloud).len(),
            5 * SURFEL_STRIDE as usize,
            "instance count and buffer length must agree"
        );
        assert!(pack_surfels(&[]).is_empty());
    }
}
