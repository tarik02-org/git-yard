# git-yard

Pick and clean up Git worktrees across projects.

- `git-yard pick` finds every worktree, branch, remote branch and open pull/merge request under your project roots, and opens one: prints its path, runs a command in it, or switches tmux to it.
- `git-yard gc` ranks linked worktrees by how worthwhile removing them is and removes the ones you choose, revalidating each one first.

## Installation

Prebuilt archives for Linux x86_64/ARM64 (static) and macOS Apple Silicon are attached to [GitHub releases](https://github.com/tarik02-org/git-yard/releases); [nightlies](https://github.com/tarik02-org/git-yard/releases) are marked as prereleases. Until the first release is published, install with Nix or Cargo:

```sh
nix profile add github:tarik02-org/git-yard
cargo install git-yard --git https://github.com/tarik02-org/git-yard --locked
```

git-yard needs `git` 2.17 or newer on `PATH`. `gh` and `glab` add pull/merge requests to the picker; Worktrunk (`wt`) is used to create worktrees when installed.

Home Manager, with `inputs.git-yard.url = "github:tarik02-org/git-yard"`:

```nix
imports = [ inputs.git-yard.homeManagerModules.default ];
programs.git-yard = {
  enable = true;
  settings.roots = [ "~/work" ];
};
```

The module installs completions and adds a `gyp` function to Bash, Zsh and Fish that picks and changes directory. Turn helpers off with `enableBashIntegration`, `enableZshIntegration` or `enableFishIntegration`.

## Usage

```sh
git-yard pick                   # pick and print the path
cd "$(git-yard pick)"           # …and go there
git-yard pick --tmux            # session per project, window per worktree
git-yard pick -- nvim {path}    # run a command; {path}, {project}, {branch}
git-yard pick --json            # print the selection without switching
git-yard gc ~/work              # cleanup UI
git-yard list --json ~/work     # every candidate with its facts and rating
git-yard remove ../app.old-fix  # revalidate and remove by path
git-yard journal                # recover interrupted removals
git-yard completions zsh        # also bash, fish, elvish, powershell
```

Without paths, commands scan the configured roots, else the repository around the current directory, else the directory itself. Global flags: `--config <file>` and `--no-cache` (neither read nor write cached observations).

`list` takes `--json`, `--no-measure` (skip sizes) and `--timings`. `remove` takes one or more worktree paths (a deleted directory can still be named), `--allow-dirty` to remove worktrees with changes, and `--json`.

Exit codes: 0 success, 1 error, 3 partial (some repositories could not be read, or some removals were not done), 64 usage error, 130 picker dismissed. Killed by SIGTERM or SIGHUP, git-yard cleans up and exits with the usual 143/129.

### Picker keys

| Key | Action |
| --- | --- |
| typing, Left/Right | edit the search |
| Up/Down, Ctrl-P/Ctrl-N, Ctrl-K/Ctrl-J, PgUp/PgDn, wheel | move |
| Enter, double click | open |
| Alt-Enter | open and print the path, whatever the handoff |
| Tab | limit to the selected project; again to leave |
| Shift-Tab, Backspace on an empty search | leave the project |
| Ctrl-O | create a new branch from the selection |
| Esc | clear the search, leave the project, quit |
| Ctrl-C | quit |

Without a search the picker shows worktrees, base branches and recent picks; a search covers every branch, remote branch and request. Projects among the top results are fetched in the background (at most every `fetch_minutes`) and their requests refreshed. Background commands never prompt and are stopped before anything is switched.

### Cleanup keys

| Key | Action |
| --- | --- |
| j/k, Up/Down, Ctrl-D/Ctrl-U, PgUp/PgDn, g/G, Home/End, wheel | move |
| J/K | move the line down/up |
| = | put the line below the focused row |
| Space, click the box | check or uncheck a row |
| D | delete every checked row |
| x x | delete the focused row now |
| c | cancel a queued removal |
| / | filter; Esc clears |
| r / R | rerank / reset the line and choices |
| a | also show main worktrees and blocked rows |
| Enter, Tab | details |
| C | hide removed rows |
| p | pause size measurement |
| ? | all keys |
| q, Ctrl-C | quit; with removals running, wait for them (press again to force) |
| drag the line | move the line |

Recommended rows (merged or squashed, clean, not protected, stale) above the line start checked; the line first sits below the top `batch_size` of them so a batch stays reviewable however long the list is, and moves with the ranking until you act on the list. Any removable row can be checked on either side. Main worktrees and blocked rows are hidden until `a`. A choice you make about a row lapses when its facts change (a new commit, edited files), so nothing is removed on stale evidence.

## What removal does

Main worktrees and bare repositories are never removed, and branches are always kept. A linked worktree is blocked while it is locked, has an operation in progress, holds private submodule repositories, has a detached HEAD with commits nothing else reaches, shares its directory with another registration, or its path is a symlink, a file, or sits on a missing (unmounted) parent. Dirty worktrees are removable but never pre-checked.

Each removal is checked again against what you saw, then the directory is moved aside with an atomic rename and inspected there: it is put back if it contains another repository, a mount, a special file, or anything changed after the assessment. Only after `git worktree remove` has dropped the registration are the files deleted. Every step is journaled; after a crash, `git-yard journal` (or the next `gc`/`remove`) puts directories back where it safely can and names any it leaves for you.

**Files ignored by `.gitignore` (build output, `node_modules`, local `.env` files) are deleted with the worktree.** They are counted in the details pane but not otherwise protected.

## Configuration

`~/.config/git-yard/config.toml` (`~/Library/Application Support/git-yard/config.toml` on macOS), or the file given with `--config`. Files inside scanned projects are never read.

```toml
roots = ["~/work"]          # relative to this file
max_depth = 4               # directory levels searched below a root
stale_days = 14             # younger use or commits keep a row unchecked
batch_size = 20             # recommended rows the line starts below
protected_branches = ["main", "master", "develop", "dev", "trunk", "staging", "production"]

[weights]                   # rating, finite and ≥ 0, not all zero
inactivity = 0.45
commit_age = 0.2
size = 0.2
integration = 0.15
protected_factor = 0.25     # multiplier for protected branches

[pick]
switcher = "auto"                 # auto (wt if installed), wt, git
worktree_path = "{main}.{branch}" # git switcher; branch slashes become dashes
# post_create = 'direnv allow'    # git switcher; runs in the new worktree
fetch_minutes = 10
request_minutes = 5
jobs = 4
```

`post_create` receives `GIT_YARD_PATH` and `GIT_YARD_BRANCH` in its environment. Caches and the removal journal live in the platform's cache and state directories under `git-yard`.

## License

[MIT](LICENSE).
