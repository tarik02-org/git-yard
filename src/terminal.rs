use std::io::{self, Stderr, Stdout, Write};
use std::panic::{self, PanicHookInfo};
use std::sync::Arc;
use std::thread;

use crossbeam_channel::{Receiver, unbounded};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::cursor::Show;
use ratatui::crossterm::event::{self, DisableMouseCapture, EnableMouseCapture, Event};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};

type PanicHook = dyn Fn(&PanicHookInfo<'_>) + Send + Sync + 'static;

pub struct Session<W: Write> {
    pub terminal: Terminal<CrosstermBackend<W>>,
    _guard: Guard,
}

pub fn stdout() -> io::Result<Session<Stdout>> {
    open(io::stdout(), || restore(io::stdout()))
}

pub fn stderr() -> io::Result<Session<Stderr>> {
    open(io::stderr(), || restore(io::stderr()))
}

fn open<W: Write>(mut output: W, restore: fn()) -> io::Result<Session<W>> {
    let guard = Guard::new(restore);
    enable_raw_mode()?;
    execute!(output, EnterAlternateScreen, EnableMouseCapture)?;
    let terminal = Terminal::new(CrosstermBackend::new(output))?;
    Ok(Session {
        terminal,
        _guard: guard,
    })
}

fn restore(mut output: impl Write) {
    let _ = execute!(output, DisableMouseCapture, LeaveAlternateScreen, Show);
    let _ = disable_raw_mode();
}

struct Guard {
    restore: fn(),
    previous_hook: Option<Arc<PanicHook>>,
}

impl Guard {
    fn new(restore: fn()) -> Self {
        let previous_hook: Arc<PanicHook> = panic::take_hook().into();
        let previous = previous_hook.clone();
        panic::set_hook(Box::new(move |info| {
            restore();
            previous(info);
        }));
        Self {
            restore,
            previous_hook: Some(previous_hook),
        }
    }
}

impl Drop for Guard {
    fn drop(&mut self) {
        (self.restore)();
        // Panic hooks cannot be changed while unwinding.
        if !thread::panicking()
            && let Some(previous) = self.previous_hook.take()
        {
            panic::set_hook(Box::new(move |info| previous(info)));
        }
    }
}

pub fn input() -> Receiver<io::Result<Event>> {
    let (sender, receiver) = unbounded();
    thread::spawn(move || {
        loop {
            let event = event::read();
            let failed = event.is_err();
            if sender.send(event).is_err() || failed {
                return;
            }
        }
    });
    receiver
}
