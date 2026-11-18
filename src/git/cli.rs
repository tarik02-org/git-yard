//! The git command line, used only for mutations: `worktree add`,
//! `worktree remove` and `fetch`.

use std::ffi::OsStr;
use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::sync::OnceLock;

use anyhow::{Context, Result, anyhow, bail};

/// `git worktree remove` appeared in 2.17.
const MINIMUM: (u32, u32) = (2, 17);

/// `git -C <dir>` with no prompts, no fsmonitor daemon, and no stdin.
/// Callers put `--` before paths and user-supplied names.
pub fn git(dir: &Path) -> Command {
    let mut command = Command::new("git");
    command
        .arg("-C")
        .arg(dir)
        .args(["-c", "core.fsmonitor=false"])
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null());
    command
}

/// Runs `command`, capturing output; a failure carries git's last stderr line.
pub fn run(mut command: Command) -> Result<Output> {
    ensure_version()?;
    let output = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .context("running git")?;
    if !output.status.success() {
        return Err(anyhow!(failure(&command, &output)));
    }
    Ok(output)
}

pub fn failure(command: &Command, output: &Output) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr);
    let reason = stderr
        .lines()
        .map(str::trim)
        .rfind(|line| !line.is_empty())
        .map_or_else(|| output.status.to_string(), str::to_owned);
    let args: Vec<_> = command
        .get_args()
        .skip_while(|arg| *arg != OsStr::new("core.fsmonitor=false"))
        .skip(1)
        .take(2)
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect();
    format!("git {}: {reason}", args.join(" "))
}

pub fn ensure_version() -> Result<()> {
    static VERSION: OnceLock<Result<(u32, u32), String>> = OnceLock::new();
    let version = VERSION.get_or_init(|| {
        let output = Command::new("git")
            .arg("--version")
            .stdin(Stdio::null())
            .output()
            .map_err(|error| format!("running git: {error}"))?;
        parse_version(&String::from_utf8_lossy(&output.stdout))
            .ok_or_else(|| "cannot read git version".to_owned())
    });
    match version {
        Ok(version) if *version >= MINIMUM => Ok(()),
        Ok((major, minor)) => bail!(
            "git {major}.{minor} is too old; {}.{} or newer is required",
            MINIMUM.0,
            MINIMUM.1
        ),
        Err(error) => bail!("{error}"),
    }
}

fn parse_version(text: &str) -> Option<(u32, u32)> {
    let version = text.trim().strip_prefix("git version ")?;
    let mut parts = version.split(|c: char| !c.is_ascii_digit());
    Some((parts.next()?.parse().ok()?, parts.next()?.parse().ok()?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_versions() {
        assert_eq!(parse_version("git version 2.54.0\n"), Some((2, 54)));
        assert_eq!(
            parse_version("git version 2.39.5 (Apple Git-154)"),
            Some((2, 39))
        );
        assert_eq!(parse_version("nonsense"), None);
    }
}
