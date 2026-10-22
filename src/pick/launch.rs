//! Opening a picked target: a switcher (Worktrunk, or plain Git) switches
//! to the worktree, creating it if needed; the result is then handed to the
//! shell, tmux or a command.

use std::io;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};

use crate::core::config::{Pick, Switcher};
use crate::git::cli;
use crate::git::refs::validate_branch_name;
use crate::git::remote::Forge;

use super::forge::Request;
use super::listing::{Project, Target};
use super::tmux;

static NEXT_FETCH: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Debug, PartialEq, Eq)]
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
        validate_branch_name(name).map_err(|error| anyhow!(error))?;
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
    use std::os::unix::fs::PermissionsExt;
    std::env::var_os("PATH").is_some_and(|path| {
        std::env::split_paths(&path).any(|dir| {
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
        Switcher::Wt => wt_switch(project, target, request.map(|(forge, _)| forge), action),
        Switcher::Git | Switcher::Auto => git_switch(config, project, target, request, action),
    }
}

/// The `wt switch` arguments for a target.
pub fn wt_args(target: &Target, forge: Option<&Forge>, action: &Action) -> Result<Vec<String>> {
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
            (Action::Open, Some(forge)) => forge.kind.shortcut(*number),
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
    forge: Option<&Forge>,
    action: &Action,
) -> Result<Switched> {
    let args = wt_args(target, forge, action)?;
    let output = Command::new("wt")
        .arg("-C")
        .arg(project.main_path.as_path())
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

/// The local branch a remote branch opens as: its own name, or
/// `<remote>-<branch>` when a different local branch already has the name.
pub fn local_name_for_remote(
    remote: &str,
    branch: &str,
    exists: impl Fn(&str) -> Result<bool>,
) -> Result<String> {
    if !exists(branch)? {
        return Ok(branch.to_owned());
    }
    let fallback = format!("{}-{branch}", remote.replace('/', "-"));
    if exists(&fallback)? {
        bail!("local branches {branch} and {fallback} already exist; select one instead");
    }
    Ok(fallback)
}

/// Creates worktrees with `git worktree add` at `worktree_path`, then runs
/// `post_create` from the user's config in them.
fn git_switch(
    config: &Pick,
    project: &Project,
    target: &Target,
    request: Option<&(Forge, Request)>,
    action: &Action,
) -> Result<Switched> {
    let main = project.main_path.as_path();
    let exists = |branch: &str| local_branch_exists(main, branch);
    // Branch of the new worktree, `git worktree add` options, and the
    // commit-ish to check out.
    let (branch, options, commitish): (String, Vec<String>, String) = match (action, target) {
        (Action::Open, Target::Worktree { path, branch }) => {
            return Ok(Switched {
                path: path.to_path_buf(),
                branch: branch.clone(),
            });
        }
        (Action::Open, Target::Local { branch }) => (branch.clone(), vec![], branch.clone()),
        (Action::Open, Target::Remote { remote, branch }) => {
            let local = local_name_for_remote(remote, branch, exists)?;
            let options = vec!["--track".into(), "-b".into(), local.clone()];
            (local, options, format!("refs/remotes/{remote}/{branch}"))
        }
        (Action::Open, Target::Request { number }) => {
            let (forge, request) = request.context("request details missing")?;
            let local = if exists(&request.branch)? {
                format!("{}-{number}", forge.kind.ref_kind())
            } else {
                request.branch.clone()
            };
            if exists(&local)? {
                bail!("local branch {local} already exists; select it or rename it first");
            }
            let path = worktree_path(&config.worktree_path, main, &local);
            if path.symlink_metadata().is_ok() {
                bail!("{} already exists", path.display());
            }
            // Fetched from the remote the request list came from.
            let commit = fetch_request(main, &forge.remote, &forge.kind.head_ref(*number))?;
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
    if path.symlink_metadata().is_ok() {
        bail!("{} already exists", path.display());
    }
    let mut command = cli::git(main);
    command
        .args(["worktree", "add"])
        .args(&options)
        .arg("--")
        .arg(&path)
        .arg(&commitish);
    foreground(command)?;

    if let Some(script) = &config.post_create {
        let status = Command::new("sh")
            .args(["-c", script])
            .env("GIT_YARD_PATH", &path)
            .env("GIT_YARD_BRANCH", &branch)
            .current_dir(&path)
            .stdin(Stdio::null())
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
    let path = crate::core::config::expand_home(Path::new(&text));
    if path.is_absolute() {
        path
    } else {
        main.parent().unwrap_or(main).join(path)
    }
}

fn local_branch_exists(main: &Path, branch: &str) -> Result<bool> {
    let mut command = cli::git(main);
    command.args(["show-ref", "--verify", "--quiet", "--"]);
    command.arg(format!("refs/heads/{branch}"));
    let status = command
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .context("checking local branch")?;
    match status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => bail!("checking local branch {branch} failed ({status})"),
    }
}

/// Fetches into a private ref so background fetches cannot replace it and
/// request checkouts never update an existing branch.
fn fetch_request(main: &Path, remote: &str, head_ref: &str) -> Result<String> {
    let temporary = format!(
        "refs/git-yard/requests/{}-{}",
        std::process::id(),
        NEXT_FETCH.fetch_add(1, Ordering::Relaxed)
    );
    let mut fetch = cli::git(main);
    fetch
        .args(["fetch", "--no-write-fetch-head", "--", remote])
        .arg(format!("{head_ref}:{temporary}"));
    foreground(fetch)?;
    let mut parse = cli::git(main);
    parse.args(["rev-parse", "--verify", &format!("{temporary}^{{commit}}")]);
    let parsed = cli::run(parse);
    let mut delete = cli::git(main);
    delete.args(["update-ref", "-d", &temporary]);
    foreground(delete)?;
    let output = parsed.context("request head is not a commit")?;
    Ok(String::from_utf8(output.stdout)?.trim().to_owned())
}

/// Runs git with its output on stderr, so stdout carries only our result.
fn foreground(mut command: Command) -> Result<()> {
    cli::ensure_version()?;
    let status = command
        .stdout(io::stderr())
        .status()
        .context("running git")?;
    if !status.success() {
        bail!(
            "git {} failed ({status})",
            command
                .get_args()
                .skip(4)
                .take(2)
                .map(|arg| arg.to_string_lossy().into_owned())
                .collect::<Vec<_>>()
                .join(" ")
        );
    }
    Ok(())
}

#[derive(Clone, Debug, PartialEq, Eq)]
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
            use std::io::Write;
            let mut out = std::io::stdout().lock();
            out.write_all(switched.path.as_os_str().as_encoded_bytes())?;
            out.write_all(b"\n")?;
            Ok(())
        }
        Handoff::Command(command) => {
            let fill = |arg: &String| {
                arg.replace("{path}", &switched.path.to_string_lossy())
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
        Handoff::Tmux => tmux::open(project, switched),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::remote::forge;

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
        let gitlab = forge("origin", "https://gitlab.com/o/r").unwrap();
        assert_eq!(
            wt_args(&request, Some(&gitlab), &Action::Open).unwrap(),
            ["mr:7"]
        );
        let main = Target::Worktree {
            path: crate::core::ids::YardPath::new("/r"),
            branch: Some("main".into()),
        };
        let create = Action::Create {
            name: "feat".into(),
        };
        assert_eq!(
            wt_args(&main, None, &create).unwrap(),
            ["--create", "feat", "--base", "main"]
        );
        assert!(wt_args(&request, None, &create).is_err());
    }

    #[test]
    fn shadowed_remote_branches_get_a_fallback_name() {
        let existing = ["fix"];
        let exists = |name: &str| Ok(existing.contains(&name));
        assert_eq!(local_name_for_remote("up", "new", exists).unwrap(), "new");
        assert_eq!(
            local_name_for_remote("up", "fix", exists).unwrap(),
            "up-fix"
        );
        let both = |name: &str| Ok(["fix", "up-fix"].contains(&name));
        assert!(local_name_for_remote("up", "fix", both).is_err());
    }

    #[test]
    fn branch_names_are_validated_before_switching() {
        let target = Target::Local {
            branch: "main".into(),
        };
        let bad = Action::Create {
            name: "--force".into(),
        };
        assert!(validate(&target, None, &bad).is_err());
    }
}
