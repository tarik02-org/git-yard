# git-yard

Pick and clean up Git worktrees across projects.

## Installation

Download a prebuilt archive from [Releases](https://github.com/tarik02-org/git-yard/releases/latest), extract it, and put `git-yard` on your `PATH`. Linux x86_64/ARM64 and macOS Apple Silicon are supported. [Nightlies](https://github.com/tarik02-org/git-yard/releases) are prereleases.

With Nix: `nix profile add github:tarik02-org/git-yard`.

From source: `cargo install --git https://github.com/tarik02-org/git-yard --locked`.

## Usage

```sh
git-yard gc ~/work             # ranked cleanup TUI
git-yard pick                  # search projects, worktrees, branches and PRs/MRs
git-yard pick --json           # select without creating or switching worktrees
git-yard pick --tmux           # session per project, window per worktree
git-yard pick -- nvim {path}    # open with a command
git-yard list --json ~/work    # inspect candidates
git-yard                      # help
```

To change directory from your shell:

```sh
p() { local dir; dir="$(git-yard pick "$@")" && cd "$dir"; }
```

Cleanup: arrows or `j`/`k` move, `J`/`K` move the selection cutoff, Space toggles a row, `D` deletes checked rows, `xx` deletes the focused row, Enter shows details, `q` quits. Deletion is permanent; branches are kept. Main, locked and unsafe worktrees are blocked, and every removal is revalidated.

Picker: type to search, arrows move, Enter opens, Tab scopes to a project, Ctrl-O creates a branch, Esc clears or quits. It uses Worktrunk when installed, otherwise Git. `gh`/`glab` enable PR/MR discovery; visible projects refresh in the background.

Optional `~/.config/git-yard/config.toml`, or `.git-yard.toml` found upward from the current directory:

```toml
roots = ["~/work"]
stale_days = 14

[pick]
switcher = "auto"                 # auto, wt, git
worktree_path = "{main}.{branch}" # git backend; branch slashes become dashes
# post_create = 'direnv allow'     # git backend; runs in the new worktree
```

Roots are relative to the config file. Hooks receive `GIT_YARD_PATH` and `GIT_YARD_BRANCH` as environment variables. Cache and removal journals use the platform's standard cache/state directories under `git-yard`.

## License

[MIT](LICENSE).
