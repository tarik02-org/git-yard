//! Exit codes and termination signals for non-interactive commands.

use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use signal_hook::consts::{SIGHUP, SIGINT, SIGTERM};

pub use crate::core::error::exit::*;

pub fn code(code: u8) -> ExitCode {
    ExitCode::from(code)
}

/// Records the first SIGTERM, SIGHUP or SIGINT instead of dying, so work in
/// progress can reach a safe point. `finish` then re-raises it, and the
/// shell sees the usual 143/129/130.
pub struct Signals {
    received: Arc<AtomicUsize>,
    ids: Vec<signal_hook::SigId>,
}

impl Signals {
    pub fn install() -> anyhow::Result<Self> {
        let received = Arc::new(AtomicUsize::new(0));
        let mut ids = Vec::new();
        for signal in [SIGTERM, SIGHUP, SIGINT] {
            let received = received.clone();
            // SAFETY: the handler only performs an atomic store.
            let id = unsafe {
                signal_hook::low_level::register(signal, move || {
                    let _ = received.compare_exchange(
                        0,
                        signal as usize,
                        Ordering::SeqCst,
                        Ordering::SeqCst,
                    );
                })
            }?;
            ids.push(id);
        }
        Ok(Self { received, ids })
    }

    pub fn received(&self) -> Option<i32> {
        match self.received.load(Ordering::SeqCst) {
            0 => None,
            signal => Some(signal as i32),
        }
    }

    /// Restores default handling and, if a signal arrived, dies of it.
    pub fn finish(self, code: ExitCode) -> ExitCode {
        let received = self.received();
        for id in self.ids {
            signal_hook::low_level::unregister(id);
        }
        if let Some(signal) = received {
            let _ = signal_hook::low_level::emulate_default_handler(signal);
            return ExitCode::from(128 + signal as u8);
        }
        code
    }
}
