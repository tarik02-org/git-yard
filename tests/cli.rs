//! The binary end to end, with its own config, cache and state directories.

mod common;

use std::ffi::OsString;
use std::fs;
use std::os::unix::ffi::OsStringExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use common::Env;

fn yard(env: &Env, cwd: &Path, args: &[&str]) -> Output {
    let home = env.dir.path().join("home");
    Command::new(env!("CARGO_BIN_EXE_git-yard"))
        .args(args)
        .current_dir(cwd)
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", &home)
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env("XDG_CACHE_HOME", env.dir.path().join("cache"))
        .env("XDG_STATE_HOME", env.dir.path().join("state"))
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .stdin(Stdio::null())
        .output()
        .unwrap()
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

#[test]
fn list_exits_partial_when_a_repository_cannot_be_read() {
    let env = Env::new();
    let main = env.repo("good");
    env.worktree(&main, "wt", &["-b", "wt"]);
    let bad = env.root.join("bad");
    fs::create_dir(&bad).unwrap();
    env.git(&bad, &["init", "-q", "--ref-format=reftable"]);

    let output = yard(&env, &env.root, &["list", "--no-measure", "."]);
    assert_eq!(output.status.code(), Some(3), "{}", text(&output.stderr));
    assert!(
        text(&output.stderr).contains("unsupported"),
        "{}",
        text(&output.stderr)
    );
    assert!(text(&output.stdout).contains("good.wt"));
}

#[test]
fn non_utf8_paths_survive_json_and_the_cache() {
    let Some(env) = Env::accepting_raw_names() else {
        eprintln!("skipped: no filesystem here accepts non-UTF-8 names");
        return;
    };
    let main = env.repo("project");
    let odd = PathBuf::from(OsString::from_vec(
        [env.root.as_os_str().as_encoded_bytes(), b"/caf\xe9"].concat(),
    ));
    let status = env
        .command(&main, &["worktree", "add", "-q", "-b", "odd"])
        .arg(&odd)
        .status()
        .unwrap();
    assert!(status.success());

    for run in 0..2 {
        let output = yard(&env, &env.root, &["list", "--json", "."]);
        assert_eq!(
            output.status.code(),
            Some(0),
            "run {run}: {}",
            text(&output.stderr)
        );
        let entries: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        let paths: Vec<&serde_json::Value> = entries
            .as_array()
            .unwrap()
            .iter()
            .map(|entry| &entry["path"])
            .collect();
        assert!(
            paths.iter().any(|path| path["bytes"].is_string()),
            "run {run}: {paths:?}"
        );
    }
    assert!(env.dir.path().join("cache/git-yard/cache.json").is_file());
}

#[test]
fn remove_takes_paths_and_keeps_the_branch() {
    let env = Env::new();
    let main = env.repo("project");
    let wt = env.worktree(&main, "done", &["-b", "done"]);

    let output = yard(&env, &env.root, &["remove", "project.done"]);
    assert_eq!(output.status.code(), Some(0), "{}", text(&output.stderr));
    assert!(!wt.exists());
    assert!(env.git_ok(&main, &["show-ref", "--verify", "refs/heads/done"]));

    let output = yard(&env, &env.root, &["remove", "--json", "nowhere"]);
    assert_eq!(output.status.code(), Some(3));
    assert!(text(&output.stdout).contains("no worktree is registered"));

    let output = yard(&env, &env.root, &["remove", "project"]);
    assert_eq!(output.status.code(), Some(3));
    assert!(text(&output.stdout).contains("not a linked worktree"));
}

#[test]
fn usage_errors_exit_64_and_help_exits_0() {
    let env = Env::new();
    assert_eq!(yard(&env, &env.root, &["bogus"]).status.code(), Some(64));
    assert_eq!(yard(&env, &env.root, &["--help"]).status.code(), Some(0));
    assert_eq!(
        yard(&env, &env.root, &["list", "missing"]).status.code(),
        Some(64)
    );
}
