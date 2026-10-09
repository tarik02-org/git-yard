use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use serde::Serialize;

use git_yard::config::Config;
use git_yard::discovery::{self, FoundRepo};
use git_yard::engine::{Engine, Measure, Options};
use git_yard::format;
use git_yard::model::{Candidate, CandidateId, Obs, Timestamp, now};
use git_yard::pick::launch::Handoff;
use git_yard::policy::{self, Block, Rating, Recommendation};
use git_yard::remove::{self, Outcome, Request};
use git_yard::state::Model;
use git_yard::store::{Cache, Journal, Paths};

mod picker;
mod tui;

// musl's allocator serialises heavily under the worker threads; mimalloc
// keeps the static build as fast as the glibc one.
#[global_allocator]
static ALLOCATOR: mimalloc::MiMalloc = mimalloc::MiMalloc;

#[derive(Parser)]
#[command(
    version,
    about = "Pick and clean up Git worktrees across projects",
    args_conflicts_with_subcommands = true
)]
struct Cli {
    /// Directories to scan instead of the configured roots or the current
    /// directory.
    paths: Vec<PathBuf>,
    /// Config file to use instead of the nearest .git-yard.toml.
    #[arg(long, global = true)]
    config: Option<PathBuf>,
    /// Ignore cached observations.
    #[arg(long, global = true)]
    no_cache: bool,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Scan to completion and print every candidate.
    List {
        paths: Vec<PathBuf>,
        #[arg(long)]
        json: bool,
        /// Skip size and modification measurement.
        #[arg(long)]
        no_measure: bool,
        /// Print stage timings to stderr.
        #[arg(long)]
        timings: bool,
    },
    /// Validate and remove the given candidates (ids from `list --json`).
    Remove {
        #[arg(required = true)]
        ids: Vec<CandidateId>,
        /// Allow removing worktrees with uncommitted or untracked changes.
        #[arg(long)]
        allow_dirty: bool,
        #[arg(long)]
        json: bool,
    },
    /// Pick a worktree, branch or request across projects and open it.
    ///
    /// Prints the worktree path by default, for `cd "$(git-yard pick)"`.
    Pick {
        /// Initial search.
        query: Vec<String>,
        /// Open in tmux: one session per project, one window per worktree.
        #[arg(long, conflicts_with = "command")]
        tmux: bool,
        /// Print the selection as JSON instead of switching to it.
        #[arg(long, conflicts_with_all = ["tmux", "command"])]
        json: bool,
        /// Command to run in the worktree instead of printing its path;
        /// `{path}`, `{project}` and `{branch}` are replaced.
        #[arg(last = true)]
        command: Vec<String>,
    },
    /// Report removals interrupted by a crash or shutdown.
    Journal {
        #[arg(long)]
        json: bool,
    },
}

fn main() -> ExitCode {
    match run() {
        Ok(code) => code,
        Err(error) => {
            eprintln!("git-yard: {error:#}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<ExitCode> {
    let cli = Cli::parse();
    let cwd = std::env::current_dir()?;
    let paths = Paths::default_locations()?;

    match cli.command {
        None => {
            let config = Config::load(&cwd, cli.config.as_deref(), &cli.paths)?;
            let journal = Arc::new(Journal::open(&paths.journal_file)?);
            let interrupted = journal.reconcile()?;
            let cached = load_cache(&paths, cli.no_cache);
            tui::run(Arc::new(config), journal, cached, interrupted, &paths)?;
            Ok(ExitCode::SUCCESS)
        }
        Some(Command::List {
            paths: list_paths,
            json,
            no_measure,
            timings,
        }) => {
            let scan_paths = if list_paths.is_empty() {
                cli.paths
            } else {
                list_paths
            };
            let config = Config::load(&cwd, cli.config.as_deref(), &scan_paths)?;
            list(config, &paths, cli.no_cache, json, !no_measure, timings)
        }
        Some(Command::Remove {
            ids,
            allow_dirty,
            json,
        }) => {
            let config = Config::load(&cwd, cli.config.as_deref(), &cli.paths)?;
            let journal = Journal::open(&paths.journal_file)?;
            remove_ids(&config, &journal, &ids, allow_dirty, json)
        }
        Some(Command::Pick {
            query,
            tmux,
            json,
            command,
        }) => {
            let config = Config::load(&cwd, cli.config.as_deref(), &cli.paths)?;
            let handoff = if json {
                None
            } else if tmux {
                Some(Handoff::Tmux)
            } else if command.is_empty() {
                Some(Handoff::Print)
            } else {
                Some(Handoff::Command(command))
            };
            picker::run(Arc::new(config), &paths, query.join(" "), handoff)
        }
        Some(Command::Journal { json }) => {
            let journal = Journal::open(&paths.journal_file)?;
            let interrupted = journal.reconcile()?;
            if json {
                println!("{}", serde_json::to_string_pretty(&interrupted)?);
            } else if interrupted.is_empty() {
                println!("no interrupted removals");
            } else {
                for item in &interrupted {
                    println!("{}", item.describe());
                }
            }
            Ok(ExitCode::SUCCESS)
        }
    }
}

fn load_cache(paths: &Paths, disabled: bool) -> Vec<Candidate> {
    if disabled {
        Vec::new()
    } else {
        Cache::load(&paths.cache_file).entries
    }
}

#[derive(Serialize)]
struct ListEntry<'a> {
    id: &'a CandidateId,
    #[serde(flatten)]
    candidate: &'a Candidate,
    last_use: Option<Timestamp>,
    eligible: bool,
    blocked: Option<&'a Block>,
    recommendation: &'a Recommendation,
    rating: Option<&'a Rating>,
}

fn list(
    config: Config,
    paths: &Paths,
    no_cache: bool,
    json: bool,
    measure: bool,
    timings: bool,
) -> Result<ExitCode> {
    let config = Arc::new(config);
    let journal = Arc::new(Journal::open(&paths.journal_file)?);
    let cached = load_cache(paths, no_cache);
    let (engine, events) = Engine::start(
        config.clone(),
        journal,
        cached,
        Options {
            measure: if measure { Measure::All } else { Measure::Off },
        },
    );
    let mut model = Model::new(config);
    loop {
        let done = model.all_assessed() && (!measure || model.all_measured());
        if done {
            break;
        }
        let event = events.recv().context("engine stopped unexpectedly")?;
        model.apply(event);
        for event in events.try_iter() {
            model.apply(event);
        }
        model.settle();
    }
    engine.stop();
    model.tick();
    model.settle();
    if measure && !no_cache {
        let fresh = model.rows.values().filter(|row| row.fresh);
        if let Err(error) = Cache::save(&paths.cache_file, fresh.map(|row| row.candidate.clone())) {
            eprintln!("git-yard: saving cache: {error:#}");
        }
    }

    let order = model.selection.order().to_vec();
    let rows: Vec<_> = order.iter().filter_map(|id| model.rows.get(id)).collect();
    let mut out = std::io::stdout().lock();
    if json {
        let entries: Vec<ListEntry> = rows
            .iter()
            .map(|row| ListEntry {
                id: row.candidate.id(),
                candidate: &row.candidate,
                last_use: row.candidate.last_use(),
                eligible: row.eligibility.is_ok(),
                blocked: row.eligibility.as_ref().err(),
                recommendation: &row.recommendation,
                rating: row.rating.as_ref(),
            })
            .collect();
        serde_json::to_writer_pretty(&mut out, &entries)?;
        writeln!(out)?;
    } else {
        for row in rows {
            let candidate = &row.candidate;
            let mark = match (&row.eligibility, row.recommendation.checked) {
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
                truncate(&candidate.seed.repo_label, 28),
                truncate(&candidate.head_label(), 28),
                candidate
                    .last_use()
                    .map_or("?".into(), |time| format::age(model.now - time)),
                candidate.size().map_or("?".into(), format::size),
                discovery::display_path(&candidate.seed.path),
                state,
            )?;
        }
    }
    for error in &model.errors {
        eprintln!("git-yard: {error}");
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
    Ok(ExitCode::SUCCESS)
}

fn truncate(text: &str, width: usize) -> String {
    if text.chars().count() <= width {
        text.to_owned()
    } else {
        let mut result: String = text.chars().take(width - 1).collect();
        result.push('…');
        result
    }
}

#[derive(Serialize)]
struct RemoveResult<'a> {
    id: &'a CandidateId,
    #[serde(flatten)]
    outcome: Outcome,
}

fn remove_ids(
    config: &Config,
    journal: &Journal,
    ids: &[CandidateId],
    allow_dirty: bool,
    json: bool,
) -> Result<ExitCode> {
    let roots = discovery::normalize_roots(&config.roots);
    let mut results = Vec::new();
    for id in ids {
        let outcome = match prepare(id, &roots, allow_dirty) {
            Ok(request) => remove::run(&request, journal, || false, || {}),
            Err(reason) => Outcome::Blocked {
                reason: format!("{reason:#}"),
            },
        };
        if !json {
            println!("{id}: {outcome}");
        }
        results.push(RemoveResult { id, outcome });
    }
    if json {
        println!("{}", serde_json::to_string_pretty(&results)?);
    }
    let all_removed = results
        .iter()
        .all(|result| matches!(result.outcome, Outcome::Removed { .. }));
    Ok(if all_removed {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

/// Assesses one candidate now and turns it into a removal request.
fn prepare(id: &CandidateId, roots: &[PathBuf], allow_dirty: bool) -> Result<Request> {
    let found = FoundRepo {
        common_dir: id.repo.clone(),
        root: id.repo.clone(),
    };
    let scan = discovery::scan_repo(&found, roots)?;
    let seed = scan
        .seeds
        .into_iter()
        .find(|seed| &seed.id == id)
        .context("no such worktree registration")?;
    let git = git_yard::assess::assess(&seed, &mut Default::default())?;
    let dirty = git.dirty.is_some_and(|dirty| !dirty.is_clean());
    let candidate = Candidate {
        seed,
        git: Obs::Complete {
            value: git,
            observed_at: now(),
        },
        usage: Obs::Pending,
    };
    if let Err(block) = policy::eligibility(&candidate) {
        bail!("{block}");
    }
    if dirty && !allow_dirty {
        bail!("worktree has uncommitted or untracked changes; pass --allow-dirty to remove it");
    }
    Request::from_candidate(&candidate).context("not a removable linked worktree")
}
