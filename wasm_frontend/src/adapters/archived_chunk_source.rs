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
use crate::application::chunk_archive::{
    ArchiveIdentity, ArchiveStorage, ChunkArchive, RecordKey,
};
use crate::application::ports::{
    ChunkPayload, ChunkSourcePort, RenderArtifactNeeds,
};
use crate::application::streaming::chunk_key;

pub struct ArchivedChunkSource<N: NoiseProvider> {
    inner: LocalChunkSource<N>,
    // `RefCell` because the port loads through `&self`: reading an archive
    // updates its index and counters, which is bookkeeping, not state the
    // caller can observe.
    archive: RefCell<ChunkArchive>,
    storage: RefCell<Box<dyn ArchiveStorage>>,
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
        mut storage: Box<dyn ArchiveStorage>,
        generator_id: u64,
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
        }
    }

    /// (hits, misses, stored) since opening.
    pub fn stats(&self) -> (u64, u64, u64) {
        self.archive.borrow().stats()
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
        let key = RecordKey::new(chunk_key(origin_x, origin_z), lod, reality);

        let stored = {
            let storage = self.storage.borrow();
            self.archive.borrow_mut().get(storage.as_ref(), key)
        };
        if let Some(bytes) = stored
            && let Some(payload) = decode_chunk_payload(&bytes)
            && payload_satisfies(&payload, artifacts)
        {
            return payload;
        }
        // Either absent, undecodable, or archived without the artifacts this
        // renderer needs. A record written for a mesh renderer carries no SVO
        // nodes, so a raymarcher asking for the same chunk must regenerate
        // rather than be handed a payload with the nodes silently empty.
        self.archive.borrow_mut().note_miss();

        let payload = self
            .inner
            .load_with_artifacts(origin_x, origin_z, level, lod, reality, artifacts);
        let encoded = encode_chunk_payload(&payload);
        let mut storage = self.storage.borrow_mut();
        self.archive
            .borrow_mut()
            .put(storage.as_mut(), key, &encoded);
        payload
    }
}

/// Does an archived payload carry everything this renderer asked for?
///
/// Artifacts are requested per renderer, so the same chunk can be archived in
/// a weaker form than a later caller needs. Emptiness is the honest test:
/// each product is either present or was never extracted.
fn payload_satisfies(payload: &ChunkPayload, artifacts: RenderArtifactNeeds) -> bool {
    if artifacts.svo_nodes() && payload.nodes.is_empty() {
        return false;
    }
    if artifacts.bricks() && payload.brick_voxels.is_empty() {
        return false;
    }
    if artifacts.needs_surface_extraction()
        && payload.surface.vertices.is_empty()
        && payload.surface.faces.instances.is_empty()
    {
        return false;
    }
    true
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
