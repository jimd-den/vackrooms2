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
const ARCHIVE_FORMAT_VERSION: u32 = 1;

pub const HEADER_BYTES: usize = 32;
/// magic + chunk x/z + lod + padding + reality hash + payload length.
const RECORD_HEADER_BYTES: usize = 4 + 8 + 8 + 4 + 8 + 4;

/// What an archive was built by. Any difference invalidates every record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ArchiveIdentity {
    pub seed: u32,
    pub level: u32,
    /// Bit patterns, not floats: this is an equality check, and `NaN != NaN`
    /// would make an archive silently never match itself.
    pub chunk_size_bits: u32,
    pub voxel_scale_bits: u32,
    /// Identity of the code that generates geometry. The composition root
    /// supplies it — a build hash, the wasm source digest, anything that
    /// changes when generation changes.
    pub generator_id: u64,
}

impl ArchiveIdentity {
    pub fn new(seed: u32, level: u32, chunk_size: f32, voxel_scale: f32, generator_id: u64) -> Self {
        Self {
            seed,
            level,
            chunk_size_bits: chunk_size.to_bits(),
            voxel_scale_bits: voxel_scale.to_bits(),
            generator_id,
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

/// Which chunk, at which detail, in which epoch of the world.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RecordKey {
    pub chunk: ChunkKey,
    pub lod: u8,
    /// Hash of the [`RealitySnapshot`] the chunk was generated against.
    /// Without this, a chunk cached before a Peripheral Shift would be
    /// served after it, and the world would not drift where the player had
    /// already been — which is precisely where drift is supposed to happen.
    pub reality: u64,
}

impl RecordKey {
    pub fn new(chunk: ChunkKey, lod: u8, reality: &RealitySnapshot) -> Self {
        Self {
            chunk,
            lod,
            reality: hash_reality(reality),
        }
    }
}

/// FNV-1a over the snapshot's transport words.
///
/// A hash, not the words themselves: snapshots are variable length and this
/// only has to distinguish epochs, not reconstruct them. A collision would
/// serve one epoch's geometry in another — at 64 bits, across the handful of
/// epochs a session produces, that is not a risk worth a larger key.
pub fn hash_reality(reality: &RealitySnapshot) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for word in reality.to_words() {
        for byte in word.to_le_bytes() {
            h ^= byte as u64;
            h = h.wrapping_mul(0x1000_0000_01b3);
        }
    }
    h
}

/// Where a record's payload lives in the file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Extent {
    pub offset: u64,
    pub len: u32,
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
            let reality = u64::from_le_bytes(header[24..32].try_into().expect("8 bytes"));
            let len = u32::from_le_bytes(header[32..36].try_into().expect("4 bytes"));

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
                    reality,
                },
                Extent {
                    offset: payload_at,
                    len,
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

    pub fn contains(&self, key: RecordKey) -> bool {
        self.index.contains_key(&key)
    }

    /// The stored payload bytes for a key, if this archive has them.
    pub fn get(&mut self, storage: &dyn ArchiveStorage, key: RecordKey) -> Option<Vec<u8>> {
        let extent = *self.index.get(&key)?;
        match storage.read_at(extent.offset, extent.len as usize) {
            Some(bytes) => {
                self.hits += 1;
                Some(bytes)
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
        record.extend_from_slice(&[0u8; 3]); // pad to keep the hash aligned
        record.extend_from_slice(&key.reality.to_le_bytes());
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
    fn one_epoch_hashes_the_same_way_twice() {
        let reality = RealitySnapshot::default();
        assert_eq!(hash_reality(&reality), hash_reality(&reality));
    }
}
