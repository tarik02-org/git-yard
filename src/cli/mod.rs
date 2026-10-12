//! Command-line entry point.

mod exit;
mod gc;
mod journal;
mod list;
mod output;
mod pick;
mod remove;

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use anyhow::Result;
use clap::{CommandFactory, Parser, Subcommand};

use crate::core::config::Config;
use crate::core::error::Error;
use crate::git::open::{GitContext, Registry};
use crate::scan::engine::Services;
use crate::store::journal::Journal;
use crate::store::locations::Locations;
use crate::store::lock::RepoLocks;

#[derive(Parser)]
#[command(version, about = "Pick and clean up Git worktrees across projects")]
struct Cli {
    /// Config file to use instead of the user config file.
    #[arg(long, global = true)]
    config: Option<PathBuf>,
    /// Neither read nor write cached observations.
    #[arg(long, global = true)]
    no_cache: bool,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Generate shell completions.
    Completions {
        #[arg(value_enum)]
        shell: clap_complete::Shell,
    },
    /// Open the ranked worktree cleanup TUI.
    Gc {
        /// Directories to scan instead of the configured roots; defaults to
        /// the repository around the current directory, or the directory.
        paths: Vec<PathBuf>,
    },
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
    /// Revalidate and remove the linked worktrees at the given paths.
    Remove {
        #[arg(required = true)]
        paths: Vec<PathBuf>,
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
    /// Recover and report removals interrupted by a crash or shutdown.
    Journal {
        #[arg(long)]
        json: bool,
    },
}

/// What every command shares.
pub(crate) struct Session {
    pub cwd: PathBuf,
    pub config: Arc<Config>,
    pub locations: Locations,
    pub no_cache: bool,
}

impl Session {
    fn new(cli_config: Option<&PathBuf>, no_cache: bool, paths: &[PathBuf]) -> Result<Self, Error> {
        let cwd = std::env::current_dir().map_err(|source| Error::Io {
            context: "reading the current directory".into(),
            source,
        })?;
        let config = Config::load(&cwd, cli_config.map(PathBuf::as_path), paths, || {
            default_root(&cwd)
        })?;
        Ok(Self {
            cwd,
            config: Arc::new(config),
            locations: Locations::default_locations()?,
            no_cache,
        })
    }

    pub fn services(&self) -> Result<Services> {
        Ok(Services {
            registry: Arc::new(Registry::new(GitContext::default())),
            journal: Arc::new(Journal::open(&self.locations.journal_file)?),
            locks: Arc::new(RepoLocks::new(&self.locations.lock_dir)),
        })
    }
}

/// The repository around `cwd` when inside one, else `cwd` itself.
fn default_root(cwd: &std::path::Path) -> PathBuf {
    match gix::discover(cwd) {
        Ok(repo) => repo
            .workdir()
            .map(std::path::Path::to_path_buf)
            .unwrap_or_else(|| repo.git_dir().to_path_buf()),
        Err(_) => cwd.to_path_buf(),
    }
}

pub fn run() -> ExitCode {
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(error) => {
            let _ = error.print();
            return match error.kind() {
                clap::error::ErrorKind::DisplayHelp | clap::error::ErrorKind::DisplayVersion => {
                    exit::code(exit::OK)
                }
                _ => exit::code(exit::USAGE),
            };
        }
    };
    match dispatch(cli) {
        Ok(code) => code,
        Err(error) if is_broken_pipe(&error) => exit::code(exit::OK),
        Err(error) => {
            eprintln!("git-yard: {error:#}");
            let code = error
                .downcast_ref::<Error>()
                .map_or(exit::ERROR, Error::exit_code);
            exit::code(code)
        }
    }
}

fn dispatch(cli: Cli) -> Result<ExitCode> {
    let Some(command) = cli.command else {
        Cli::command().print_help()?;
        println!();
        return Ok(exit::code(exit::OK));
    };
    let config = cli.config.as_ref();
    match command {
        Command::Completions { shell } => {
            clap_complete::generate(
                shell,
                &mut Cli::command(),
                "git-yard",
                &mut std::io::stdout(),
            );
            Ok(exit::code(exit::OK))
        }
        Command::Gc { paths } => gc::run(&Session::new(config, cli.no_cache, &paths)?),
        Command::List {
            paths,
            json,
            no_measure,
            timings,
        } => list::run(
            &Session::new(config, cli.no_cache, &paths)?,
            json,
            !no_measure,
            timings,
        ),
        Command::Remove {
            paths,
            allow_dirty,
            json,
        } => remove::run(
            &Session::new(config, cli.no_cache, &[])?,
            &paths,
            allow_dirty,
            json,
        ),
        Command::Pick {
            query,
            tmux,
            json,
            command,
        } => pick::run(
            &Session::new(config, cli.no_cache, &[])?,
            query.join(" "),
            tmux,
            json,
            command,
        ),
        Command::Journal { json } => journal::run(&Session::new(config, cli.no_cache, &[])?, json),
    }
}

/// The reader of our output went away (`git-yard list | head`): not an error.
fn is_broken_pipe(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause
            .downcast_ref::<std::io::Error>()
            .is_some_and(|error| error.kind() == std::io::ErrorKind::BrokenPipe)
            || cause
                .downcast_ref::<serde_json::Error>()
                .and_then(serde_json::Error::io_error_kind)
                == Some(std::io::ErrorKind::BrokenPipe)
    })
}
