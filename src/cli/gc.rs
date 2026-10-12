//! `git-yard gc`: the cleanup UI.

use std::process::ExitCode;

use anyhow::Result;

use crate::removal::reconcile::{Action, reconcile};
use crate::scan::engine::{Engine, Measure};
use crate::scan::model::Model;
use crate::ui::event_loop;
use crate::ui::gc_app::{Exit, GcApp};
use crate::ui::terminal::{self, SignalEvents};

use super::Session;
use super::exit;
use super::list::{load_cache, save_cache};

pub(crate) fn run(session: &Session) -> Result<ExitCode> {
    let services = session.services()?;
    let (recoveries, _) = reconcile(&services.journal, &services.locks)?;
    let mut screen = terminal::open("gc")?;
    let signals = SignalEvents::install()?;
    let input = terminal::input();

    let (engine, events) = Engine::start(
        session.config.clone(),
        services,
        load_cache(session),
        Measure::OnDemand,
    );
    let mut app = GcApp::new(Model::new(session.config.clone()), engine);
    app.notices = recoveries
        .iter()
        .filter(|recovery| recovery.action != Action::Resolved)
        .map(|recovery| recovery.describe())
        .collect();

    let result = event_loop::run(&mut screen, &input, &events, &signals.events, &mut app);
    drop(input);
    drop(screen);
    save_cache(session, &app.model);
    let outcome = result?;
    match outcome.output {
        // Running removals stop wherever they are; the journal lets the
        // next start recover them.
        Exit::Forced => app.control.stop(),
        Exit::Clean => app.control.shutdown(),
    }
    signals.finish(outcome.signal);
    Ok(exit::code(exit::OK))
}
