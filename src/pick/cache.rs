//! The picker's cache: last known listings and requests, shown at once on
//! the next start, and when each target was last picked.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::core::ids::{RepoKey, Timestamp};
use crate::store::cache;

use super::forge::Requests;
use super::listing::Listing;

/// Picks older than this no longer influence ranking and are dropped.
const PICK_MEMORY: Timestamp = 90 * 86_400;

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct PickCache {
    pub listings: Vec<Listing>,
    pub requests: Vec<(RepoKey, Requests)>,
    /// History key → time of the last pick.
    pub picks: BTreeMap<String, Timestamp>,
}

impl PickCache {
    pub fn load(path: &Path) -> PickCache {
        cache::load(path).unwrap_or_default()
    }

    pub fn save(&mut self, path: &Path, now: Timestamp) -> Result<()> {
        self.picks.retain(|_, picked| now - *picked < PICK_MEMORY);
        cache::save(path, self)
    }
}
