//! Eligibility (may this be removed at all), recommendation (should it start
//! checked), and rating (how worthwhile is removal). The three are separate:
//! a high rating never makes a blocked candidate removable.

use std::fmt;

use serde::Serialize;

use super::candidate::{Candidate, Kind};
use super::config::Config;
use super::facts::{DirState, EntryKind, Integration, StorageFacts};
use super::format::{ago, size};
use super::ids::Timestamp;
use super::obs::Obs;

const DAY: f64 = 86_400.0;

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "reason", content = "detail", rename_all = "kebab-case")]
pub enum Block {
    MainWorktree,
    BareRepository,
    UnreadableRegistration(String),
    Conflict(String),
    Locked(Option<String>),
    /// A symlink, file or special file sits at the worktree path.
    NotADirectory(EntryKind),
    /// The path's parent is missing too, as on an unmounted volume.
    ParentMissing,
    UnreadableDirectory,
    Pending,
    AssessmentFailed(String),
    UnreachableCommits,
    PrivateSubmodules,
    InProgress(&'static str),
    /// The directory is gone but its index has staged changes.
    StagedWithoutDirectory,
}

impl fmt::Display for Block {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Block::MainWorktree => write!(f, "main worktree"),
            Block::BareRepository => write!(f, "bare repository"),
            Block::UnreadableRegistration(error) => write!(f, "unreadable registration: {error}"),
            Block::Conflict(reason) => write!(f, "conflict: {reason}"),
            Block::Locked(None) => write!(f, "locked"),
            Block::Locked(Some(reason)) => write!(f, "locked: {reason}"),
            Block::NotADirectory(kind) => {
                let kind = match kind {
                    EntryKind::Symlink => "a symlink",
                    EntryKind::File => "a file",
                    EntryKind::Special => "a special file",
                };
                write!(f, "worktree path is {kind}")
            }
            Block::ParentMissing => {
                write!(f, "parent directory missing (unmounted volume?)")
            }
            Block::UnreadableDirectory => write!(f, "worktree directory unreadable"),
            Block::Pending => write!(f, "assessing"),
            Block::AssessmentFailed(error) => write!(f, "assessment failed: {error}"),
            Block::UnreachableCommits => write!(f, "detached with unreachable commits"),
            Block::PrivateSubmodules => {
                write!(f, "registration holds private submodule repositories")
            }
            Block::InProgress(operation) => write!(f, "{operation} in progress"),
            Block::StagedWithoutDirectory => {
                write!(f, "directory missing but the index has staged changes")
            }
        }
    }
}

impl Block {
    pub fn short(&self) -> &'static str {
        match self {
            Block::MainWorktree => "main",
            Block::BareRepository => "bare",
            Block::UnreadableRegistration(_) => "unreadable",
            Block::Conflict(_) => "conflict",
            Block::Locked(_) => "locked",
            Block::NotADirectory(_) => "not a dir",
            Block::ParentMissing => "unmounted?",
            Block::UnreadableDirectory => "unreadable",
            Block::Pending => "assessing",
            Block::AssessmentFailed(_) => "failed",
            Block::UnreachableCommits => "unreachable",
            Block::PrivateSubmodules => "submodules",
            Block::InProgress(operation) => operation,
            Block::StagedWithoutDirectory => "staged",
        }
    }
}

pub fn eligibility(candidate: &Candidate) -> Result<(), Block> {
    let seed = &candidate.seed;
    match seed.kind {
        Kind::MainWorktree => return Err(Block::MainWorktree),
        Kind::BareRepository => return Err(Block::BareRepository),
        Kind::LinkedWorktree => {}
    }
    if let Some(error) = &seed.registration_error {
        return Err(Block::UnreadableRegistration(error.clone()));
    }
    if let Some(conflict) = &seed.conflict {
        return Err(Block::Conflict(conflict.clone()));
    }
    if let Some(lock) = &seed.lock {
        return Err(Block::Locked(lock.reason.clone()));
    }
    match seed.dir {
        DirState::Present { .. }
        | DirState::Missing {
            parent_present: true,
        } => {}
        DirState::Missing {
            parent_present: false,
        } => return Err(Block::ParentMissing),
        DirState::NotADirectory { kind } => return Err(Block::NotADirectory(kind)),
        DirState::Unreadable => return Err(Block::UnreadableDirectory),
    }
    let git = match &candidate.git {
        Obs::Complete { value, .. } => value,
        Obs::Failed { error } => return Err(Block::AssessmentFailed(error.clone())),
        Obs::Pending | Obs::Stale { .. } | Obs::Partial { .. } => return Err(Block::Pending),
    };
    if git.private_modules {
        return Err(Block::PrivateSubmodules);
    }
    if let Some(operation) = git.in_progress {
        return Err(Block::InProgress(operation.describe()));
    }
    if git.unreachable_commits {
        return Err(Block::UnreachableCommits);
    }
    if seed.dir.is_missing_leaf() && git.status.is_some_and(|status| status.staged > 0) {
        return Err(Block::StagedWithoutDirectory);
    }
    Ok(())
}

/// Whether an eligible candidate is marked when it is above the cutoff, with
/// the reasons it is not.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct Recommendation {
    pub recommended: bool,
    pub reasons: Vec<String>,
}

pub fn recommend(candidate: &Candidate, config: &Config, now: Timestamp) -> Recommendation {
    let mut reasons = Vec::new();
    if let Err(block) = eligibility(candidate) {
        return Recommendation {
            recommended: false,
            reasons: vec![block.to_string()],
        };
    }
    let Some(git) = candidate.git.complete() else {
        unreachable!("eligibility requires a complete assessment");
    };
    let missing = candidate.seed.dir.is_missing_leaf();
    if missing {
        // Pruning is safe, but a vanished directory deserves a look first.
        reasons.push("directory missing".into());
    }

    if let Some(branch) = git.head.branch()
        && config.is_protected(branch)
    {
        reasons.push(format!("protected branch {branch}"));
    }
    match &git.integration {
        Integration::Merged { .. } | Integration::ChangesPresent { .. } => {}
        other => reasons.push(other.describe()),
    }
    if let Some(status) = git.status
        && !status.is_clean()
    {
        reasons.push(format!("dirty: {}", status.describe()));
    }
    match &candidate.storage {
        Obs::Complete { value, .. } | Obs::Stale { value, .. } => {
            reasons.extend(storage_reasons(value));
        }
        _ => {}
    }

    let stale_after = i64::from(config.stale_days) * 86_400;
    let measured = matches!(candidate.storage, Obs::Complete { .. } | Obs::Stale { .. });
    if !missing && !measured {
        reasons.push("modification time not measured yet".into());
    } else {
        match candidate.last_use() {
            Some(last_use) if now - last_use < stale_after => {
                reasons.push(format!("used {}", ago(now - last_use)))
            }
            None if !missing => reasons.push("last use unknown".into()),
            _ => {}
        }
    }
    if let Some(last_commit) = git.last_commit
        && now - last_commit < stale_after
    {
        reasons.push(format!("committed {}", ago(now - last_commit)));
    }

    Recommendation {
        recommended: reasons.is_empty(),
        reasons,
    }
}

fn storage_reasons(facts: &StorageFacts) -> Vec<String> {
    let mut reasons = Vec::new();
    if facts.nested_repos > 0 {
        reasons.push(format!(
            "contains {} nested repositories",
            facts.nested_repos
        ));
    }
    if facts.mounts > 0 {
        reasons.push(format!("contains {} mount points", facts.mounts));
    }
    if facts.errors > 0 {
        reasons.push(format!("{} unreadable entries", facts.errors));
    }
    reasons
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Rating {
    /// 0–100; higher means more worthwhile to remove.
    pub score: f64,
    pub parts: Vec<Part>,
    /// Some inputs were unknown and contributed nothing.
    pub uncertain: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Part {
    pub name: &'static str,
    /// 0–1 before weighting.
    pub value: f64,
    pub weight: f64,
    pub note: String,
}

/// Each input maps to 0–1 against fixed reference points, so scores do not
/// depend on which other candidates exist. Partial inputs still count; they
/// only mark the rating uncertain.
pub fn rate(candidate: &Candidate, config: &Config, now: Timestamp) -> Option<Rating> {
    if candidate.seed.kind != Kind::LinkedWorktree {
        return None;
    }
    let weights = &config.weights;
    let git = candidate.git.value();
    let measured = matches!(candidate.storage, Obs::Complete { .. });
    let mut uncertain = false;
    let mut parts = Vec::new();

    let inactivity = candidate.last_use().map(|last_use| {
        let days = ((now - last_use) as f64 / DAY).max(0.0);
        (
            days / (days + 30.0),
            format!("last use {}", ago(now - last_use)),
        )
    });
    uncertain |= inactivity.is_none() || !(measured || candidate.seed.dir.is_missing_leaf());
    parts.push(part(
        "inactivity",
        weights.inactivity,
        inactivity,
        "last use unknown",
    ));

    let commit_age = git.and_then(|git| git.last_commit).map(|commit| {
        let days = ((now - commit) as f64 / DAY).max(0.0);
        (
            days / (days + 60.0),
            format!("last commit {}", ago(now - commit)),
        )
    });
    uncertain |= commit_age.is_none();
    parts.push(part(
        "commit age",
        weights.commit_age,
        commit_age,
        "unknown",
    ));

    // Saturates at 10 GiB.
    let bytes = candidate.size().map(|bytes| {
        let mib = bytes as f64 / (1024.0 * 1024.0);
        let value = ((1.0 + mib).log2() / (1.0 + 10_240f64).log2()).min(1.0);
        (value, size(bytes))
    });
    uncertain |= bytes.is_none() || !measured;
    parts.push(part("size", weights.size, bytes, "not measured"));

    let integration = git.map(|git| {
        let value = if git.integration.is_integrated() {
            1.0
        } else {
            0.0
        };
        (value, git.integration.describe())
    });
    uncertain |= integration.is_none();
    parts.push(part(
        "integration",
        weights.integration,
        integration,
        "not assessed",
    ));

    let total_weight: f64 = parts.iter().map(|part| part.weight).sum();
    let mut score = if total_weight > 0.0 {
        parts
            .iter()
            .map(|part| part.value * part.weight)
            .sum::<f64>()
            / total_weight
            * 100.0
    } else {
        0.0
    };

    if let Some(branch) = git.and_then(|git| git.head.branch())
        && config.is_protected(branch)
    {
        score *= weights.protected_factor;
        parts.push(Part {
            name: "protected",
            value: weights.protected_factor,
            weight: 0.0,
            note: format!("{branch} is protected; score ×{}", weights.protected_factor),
        });
    }

    Some(Rating {
        score,
        parts,
        uncertain,
    })
}

fn part(name: &'static str, weight: f64, input: Option<(f64, String)>, unknown: &str) -> Part {
    let (value, note) = input.unwrap_or_else(|| (0.0, unknown.to_owned()));
    Part {
        name,
        value,
        weight,
        note,
    }
}
