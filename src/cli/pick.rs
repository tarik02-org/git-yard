//! `git-yard pick`: choose a target, switch to it, and hand it off.

use std::process::ExitCode;
use std::sync::Arc;

use anyhow::Result;
use serde::Serialize;

use crate::core::ids::{RepoKey, YardPath, now};
use crate::git::open::{GitContext, Registry};
use crate::git::process::Supervisor;
use crate::git::remote::Forge;
use crate::pick::cache::PickCache;
use crate::pick::engine::Engine;
use crate::pick::forge::Request;
use crate::pick::launch::{self, Action, Handoff};
use crate::pick::listing::{Target, history_key};
use crate::ui::event_loop;
use crate::ui::pick_app::PickApp;
use crate::ui::terminal::{self, SignalEvents};

use super::Session;
use super::exit;

/// What `--json` prints: the selection, without switching.
#[derive(Serialize)]
struct Selection<'a> {
    project: &'a str,
    repo: &'a RepoKey,
    main: &'a YardPath,
    target: &'a Target,
    #[serde(skip_serializing_if = "Option::is_none")]
    forge: Option<&'a Forge>,
    #[serde(skip_serializing_if = "Option::is_none")]
    request: Option<&'a Request>,
    /// Name of the branch to create from the target (Ctrl-O).
    #[serde(skip_serializing_if = "Option::is_none")]
    create: Option<&'a str>,
}

pub(crate) fn run(
    session: &Session,
    query: String,
    tmux: bool,
    json: bool,
    command: Vec<String>,
) -> Result<ExitCode> {
    let handoff = if json {
        None
    } else if tmux {
        Some(Handoff::Tmux)
    } else if command.is_empty() {
        Some(Handoff::Print)
    } else {
        Some(Handoff::Command(command))
    };
    let cache_file = &session.locations.pick_file;
    let mut cache = if session.no_cache {
        PickCache::default()
    } else {
        PickCache::load(cache_file)
    };

    let mut screen = terminal::open("pick")?;
    let signals = SignalEvents::install()?;
    let input = terminal::input();
    let supervisor = Arc::new(Supervisor::default());
    let registry = Arc::new(Registry::new(GitContext::default()));
    let (engine, events) = Engine::start(session.config.clone(), registry, supervisor.clone());
    let mut app = PickApp::new(
        session.config.clone(),
        engine,
        std::mem::take(&mut cache.listings),
        std::mem::take(&mut cache.requests),
        cache.picks.clone(),
        query,
    );
    let result = event_loop::run(&mut screen, &input, &events, &signals.events, &mut app);
    drop(input);
    drop(screen);

    cache.listings = app.listings.into_values().collect();
    cache.requests = app.requests.into_iter().collect();
    let save = |cache: &mut PickCache| {
        if !session.no_cache
            && let Err(error) = cache.save(cache_file, now())
        {
            eprintln!("git-yard: saving picker cache: {error:#}");
        }
    };
    let outcome = match result {
        Ok(outcome) => outcome,
        Err(error) => {
            supervisor.shutdown();
            save(&mut cache);
            return Err(error);
        }
    };
    let Some(choice) = outcome.output else {
        supervisor.shutdown();
        save(&mut cache);
        signals.finish(outcome.signal);
        return Ok(exit::code(exit::CANCELLED));
    };
    // The chosen repository is about to change; nothing else may run.
    supervisor.kill_tagged(&choice.project.repo);
    supervisor.shutdown();
    signals.finish(None);

    let Some(handoff) = handoff else {
        let selection = Selection {
            project: &choice.project.label,
            repo: &choice.project.repo,
            main: &choice.project.main_path,
            target: &choice.target,
            forge: choice.request.as_ref().map(|(forge, _)| forge),
            request: choice.request.as_ref().map(|(_, request)| request),
            create: match &choice.action {
                Action::Open => None,
                Action::Create { name } => Some(name),
            },
        };
        println!("{}", serde_json::to_string(&selection)?);
        cache.picks.insert(choice.key, now());
        save(&mut cache);
        return Ok(exit::code(exit::OK));
    };

    let switched = match launch::switch(
        &session.config.pick,
        &choice.project,
        &choice.target,
        choice.request.as_ref(),
        &choice.action,
    ) {
        Ok(switched) => switched,
        Err(error) => {
            save(&mut cache);
            return Err(error);
        }
    };
    let picked = now();
    cache.picks.insert(choice.key, picked);
    let worktree = Target::Worktree {
        path: YardPath::new(&switched.path),
        branch: switched.branch.clone(),
    };
    cache
        .picks
        .insert(history_key(choice.project.repo.path(), &worktree), picked);
    save(&mut cache);

    let handoff = if choice.print_only {
        Handoff::Print
    } else {
        handoff
    };
    launch::hand_off(&handoff, &choice.project, &switched)?;
    Ok(exit::code(exit::OK))
}
