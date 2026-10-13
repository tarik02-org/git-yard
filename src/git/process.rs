//! Supervised background commands (fetches, `gh`, `glab`): each runs in its
//! own session without a terminal, so prompts fail instead of drawing over
//! the UI; it is killed on timeout and when the supervisor shuts down.
//! Mutations never run here.

use std::collections::HashMap;
use std::io::Read;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow, bail};

use crate::core::ids::RepoKey;

/// Time between SIGTERM and SIGKILL.
const GRACE: Duration = Duration::from_secs(2);

#[derive(Default)]
pub struct Supervisor {
    running: Mutex<HashMap<u64, (libc::pid_t, Option<RepoKey>)>>,
    next: AtomicU64,
    shut: AtomicBool,
}

impl Supervisor {
    /// Runs `command` in `dir` and returns its stdout. A non-zero exit
    /// carries the last line of stderr.
    pub fn run(
        &self,
        mut command: Command,
        dir: &Path,
        timeout: Duration,
        tag: Option<&RepoKey>,
    ) -> Result<Vec<u8>> {
        command
            .current_dir(dir)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        // SAFETY: setsid is async-signal-safe and touches no parent state.
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let mut child = {
            let mut running = self.running.lock().unwrap_or_else(PoisonError::into_inner);
            if self.shut.load(Ordering::SeqCst) {
                bail!("shutting down");
            }
            let child = command.spawn()?;
            running.insert(id, (child.id() as libc::pid_t, tag.cloned()));
            child
        };
        let result = wait(&mut child, timeout);
        self.running
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&id);
        result
    }

    /// Kills every command tagged with `repo`, such as a fetch of the
    /// repository the user is switching to.
    pub fn kill_tagged(&self, repo: &RepoKey) {
        let ids: Vec<u64> = self
            .running
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .filter(|(_, (_, tag))| tag.as_ref() == Some(repo))
            .map(|(id, _)| *id)
            .collect();
        self.terminate(&ids);
    }

    /// Refuses new commands and kills the running ones.
    pub fn shutdown(&self) {
        let ids: Vec<u64> = {
            let running = self.running.lock().unwrap_or_else(PoisonError::into_inner);
            self.shut.store(true, Ordering::SeqCst);
            running.keys().copied().collect()
        };
        self.terminate(&ids);
    }

    /// SIGTERM to each process group; SIGKILL to those whose command has
    /// not been reaped by its `run` call after the grace period.
    fn terminate(&self, ids: &[u64]) {
        let pid_of = |id: &u64| {
            self.running
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .get(id)
                .map(|(pid, _)| *pid)
        };
        for pid in ids.iter().filter_map(pid_of) {
            signal_group(pid, libc::SIGTERM);
        }
        let deadline = Instant::now() + GRACE;
        while ids.iter().any(|id| pid_of(id).is_some()) && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(20));
        }
        for pid in ids.iter().filter_map(pid_of) {
            signal_group(pid, libc::SIGKILL);
        }
    }
}

fn signal_group(pid: libc::pid_t, signal: libc::c_int) {
    // SAFETY: each child leads its own process group after setsid.
    unsafe { libc::kill(-pid, signal) };
}

fn wait(child: &mut Child, timeout: Duration) -> Result<Vec<u8>> {
    let mut stdout = child.stdout.take().expect("piped stdout");
    let mut stderr = child.stderr.take().expect("piped stderr");
    let out = thread::spawn(move || {
        let mut buffer = Vec::new();
        let _ = stdout.read_to_end(&mut buffer);
        buffer
    });
    let err = thread::spawn(move || {
        let mut buffer = Vec::new();
        let _ = stderr.read_to_end(&mut buffer);
        buffer
    });
    let deadline = Instant::now() + timeout;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if Instant::now() >= deadline {
            let pid = child.id() as libc::pid_t;
            signal_group(pid, libc::SIGTERM);
            let grace = Instant::now() + GRACE;
            while child.try_wait()?.is_none() && Instant::now() < grace {
                thread::sleep(Duration::from_millis(20));
            }
            signal_group(pid, libc::SIGKILL);
            let _ = child.wait();
            bail!("timed out after {}s", timeout.as_secs());
        }
        thread::sleep(Duration::from_millis(20));
    };
    let out = out.join().unwrap_or_default();
    let err = err.join().unwrap_or_default();
    if !status.success() {
        let err = String::from_utf8_lossy(&err);
        let reason = err
            .lines()
            .map(str::trim)
            .rfind(|line| !line.is_empty())
            .map_or_else(|| status.to_string(), str::to_owned);
        return Err(anyhow!(reason));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sh(script: &str) -> Command {
        let mut command = Command::new("sh");
        command.args(["-c", script]);
        command
    }

    #[test]
    fn captures_output_and_failures() {
        let supervisor = Supervisor::default();
        let dir = std::env::temp_dir();
        let out = supervisor
            .run(sh("echo hi"), &dir, Duration::from_secs(5), None)
            .unwrap();
        assert_eq!(out, b"hi\n");
        let error = supervisor
            .run(
                sh("echo bad >&2; exit 3"),
                &dir,
                Duration::from_secs(5),
                None,
            )
            .unwrap_err();
        assert_eq!(error.to_string(), "bad");
    }

    #[test]
    fn has_no_terminal_and_is_killed_with_its_children() {
        let supervisor = Supervisor::default();
        let dir = std::env::temp_dir();
        // No controlling terminal: opening /dev/tty fails.
        let error = supervisor
            .run(sh(": < /dev/tty"), &dir, Duration::from_secs(5), None)
            .unwrap_err();
        assert!(!error.to_string().is_empty());

        let started = Instant::now();
        let error = supervisor
            .run(
                sh("sleep 30 & sleep 30"),
                &dir,
                Duration::from_millis(200),
                None,
            )
            .unwrap_err();
        assert!(error.to_string().contains("timed out"));
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn shutdown_kills_running_and_refuses_new_commands() {
        let supervisor = std::sync::Arc::new(Supervisor::default());
        let repo = RepoKey::new("/r/.git");
        let running = {
            let (supervisor, repo) = (supervisor.clone(), repo.clone());
            thread::spawn(move || {
                supervisor.run(
                    sh("sleep 30"),
                    &std::env::temp_dir(),
                    Duration::from_secs(60),
                    Some(&repo),
                )
            })
        };
        thread::sleep(Duration::from_millis(200));
        supervisor.kill_tagged(&repo);
        assert!(running.join().unwrap().is_err());
        supervisor.shutdown();
        assert!(
            supervisor
                .run(
                    sh("true"),
                    &std::env::temp_dir(),
                    Duration::from_secs(5),
                    None
                )
                .is_err()
        );
    }
}
