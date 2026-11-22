//! What `list --json` prints for one row.

use serde::Serialize;

use crate::core::candidate::Kind;
use crate::core::facts::{DirState, GitFacts, StorageFacts};
use crate::core::ids::{Timestamp, WorktreeKey, YardPath};
use crate::core::obs::Obs;
use crate::core::policy::{Block, Rating, Recommendation};
use crate::scan::model::Row;

#[derive(Serialize)]
pub struct ListEntry<'a> {
    pub key: &'a WorktreeKey,
    pub kind: Kind,
    pub path: &'a YardPath,
    pub repo: &'a str,
    pub dir: &'a DirState,
    pub locked: Option<Option<&'a str>>,
    pub git: &'a Obs<GitFacts>,
    pub storage: &'a Obs<StorageFacts>,
    pub last_use: Option<Timestamp>,
    pub eligible: bool,
    pub blocked: Option<&'a Block>,
    pub recommendation: &'a Recommendation,
    pub rating: Option<&'a Rating>,
}

impl<'a> ListEntry<'a> {
    pub fn new(row: &'a Row) -> Self {
        let candidate = &row.candidate;
        let seed = &candidate.seed;
        ListEntry {
            key: &seed.key,
            kind: seed.kind,
            path: &seed.path,
            repo: &seed.repo_label,
            dir: &seed.dir,
            locked: seed.lock.as_ref().map(|lock| lock.reason.as_deref()),
            git: &candidate.git,
            storage: &candidate.storage,
            last_use: candidate.last_use(),
            eligible: row.eligibility.is_ok(),
            blocked: row.eligibility.as_ref().err(),
            recommendation: &row.recommendation,
            rating: row.rating.as_ref(),
        }
    }
}
