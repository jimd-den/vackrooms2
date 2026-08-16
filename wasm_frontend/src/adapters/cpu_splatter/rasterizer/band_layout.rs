//! Split the framebuffer into horizontal bands that can be rendered apart.
//!
//! A band is the unit of parallelism for the threaded renderer: one worker
//! owns one band and runs both traversal and raster for it. Splitting the
//! *screen* rather than the *scene* is what makes the split work without
//! shared memory — two bands never touch the same pixel, so they need no
//! synchronisation and no depth-buffer merge.
//!
//! Boundaries are quantised to [`COARSE_TILE`] rows. The hierarchical-Z
//! summary stores one entry per 8x8 tile and only writes it once a tile is
//! fully covered; a tile straddling two bands would be counted twice, each
//! band seeing only its own half as "fully covered". Quantising removes that
//! case rather than special-casing it.

use super::write_depth_tested_splats::COARSE_TILE;

/// One horizontal slice of the framebuffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct BandLayout {
    /// First absolute framebuffer row this band owns.
    pub(super) row_offset: usize,
    /// Row count this band owns. A multiple of [`COARSE_TILE`] except for the
    /// last band, which absorbs a framebuffer height that is not a multiple.
    pub(super) height: usize,
}

impl BandLayout {
    /// Absolute row just past this band.
    pub(super) fn row_end(self) -> usize {
        self.row_offset + self.height
    }
}

/// Partitions `total_height` rows into at most `band_count` bands.
///
/// Returns fewer bands than requested when the framebuffer has fewer coarse
/// tile rows than bands — an empty band would be a worker with nothing to do
/// and a zero-sized `OffscreenCanvas`. One band always reproduces the
/// unpartitioned target exactly.
pub(super) fn partition_bands(total_height: usize, band_count: usize) -> Vec<BandLayout> {
    let total_height = total_height.max(1);
    let total_tiles = total_height.div_ceil(COARSE_TILE);
    let bands = band_count.clamp(1, total_tiles);

    // Spread the remainder over the first bands rather than dumping it on
    // one, so band workloads stay within one tile row of each other.
    let base_tiles = total_tiles / bands;
    let extra_tiles = total_tiles % bands;

    let mut layouts = Vec::with_capacity(bands);
    let mut row_offset = 0;
    for band in 0..bands {
        let tiles = base_tiles + usize::from(band < extra_tiles);
        let height = (tiles * COARSE_TILE).min(total_height - row_offset);
        layouts.push(BandLayout { row_offset, height });
        row_offset += height;
    }
    debug_assert_eq!(row_offset, total_height, "bands must tile the framebuffer");
    layouts
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_tiles_the_framebuffer(layouts: &[BandLayout], total_height: usize) {
        assert!(!layouts.is_empty(), "at least one band is always produced");
        assert_eq!(layouts[0].row_offset, 0);
        for pair in layouts.windows(2) {
            assert_eq!(
                pair[0].row_end(),
                pair[1].row_offset,
                "bands must be contiguous with no gap or overlap"
            );
        }
        assert_eq!(layouts.last().unwrap().row_end(), total_height);
        assert!(layouts.iter().all(|band| band.height > 0));
    }

    #[test]
    fn one_band_covers_the_whole_framebuffer() {
        for height in [1, 7, 8, 9, 64, 1080] {
            let layouts = partition_bands(height, 1);
            assert_eq!(
                layouts,
                vec![BandLayout {
                    row_offset: 0,
                    height
                }]
            );
        }
    }

    #[test]
    fn every_boundary_but_the_last_falls_on_a_coarse_tile() {
        for height in [8, 9, 63, 64, 65, 100, 1080] {
            for band_count in 1..=9 {
                let layouts = partition_bands(height, band_count);
                assert_tiles_the_framebuffer(&layouts, height);
                for band in &layouts {
                    assert_eq!(
                        band.row_offset % COARSE_TILE,
                        0,
                        "band start must not split a coarse tile (h={height}, n={band_count})"
                    );
                }
                for band in &layouts[..layouts.len() - 1] {
                    assert_eq!(
                        band.height % COARSE_TILE,
                        0,
                        "only the last band may hold a partial tile (h={height}, n={band_count})"
                    );
                }
            }
        }
    }

    #[test]
    fn band_count_is_capped_by_the_available_tile_rows() {
        // 16 rows is two coarse tiles, so a 4-worker pool gets two bands.
        let layouts = partition_bands(16, 4);
        assert_eq!(layouts.len(), 2);
        assert_tiles_the_framebuffer(&layouts, 16);

        // Fewer rows than one tile collapses to a single band.
        assert_eq!(partition_bands(5, 8).len(), 1);
    }

    #[test]
    fn heights_stay_within_one_tile_row_of_each_other() {
        let layouts = partition_bands(1080, 4);
        assert_tiles_the_framebuffer(&layouts, 1080);
        let min = layouts.iter().map(|band| band.height).min().unwrap();
        let max = layouts.iter().map(|band| band.height).max().unwrap();
        assert!(max - min <= COARSE_TILE, "got {min}..={max}");
    }

    #[test]
    fn a_zero_height_target_still_yields_one_band() {
        assert_eq!(
            partition_bands(0, 4),
            vec![BandLayout {
                row_offset: 0,
                height: 1
            }]
        );
    }
}
