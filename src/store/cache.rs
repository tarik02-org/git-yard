//! Versioned JSON files written atomically.

use std::fs::{self, File};
use std::io::Write;
use std::path::Path;

use anyhow::{Context, Result};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

const VERSION: u32 = 1;

#[derive(Serialize)]
struct EnvelopeOut<'a, T> {
    version: u32,
    data: &'a T,
}

#[derive(Deserialize)]
struct EnvelopeIn<T> {
    version: u32,
    data: T,
}

/// Reads `path`; a missing, unreadable, foreign-version or corrupt file is
/// treated as empty, since the cache only speeds up the first frame.
pub fn load<T: DeserializeOwned>(path: &Path) -> Option<T> {
    let bytes = fs::read(path).ok()?;
    let envelope: EnvelopeIn<T> = serde_json::from_slice(&bytes).ok()?;
    (envelope.version == VERSION).then_some(envelope.data)
}

pub fn save<T: Serialize>(path: &Path, data: &T) -> Result<()> {
    let bytes = serde_json::to_vec(&EnvelopeOut {
        version: VERSION,
        data,
    })?;
    write_atomic(path, &bytes)
}

/// Writes a temporary file beside `path`, syncs it, renames it over `path`
/// and syncs the directory, so a crash leaves either the old or new file.
pub fn write_atomic(path: &Path, data: &[u8]) -> Result<()> {
    let parent = path.parent().context("path has no parent")?;
    fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
    temporary.write_all(data)?;
    temporary.as_file().sync_all()?;
    temporary
        .persist(path)
        .with_context(|| format!("replacing {}", path.display()))?;
    File::open(parent)?.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_and_ignores_foreign_versions() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sub/cache.json");
        save(&path, &vec![1, 2, 3]).unwrap();
        assert_eq!(load::<Vec<i32>>(&path), Some(vec![1, 2, 3]));
        fs::write(&path, r#"{"version":99,"data":[1]}"#).unwrap();
        assert_eq!(load::<Vec<i32>>(&path), None);
        fs::write(&path, "{").unwrap();
        assert_eq!(load::<Vec<i32>>(&path), None);
    }
}
