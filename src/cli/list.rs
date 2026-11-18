//! `git-yard list`: scan to completion and print every candidate.

use std::io::Write;
use std::process::ExitCode;

use anyhow::{Context, Result};

use crate::core::candidate::Candidate;
use crate::core::format;
use crate::core::obs::Obs;
use crate::scan::engine::{Engine, Measure};
use crate::scan::model::Model;
use crate::store::cache;

use super::Session;
use super::exit;
use super::output::ListEntry;

pub(crate) fn load_cache(session: &Session) -> Vec<Candidate> {
    if session.no_cache {
        return Vec::new();
    }
    let mut candidates: Vec<Candidate> =
        cache::load(&session.locations.cache_file).unwrap_or_default();
    for candidate in &mut candidates {
        candidate.git = std::mem::replace(&mut candidate.git, Obs::Pending).into_stale();
        candidate.storage = std::mem::replace(&mut candidate.storage, Obs::Pending).into_stale();
        candidate.watermark = None;
    }
    candidates
}

pub(crate) fn save_cache(session: &Session, model: &Model) {
    if session.no_cache {
        return;
    }
    if let Err(error) = cache::save(&session.locations.cache_file, &model.cacheable()) {
        eprintln!("git-yard: saving cache: {error:#}");
    }
}

pub(crate) fn run(session: &Session, json: bool, measure: bool, timings: bool) -> Result<ExitCode> {
    let services = session.services()?;
    let (engine, events) = Engine::start(
        session.config.clone(),
        services,
        load_cache(session),
        if measure { Measure::All } else { Measure::Off },
    );
    let mut model = Model::new(session.config.clone());
    while !(model.all_assessed() && (!measure || model.all_measured())) {
        let event = events.recv().context("scan stopped unexpectedly")?;
        model.apply(event);
        for event in events.try_iter() {
            model.apply(event);
        }
        model.settle();
    }
    engine.shutdown();
    model.tick();
    model.settle();
    if measure {
        save_cache(session, &model);
    }

    let rows: Vec<_> = model
        .selection
        .order()
        .iter()
        .filter_map(|key| model.rows.get(key))
        .collect();
    let mut out = std::io::stdout().lock();
    if json {
        let entries: Vec<ListEntry> = rows.iter().map(|row| ListEntry::new(row)).collect();
        serde_json::to_writer_pretty(&mut out, &entries)?;
        writeln!(out)?;
    } else {
        for row in &rows {
            let candidate = &row.candidate;
            let mark = match (&row.eligibility, row.recommendation.recommended) {
                (Err(_), _) => "-",
                (Ok(()), true) => "x",
                (Ok(()), false) => " ",
            };
            let state = match &row.eligibility {
                Err(block) => block.short().to_owned(),
                Ok(()) => row
                    .recommendation
                    .reasons
                    .first()
                    .cloned()
                    .unwrap_or_default(),
            };
            writeln!(
                out,
                "[{mark}] {:>5} {:<28} {:<28} {:>6} {:>6} {}  {}",
                row.rating
                    .as_ref()
                    .map_or("-".into(), |rating| format!("{:.0}", rating.score)),
                format::truncate_end(&candidate.seed.repo_label, 28),
                format::truncate_end(&candidate.head_label(), 28),
                candidate
                    .last_use()
                    .map_or("?".into(), |time| format::age(model.now - time)),
                candidate.size().map_or("?".into(), format::size),
                format::path(&candidate.seed.path),
                state,
            )?;
        }
    }
    out.flush()?;
    let failed = rows
        .iter()
        .filter(|row| matches!(row.candidate.git, Obs::Failed { .. }))
        .count();
    for error in &model.errors {
        eprintln!("git-yard: {error}");
    }
    if failed > 0 {
        eprintln!("git-yard: {failed} worktrees could not be assessed");
    }
    if timings {
        let t = &model.timings;
        eprintln!(
            "repos {} candidates {} | first candidate {:?} | discovery {:?} | assessed {:?} | measured {:?}",
            model.repos,
            model.rows.len(),
            t.first_candidate,
            t.discovery,
            t.assessed,
            t.measured
        );
    }
    Ok(exit::code(if model.errors.is_empty() && failed == 0 {
        exit::OK
    } else {
        exit::PARTIAL
    }))
}
