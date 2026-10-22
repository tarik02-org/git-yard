//! tmux handoff: one session per project, one window per worktree, found
//! again by a window option holding the worktree path.

use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};

use super::launch::Switched;
use super::listing::Project;

const PATH_OPTION: &str = "@git_yard_path";

/// tmux reserves `.` and `:` in target names; `#` would start a format.
pub fn session_name(label: &str) -> String {
    label.replace(['.', ':', '#'], "_")
}

/// `-c` and `-n` values are format-expanded by tmux: a literal `#` is `##`.
fn literal(text: &str) -> String {
    text.replace('#', "##")
}

fn window_name(switched: &Switched) -> String {
    switched.branch.clone().unwrap_or_else(|| {
        switched
            .path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned()
    })
}

/// Arguments creating the session or window for `switched`.
pub fn create_args(session: &str, exists: bool, switched: &Switched) -> Vec<String> {
    let path = literal(&switched.path.to_string_lossy());
    let name = literal(&window_name(switched));
    let mut args: Vec<String> = if exists {
        vec![
            "new-window".into(),
            "-d".into(),
            "-t".into(),
            format!("={session}:"),
        ]
    } else {
        vec![
            "new-session".into(),
            "-d".into(),
            "-s".into(),
            session.into(),
        ]
    };
    args.extend(["-P", "-F", "#{window_id}", "-c"].map(String::from));
    args.extend([path, "-n".into(), name]);
    args
}

pub fn open(project: &Project, switched: &Switched) -> Result<()> {
    let session = session_name(&project.label);
    let exact = format!("={session}");
    let path = switched.path.to_string_lossy().into_owned();

    let exists = Command::new("tmux")
        .args(["has-session", "-t", &exact])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .context("running tmux")?
        .success();
    let existing = if exists {
        let windows = output(&[
            "list-windows",
            "-t",
            &exact,
            "-F",
            &format!("#{{window_id}}\t#{{{PATH_OPTION}}}"),
        ])?;
        windows.lines().find_map(|line| {
            let (id, window_path) = line.split_once('\t')?;
            (window_path == path).then(|| id.to_owned())
        })
    } else {
        None
    };
    let window = match existing {
        Some(window) => window,
        None => {
            let args = create_args(&session, exists, switched);
            output(&args.iter().map(String::as_str).collect::<Vec<_>>())?
        }
    };
    output(&["set-option", "-w", "-t", &window, PATH_OPTION, &path])?;
    output(&["select-window", "-t", &window])?;

    if std::env::var_os("TMUX").is_some() {
        output(&["switch-client", "-t", &window])?;
        Ok(())
    } else {
        let error = Command::new("tmux")
            .args(["attach-session", "-t", &window])
            .exec();
        Err(error).context("attaching to tmux")
    }
}

fn output(args: &[&str]) -> Result<String> {
    let output = Command::new("tmux")
        .args(args)
        .stdin(Stdio::null())
        .output()
        .context("running tmux")?;
    if !output.status.success() {
        bail!(
            "tmux {}: {}",
            args[0],
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hashes_are_escaped_only_where_tmux_expands_formats() {
        let switched = Switched {
            path: "/w/c#.feat".into(),
            branch: Some("fix/#12".into()),
        };
        assert_eq!(session_name("ns/c#.d:e"), "ns/c__d_e");
        assert_eq!(
            create_args("ns", true, &switched),
            [
                "new-window",
                "-d",
                "-t",
                "=ns:",
                "-P",
                "-F",
                "#{window_id}",
                "-c",
                "/w/c##.feat",
                "-n",
                "fix/##12"
            ]
        );
        assert_eq!(
            create_args("ns", false, &switched)[..4],
            ["new-session", "-d", "-s", "ns"]
        );
    }
}
