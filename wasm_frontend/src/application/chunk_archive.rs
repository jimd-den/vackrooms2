//! An append-only archive of generated chunks, and the index over it.
//!
//! Generating a chunk costs 42 ms at full resolution; decoding one from
//! bytes costs 0.46 ms. Two orders of magnitude. Everything here follows
//! from that single measurement: once a chunk has been generated, spending
//! 42 ms to produce it a second time is pure waste, and a store that turns
//! revisits into 0.46 ms reads lets the streaming frontier move faster than
//! a player can walk instead of racing them.
//!
//! Append-only, deliberately. Updating a record in place means holes, a free
//! list, and a file that can be corrupted by a crash halfway through a write.
//! Appending means the only mutation is "make the file longer", the newest
//! record for a key wins, and a torn tail costs exactly the one record that
//! was being written — [`ChunkArchive::open`] stops at the first incomplete
//! record rather than trusting a length it cannot verify.
//!
//! # Staleness is the whole risk
//!
//! A cache of generated geometry is only correct while the thing that
//! generated it is unchanged. Serving a chunk built by last week's generator
//! is far worse than regenerating it: the world silently disagrees with
//! itself, half from disk and half from code, and nothing reports an error.
//!
//! So identity is checked at two levels and neither is optional:
//!
//! * The **file** carries an [`ArchiveIdentity`] — format version, seed,
//!   level, chunk size, voxel scale, and a generator build id. Any mismatch
//!   discards the entire archive rather than trying to salvage it.
//! * Each **record** carries its LOD and a hash of the [`RealitySnapshot`]
//!   it was generated against, because Peripheral Shift makes the same
//!   coordinates a different world at a different drift epoch.
//!
//! The storage itself is a port: a real file natively, an OPFS handle in the
//! browser. This module never names either, so it is unit-tested natively at
//! full speed against a vector of bytes.

use std::collections::HashMap;

use crate::application::streaming::ChunkKey;
use vackrooms::domain::entities::anomaly::RealitySnapshot;

/// Magic at the head of the file, and at the head of every record.
///
/// Repeating it per record is what makes a torn tail detectable: a record
/// that does not begin with this is not a record, whatever the previous
/// length field claimed.
const FILE_MAGIC: u32 = 0x5643_4B41; // "VCKA"
const RECORD_MAGIC: u32 = 0x5643_4B52; // "VCKR"

/// Bumped whenever the record framing changes.
///
/// This is *not* a substitute for [`ArchiveIdentity::generator_id`]: the
/// framing can be stable while the geometry it frames changes completely.
const ARCHIVE_FORMAT_VERSION: u32 = 2;

pub const HEADER_BYTES: usize = 32;
/// magic + chunk x/z + lod + artifacts + padding + reality hash + checksum
/// + payload length.
const RECORD_HEADER_BYTES: usize = 4 + 8 + 8 + 1 + 1 + 2 + 8 + 8 + 4;

/// What an archive was built by. Any difference invalidates every record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ArchiveIdentity {
    pub seed: u32,
    pub level: u32,
    /// Bit patterns, not floats: this is an equality check, and `NaN != NaN`
    /// would make an archive silently never match itself.
    pub chunk_size_bits: u32,
    pub voxel_scale_bits: u32,
    /// Identity of the code and parameters that generate geometry.
    ///
    /// Always folded together with [`build_id`] by [`new`](Self::new), so a
    /// caller cannot forget to invalidate archives when the generator itself
    /// changes. That mistake is not recoverable at runtime: the archive would
    /// keep serving last build's geometry, and nothing downstream can tell
    /// that a wall is in the wrong place.
    pub generator_id: u64,
}

/// Identity of this build, folded into every [`ArchiveIdentity`].
///
/// `VACKROOMS_BUILD_ID` is set by `scripts/build-wasm.mjs` to the same source
/// digest that stamps `static/pkg/.source-sha256`, so any change to generator
/// source produces a different id and orphans every archive written by the
/// previous build.
///
/// Without it — a plain `cargo build`, or an editor running tests — this
/// falls back to the crate version, which does *not* change per build. In
/// that case a persistent archive can outlive a code change, so
/// [`is_build_id_pinned`] reports whether the guarantee is real and callers
/// that persist across builds should refuse to when it is not.
pub fn build_id() -> u64 {
    fnv64(
        option_env!("VACKROOMS_BUILD_ID")
            .unwrap_or(concat!("unpinned-", env!("CARGO_PKG_VERSION")))
            .as_bytes(),
    )
}

/// Whether this build carries a real source digest (see [`build_id`]).
pub fn is_build_id_pinned() -> bool {
    option_env!("VACKROOMS_BUILD_ID").is_some()
}

impl ArchiveIdentity {
    pub fn new(seed: u32, level: u32, chunk_size: f32, voxel_scale: f32, generator_id: u64) -> Self {
        Self {
            seed,
            level,
            chunk_size_bits: chunk_size.to_bits(),
            voxel_scale_bits: voxel_scale.to_bits(),
            // Mixed, not stored raw: the caller's id describes the world's
            // parameters, `build_id` describes the code that reads them, and
            // an archive is only valid when both match.
            generator_id: generator_id ^ build_id().rotate_left(17),
        }
    }

    fn to_bytes(self) -> [u8; HEADER_BYTES] {
        let mut out = [0u8; HEADER_BYTES];
        out[0..4].copy_from_slice(&FILE_MAGIC.to_le_bytes());
        out[4..8].copy_from_slice(&ARCHIVE_FORMAT_VERSION.to_le_bytes());
        out[8..12].copy_from_slice(&self.seed.to_le_bytes());
        out[12..16].copy_from_slice(&self.level.to_le_bytes());
        out[16..20].copy_from_slice(&self.chunk_size_bits.to_le_bytes());
        out[20..24].copy_from_slice(&self.voxel_scale_bits.to_le_bytes());
        out[24..32].copy_from_slice(&self.generator_id.to_le_bytes());
        out
    }

    fn from_bytes(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < HEADER_BYTES
            || u32::from_le_bytes(bytes[0..4].try_into().ok()?) != FILE_MAGIC
            || u32::from_le_bytes(bytes[4..8].try_into().ok()?) != ARCHIVE_FORMAT_VERSION
        {
            return None;
        }
        Some(Self {
            seed: u32::from_le_bytes(bytes[8..12].try_into().ok()?),
            level: u32::from_le_bytes(bytes[12..16].try_into().ok()?),
            chunk_size_bits: u32::from_le_bytes(bytes[16..20].try_into().ok()?),
            voxel_scale_bits: u32::from_le_bytes(bytes[20..24].try_into().ok()?),
            generator_id: u64::from_le_bytes(bytes[24..32].try_into().ok()?),
        })
    }
}

/// Which chunk, at which detail, built for which renderer, in which epoch of
/// the world.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RecordKey {
    pub chunk: ChunkKey,
    pub lod: u8,
    /// The artifact bits the record was generated for.
    ///
    /// Part of the key, not something inferred from the payload afterwards.
    /// A record built for the splat renderer holds face instances and no
    /// vertices; one built for the mesh renderer holds the reverse. Both
    /// answer `needs_surface_extraction()`, so any attempt to recognise them
    /// by which fields came back non-empty gets it wrong in one direction or
    /// the other — and the failure is silent, because a renderer handed the
    /// wrong products draws nothing rather than reporting an error.
    pub artifacts: u8,
    /// Hash of the [`RealitySnapshot`] the chunk was generated against.
    /// Without this, a chunk cached before a Peripheral Shift would be
    /// served after it, and the world would not drift where the player had
    /// already been — which is precisely where drift is supposed to happen.
    pub reality: u64,
}

impl RecordKey {
    pub fn new(chunk: ChunkKey, lod: u8, artifacts: u8, reality: &RealitySnapshot) -> Self {
        Self {
            chunk,
            lod,
            artifacts,
            reality: hash_reality(reality),
        }
    }
}

/// FNV-1a over bytes. Used for the payload checksum and the reality hash.
fn fnv64(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &byte in bytes {
        h ^= byte as u64;
        h = h.wrapping_mul(0x1000_0000_01b3);
    }
    h
}

/// FNV-1a over the snapshot's transport words.
///
/// A hash, not the words themselves: snapshots are variable length and this
/// only has to distinguish epochs, not reconstruct them. A collision would
/// serve one epoch's geometry in another — at 64 bits, across the handful of
/// epochs a session produces, that is not a risk worth a larger key.
pub fn hash_reality(reality: &RealitySnapshot) -> u64 {
    let words = reality.to_words();
    let mut bytes = Vec::with_capacity(words.len() * 4);
    for word in words {
        bytes.extend_from_slice(&word.to_le_bytes());
    }
    fnv64(&bytes)
}

/// Where a record's payload lives in the file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Extent {
    pub offset: u64,
    pub len: u32,
    /// FNV-1a of the payload, verified on every read.
    ///
    /// Without it a flipped bit decodes into *plausible* geometry — a wall
    /// in the wrong place, a light with an absurd radius — and nothing ever
    /// reports a problem. Regenerating a chunk costs 42 ms; rendering a
    /// corrupt one costs a bug nobody can reproduce.
    pub checksum: u64,
}

/// Byte storage behind an archive. A file natively; an OPFS access handle in
/// the browser.
pub trait ArchiveStorage {
    fn len(&self) -> u64;
    fn is_empty(&self) -> bool {
        self.len() == 0
    }
    /// Exactly `len` bytes at `offset`, or `None` if they are not all there.
    fn read_at(&self, offset: u64, len: usize) -> Option<Vec<u8>>;
    /// Appends and returns the offset written to, or `None` if the write
    /// failed (a full disk, an exceeded browser quota).
    fn append(&mut self, bytes: &[u8]) -> Option<u64>;
    /// Discards everything. Used when the identity does not match.
    fn reset(&mut self);
    /// Shortens the storage to `len` bytes. Used to drop a torn tail before
    /// appending over it.
    fn truncate(&mut self, len: u64);
}

/// An index over an append-only chunk log.
pub struct ChunkArchive {
    identity: ArchiveIdentity,
    index: HashMap<RecordKey, Extent>,
    /// Bytes that were scanned and indexed. A torn tail beyond this is
    /// overwritten by the next append rather than being read.
    end: u64,
    hits: u64,
    misses: u64,
    stored: u64,
    corrupt: u64,
}

impl ChunkArchive {
    /// Opens an archive over `storage`, rebuilding the index by replaying it.
    ///
    /// Resets the storage when it is empty, unreadable, or was written by a
    /// different generator — see the module note on why salvage is not
    /// attempted.
    pub fn open(storage: &mut dyn ArchiveStorage, identity: ArchiveIdentity) -> Self {
        let existing = storage
            .read_at(0, HEADER_BYTES)
            .and_then(|bytes| ArchiveIdentity::from_bytes(&bytes));
        let mut archive = Self {
            identity,
            index: HashMap::new(),
            end: HEADER_BYTES as u64,
            hits: 0,
            misses: 0,
            stored: 0,
            corrupt: 0,
        };
        if existing == Some(identity) {
            archive.replay(storage);
        } else {
            storage.reset();
            storage.append(&identity.to_bytes());
        }
        archive
    }

    /// Rebuilds the index by walking records from the header to the first one
    /// that is not wholly present.
    fn replay(&mut self, storage: &dyn ArchiveStorage) {
        let total = storage.len();
        let mut offset = HEADER_BYTES as u64;
        while offset + RECORD_HEADER_BYTES as u64 <= total {
            let Some(header) = storage.read_at(offset, RECORD_HEADER_BYTES) else {
                break;
            };
            if u32::from_le_bytes(header[0..4].try_into().expect("4 bytes")) != RECORD_MAGIC {
                // Not a record boundary: the tail is torn. Everything before
                // it is still good, and the next append overwrites this.
                break;
            }
            let chunk_x = i64::from_le_bytes(header[4..12].try_into().expect("8 bytes"));
            let chunk_z = i64::from_le_bytes(header[12..20].try_into().expect("8 bytes"));
            let lod = header[20];
            let artifacts = header[21];
            let reality = u64::from_le_bytes(header[24..32].try_into().expect("8 bytes"));
            let checksum = u64::from_le_bytes(header[32..40].try_into().expect("8 bytes"));
            let len = u32::from_le_bytes(header[40..44].try_into().expect("4 bytes"));

            let payload_at = offset + RECORD_HEADER_BYTES as u64;
            if payload_at + len as u64 > total {
                break; // Truncated payload: the record never finished.
            }
            // A later record for the same key shadows an earlier one, which
            // is what makes an append-only log support rewrites at all.
            self.index.insert(
                RecordKey {
                    chunk: (chunk_x, chunk_z),
                    lod,
                    artifacts,
                    reality,
                },
                Extent {
                    offset: payload_at,
                    len,
                    checksum,
                },
            );
            offset = payload_at + len as u64;
        }
        self.end = offset;
    }

    pub fn identity(&self) -> ArchiveIdentity {
        self.identity
    }

    pub fn len(&self) -> usize {
        self.index.len()
    }

    pub fn is_empty(&self) -> bool {
        self.index.is_empty()
    }

    /// Reads (hits, misses, stored) since opening.
    pub fn stats(&self) -> (u64, u64, u64) {
        (self.hits, self.misses, self.stored)
    }

    /// Records rejected by their checksum. Any non-zero value means the
    /// storage under this archive is damaging data.
    pub fn corrupt_records(&self) -> u64 {
        self.corrupt
    }

    pub fn contains(&self, key: RecordKey) -> bool {
        self.index.contains_key(&key)
    }

    /// The stored payload bytes for a key, if this archive has them.
    pub fn get(&mut self, storage: &dyn ArchiveStorage, key: RecordKey) -> Option<Vec<u8>> {
        let extent = *self.index.get(&key)?;
        match storage.read_at(extent.offset, extent.len as usize) {
            Some(bytes) if fnv64(&bytes) == extent.checksum => {
                self.hits += 1;
                Some(bytes)
            }
            Some(_) => {
                // Right length, wrong bytes. Drop the record and regenerate:
                // corrupt geometry that renders is far worse than a cache
                // miss, because nothing downstream can tell it is wrong.
                self.index.remove(&key);
                self.corrupt += 1;
                self.misses += 1;
                None
            }
            None => {
                // Indexed but unreadable: the file shrank under us. Forget it
                // rather than returning a short read as if it were geometry.
                self.index.remove(&key);
                self.misses += 1;
                None
            }
        }
    }

    /// Appends a payload for a key. A failed write is not an error the
    /// caller has to handle — the chunk was generated and is usable; it just
    /// will not be there next time.
    pub fn put(&mut self, storage: &mut dyn ArchiveStorage, key: RecordKey, payload: &[u8]) {
        let mut record = Vec::with_capacity(RECORD_HEADER_BYTES + payload.len());
        record.extend_from_slice(&RECORD_MAGIC.to_le_bytes());
        record.extend_from_slice(&key.chunk.0.to_le_bytes());
        record.extend_from_slice(&key.chunk.1.to_le_bytes());
        record.push(key.lod);
        record.push(key.artifacts);
        record.extend_from_slice(&[0u8; 2]); // pad to keep the hashes aligned
        record.extend_from_slice(&key.reality.to_le_bytes());
        let checksum = fnv64(payload);
        record.extend_from_slice(&checksum.to_le_bytes());
        record.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        record.extend_from_slice(payload);

        // Drop anything past the last record the replay could verify.
        // Appending *after* a torn tail leaves the garbage in the log, and
        // the next replay then reads a corrupt length, walks off the record
        // boundary, and loses every record written after it — turning one
        // bad write into the loss of a whole session.
        if storage.len() != self.end {
            storage.truncate(self.end);
        }
        if let Some(offset) = storage.append(&record) {
            let payload_at = offset + RECORD_HEADER_BYTES as u64;
            self.index.insert(
                key,
                Extent {
                    offset: payload_at,
                    len: payload.len() as u32,
                    checksum,
                },
            );
            self.end = payload_at + payload.len() as u64;
            self.stored += 1;
        }
    }

    /// Counts a generation that bypassed the archive, for the hit rate.
    pub fn note_miss(&mut self) {
        self.misses += 1;
    }

    /// Bytes the archive currently occupies.
    pub fn bytes(&self) -> u64 {
        self.end
    }

    /// Drops every record and starts the log again.
    ///
    /// An append-only log cannot free one record, so a bounded archive
    /// discards all of them at once. That is crude — it throws away chunks
    /// the player is standing next to along with ones they left an hour ago —
    /// but it is correct, it is O(1), and the cost of being wrong is one
    /// regeneration. A reuse-ordered eviction would need either a rewritten
    /// log or an index of holes, which is the complexity this format exists
    /// to avoid.
    pub fn clear(&mut self, storage: &mut dyn ArchiveStorage) {
        storage.reset();
        storage.append(&self.identity.to_bytes());
        self.index.clear();
        self.end = HEADER_BYTES as u64;
    }
}

/// An in-memory [`ArchiveStorage`], for tests and for a browser session that
/// has no persistent storage available.
#[derive(Debug, Default)]
pub struct MemoryStorage {
    bytes: Vec<u8>,
}

impl MemoryStorage {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn from_bytes(bytes: Vec<u8>) -> Self {
        Self { bytes }
    }

    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// Simulates a torn write: the tail is lost, the header is not.
    pub fn truncate(&mut self, len: usize) {
        self.bytes.truncate(len);
    }
}

impl ArchiveStorage for MemoryStorage {
    fn len(&self) -> u64 {
        self.bytes.len() as u64
    }

    fn read_at(&self, offset: u64, len: usize) -> Option<Vec<u8>> {
        let start = usize::try_from(offset).ok()?;
        let end = start.checked_add(len)?;
        self.bytes.get(start..end).map(<[u8]>::to_vec)
    }

    fn append(&mut self, bytes: &[u8]) -> Option<u64> {
        let offset = self.bytes.len() as u64;
        self.bytes.extend_from_slice(bytes);
        Some(offset)
    }

    fn reset(&mut self) {
        self.bytes.clear();
    }

    fn truncate(&mut self, len: u64) {
        if let Ok(len) = usize::try_from(len) {
            self.bytes.truncate(len);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity() -> ArchiveIdentity {
        ArchiveIdentity::new(42, 0, 10.0, 0.2, 0xABCD)
    }

    fn key(x: i64, z: i64, lod: u8) -> RecordKey {
        RecordKey {
            chunk: (x, z),
            lod,
            artifacts: 1,
            reality: 7,
        }
    }

    #[test]
    fn a_stored_chunk_comes_back_byte_for_byte() {
        let mut storage = MemoryStorage::new();
        let mut archive = ChunkArchive::open(&mut storage, identity());
        archive.put(&mut storage, key(0, 0, 0), b"the geometry");
        assert_eq!(
            archive.get(&storage, key(0, 0, 0)).as_deref(),
            Some(b"the geometry".as_slice())
        );
        assert_eq!(archive.get(&storage, key(1, 0, 0)), None);
    }

    #[test]
    fn the_index_survives_reopening() {
        // The point of a file: a later session must find what an earlier one
        // wrote, without regenerating anything to discover it.
        let mut storage = MemoryStorage::new();
        {
            let mut archive = ChunkArchive::open(&mut storage, identity());
            archive.put(&mut storage, key(0, 0, 0), b"first");
            archive.put(&mut storage, key(3, -4, 2), b"second");
        }
        let mut reopened = ChunkArchive::open(&mut storage, identity());
        assert_eq!(reopened.len(), 2);
        assert_eq!(
            reopened.get(&storage, key(3, -4, 2)).as_deref(),
            Some(b"second".as_slice())
        );
    }

    #[test]
    fn the_newest_record_for_a_key_wins() {
        let mut storage = MemoryStorage::new();
        let mut archive = ChunkArchive::open(&mut storage, identity());
        archive.put(&mut storage, key(0, 0, 0), b"stale");
        archive.put(&mut storage, key(0, 0, 0), b"fresh");
        assert_eq!(
            archive.get(&storage, key(0, 0, 0)).as_deref(),
            Some(b"fresh".as_slice())
        );
        let mut reopened = ChunkArchive::open(&mut storage, identity());
        assert_eq!(
            reopened.get(&storage, key(0, 0, 0)).as_deref(),
            Some(b"fresh".as_slice()),
            "replaying the log must end on the newest record, not the first"
        );
    }

    #[test]
    fn a_different_generator_discards_the_whole_archive() {
        // The failure this prevents is the worst one available: a world half
        // from disk and half from code, disagreeing with itself, reporting
        // nothing.
        let mut storage = MemoryStorage::new();
        {
            let mut archive = ChunkArchive::open(&mut storage, identity());
            archive.put(&mut storage, key(0, 0, 0), b"built by the old code");
        }
        let newer = ArchiveIdentity {
            generator_id: 0x1234,
            ..identity()
        };
        let mut archive = ChunkArchive::open(&mut storage, newer);
        assert!(archive.is_empty());
        assert_eq!(archive.get(&storage, key(0, 0, 0)), None);
    }

    #[test]
    fn every_field_of_identity_invalidates() {
        let variants = [
            ArchiveIdentity::new(43, 0, 10.0, 0.2, 0xABCD),
            ArchiveIdentity::new(42, 1, 10.0, 0.2, 0xABCD),
            ArchiveIdentity::new(42, 0, 20.0, 0.2, 0xABCD),
            ArchiveIdentity::new(42, 0, 10.0, 0.1, 0xABCD),
            ArchiveIdentity::new(42, 0, 10.0, 0.2, 0xFFFF),
        ];
        for variant in variants {
            let mut storage = MemoryStorage::new();
            {
                let mut archive = ChunkArchive::open(&mut storage, identity());
                archive.put(&mut storage, key(0, 0, 0), b"geometry");
            }
            let archive = ChunkArchive::open(&mut storage, variant);
            assert!(archive.is_empty(), "{variant:?} reused a foreign archive");
        }
    }

    #[test]
    fn lod_and_epoch_are_part_of_the_key() {
        let mut storage = MemoryStorage::new();
        let mut archive = ChunkArchive::open(&mut storage, identity());
        archive.put(&mut storage, key(0, 0, 0), b"fine");
        archive.put(&mut storage, key(0, 0, 2), b"coarse");
        let other_epoch = RecordKey {
            reality: 99,
            ..key(0, 0, 0)
        };
        archive.put(&mut storage, other_epoch, b"after the shift");

        assert_eq!(archive.get(&storage, key(0, 0, 0)).as_deref(), Some(b"fine".as_slice()));
        assert_eq!(
            archive.get(&storage, key(0, 0, 2)).as_deref(),
            Some(b"coarse".as_slice())
        );
        assert_eq!(
            archive.get(&storage, other_epoch).as_deref(),
            Some(b"after the shift".as_slice()),
            "a drift epoch must not read the pre-shift world back"
        );
    }

    #[test]
    fn a_torn_tail_costs_only_the_record_being_written() {
        // A crash, a closed tab, or an exceeded quota mid-append. Everything
        // written before it must still be readable, because the alternative
        // is losing an entire session's generation to one bad write.
        let mut storage = MemoryStorage::new();
        {
            let mut archive = ChunkArchive::open(&mut storage, identity());
            archive.put(&mut storage, key(0, 0, 0), b"complete");
            archive.put(&mut storage, key(1, 1, 0), b"interrupted halfway");
        }
        let full = storage.bytes().len();
        storage.truncate(full - 8);

        let mut archive = ChunkArchive::open(&mut storage, identity());
        assert_eq!(
            archive.get(&storage, key(0, 0, 0)).as_deref(),
            Some(b"complete".as_slice()),
            "an earlier complete record was lost to a later torn one"
        );
        assert_eq!(archive.get(&storage, key(1, 1, 0)), None);

        // And the archive stays writable: the next append lands on the torn
        // boundary and reopening finds it.
        archive.put(&mut storage, key(2, 2, 0), b"after recovery");
        let mut reopened = ChunkArchive::open(&mut storage, identity());
        assert_eq!(
            reopened.get(&storage, key(2, 2, 0)).as_deref(),
            Some(b"after recovery".as_slice())
        );
    }

    #[test]
    fn garbage_in_the_middle_stops_the_replay_without_panicking() {
        let mut storage = MemoryStorage::new();
        {
            let mut archive = ChunkArchive::open(&mut storage, identity());
            archive.put(&mut storage, key(0, 0, 0), b"good");
        }
        storage.append(&[0xFF; 64]);
        let archive = ChunkArchive::open(&mut storage, identity());
        assert_eq!(archive.len(), 1);
    }

    #[test]
    fn a_failed_append_is_survivable() {
        // A browser that has run out of quota must degrade to "generate every
        // time", not to a broken index that hands out offsets nothing wrote.
        struct FullDisk(MemoryStorage);
        impl ArchiveStorage for FullDisk {
            fn len(&self) -> u64 {
                self.0.len()
            }
            fn read_at(&self, offset: u64, len: usize) -> Option<Vec<u8>> {
                self.0.read_at(offset, len)
            }
            fn append(&mut self, bytes: &[u8]) -> Option<u64> {
                // The header write succeeds; every record write fails.
                if self.0.len() == 0 {
                    self.0.append(bytes)
                } else {
                    None
                }
            }
            fn reset(&mut self) {
                self.0.reset();
            }
            fn truncate(&mut self, len: u64) {
                self.0.truncate(len as usize);
            }
        }

        let mut storage = FullDisk(MemoryStorage::new());
        let mut archive = ChunkArchive::open(&mut storage, identity());
        archive.put(&mut storage, key(0, 0, 0), b"never lands");
        assert!(archive.is_empty());
        assert_eq!(archive.get(&storage, key(0, 0, 0)), None);
    }

    #[test]
    fn clearing_frees_the_log_but_keeps_it_usable() {
        let mut storage = MemoryStorage::new();
        let mut archive = ChunkArchive::open(&mut storage, identity());
        archive.put(&mut storage, key(0, 0, 0), b"old geometry");
        let grown = archive.bytes();

        archive.clear(&mut storage);
        assert!(archive.is_empty());
        assert_eq!(archive.get(&storage, key(0, 0, 0)), None);
        assert!(archive.bytes() < grown);

        archive.put(&mut storage, key(1, 1, 0), b"new geometry");
        let mut reopened = ChunkArchive::open(&mut storage, identity());
        assert_eq!(
            reopened.get(&storage, key(1, 1, 0)).as_deref(),
            Some(b"new geometry".as_slice()),
            "a cleared archive must still be a valid log"
        );
        assert_eq!(reopened.len(), 1, "the cleared records came back");
    }

    #[test]
    fn a_record_built_for_another_renderer_is_not_found() {
        // The bug this replaced: artifact presence was inferred from which
        // payload fields came back non-empty. A splat record has face
        // instances and no vertices, a mesh record the reverse, and both
        // answer `needs_surface_extraction()` — so the mesh renderer was
        // handed splat records and would have drawn nothing, silently.
        let mut storage = MemoryStorage::new();
        let mut archive = ChunkArchive::open(&mut storage, identity());
        let splat = RecordKey {
            artifacts: 0b10,
            ..key(0, 0, 0)
        };
        archive.put(&mut storage, splat, b"face instances");

        let mesh = RecordKey {
            artifacts: 0b01,
            ..key(0, 0, 0)
        };
        assert_eq!(
            archive.get(&storage, mesh),
            None,
            "a splat record was served to a mesh renderer"
        );
        assert_eq!(
            archive.get(&storage, splat).as_deref(),
            Some(b"face instances".as_slice())
        );
    }

    #[test]
    fn a_corrupted_payload_is_refused_rather_than_rendered() {
        // A flipped bit decodes into plausible geometry — a wall in the wrong
        // place — and nothing downstream can tell. A miss costs 42 ms; a
        // corrupt hit costs a bug nobody can reproduce.
        let mut storage = MemoryStorage::new();
        let mut archive = ChunkArchive::open(&mut storage, identity());
        archive.put(&mut storage, key(0, 0, 0), b"honest geometry");

        // Flip a bit inside the payload, leaving every length intact.
        let mut bytes = storage.bytes().to_vec();
        let last = bytes.len() - 1;
        bytes[last] ^= 0b0000_1000;
        let mut damaged = MemoryStorage::from_bytes(bytes);

        let mut archive = ChunkArchive::open(&mut damaged, identity());
        assert_eq!(archive.get(&damaged, key(0, 0, 0)), None);
        assert_eq!(archive.corrupt_records(), 1);
        // And the bad record is forgotten, so it is not re-read every frame.
        assert!(!archive.contains(key(0, 0, 0)));
    }

    #[test]
    fn an_older_format_is_never_read_as_the_current_one() {
        // Adding the artifact byte and the checksum moved every field. An
        // archive from before that must be discarded, not reinterpreted.
        let mut storage = MemoryStorage::new();
        {
            let mut archive = ChunkArchive::open(&mut storage, identity());
            archive.put(&mut storage, key(0, 0, 0), b"geometry");
        }
        // Rewrite the header's version word to the previous format.
        let mut bytes = storage.bytes().to_vec();
        bytes[4..8].copy_from_slice(&1u32.to_le_bytes());
        let mut old = MemoryStorage::from_bytes(bytes);

        let archive = ChunkArchive::open(&mut old, identity());
        assert!(archive.is_empty(), "a v1 archive was read as v2");
    }

    #[test]
    fn arbitrary_bytes_never_panic_and_never_become_geometry() {
        // An archive is read from a file the process does not control: a
        // half-synced OPFS handle, a copied save, a disk that lied. Opening
        // one must never panic and must never manufacture a record, whatever
        // the bytes are. Lengths are attacker-shaped here on purpose —
        // u32::MAX payload lengths and offsets past the end.
        let mut state: u64 = 0x1234_5678_9abc_def0;
        let mut next = || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        for case in 0..300 {
            let len = (next() % 512) as usize;
            let mut bytes: Vec<u8> = (0..len).map(|_| (next() & 0xFF) as u8).collect();
            // Half the cases wear a valid file header, so the replay actually
            // walks into the garbage instead of rejecting it up front.
            if case % 2 == 0 {
                let mut storage = MemoryStorage::new();
                ChunkArchive::open(&mut storage, identity());
                let mut framed = storage.bytes().to_vec();
                framed.append(&mut bytes);
                bytes = framed;
            }
            let mut storage = MemoryStorage::from_bytes(bytes);
            let mut archive = ChunkArchive::open(&mut storage, identity());
            // Whatever it made of that, no key may resolve to geometry.
            assert_eq!(archive.get(&storage, key(0, 0, 0)), None, "case {case}");
        }
    }

    #[test]
    fn a_record_claiming_an_absurd_length_is_ignored() {
        let mut storage = MemoryStorage::new();
        {
            let mut archive = ChunkArchive::open(&mut storage, identity());
            archive.put(&mut storage, key(0, 0, 0), b"real");
        }
        // Rewrite the record's length field to claim the rest of the address
        // space. The replay must stop, not allocate.
        let mut bytes = storage.bytes().to_vec();
        let len_at = HEADER_BYTES + 40;
        bytes[len_at..len_at + 4].copy_from_slice(&u32::MAX.to_le_bytes());
        let mut damaged = MemoryStorage::from_bytes(bytes);

        let archive = ChunkArchive::open(&mut damaged, identity());
        assert!(archive.is_empty());
    }

    #[test]
    fn the_build_is_part_of_every_archive_identity() {
        // The caller supplies a world id; the build id is folded in whether
        // they remember it or not, because forgetting is unrecoverable —
        // the archive would serve the previous build's geometry silently.
        let a = ArchiveIdentity::new(42, 0, 10.0, 0.2, 0);
        assert_ne!(
            a.generator_id, 0,
            "the build id was not folded into the identity"
        );
        // And it still discriminates the caller's own id.
        let b = ArchiveIdentity::new(42, 0, 10.0, 0.2, 1);
        assert_ne!(a.generator_id, b.generator_id);
    }

    #[test]
    fn one_epoch_hashes_the_same_way_twice() {
        let reality = RealitySnapshot::default();
        assert_eq!(hash_reality(&reality), hash_reality(&reality));
    }
}
