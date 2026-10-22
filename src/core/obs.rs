//! An observation and how much it can be trusted.

use serde::{Deserialize, Serialize};

use super::ids::Timestamp;

/// Cancelled work is never represented: it produces no observation at all,
/// so it can neither be shown as complete nor cached.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "kebab-case")]
pub enum Obs<T> {
    Pending,
    /// Loaded from cache; never authorizes deletion.
    Stale {
        value: T,
        observed_at: Timestamp,
    },
    /// Still being collected.
    Partial {
        value: T,
    },
    Complete {
        value: T,
        observed_at: Timestamp,
    },
    Failed {
        error: String,
    },
}

impl<T> Obs<T> {
    pub fn value(&self) -> Option<&T> {
        match self {
            Obs::Stale { value, .. } | Obs::Partial { value } | Obs::Complete { value, .. } => {
                Some(value)
            }
            Obs::Pending | Obs::Failed { .. } => None,
        }
    }

    pub fn complete(&self) -> Option<&T> {
        match self {
            Obs::Complete { value, .. } => Some(value),
            _ => None,
        }
    }

    pub fn is_settled(&self) -> bool {
        matches!(self, Obs::Complete { .. } | Obs::Failed { .. })
    }

    /// A previous observation shown while a fresh one is collected.
    pub fn into_stale(self) -> Obs<T> {
        match self {
            Obs::Complete { value, observed_at } | Obs::Stale { value, observed_at } => {
                Obs::Stale { value, observed_at }
            }
            other => other,
        }
    }
}
