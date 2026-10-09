//! The picker's cache: last known listings and requests, shown at once on
//! the next start, and when each target was last picked.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::Result;
use serde::{Deserialize, Serialize};

use super::Listing;
use super::forge::Requests;
use crate::model::Timestamp;

/// Picks older than this no longer influence ranking and are dropped.
const PICK_MEMORY: Timestamp = 90 * 86_400;

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct PickCache {
    pub listings: Vec<Listing>,
    /// Keyed by common Git directory.
    pub requests: BTreeMap<PathBuf, Requests>,
    /// `pick_key` → time of the last pick.
    pub picks: BTreeMap<String, Timestamp>,
}

pub fn pick_key(common_dir: &Path, target_key: &str) -> String {
    format!("{}::{target_key}", common_dir.display())
}

impl PickCache {
    pub fn load(path: &Path) -> PickCache {
        fs::read(path)
            .ok()
            .and_then(|data| serde_json::from_slice(&data).ok())
            .unwrap_or_default()
    }

    pub fn save(&mut self, path: &Path, now: Timestamp) -> Result<()> {
        self.picks.retain(|_, picked| now - *picked < PICK_MEMORY);
        crate::store::write_cache(path, &serde_json::to_vec(self)?)
    }
}
