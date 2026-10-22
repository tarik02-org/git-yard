//! The terminal session: drawn on `/dev/tty`, so stdout stays free for
//! results (`cd "$(git-yard pick)"`, `git-yard gc | tee`). Restores the
//! terminal on drop, on a UI-thread panic, and before re-raising signals.

use std::fs::{File, OpenOptions};
use std::io::{self, BufWriter, IsTerminal, Write};
use std::panic::{self, PanicHookInfo};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::{self, JoinHandle, ThreadId};
use std::time::Duration;

use crossbeam_channel::{Receiver, unbounded};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::cursor::Show;
use ratatui::crossterm::event::{self, DisableMouseCapture, EnableMouseCapture, Event};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::{
    EndSynchronizedUpdate, EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode,
    enable_raw_mode,
};
use signal_hook::consts::{SIGHUP, SIGINT, SIGTERM};
use signal_hook::iterator::Signals;

use crate::core::error::Error;

type PanicHook = dyn Fn(&PanicHookInfo<'_>) + Send + Sync + 'static;

/// Buffered so a frame leaves in one write instead of one per escape
/// sequence; terminal multiplexers otherwise show it half-drawn.
pub type Backend = CrosstermBackend<BufWriter<File>>;

/// Panic messages from other threads while the screen is ours; printing
/// them would scribble over the UI.
static SUPPRESSED: Mutex<Vec<String>> = Mutex::new(Vec::new());

pub struct Session {
    pub terminal: Terminal<Backend>,
    previous_hook: Option<Arc<PanicHook>>,
    active: Arc<AtomicBool>,
}

/// Opens the controlling terminal, or explains why there is none.
pub fn open(command: &str) -> Result<Session, Error> {
    let tty = OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/tty")
        .ok()
        .filter(|tty| tty.is_terminal())
        .ok_or_else(|| {
            Error::NotATerminal(format!(
                "git-yard {command} is interactive and needs a terminal"
            ))
        })?;
    let active = Arc::new(AtomicBool::new(true));
    let previous_hook = install_panic_hook(thread::current().id(), active.clone());
    let setup = || -> io::Result<Terminal<Backend>> {
        enable_raw_mode()?;
        let mut output = tty.try_clone()?;
        execute!(output, EnterAlternateScreen, EnableMouseCapture)?;
        Terminal::new(CrosstermBackend::new(BufWriter::new(output)))
    };
    // No clear(): crossterm asks for the cursor position on stdout, which
    // may be a pipe; the alternate screen starts blank anyway.
    Ok(Session {
        terminal: match setup() {
            Ok(terminal) => terminal,
            Err(error) => {
                restore();
                return Err(Error::Io {
                    context: "setting up the terminal".into(),
                    source: error,
                });
            }
        },
        previous_hook: Some(previous_hook),
        active,
    })
}

fn restore() {
    if let Ok(mut tty) = OpenOptions::new().write(true).open("/dev/tty") {
        // A panic mid-frame would leave the terminal holding back output.
        let _ = execute!(
            tty,
            EndSynchronizedUpdate,
            DisableMouseCapture,
            LeaveAlternateScreen,
            Show
        );
        let _ = tty.flush();
    }
    let _ = disable_raw_mode();
}

fn install_panic_hook(ui: ThreadId, active: Arc<AtomicBool>) -> Arc<PanicHook> {
    let previous: Arc<PanicHook> = panic::take_hook().into();
    let chained = previous.clone();
    panic::set_hook(Box::new(move |info| {
        if !active.load(Ordering::SeqCst) {
            chained(info);
        } else if thread::current().id() == ui {
            active.store(false, Ordering::SeqCst);
            restore();
            chained(info);
        } else {
            SUPPRESSED
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(info.to_string());
        }
    }));
    previous
}

impl Drop for Session {
    fn drop(&mut self) {
        if self.active.swap(false, Ordering::SeqCst) {
            restore();
        }
        // Hooks cannot be replaced while unwinding.
        if !thread::panicking()
            && let Some(previous) = self.previous_hook.take()
        {
            panic::set_hook(Box::new(move |info| previous(info)));
        }
        let suppressed =
            std::mem::take(&mut *SUPPRESSED.lock().unwrap_or_else(PoisonError::into_inner));
        for message in suppressed {
            eprintln!("git-yard: worker {message}");
        }
    }
}

/// Terminal input on a thread of its own.
pub struct Input {
    pub events: Receiver<io::Result<Event>>,
    stopped: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl Drop for Input {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::Relaxed);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

pub fn input() -> Input {
    let (sender, receiver) = unbounded();
    let stopped = Arc::new(AtomicBool::new(false));
    let stopping = stopped.clone();
    let worker = thread::spawn(move || {
        while !stopping.load(Ordering::Relaxed) {
            let event = match event::poll(Duration::from_millis(20)) {
                Ok(false) => continue,
                Ok(true) if stopping.load(Ordering::Relaxed) => return,
                Ok(true) => event::read(),
                Err(error) => Err(error),
            };
            let failed = event.is_err();
            if sender.send(event).is_err() || failed {
                return;
            }
        }
    });
    Input {
        events: receiver,
        stopped,
        worker: Some(worker),
    }
}

/// Termination signals as events: the UI quits gracefully on the first and
/// at once on the second; the caller re-raises after cleanup.
pub struct SignalEvents {
    pub events: Receiver<i32>,
    handle: signal_hook::iterator::Handle,
    worker: Option<JoinHandle<()>>,
}

impl SignalEvents {
    pub fn install() -> io::Result<Self> {
        let mut signals = Signals::new([SIGTERM, SIGHUP, SIGINT])?;
        let handle = signals.handle();
        let (sender, receiver) = unbounded();
        let worker = thread::spawn(move || {
            for signal in signals.forever() {
                if sender.send(signal).is_err() {
                    return;
                }
            }
        });
        Ok(Self {
            events: receiver,
            handle,
            worker: Some(worker),
        })
    }

    /// Stops listening and, if `signal` arrived, dies of it with the
    /// default disposition so the shell sees the usual status.
    pub fn finish(mut self, signal: Option<i32>) {
        self.handle.close();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
        if let Some(signal) = signal {
            let _ = signal_hook::low_level::emulate_default_handler(signal);
        }
    }
}
