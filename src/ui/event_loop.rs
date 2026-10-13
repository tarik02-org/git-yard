//! The loop shared by both UIs: terminal input, background events, a
//! once-a-second tick, signals, and redraws throttled to one per frame.

use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use crossbeam_channel::{Receiver, select, tick};
use ratatui::Frame;
use ratatui::crossterm::event::{Event as TermEvent, KeyEventKind};

use super::terminal::{Input, Session};

/// Minimum time between redraws; input is still handled at once.
const FRAME: Duration = Duration::from_millis(16);
/// Background events applied between input checks.
const EVENTS_PER_BATCH: usize = 5000;

pub enum Flow<T> {
    Continue,
    Exit(T),
}

pub trait App {
    type Event;
    type Output;

    fn on_input(&mut self, event: TermEvent) -> Flow<Self::Output>;
    fn on_event(&mut self, event: Self::Event);
    fn on_tick(&mut self) {}
    /// A termination signal arrived; `repeated` when it is not the first.
    /// `Exit` leaves now; `Continue` lets the app wind down and report
    /// through `finished`.
    fn on_signal(&mut self, repeated: bool) -> Flow<Self::Output>;
    /// The first signal asked for a graceful exit that is now complete.
    fn finished(&mut self) -> Option<Self::Output> {
        None
    }
    /// Brings derived state up to date before drawing or handling input.
    fn settle(&mut self) {}
    /// A deadline the app wants to be woken at, such as a typing pause.
    fn wake_at(&self) -> Option<Instant> {
        None
    }
    fn on_wake(&mut self) {}
    fn draw(&mut self, frame: &mut Frame);
}

pub struct Outcome<T> {
    pub output: T,
    /// The termination signal that ended the session, to re-raise.
    pub signal: Option<i32>,
}

pub fn run<A: App>(
    session: &mut Session,
    input: &Input,
    events: &Receiver<A::Event>,
    signals: &Receiver<i32>,
    app: &mut A,
) -> Result<Outcome<A::Output>> {
    let ticker = tick(Duration::from_secs(1));
    let mut dirty = true;
    let mut last_draw = Instant::now() - FRAME;
    let mut signal = None;
    loop {
        if dirty && last_draw.elapsed() >= FRAME {
            app.settle();
            session.terminal.draw(|frame| app.draw(frame))?;
            last_draw = Instant::now();
            dirty = false;
        }
        if app.wake_at().is_some_and(|at| at <= Instant::now()) {
            app.settle();
            app.on_wake();
            dirty = true;
        }
        if let Some(output) = app.finished() {
            return Ok(Outcome { output, signal });
        }
        let wake = app
            .wake_at()
            .map(|at| at.saturating_duration_since(Instant::now()));
        let wait = if dirty {
            FRAME.saturating_sub(last_draw.elapsed())
        } else {
            wake.unwrap_or(Duration::from_secs(1))
        };
        select! {
            recv(input.events) -> event => {
                // Handlers read derived state, so pending events settle first.
                app.settle();
                let event = event
                    .context("terminal input disconnected")?
                    .context("reading terminal input")?;
                let pressed = !matches!(&event, TermEvent::Key(key) if key.kind != KeyEventKind::Press);
                if pressed && let Flow::Exit(output) = app.on_input(event) {
                    return Ok(Outcome { output, signal });
                }
                dirty = true;
            }
            recv(events) -> event => {
                let Ok(event) = event else {
                    anyhow::bail!("background workers stopped unexpectedly");
                };
                app.on_event(event);
                for event in events.try_iter().take(EVENTS_PER_BATCH) {
                    app.on_event(event);
                }
                dirty = true;
            }
            recv(signals) -> received => {
                if let Ok(received) = received {
                    let repeated = signal.is_some();
                    signal.get_or_insert(received);
                    if let Flow::Exit(output) = app.on_signal(repeated) {
                        return Ok(Outcome { output, signal });
                    }
                }
                dirty = true;
            }
            recv(ticker) -> _ => {
                app.on_tick();
                dirty = true;
            }
            default(wait) => {}
        }
    }
}
