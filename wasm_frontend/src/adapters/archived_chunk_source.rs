//! A [`ChunkSourcePort`] that reads from a [`ChunkArchive`] first and
//! generates only what the archive does not have — writing back whatever it
//! had to generate.
//!
//! The measurement this exists for, on one 10 u chunk:
//!
//! ```text
//!            generate    decode from archive
//! lod 0      42.0 ms     0.46 ms      (92x)
//! lod 2       6.5 ms     0.04 ms     (150x)
//! ```
//!
//! Two orders of magnitude changes what streaming *is*. Generation-bound, the
//! frontier races the player and the LOD ladder has to be sized against how
//! much can be produced per tick. Archive-bound, a chunk costs less than a
//! frame's slack, and revisiting somewhere is free — so a player can turn
//! around, backtrack, or circle a floor they have already walked without the
//! engine doing any architectural work at all.
//!
//! What it does *not* buy is residency. A square mile is 25600 chunks:
//! 8.8 GB archived at full resolution, 0.9 GB at lod 2. The archive removes
//! the cost of *producing* geometry, never the cost of *holding* it — that
//! is still the LOD ladder's job.
//!
//! Writes are best-effort throughout. A full disk or an exceeded browser
//! quota makes a session slower, never wrong: every miss is generated, and a
//! failed append is dropped.

use std::cell::RefCell;

use vackrooms::domain::entities::anomaly::RealitySnapshot;
use vackrooms::use_cases::generate_chunk::GeneratorConfig;
use vackrooms::use_cases::ports::NoiseProvider;

use crate::adapters::chunk_codec::{decode_chunk_payload, encode_chunk_payload};
use crate::adapters::local_chunk_source::LocalChunkSource;
use crate::adapters::surface_mesh::SurfelDensity;
use crate::application::chunk_archive::{
    ArchiveIdentity, ArchiveStorage, ChunkArchive, RecordKey,
};
use crate::application::ports::{
    ChunkPayload, ChunkSourcePort, RenderArtifactNeeds,
};
use crate::application::streaming::chunk_key;

/// Default ceiling on one archive, bytes.
///
/// 64 MB holds roughly 500 full-resolution 10 u chunks or 2000 at the
/// ladder's mid rung — far more than the rolling window ever has resident, so
/// a player pacing an area never sees a clear. A worker pool multiplies this,
/// which is why the pool shards the world rather than each worker archiving
/// all of it.
pub const DEFAULT_ARCHIVE_BYTES: u64 = 64 * 1024 * 1024;

pub struct ArchivedChunkSource<N: NoiseProvider> {
    inner: LocalChunkSource<N>,
    // `RefCell` because the port loads through `&self`: reading an archive
    // updates its index and counters, which is bookkeeping, not state the
    // caller can observe.
    archive: RefCell<ChunkArchive>,
    storage: RefCell<Box<dyn ArchiveStorage>>,
    max_bytes: u64,
    clears: RefCell<u64>,
}

impl<N: NoiseProvider> ArchivedChunkSource<N> {
    /// Wraps a generating source with an archive over `storage`.
    ///
    /// `generator_id` must change whenever generated geometry would — a
    /// build hash or source digest. Getting this wrong is the one failure
    /// mode that matters: an archive keyed to a generator it does not match
    /// serves a world that is half disk and half code.
    pub fn new(
        noise: N,
        seed: u32,
        config: GeneratorConfig,
        storage: Box<dyn ArchiveStorage>,
        generator_id: u64,
    ) -> Self {
        Self::with_budget(
            noise,
            seed,
            config,
            storage,
            generator_id,
            DEFAULT_ARCHIVE_BYTES,
        )
    }

    /// As [`new`](Self::new), with an explicit ceiling on the archive.
    pub fn with_budget(
        noise: N,
        seed: u32,
        config: GeneratorConfig,
        mut storage: Box<dyn ArchiveStorage>,
        generator_id: u64,
        max_bytes: u64,
    ) -> Self {
        let identity = ArchiveIdentity::new(
            seed,
            config.level,
            config.chunk_size,
            config.voxel_scale,
            generator_id,
        );
        let archive = ChunkArchive::open(storage.as_mut(), identity);
        Self {
            inner: LocalChunkSource::new(noise, seed, config),
            archive: RefCell::new(archive),
            storage: RefCell::new(storage),
            max_bytes,
            clears: RefCell::new(0),
        }
    }

    /// Sets how finely the surfel artifact samples the surface. The archive
    /// needs no invalidation for this: `generator_id` already folds in the
    /// query string that carries the density.
    pub fn with_surfel_density(mut self, density: SurfelDensity) -> Self {
        self.inner = self.inner.with_surfel_density(density);
        self
    }

    /// How many times the archive has been emptied to stay inside its budget.
    /// A session that keeps climbing is thrashing and wants a larger budget.
    pub fn clears(&self) -> u64 {
        *self.clears.borrow()
    }

    /// (hits, misses, stored) since opening.
    pub fn stats(&self) -> (u64, u64, u64) {
        self.archive.borrow().stats()
    }

    /// Records rejected by their checksum. Non-zero means the storage under
    /// this archive is damaging data.
    pub fn corrupt_records(&self) -> u64 {
        self.archive.borrow().corrupt_records()
    }

    /// Chunks currently indexed.
    pub fn archived_chunks(&self) -> usize {
        self.archive.borrow().len()
    }

    fn fetch(
        &self,
        origin_x: f32,
        origin_z: f32,
        level: u32,
        lod: u8,
        reality: &RealitySnapshot,
        artifacts: RenderArtifactNeeds,
    ) -> ChunkPayload {
        let key = RecordKey::new(
            chunk_key(origin_x, origin_z),
            lod,
            artifacts.bits(),
            reality,
        );

        let stored = {
            let storage = self.storage.borrow();
            self.archive.borrow_mut().get(storage.as_ref(), key)
        };
        if let Some(bytes) = stored
            && let Some(payload) = decode_chunk_payload(&bytes)
        {
            return payload;
        }
        // Absent, corrupt, or archived for a different renderer — the
        // artifact bits are part of the key, so a record built for the splat
        // renderer is simply not found by a mesh renderer rather than being
        // handed over with the wrong products.
        self.archive.borrow_mut().note_miss();

        let payload = self
            .inner
            .load_with_artifacts(origin_x, origin_z, level, lod, reality, artifacts);
        let encoded = encode_chunk_payload(&payload);
        let mut storage = self.storage.borrow_mut();
        let mut archive = self.archive.borrow_mut();
        // Bound the log before growing it. An append-only file cannot free
        // one record, so staying inside a budget means emptying it — see
        // `ChunkArchive::clear` for why that trade is the right one here.
        if archive.bytes() + encoded.len() as u64 > self.max_bytes {
            archive.clear(storage.as_mut());
            *self.clears.borrow_mut() += 1;
        }
        archive.put(storage.as_mut(), key, &encoded);
        drop(archive);
        payload
    }
}

impl<N: NoiseProvider> ChunkSourcePort for ArchivedChunkSource<N> {
    fn load(&self, origin_x: f32, origin_z: f32, level: u32, lod: u8) -> ChunkPayload {
        self.fetch(
            origin_x,
            origin_z,
            level,
            lod,
            &RealitySnapshot::default(),
            RenderArtifactNeeds::ALL,
        )
    }

    fn load_with_reality(
        &self,
        origin_x: f32,
        origin_z: f32,
        level: u32,
        lod: u8,
        reality: &RealitySnapshot,
    ) -> ChunkPayload {
        self.fetch(
            origin_x,
            origin_z,
            level,
            lod,
            reality,
            RenderArtifactNeeds::ALL,
        )
    }

    fn load_with_artifacts(
        &self,
        origin_x: f32,
        origin_z: f32,
        level: u32,
        lod: u8,
        reality: &RealitySnapshot,
        artifacts: RenderArtifactNeeds,
    ) -> ChunkPayload {
        self.fetch(origin_x, origin_z, level, lod, reality, artifacts)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::application::chunk_archive::MemoryStorage;
    use std::time::Instant;
    use vackrooms::frameworks_drivers::simple_noise::SimpleNoiseProvider;

    fn source(storage: Box<dyn ArchiveStorage>) -> ArchivedChunkSource<SimpleNoiseProvider> {
        ArchivedChunkSource::new(
            SimpleNoiseProvider::new(),
            42,
            GeneratorConfig::low_spec(),
            storage,
            1,
        )
    }

    #[test]
    fn an_archived_chunk_is_identical_to_the_generated_one() {
        // The load-bearing property. An archive that returns *nearly* the
        // right geometry is worse than no archive: the world would differ
        // depending on whether you had been somewhere before.
        let direct = LocalChunkSource::new(
            SimpleNoiseProvider::new(),
            42,
            GeneratorConfig::low_spec(),
        );
        let archived = source(Box::new(MemoryStorage::new()));
        for (ox, oz) in [(0.0, 30.0), (10.0, 10.0), (-40.0, 80.0)] {
            let expected = direct.load_with_artifacts(
                ox,
                oz,
                0,
                0,
                &RealitySnapshot::default(),
                RenderArtifactNeeds::SURFACE,
            );
            // First call generates and writes; second reads back.
            for pass in 0..2 {
                let got = archived.load_with_artifacts(
                    ox,
                    oz,
                    0,
                    0,
                    &RealitySnapshot::default(),
                    RenderArtifactNeeds::SURFACE,
                );
                assert_eq!(got.collision, expected.collision, "pass {pass} at ({ox},{oz})");
                assert_eq!(
                    got.surface.vertices.len(),
                    expected.surface.vertices.len(),
                    "pass {pass} at ({ox},{oz})"
                );
                assert_eq!(got.lights.len(), expected.lights.len(), "pass {pass}");
            }
        }
        let (hits, _, stored) = archived.stats();
        assert_eq!(stored, 3, "each chunk should be written exactly once");
        assert_eq!(hits, 3, "the second pass should have been served from disk");
    }

    #[test]
    fn revisiting_is_dramatically_cheaper_than_generating() {
        // The number the whole design rests on. Deliberately a loose bound —
        // this asserts an order of magnitude, not a benchmark, so it does not
        // become flaky on a loaded machine.
        let archived = source(Box::new(MemoryStorage::new()));
        let load = |ox: f32| {
            archived.load_with_artifacts(
                ox,
                0.0,
                0,
                0,
                &RealitySnapshot::default(),
                RenderArtifactNeeds::SURFACE,
            )
        };
        let cold = Instant::now();
        for i in 0..4 {
            load(i as f32 * 10.0);
        }
        let cold = cold.elapsed();

        let warm = Instant::now();
        for i in 0..4 {
            load(i as f32 * 10.0);
        }
        let warm = warm.elapsed();

        assert!(
            warm * 5 < cold,
            "archived reads ({warm:?}) were not much faster than generation ({cold:?})"
        );
    }

    #[test]
    fn a_second_session_generates_nothing_it_already_has() {
        // Persistence end to end, over a real file: the bytes outlive the
        // source that wrote them, which is the whole difference between an
        // archive and a cache.
        use crate::drivers::file_archive::FileStorage;
        let mut path = std::env::temp_dir();
        path.push(format!("vackrooms-archived-source-{}", std::process::id()));
        let _ = std::fs::remove_file(&path);

        let origins = [0.0f32, 10.0, 20.0, 30.0];
        let first = source(Box::new(FileStorage::open(&path)));
        for ox in origins {
            first.load(ox, 0.0, 0, 0);
        }
        assert_eq!(first.stats().2, 4, "first session should have written 4");
        drop(first);

        let second = source(Box::new(FileStorage::open(&path)));
        assert_eq!(
            second.archived_chunks(),
            4,
            "a new session did not find the previous session's chunks"
        );
        for ox in origins {
            second.load(ox, 0.0, 0, 0);
        }
        let (hits, _, stored) = second.stats();
        assert_eq!(hits, 4, "the second session re-read all four");
        assert_eq!(stored, 0, "the second session generated something it had");

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_chunk_that_leaves_the_window_and_returns_is_not_regenerated() {
        // The rolling window: the engine evicts whatever the player cannot
        // see and reloads it on return. Without an archive that return costs
        // a full generation, so turning around in a corridor rebuilds the
        // corridor. With one it is a decode.
        let archived = source(Box::new(MemoryStorage::new()));
        let reality = RealitySnapshot::default();
        let mut visit = |ox: f32| {
            archived.load_with_artifacts(ox, 0.0, 0, 1, &reality, RenderArtifactNeeds::SURFACE)
        };

        // Walk out along a row...
        for i in 0..6 {
            visit(i as f32 * 10.0);
        }
        let (_, _, stored_after_walk) = archived.stats();
        assert_eq!(stored_after_walk, 6);

        // ...and walk back over the same ground, as an evicted window would.
        for i in (0..6).rev() {
            visit(i as f32 * 10.0);
        }
        let (hits, _, stored) = archived.stats();
        assert_eq!(
            stored, 6,
            "walking back over known ground generated {} new chunks",
            stored - 6
        );
        assert_eq!(hits, 6, "the return trip was not served from the archive");
    }

    #[test]
    fn the_archive_stays_inside_its_budget() {
        // An append-only log grows forever unless something bounds it. A
        // worker that archived every chunk of an hour-long session would end
        // the session as the reason the tab died.
        let archived = ArchivedChunkSource::with_budget(
            SimpleNoiseProvider::new(),
            42,
            GeneratorConfig::low_spec(),
            Box::new(MemoryStorage::new()),
            1,
            256 * 1024,
        );
        let reality = RealitySnapshot::default();
        for i in 0..12 {
            archived.load_with_artifacts(
                i as f32 * 10.0,
                0.0,
                0,
                1,
                &reality,
                RenderArtifactNeeds::SURFACE,
            );
        }
        assert!(
            archived.clears() > 0,
            "a 256 KB budget was never enforced across 12 chunks"
        );
        assert!(
            archived.storage.borrow().len() <= 256 * 1024 + (128 * 1024),
            "the archive overran its budget by more than one record"
        );
    }

    #[test]
    fn a_splat_record_is_never_served_to_a_mesh_renderer() {
        // Both renderers "need surface extraction", but one wants indexed
        // vertices and the other face instances. Serving one's record to the
        // other draws nothing at all — and reports nothing, which is why this
        // has to be a key, not an inference from which fields are non-empty.
        let archived = source(Box::new(MemoryStorage::new()));
        let reality = RealitySnapshot::default();

        let splat =
            archived.load_with_artifacts(0.0, 30.0, 0, 0, &reality, RenderArtifactNeeds::SPLAT);
        assert!(!splat.surface.faces.instances.is_empty());
        assert!(splat.surface.vertices.is_empty());

        let mesh =
            archived.load_with_artifacts(0.0, 30.0, 0, 0, &reality, RenderArtifactNeeds::SURFACE);
        assert!(
            !mesh.surface.vertices.is_empty(),
            "the mesh renderer was handed a splat record and would draw nothing"
        );
    }

    #[test]
    fn every_renderer_gets_its_own_products_back_on_a_revisit() {
        // Each renderer's records must round-trip independently, so a session
        // that switches renderer does not poison the archive for the other.
        let archived = source(Box::new(MemoryStorage::new()));
        let reality = RealitySnapshot::default();
        for artifacts in [
            RenderArtifactNeeds::SURFACE,
            RenderArtifactNeeds::SPLAT,
            RenderArtifactNeeds::RAYMARCH,
        ] {
            let first = archived.load_with_artifacts(0.0, 30.0, 0, 0, &reality, artifacts);
            let second = archived.load_with_artifacts(0.0, 30.0, 0, 0, &reality, artifacts);
            assert_eq!(
                first.surface.vertices.len(),
                second.surface.vertices.len(),
                "{artifacts:?} vertices changed on revisit"
            );
            assert_eq!(
                first.surface.faces.instances.len(),
                second.surface.faces.instances.len(),
                "{artifacts:?} face instances changed on revisit"
            );
            assert_eq!(
                first.nodes.len(),
                second.nodes.len(),
                "{artifacts:?} svo nodes changed on revisit"
            );
            assert_eq!(first.collision, second.collision, "{artifacts:?} collision");
        }
    }

    #[test]
    fn a_renderer_needing_more_artifacts_regenerates() {
        // A chunk archived for a mesh renderer has no SVO nodes. Handing it
        // to the raymarcher would render nothing and look like a bug in the
        // shader.
        let archived = source(Box::new(MemoryStorage::new()));
        let reality = RealitySnapshot::default();
        let surface =
            archived.load_with_artifacts(0.0, 30.0, 0, 0, &reality, RenderArtifactNeeds::SURFACE);
        assert!(surface.nodes.is_empty());

        let raymarch =
            archived.load_with_artifacts(0.0, 30.0, 0, 0, &reality, RenderArtifactNeeds::RAYMARCH);
        assert!(
            !raymarch.nodes.is_empty(),
            "a weaker archived record was served to a renderer that needs nodes"
        );
    }
}
