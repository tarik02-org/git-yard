//! Append-only JSONL log of removal operations. Every line is synced before
//! the step it announces runs, so a crash leaves enough to recover.

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, PoisonError};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::core::ids::{Timestamp, WorktreeKey, YardPath, now};

/// Journals larger than this are compacted on open when no other process
/// has them open.
const COMPACT_ABOVE: u64 = 1 << 20;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Quarantine {
    pub path: YardPath,
    /// Identity of the worktree directory before it was moved.
    pub dev: u64,
    pub ino: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "phase", rename_all = "kebab-case")]
pub enum Phase {
    /// Written before anything is touched. `quarantine` is `None` when the
    /// directory was already missing.
    Intent {
        quarantine: Option<Quarantine>,
    },
    /// The directory now lives at the quarantine path.
    Quarantined,
    /// `git worktree remove` succeeded and the registration is gone.
    RegistrationRemoved,
    /// The quarantine was deleted.
    Deleted,
    Done,
    /// The directory was moved back; nothing changed.
    RolledBack,
    /// Stopped with a problem described in `detail`; may leave a quarantine.
    Failed,
    /// Recovery put an interrupted operation's directory back.
    Restored,
    /// Recovery found nothing left to do.
    Resolved,
}

impl Phase {
    /// No further recovery is needed after this phase.
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            Phase::Done | Phase::RolledBack | Phase::Restored | Phase::Resolved
        )
    }

    pub fn name(&self) -> &'static str {
        match self {
            Phase::Intent { .. } => "intent",
            Phase::Quarantined => "quarantined",
            Phase::RegistrationRemoved => "registration-removed",
            Phase::Deleted => "deleted",
            Phase::Done => "done",
            Phase::RolledBack => "rolled-back",
            Phase::Failed => "failed",
            Phase::Restored => "restored",
            Phase::Resolved => "resolved",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    pub op: String,
    pub at: Timestamp,
    pub key: WorktreeKey,
    pub path: YardPath,
    #[serde(flatten)]
    pub phase: Phase,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// One operation's entries, oldest first.
#[derive(Clone, Debug)]
pub struct Operation {
    pub entries: Vec<Entry>,
}

impl Operation {
    pub fn op(&self) -> &str {
        &self.entries[0].op
    }

    pub fn key(&self) -> &WorktreeKey {
        &self.entries[0].key
    }

    pub fn path(&self) -> &YardPath {
        &self.entries[0].path
    }

    pub fn last(&self) -> &Entry {
        self.entries.last().expect("an operation has entries")
    }

    pub fn quarantine(&self) -> Option<&Quarantine> {
        self.entries.iter().find_map(|entry| match &entry.phase {
            Phase::Intent { quarantine } => quarantine.as_ref(),
            _ => None,
        })
    }

    pub fn reached(&self, phase: fn(&Phase) -> bool) -> bool {
        self.entries.iter().any(|entry| phase(&entry.phase))
    }
}

#[derive(Debug)]
pub struct Journal {
    file: Mutex<File>,
    path: PathBuf,
    /// Shared while open; compaction needs it exclusively.
    _lock: File,
    next_op: AtomicU64,
}

impl Journal {
    pub fn open(path: &Path) -> Result<Journal> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
        }
        let lock_path = path.with_extension("lock");
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&lock_path)
            .with_context(|| format!("opening {}", lock_path.display()))?;
        if lock.try_lock().is_ok() {
            compact_if_large(path)?;
            lock.unlock()?;
        }
        lock.lock_shared().context("locking journal")?;

        let mut file = OpenOptions::new()
            .create(true)
            .read(true)
            .append(true)
            .open(path)
            .with_context(|| format!("opening journal {}", path.display()))?;
        repair_last_line(&mut file)?;
        Ok(Journal {
            file: Mutex::new(file),
            path: path.to_path_buf(),
            _lock: lock,
            next_op: AtomicU64::new(0),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// A process-unique operation id.
    pub fn new_op(&self) -> String {
        format!(
            "{}-{}-{}",
            now(),
            std::process::id(),
            self.next_op.fetch_add(1, Ordering::Relaxed)
        )
    }

    pub fn append(&self, entry: &Entry) -> Result<()> {
        let mut line = serde_json::to_vec(entry)?;
        line.push(b'\n');
        let mut file = self.file.lock().unwrap_or_else(PoisonError::into_inner);
        file.write_all(&line)
            .and_then(|()| file.sync_data())
            .with_context(|| format!("writing journal {}", self.path.display()))
    }

    /// Every operation in order of first appearance, and how many lines
    /// could not be parsed.
    pub fn operations(&self) -> Result<(Vec<Operation>, usize)> {
        read_operations(&self.path)
    }

    /// Operations that did not reach a terminal phase.
    pub fn unfinished(&self) -> Result<(Vec<Operation>, usize)> {
        let (operations, errors) = self.operations()?;
        let unfinished = operations
            .into_iter()
            .filter(|operation| !operation.last().phase.is_terminal())
            .collect();
        Ok((unfinished, errors))
    }
}

fn read_operations(path: &Path) -> Result<(Vec<Operation>, usize)> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok((Vec::new(), 0));
        }
        Err(error) => {
            return Err(error).with_context(|| format!("reading journal {}", path.display()));
        }
    };
    let mut order: Vec<String> = Vec::new();
    let mut by_op: BTreeMap<String, Vec<Entry>> = BTreeMap::new();
    let mut errors = 0;
    for line in BufReader::new(file).split(b'\n') {
        let line = line?;
        if line.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        match serde_json::from_slice::<Entry>(&line) {
            Ok(entry) => {
                if !by_op.contains_key(&entry.op) {
                    order.push(entry.op.clone());
                }
                by_op.entry(entry.op.clone()).or_default().push(entry);
            }
            Err(_) => errors += 1,
        }
    }
    let operations = order
        .into_iter()
        .filter_map(|op| by_op.remove(&op))
        .map(|entries| Operation { entries })
        .collect();
    Ok((operations, errors))
}

/// A crash mid-write leaves a line without its newline; terminate it so the
/// next entry starts on a line of its own.
fn repair_last_line(file: &mut File) -> Result<()> {
    let length = file.metadata()?.len();
    if length == 0 {
        return Ok(());
    }
    let mut last = [0u8];
    file.seek(SeekFrom::Start(length - 1))?;
    file.read_exact(&mut last)?;
    if last[0] != b'\n' {
        file.write_all(b"\n")?;
        file.sync_data()?;
    }
    Ok(())
}

/// Rewrites the journal with only unfinished operations. The caller holds
/// the journal lock exclusively.
fn compact_if_large(path: &Path) -> Result<()> {
    let large = fs::metadata(path).is_ok_and(|meta| meta.len() > COMPACT_ABOVE);
    if !large {
        return Ok(());
    }
    let (operations, _) = read_operations(path)?;
    let mut data = Vec::new();
    for operation in operations {
        if operation.last().phase.is_terminal() {
            continue;
        }
        for entry in &operation.entries {
            serde_json::to_writer(&mut data, entry)?;
            data.push(b'\n');
        }
    }
    super::cache::write_atomic(path, &data)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::ids::RepoKey;

    fn entry(op: &str, phase: Phase) -> Entry {
        Entry {
            op: op.into(),
            at: 0,
            key: WorktreeKey::linked(&RepoKey::new("/r/.git"), "w"),
            path: YardPath::new("/r.w"),
            phase,
            detail: None,
        }
    }

    #[test]
    fn torn_lines_are_counted_and_repaired() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("journal.jsonl");
        let journal = Journal::open(&path).unwrap();
        journal
            .append(&entry("a", Phase::Intent { quarantine: None }))
            .unwrap();
        drop(journal);
        let mut file = OpenOptions::new().append(true).open(&path).unwrap();
        file.write_all(br#"{"op":"a","at":0,"#).unwrap();
        drop(file);

        let journal = Journal::open(&path).unwrap();
        journal.append(&entry("a", Phase::Done)).unwrap();
        let (operations, errors) = journal.operations().unwrap();
        assert_eq!(errors, 1);
        assert_eq!(operations.len(), 1);
        assert_eq!(operations[0].last().phase, Phase::Done);
        assert!(journal.unfinished().unwrap().0.is_empty());
    }

    #[test]
    fn compaction_keeps_unfinished_operations() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("journal.jsonl");
        let mut data = Vec::new();
        let mut push = |entry: Entry| {
            serde_json::to_writer(&mut data, &entry).unwrap();
            data.push(b'\n');
        };
        push(entry("open", Phase::Intent { quarantine: None }));
        for index in 0..8000 {
            let op = format!("done-{index}");
            push(entry(&op, Phase::Intent { quarantine: None }));
            push(entry(&op, Phase::Done));
        }
        assert!(data.len() as u64 > COMPACT_ABOVE);
        fs::write(&path, data).unwrap();

        let journal = Journal::open(&path).unwrap();
        let (operations, _) = journal.operations().unwrap();
        assert_eq!(operations.len(), 1);
        assert_eq!(operations[0].op(), "open");
    }
}
