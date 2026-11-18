//! `git-yard journal`: recover and report interrupted removals.

use std::process::ExitCode;

use anyhow::Result;

use crate::removal::reconcile::{Action, reconcile};

use super::Session;
use super::exit;

pub(crate) fn run(session: &Session, json: bool) -> Result<ExitCode> {
    let services = session.services()?;
    let (recoveries, parse_errors) = reconcile(&services.journal, &services.locks)?;
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "recoveries": recoveries,
                "unreadable_lines": parse_errors,
            }))?
        );
    } else {
        if recoveries.is_empty() {
            println!("no interrupted removals");
        }
        for recovery in &recoveries {
            println!("{}", recovery.describe());
        }
        if parse_errors > 0 {
            eprintln!(
                "git-yard: {parse_errors} unreadable lines in {}",
                services.journal.path().display()
            );
        }
    }
    let attention = recoveries
        .iter()
        .any(|recovery| matches!(recovery.action, Action::Attention { .. } | Action::Busy));
    Ok(exit::code(if attention { exit::PARTIAL } else { exit::OK }))
}
