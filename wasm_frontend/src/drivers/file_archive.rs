//! A chunk archive backed by a real file.
//!
//! Native only. The browser's equivalent is an OPFS
//! `FileSystemSyncAccessHandle`, which offers the same four operations
//! (`getSize`, `read` at an offset, `write` at an offset, `truncate`) — the
//! port in [`crate::application::chunk_archive`] is shaped around that
//! intersection deliberately, so the browser driver is a transcription of
//! this one rather than a redesign.
//!
//! Every operation degrades to `None`/no-op rather than propagating an error.
//! An archive is an optimization: a session with an unwritable file must be
//! slower, never broken.

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use crate::application::chunk_archive::ArchiveStorage;

pub struct FileStorage {
    path: PathBuf,
    file: Option<File>,
}

impl FileStorage {
    /// Opens (creating if absent) the archive at `path`.
    pub fn open(path: impl AsRef<Path>) -> Self {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .ok();
        Self { path, file }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl ArchiveStorage for FileStorage {
    fn len(&self) -> u64 {
        self.file
            .as_ref()
            .and_then(|f| f.metadata().ok())
            .map_or(0, |m| m.len())
    }

    fn read_at(&self, offset: u64, len: usize) -> Option<Vec<u8>> {
        let mut file = self.file.as_ref()?;
        file.seek(SeekFrom::Start(offset)).ok()?;
        let mut buffer = vec![0u8; len];
        // `read_exact`, not `read`: a short read must be a miss, never a
        // truncated payload handed back as if it were geometry.
        file.read_exact(&mut buffer).ok()?;
        Some(buffer)
    }

    fn append(&mut self, bytes: &[u8]) -> Option<u64> {
        let file = self.file.as_mut()?;
        let offset = file.seek(SeekFrom::End(0)).ok()?;
        match file.write_all(bytes) {
            Ok(()) => Some(offset),
            Err(_) => {
                // A partial write leaves a torn tail. That is exactly what
                // the archive's replay is built to survive, so the only job
                // here is to not claim it succeeded.
                None
            }
        }
    }

    fn reset(&mut self) {
        if let Some(file) = self.file.as_mut() {
            let _ = file.set_len(0);
            let _ = file.seek(SeekFrom::Start(0));
        }
    }

    fn truncate(&mut self, len: u64) {
        if let Some(file) = self.file.as_mut() {
            let _ = file.set_len(len);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::application::chunk_archive::{ArchiveIdentity, ChunkArchive, RecordKey};

    fn scratch(name: &str) -> PathBuf {
        let mut path = std::env::temp_dir();
        path.push(format!("vackrooms-archive-test-{name}-{}", std::process::id()));
        let _ = std::fs::remove_file(&path);
        path
    }

    fn identity() -> ArchiveIdentity {
        ArchiveIdentity::new(42, 0, 10.0, 0.2, 1)
    }

    fn key(x: i64) -> RecordKey {
        RecordKey {
            chunk: (x, 0),
            lod: 0,
            reality: 0,
        }
    }

    #[test]
    fn chunks_written_in_one_session_are_read_back_in_the_next() {
        // The whole proposition: generate once, then never again.
        let path = scratch("round-trip");
        {
            let mut storage = FileStorage::open(&path);
            let mut archive = ChunkArchive::open(&mut storage, identity());
            for i in 0..8 {
                archive.put(&mut storage, key(i), format!("chunk {i}").as_bytes());
            }
        }
        {
            let mut storage = FileStorage::open(&path);
            let mut archive = ChunkArchive::open(&mut storage, identity());
            assert_eq!(archive.len(), 8);
            for i in 0..8 {
                assert_eq!(
                    archive.get(&storage, key(i)).as_deref(),
                    Some(format!("chunk {i}").as_bytes()),
                    "chunk {i} did not survive the session boundary"
                );
            }
        }
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_regenerated_world_does_not_read_the_old_one_back() {
        let path = scratch("identity");
        {
            let mut storage = FileStorage::open(&path);
            let mut archive = ChunkArchive::open(&mut storage, identity());
            archive.put(&mut storage, key(0), b"seed 42's geometry");
        }
        {
            let mut storage = FileStorage::open(&path);
            let other = ArchiveIdentity::new(43, 0, 10.0, 0.2, 1);
            let mut archive = ChunkArchive::open(&mut storage, other);
            assert!(archive.is_empty());
            assert_eq!(archive.get(&storage, key(0)), None);
        }
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn an_unopenable_path_degrades_to_no_archive() {
        // A directory is never a writable file. The session must still run.
        let mut storage = FileStorage::open(std::env::temp_dir());
        let mut archive = ChunkArchive::open(&mut storage, identity());
        archive.put(&mut storage, key(0), b"nowhere to go");
        assert_eq!(archive.get(&storage, key(0)), None);
    }

    #[test]
    fn a_truncated_file_keeps_everything_before_the_tear() {
        let path = scratch("torn");
        {
            let mut storage = FileStorage::open(&path);
            let mut archive = ChunkArchive::open(&mut storage, identity());
            archive.put(&mut storage, key(0), b"complete record");
            archive.put(&mut storage, key(1), b"a much longer record that gets cut");
        }
        let full = std::fs::metadata(&path).expect("archive exists").len();
        {
            let mut storage = FileStorage::open(&path);
            storage.truncate(full - 20);
        }
        {
            let mut storage = FileStorage::open(&path);
            let mut archive = ChunkArchive::open(&mut storage, identity());
            assert_eq!(
                archive.get(&storage, key(0)).as_deref(),
                Some(b"complete record".as_slice())
            );
            assert_eq!(archive.get(&storage, key(1)), None);
            // Still writable afterwards.
            archive.put(&mut storage, key(2), b"recovered");
            assert_eq!(
                archive.get(&storage, key(2)).as_deref(),
                Some(b"recovered".as_slice())
            );
        }
        let _ = std::fs::remove_file(&path);
    }
}
