//! Eligibility (may this be removed at all), recommendation (should it start
//! checked), and rating (how worthwhile is removal). The three are separate:
//! a high rating never makes a blocked candidate removable.

use std::fmt;

use serde::Serialize;

use crate::config::Config;
use crate::format::ago;
use crate::model::{Candidate, DirState, Integration, Kind, Obs, Timestamp};

const DAY: f64 = 86_400.0;

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "reason", content = "detail", rename_all = "kebab-case")]
pub enum Block {
    MainWorktree,
    BareRepository,
    Locked(Option<String>),
    Conflict(String),
    Pending,
    AssessmentFailed(String),
    UnreachableCommits,
}

impl fmt::Display for Block {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Block::MainWorktree => write!(f, "main worktree"),
            Block::BareRepository => write!(f, "bare repository"),
            Block::Locked(None) => write!(f, "locked"),
            Block::Locked(Some(reason)) => write!(f, "locked: {reason}"),
            Block::Conflict(reason) => write!(f, "conflict: {reason}"),
            Block::Pending => write!(f, "assessing"),
            Block::AssessmentFailed(error) => write!(f, "assessment failed: {error}"),
            Block::UnreachableCommits => write!(f, "detached with unreachable commits"),
        }
    }
}

impl Block {
    pub fn short(&self) -> &'static str {
        match self {
            Block::MainWorktree => "main",
            Block::BareRepository => "bare",
            Block::Locked(_) => "locked",
            Block::Conflict(_) => "conflict",
            Block::Pending => "assessing",
            Block::AssessmentFailed(_) => "failed",
            Block::UnreachableCommits => "unreachable",
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
    if let Some(conflict) = &seed.conflict {
        return Err(Block::Conflict(conflict.clone()));
    }
    if let Some(lock) = &seed.locked {
        return Err(Block::Locked(lock.reason.clone()));
    }
    let git = match &candidate.git {
        Obs::Complete { value, .. } => value,
        Obs::Failed { error } => return Err(Block::AssessmentFailed(error.clone())),
        Obs::Pending | Obs::Stale { .. } | Obs::Partial { .. } => return Err(Block::Pending),
    };
    if git.unreachable_commits {
        return Err(Block::UnreachableCommits);
    }
    Ok(())
}

/// Whether an eligible candidate starts checked when it is above the cutoff,
/// with the reasons it does not.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct Recommendation {
    pub checked: bool,
    pub reasons: Vec<String>,
}

pub fn recommend(candidate: &Candidate, config: &Config, now: Timestamp) -> Recommendation {
    let mut reasons = Vec::new();
    if let Err(block) = eligibility(candidate) {
        reasons.push(block.to_string());
        return Recommendation {
            checked: false,
            reasons,
        };
    }
    let Some(git) = candidate.git.complete() else {
        unreachable!("eligibility requires a complete assessment");
    };

    if let Some(branch) = git.head.branch()
        && config.is_protected(branch)
    {
        reasons.push(format!("protected branch {branch}"));
    }
    match &git.integration {
        integration if integration.is_integrated() => {}
        Integration::IsBase => reasons.push("base branch".into()),
        Integration::NotIntegrated { base, ahead } => {
            reasons.push(format!("{ahead} commits not in {base}"))
        }
        Integration::NoBase => reasons.push("no base branch to compare".into()),
        Integration::Unborn => reasons.push("no commits".into()),
        Integration::Merged { .. } | Integration::ChangesPresent { .. } => {}
    }
    if let Some(dirty) = git.dirty
        && !dirty.is_clean()
    {
        reasons.push(format!(
            "dirty: {} changed, {} untracked, {} conflicted",
            dirty.changed, dirty.untracked, dirty.conflicted
        ));
    }

    let stale_after = i64::from(config.stale_days) * 86_400;
    let missing = candidate.seed.dir == DirState::Missing;
    let measured = matches!(candidate.usage, Obs::Complete { .. } | Obs::Stale { .. });
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
        checked: reasons.is_empty(),
        reasons,
    }
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
/// depend on which other candidates exist.
pub fn rate(candidate: &Candidate, config: &Config, now: Timestamp) -> Option<Rating> {
    if candidate.seed.kind != Kind::LinkedWorktree {
        return None;
    }
    let weights = &config.weights;
    let git = candidate.git.value();
    let mut uncertain = false;
    let mut parts = Vec::new();

    let inactivity = match candidate.last_use() {
        Some(last_use) => {
            let days = ((now - last_use) as f64 / DAY).max(0.0);
            if !candidate.last_use_complete() {
                uncertain = true;
            }
            Part {
                name: "inactivity",
                value: days / (days + 30.0),
                weight: weights.inactivity,
                note: format!("last use {}", ago(now - last_use)),
            }
        }
        None => {
            uncertain = true;
            Part {
                name: "inactivity",
                value: 0.0,
                weight: weights.inactivity,
                note: "last use unknown".into(),
            }
        }
    };
    parts.push(inactivity);

    let commit_age = match git.and_then(|git| git.last_commit) {
        Some(commit) => {
            let days = ((now - commit) as f64 / DAY).max(0.0);
            Part {
                name: "commit age",
                value: days / (days + 60.0),
                weight: weights.commit_age,
                note: format!("last commit {}", ago(now - commit)),
            }
        }
        None => {
            uncertain = true;
            Part {
                name: "commit age",
                value: 0.0,
                weight: weights.commit_age,
                note: "unknown".into(),
            }
        }
    };
    parts.push(commit_age);

    let size = match candidate.size() {
        Some(bytes) => {
            if !matches!(candidate.usage, Obs::Complete { .. }) {
                uncertain = true;
            }
            // Saturates at 10 GiB.
            let mib = bytes as f64 / (1024.0 * 1024.0);
            Part {
                name: "size",
                value: ((1.0 + mib).log2() / (1.0 + 10_240f64).log2()).min(1.0),
                weight: weights.size,
                note: crate::format::size(bytes),
            }
        }
        None => {
            uncertain = true;
            Part {
                name: "size",
                value: 0.0,
                weight: weights.size,
                note: "not measured".into(),
            }
        }
    };
    parts.push(size);

    let integration = match git.map(|git| &git.integration) {
        Some(integration) if integration.is_integrated() => Part {
            name: "integration",
            value: 1.0,
            weight: weights.integration,
            note: crate::format::integration(integration),
        },
        Some(integration) => Part {
            name: "integration",
            value: 0.0,
            weight: weights.integration,
            note: crate::format::integration(integration),
        },
        None => {
            uncertain = true;
            Part {
                name: "integration",
                value: 0.0,
                weight: weights.integration,
                note: "not assessed".into(),
            }
        }
    };
    parts.push(integration);

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

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::model::*;

    const NOW: Timestamp = 1_800_000_000;
    const DAYS: i64 = 86_400;

    fn candidate(integration: Integration, dirty: Dirty, last_use_days: i64) -> Candidate {
        Candidate {
            seed: Seed {
                id: CandidateId {
                    repo: PathBuf::from("/r/.git"),
                    registration: Registration::Linked("feature".into()),
                },
                kind: Kind::LinkedWorktree,
                path: PathBuf::from("/r-feature"),
                repo_label: "ns/r".into(),
                gitdir: PathBuf::from("/r/.git/worktrees/feature"),
                locked: None,
                dir: DirState::Present { dev: 1, ino: 2 },
                git_activity: Some(NOW - last_use_days * DAYS),
                conflict: None,
                base: Vec::new(),
            },
            git: Obs::Complete {
                value: GitFacts {
                    head: Head::Branch {
                        name: "feature".into(),
                        oid: "0".repeat(40),
                    },
                    last_commit: Some(NOW - 60 * DAYS),
                    subject: None,
                    integration,
                    dirty: Some(dirty),
                    unreachable_commits: false,
                },
                observed_at: NOW,
            },
            usage: Obs::Complete {
                value: Usage {
                    logical: 1 << 30,
                    allocated: 1 << 30,
                    newest_mtime: Some(NOW - last_use_days * DAYS),
                    ..Usage::default()
                },
                observed_at: NOW,
            },
        }
    }

    fn merged() -> Integration {
        Integration::Merged {
            into: "main".into(),
        }
    }

    #[test]
    fn stale_clean_merged_worktree_starts_checked() {
        let config = Config::default();
        let recommendation = recommend(&candidate(merged(), Dirty::default(), 30), &config, NOW);
        assert_eq!(recommendation.reasons, Vec::<String>::new());
        assert!(recommendation.checked);
    }

    #[test]
    fn dirty_or_recent_or_unmerged_stays_unchecked_but_eligible() {
        let config = Config::default();
        let dirty = Dirty {
            untracked: 1,
            ..Dirty::default()
        };
        let unmerged = Integration::NotIntegrated {
            base: "main".into(),
            ahead: 2,
        };
        for candidate in [
            candidate(merged(), dirty, 30),
            candidate(merged(), Dirty::default(), 2),
            candidate(unmerged, Dirty::default(), 30),
        ] {
            assert_eq!(eligibility(&candidate), Ok(()));
            assert!(!recommend(&candidate, &config, NOW).checked);
        }
    }

    #[test]
    fn locked_and_unassessed_are_blocked_regardless_of_rating() {
        let mut locked = candidate(merged(), Dirty::default(), 300);
        locked.seed.locked = Some(Lock { reason: None });
        assert_eq!(eligibility(&locked), Err(Block::Locked(None)));
        assert!(rate(&locked, &Config::default(), NOW).unwrap().score > 50.0);

        let mut stale = candidate(merged(), Dirty::default(), 300);
        stale.git = Obs::Stale {
            value: stale.git.complete().unwrap().clone(),
            observed_at: NOW,
        };
        assert_eq!(eligibility(&stale), Err(Block::Pending));
    }

    #[test]
    fn rating_prefers_old_large_merged_and_penalises_protected() {
        let config = Config::default();
        let old = rate(&candidate(merged(), Dirty::default(), 90), &config, NOW).unwrap();
        let recent = rate(&candidate(merged(), Dirty::default(), 1), &config, NOW).unwrap();
        assert!(old.score > recent.score);
        assert!(!old.uncertain);

        let mut protected = candidate(merged(), Dirty::default(), 90);
        if let Obs::Complete { value, .. } = &mut protected.git {
            value.head = Head::Branch {
                name: "develop".into(),
                oid: "0".repeat(40),
            };
        }
        let protected = rate(&protected, &config, NOW).unwrap();
        assert!(protected.score < old.score * 0.5);
    }
}
