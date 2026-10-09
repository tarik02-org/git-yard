//! Opening a picked target: a switcher (Worktrunk, or plain Git) switches
//! to the worktree, creating it if needed, then the result is handed to the
//! shell, tmux or a command.

use std::io;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result, anyhow, bail, ensure};
use serde::{Deserialize, Serialize};

use super::forge::{self, Forge, Request};
use super::{Project, Target};
use crate::config::{Pick, Switcher};

static NEXT_FETCH: AtomicU64 = AtomicU64::new(0);

pub enum Action {
    Open,
    /// A new branch named `name` based on the target.
    Create {
        name: String,
    },
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Switched {
    pub path: PathBuf,
    pub branch: Option<String>,
}

/// Rejects combinations no switcher can carry out.
pub fn validate(
    target: &Target,
    request: Option<&(Forge, Request)>,
    action: &Action,
) -> Result<()> {
    if let Action::Create { name } = action {
        ensure!(
            !name.starts_with('-')
                && name != "HEAD"
                && git2::Reference::is_valid_name(&format!("refs/heads/{name}")),
            "invalid branch name: {name}"
        );
    }
    match (target, action) {
        (Target::Worktree { branch: None, .. }, Action::Create { .. }) => {
            bail!("a detached worktree cannot be a base")
        }
        (Target::Request { .. }, Action::Create { .. }) => {
            bail!("open the request first, then branch off its worktree")
        }
        (Target::Request { number }, Action::Open) if request.is_none() => {
            bail!("unknown forge for request {number}")
        }
        _ => Ok(()),
    }
}

/// `auto` uses Worktrunk when it is installed.
pub fn resolve(switcher: Switcher) -> Switcher {
    match switcher {
        Switcher::Auto if on_path("wt") => Switcher::Wt,
        Switcher::Auto => Switcher::Git,
        other => other,
    }
}

fn on_path(program: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|path| {
        std::env::split_paths(&path).any(|dir| {
            use std::os::unix::fs::PermissionsExt;
            dir.join(program).metadata().is_ok_and(|metadata| {
                metadata.is_file() && metadata.permissions().mode() & 0o111 != 0
            })
        })
    })
}

pub fn switch(
    config: &Pick,
    project: &Project,
    target: &Target,
    request: Option<&(Forge, Request)>,
    action: &Action,
) -> Result<Switched> {
    validate(target, request, action)?;
    match resolve(config.switcher) {
        Switcher::Wt => wt_switch(project, target, request.map(|(forge, _)| *forge), action),
        Switcher::Git | Switcher::Auto => git_switch(config, project, target, request, action),
    }
}

/// The `wt switch` arguments for a target.
pub fn wt_args(target: &Target, forge: Option<Forge>, action: &Action) -> Result<Vec<String>> {
    let reference = match target {
        Target::Worktree { path, branch } => match action {
            Action::Open => path.display().to_string(),
            Action::Create { .. } => branch
                .clone()
                .ok_or_else(|| anyhow!("a detached worktree cannot be a base"))?,
        },
        Target::Local { branch } => branch.clone(),
        Target::Remote { remote, branch } => format!("{remote}/{branch}"),
        Target::Request { number } => match (action, forge) {
            (Action::Open, Some(forge)) => forge.shortcut(*number),
            (Action::Open, None) => bail!("unknown forge for request {number}"),
            (Action::Create { .. }, _) => {
                bail!("open the request first, then branch off its worktree")
            }
        },
    };
    Ok(match action {
        Action::Open => vec![reference],
        Action::Create { name } => {
            vec!["--create".into(), name.clone(), "--base".into(), reference]
        }
    })
}

/// Runs `wt switch` in the project. Its prompts and hook output use the
/// terminal; only the JSON result is captured.
fn wt_switch(
    project: &Project,
    target: &Target,
    forge: Option<Forge>,
    action: &Action,
) -> Result<Switched> {
    let args = wt_args(target, forge, action)?;
    let output = Command::new("wt")
        .arg("-C")
        .arg(&project.main_path)
        .arg("switch")
        .args(&args)
        .args(["--no-cd", "--format", "json"])
        .stdin(Stdio::inherit())
        .stderr(Stdio::inherit())
        .stdout(Stdio::piped())
        .output()
        .context("running wt")?;
    if !output.status.success() {
        bail!("wt switch {} failed ({})", args.join(" "), output.status);
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let line = text
        .lines()
        .rfind(|line| line.trim_start().starts_with('{'))
        .ok_or_else(|| anyhow!("wt printed no result"))?;
    serde_json::from_str(line).context("decoding wt result")
}

/// Creates worktrees with `git worktree add` at `worktree_path`, then runs
/// `post_create` in them.
fn git_switch(
    config: &Pick,
    project: &Project,
    target: &Target,
    request: Option<&(Forge, Request)>,
    action: &Action,
) -> Result<Switched> {
    let main = &project.main_path;
    // Branch of the new worktree, `git worktree add` options, and the
    // commit-ish to check out.
    let (branch, options, commitish): (String, Vec<String>, String) = match (action, target) {
        (Action::Open, Target::Worktree { path, branch }) => {
            return Ok(Switched {
                path: path.clone(),
                branch: branch.clone(),
            });
        }
        (Action::Open, Target::Local { branch }) => (branch.clone(), vec![], branch.clone()),
        (Action::Open, Target::Remote { remote, branch }) => {
            if local_branch_exists(main, branch)? {
                bail!("local branch {branch} already exists; select it instead");
            }
            let options = vec!["--track".into(), "-b".into(), branch.clone()];
            (
                branch.clone(),
                options,
                format!("refs/remotes/{remote}/{branch}"),
            )
        }
        (Action::Open, Target::Request { number }) => {
            let (forge, request) = request.context("request details missing")?;
            let local = if local_branch_exists(main, &request.branch)? {
                format!("{}-{number}", forge.ref_kind())
            } else {
                request.branch.clone()
            };
            let remote = forge::remote_for(project).with_context(|| {
                format!("no remote on {forge:?} to fetch request {number} from")
            })?;
            if local_branch_exists(main, &local)? {
                bail!("local branch {local} already exists; select it or rename it first");
            }
            let path = worktree_path(&config.worktree_path, main, &local);
            if path.try_exists()? {
                bail!("{} already exists", path.display());
            }
            let commit = fetch_request(main, &remote.name, &forge.head_ref(*number))?;
            let options = vec!["--no-track".into(), "-b".into(), local.clone()];
            (local, options, commit)
        }
        (Action::Create { name }, target) => {
            let base = match target {
                Target::Worktree {
                    branch: Some(branch),
                    ..
                }
                | Target::Local { branch } => format!("refs/heads/{branch}"),
                Target::Remote { remote, branch } => format!("refs/remotes/{remote}/{branch}"),
                Target::Worktree { branch: None, .. } | Target::Request { .. } => {
                    unreachable!("rejected by validate")
                }
            };
            let options = vec!["--no-track".into(), "-b".into(), name.clone()];
            (name.clone(), options, base)
        }
    };

    let path = worktree_path(&config.worktree_path, main, &branch);
    if path.try_exists()? || path.symlink_metadata().is_ok() {
        bail!("{} already exists", path.display());
    }
    let path_text = path.display().to_string();
    let mut args: Vec<&str> = vec!["worktree", "add"];
    args.extend(options.iter().map(String::as_str));
    args.extend(["--", path_text.as_str(), commitish.as_str()]);
    git(main, &args)?;

    if let Some(command) = &config.post_create {
        let status = Command::new("sh")
            .args(["-c", command])
            .env("GIT_YARD_PATH", &path)
            .env("GIT_YARD_BRANCH", &branch)
            .current_dir(&path)
            .stdout(io::stderr())
            .status()
            .context("running post_create")?;
        if !status.success() {
            eprintln!("git-yard: post_create failed ({status}); the worktree was created");
        }
    }
    Ok(Switched {
        path,
        branch: Some(branch),
    })
}

/// Expands `{main}` (main worktree path) and `{branch}` (slashes become
/// dashes, as Worktrunk does). Relative results are relative to the main
/// worktree's parent.
pub fn worktree_path(template: &str, main: &Path, branch: &str) -> PathBuf {
    let text = template
        .replace("{main}", &main.display().to_string())
        .replace("{branch}", &branch.replace(['/', '\\'], "-"));
    let path = match (text.strip_prefix("~/"), dirs::home_dir()) {
        (Some(rest), Some(home)) => home.join(rest),
        _ => PathBuf::from(text),
    };
    if path.is_absolute() {
        path
    } else {
        main.parent().unwrap_or(main).join(path)
    }
}

fn local_branch_exists(main: &Path, branch: &str) -> Result<bool> {
    let status = Command::new("git")
        .arg("-C")
        .arg(main)
        .args([
            "show-ref",
            "--verify",
            "--quiet",
            &format!("refs/heads/{branch}"),
        ])
        .status()
        .context("checking local branch")?;
    match status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => bail!("checking local branch {branch} failed ({status})"),
    }
}

fn fetch_request(main: &Path, remote: &str, head_ref: &str) -> Result<String> {
    // Fetch into a private ref so background fetches cannot replace our base,
    // and request checkouts never update an existing user branch.
    let temporary = format!(
        "refs/git-yard/requests/{}-{}",
        std::process::id(),
        NEXT_FETCH.fetch_add(1, Ordering::Relaxed)
    );
    git(
        main,
        &[
            "fetch",
            "--no-write-fetch-head",
            "--",
            remote,
            &format!("{head_ref}:{temporary}"),
        ],
    )?;
    let output = Command::new("git")
        .arg("-C")
        .arg(main)
        .args(["rev-parse", "--verify", &format!("{temporary}^{{commit}}")])
        .output()
        .context("resolving fetched request")?;
    git(main, &["update-ref", "-d", &temporary])?;
    if !output.status.success() {
        bail!("request head is not a commit");
    }
    Ok(String::from_utf8(output.stdout)?.trim().to_owned())
}

/// Runs git with the terminal for prompts and progress; its stdout goes to
/// stderr so ours carries only the result.
fn git(dir: &Path, args: &[&str]) -> Result<()> {
    let status = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .stdout(io::stderr())
        .status()
        .context("running git")?;
    if !status.success() {
        bail!("git {} failed ({status})", args.join(" "));
    }
    Ok(())
}

pub enum Handoff {
    /// Print the path, for `cd "$(git-yard pick)"`.
    Print,
    /// One session per project, one window per worktree.
    Tmux,
    /// Replace the process with this command, placeholders filled in.
    Command(Vec<String>),
}

pub fn hand_off(handoff: &Handoff, project: &Project, switched: &Switched) -> Result<()> {
    match handoff {
        Handoff::Print => {
            println!("{}", switched.path.display());
            Ok(())
        }
        Handoff::Command(command) => {
            let fill = |arg: &String| {
                arg.replace("{path}", &switched.path.display().to_string())
                    .replace("{project}", &project.label)
                    .replace("{branch}", switched.branch.as_deref().unwrap_or(""))
            };
            let mut args = command.iter().map(fill);
            let program = args.next().context("empty command")?;
            let error = Command::new(&program)
                .args(args)
                .current_dir(&switched.path)
                .exec();
            Err(error).with_context(|| format!("running {program}"))
        }
        Handoff::Tmux => tmux(project, switched),
    }
}

/// tmux reserves `.` and `:` in target names.
pub fn session_name(label: &str) -> String {
    label.replace(['.', ':'], "_")
}

const PATH_OPTION: &str = "@git_yard_path";

fn tmux(project: &Project, switched: &Switched) -> Result<()> {
    let session = session_name(&project.label);
    let exact = format!("={session}");
    let path = switched.path.display().to_string();
    let name = window_name(switched);

    let exists = Command::new("tmux")
        .args(["has-session", "-t", &exact])
        .stderr(Stdio::null())
        .status()
        .context("running tmux")?
        .success();
    let window = if exists {
        let windows = tmux_output(&[
            "list-windows",
            "-t",
            &exact,
            "-F",
            &format!("#{{window_id}}\t#{{{PATH_OPTION}}}"),
        ])?;
        let found = windows.lines().find_map(|line| {
            let (id, window_path) = line.split_once('\t')?;
            (window_path == path).then(|| id.to_owned())
        });
        match found {
            Some(id) => id,
            None => tmux_output(&[
                "new-window",
                "-d",
                "-P",
                "-F",
                "#{window_id}",
                "-t",
                &format!("{exact}:"),
                "-c",
                &path,
                "-n",
                &name,
            ])?,
        }
    } else {
        tmux_output(&[
            "new-session",
            "-d",
            "-P",
            "-F",
            "#{window_id}",
            "-s",
            &session,
            "-c",
            &path,
            "-n",
            &name,
        ])?
    };
    tmux_output(&["set-option", "-w", "-t", &window, PATH_OPTION, &path])?;
    tmux_output(&["select-window", "-t", &window])?;

    if std::env::var_os("TMUX").is_some() {
        tmux_output(&["switch-client", "-t", &exact])?;
        Ok(())
    } else {
        let error = Command::new("tmux")
            .args(["attach-session", "-t", &exact])
            .exec();
        Err(error).context("attaching to tmux")
    }
}

fn window_name(switched: &Switched) -> String {
    switched.branch.clone().unwrap_or_else(|| {
        Path::new(&switched.path)
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned()
    })
}

fn tmux_output(args: &[&str]) -> Result<String> {
    let output = Command::new("tmux")
        .args(args)
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
    fn targets_map_to_worktrunk_arguments() {
        let remote = Target::Remote {
            remote: "alice".into(),
            branch: "fix/x".into(),
        };
        assert_eq!(
            wt_args(&remote, None, &Action::Open).unwrap(),
            ["alice/fix/x"]
        );
        let request = Target::Request { number: 7 };
        assert_eq!(
            wt_args(&request, Some(Forge::GitLab), &Action::Open).unwrap(),
            ["mr:7"]
        );
        let main = Target::Worktree {
            path: "/r".into(),
            branch: Some("main".into()),
        };
        assert_eq!(
            wt_args(
                &main,
                None,
                &Action::Create {
                    name: "feat".into()
                }
            )
            .unwrap(),
            ["--create", "feat", "--base", "main"]
        );
        assert!(wt_args(&request, None, &Action::Create { name: "x".into() }).is_err());
    }
}
