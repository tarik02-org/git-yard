//! Observation cache and the deletion journal.

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::discovery;
use crate::model::{Candidate, CandidateId, Obs, Timestamp, now};

#[derive(Clone, Debug)]
pub struct Paths {
    pub cache_file: PathBuf,
    pub journal_file: PathBuf,
    pub pick_file: PathBuf,
}

impl Paths {
    pub fn default_locations() -> Result<Self> {
        let cache = dirs::cache_dir().context("no cache directory")?;
        let state = dirs::state_dir()
            .or_else(dirs::data_local_dir)
            .context("no state directory")?;
        Ok(Self {
            cache_file: cache.join("git-yard/cache-v1.json"),
            pick_file: cache.join("git-yard/pick-v1.json"),
            journal_file: state.join("git-yard/journal.jsonl"),
        })
    }

    pub fn under(dir: &Path) -> Self {
        Self {
            cache_file: dir.join("cache-v1.json"),
            pick_file: dir.join("pick-v1.json"),
            journal_file: dir.join("journal.jsonl"),
        }
    }
}

/// Cached candidates are shown as stale on startup and refreshed in the
/// background; they never authorize deletion.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Cache {
    pub entries: Vec<Candidate>,
}

impl Cache {
    pub fn load(path: &Path) -> Cache {
        let Ok(text) = fs::read(path) else {
            return Cache::default();
        };
        let mut cache: Cache = serde_json::from_slice(&text).unwrap_or_default();
        for entry in &mut cache.entries {
            entry.git = stale(std::mem::replace(&mut entry.git, Obs::Pending));
            entry.usage = stale(std::mem::replace(&mut entry.usage, Obs::Pending));
        }
        cache
    }

    pub fn save(path: &Path, candidates: impl Iterator<Item = Candidate>) -> Result<()> {
        let entries = candidates
            .filter(|candidate| candidate.git.complete().is_some())
            .collect();
        let data = serde_json::to_vec(&Cache { entries })?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        write_cache(path, &data)
    }
}

pub(crate) fn write_cache(path: &Path, data: &[u8]) -> Result<()> {
    let parent = path.parent().context("cache path has no parent")?;
    fs::create_dir_all(parent)?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
    temporary.write_all(data)?;
    temporary.persist(path)?;
    Ok(())
}

fn stale<T>(observation: Obs<T>) -> Obs<T> {
    match observation {
        Obs::Complete { value, observed_at } | Obs::Stale { value, observed_at } => {
            Obs::Stale { value, observed_at }
        }
        _ => Obs::Pending,
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Phase {
    /// Written before destructive work starts.
    Intent,
    DirectoryRemoved,
    RegistrationPruned,
    Done,
    Failed,
    /// An interrupted operation was reported after a restart.
    Reconciled,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct JournalEntry {
    pub op: String,
    pub at: Timestamp,
    pub id: CandidateId,
    pub path: PathBuf,
    pub gitdir: PathBuf,
    pub phase: Phase,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

pub struct Journal {
    file: Mutex<File>,
    path: PathBuf,
}

impl Journal {
    pub fn open(path: &Path) -> Result<Journal> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .with_context(|| format!("opening journal {}", path.display()))?;
        Ok(Journal {
            file: Mutex::new(file),
            path: path.to_path_buf(),
        })
    }

    pub fn append(&self, entry: &JournalEntry) -> Result<()> {
        let mut line = serde_json::to_vec(entry)?;
        line.push(b'\n');
        let mut file = self.file.lock().unwrap_or_else(|error| error.into_inner());
        file.write_all(&line)?;
        file.sync_data()?;
        Ok(())
    }

    pub fn entries(&self) -> Result<Vec<JournalEntry>> {
        let file = File::open(&self.path)?;
        let mut entries = Vec::new();
        for line in BufReader::new(file).lines() {
            let line = line?;
            // A torn final line from a crash is skipped rather than fatal.
            if let Ok(entry) = serde_json::from_str(&line) {
                entries.push(entry);
            }
        }
        Ok(entries)
    }

    /// Finds operations that started destructive work without finishing,
    /// reports their actual state, and marks them reconciled. Nothing is
    /// resumed.
    pub fn reconcile(&self) -> Result<Vec<Interrupted>> {
        let mut last: BTreeMap<String, JournalEntry> = BTreeMap::new();
        for entry in self.entries()? {
            last.insert(entry.op.clone(), entry);
        }
        let mut interrupted = Vec::new();
        for entry in last.into_values() {
            if matches!(entry.phase, Phase::Done | Phase::Failed | Phase::Reconciled) {
                continue;
            }
            let directory_exists = fs::symlink_metadata(&entry.path).is_ok();
            let registration_exists = entry.gitdir.exists();
            let report = Interrupted {
                id: entry.id.clone(),
                path: entry.path.clone(),
                last_phase: entry.phase.clone(),
                directory_exists,
                registration_exists,
            };
            self.append(&JournalEntry {
                at: now(),
                phase: Phase::Reconciled,
                detail: Some(report.describe()),
                ..entry
            })?;
            interrupted.push(report);
        }
        Ok(interrupted)
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct Interrupted {
    pub id: CandidateId,
    pub path: PathBuf,
    pub last_phase: Phase,
    pub directory_exists: bool,
    pub registration_exists: bool,
}

impl Interrupted {
    pub fn describe(&self) -> String {
        let directory = if self.directory_exists {
            "directory still present (possibly partially removed)"
        } else {
            "directory gone"
        };
        let registration = if self.registration_exists {
            "registration still present"
        } else {
            "registration gone"
        };
        format!(
            "interrupted removal of {}: {directory}, {registration}",
            discovery::display_path(&self.path)
        )
    }
}
