# workbenches

Instant, independent copies of a git repo. Each copy is on its own branch, has its own ports, and can run its own dev server, all at the same time.

```
~/code/myapp                        main          :3000
~/.workbenches/myapp/login-fix      login-fix     :3100
~/.workbenches/myapp/new-dashboard  new-dashboard :3110
```

It isn't a git worktree. Each workbench is a full copy of your folder with its own `.git`, so:

- any branch can be checked out anywhere, even the same one in several places
- `.env`, `node_modules`, build caches and uncommitted work all come along
- deleting one is deleting a folder, with nothing left behind in your repo

On APFS (macOS), btrfs/XFS (Linux) and ReFS/Dev Drive (Windows), copies are copy-on-write: a 10 GB repo copies in a couple of seconds and uses almost no disk until files change.

## Install

```
cargo install --path .
```

## Use

```
wb new login-fix                    # copy this repo, branch login-fix, ports 3100-3109
wb run login-fix -- npm run dev     # run anything inside it, with PORT set
wb run login-fix -- sh -c 'vite --port $PORT'   # sh -c to use $PORT in args
wb shell login-fix                  # or open a shell in it
cd "$(wb path login-fix)"           # or just go there

wb ls                               # what exists, what's running, what changed
wb land login-fix                   # bring its branch back into the original repo
wb rm login-fix                     # stop its processes, delete it
```

`wb --help` is the full guide and `wb <command> --help` explains each command. `wb --agents` prints a complete usage guide for AI agents, as a skill file:

```
mkdir -p ~/.claude/skills/workbenches && wb --agents > ~/.claude/skills/workbenches/SKILL.md
```

`wb new x --branch some-existing-branch` checks out an existing branch instead of making one. A branch that only exists on a remote is checked out tracking it, like `git switch` does (origin wins when several remotes have it).

`wb rm` refuses when the workbench would lose work: uncommitted changes (untracked files included), or commits the original can't reach and no remote has, whether on a branch, a tag, in the stash or only in the reflog. Submodules are checked too, and if git can't answer, it refuses rather than guess. Land or push first, or use `--force`. It stops the processes running inside the copy but leaves shells alone, so a terminal tab `cd`'d into it stays open.

## What `wb new` does

1. **Checks the source.** It refuses a linked worktree or submodule (the copy would share its branch with the original), and a repo that's mid-merge, mid-rebase or has a running git command.
2. **Copies the folder**, in one `clonefile` call on macOS, or reflinking each file elsewhere.
3. **Makes the copied `.git` independent.**
   - Drops the original's worktree list.
   - Removes stale lock, pid and socket files.
   - Fixes relative remotes and alternates.
   - Keeps your git identity, even if it came from an `includeIf "gitdir:..."`.
   - Refreshes the index so the first `git status` is instant.
   - Switches to the branch.
4. **Rewrites leftover paths.** Untracked text files that still contain the original folder's path get the copy's path instead. This fixes Python venv scripts (otherwise `pip` installs into the original's venv), pnpm shims, Bundler config, editable installs, git hooks and absolute symlinks, without knowing any of those tools.
   - Committed files are never modified, only reported. That includes committed symlinks and files in submodules or nested repos. If git can't list a repo's committed files (a broken vendored `.git`, say), nothing in that repo is rewritten, and `wb` says so.
   - Binary files are never touched.
5. **Removes pid and lock files** left by processes running in the original, so the copy's dev server doesn't think it's already running.
6. **Runs `.wb/setup`** if your repo has one (see below).

## Environment

`wb run`, `wb shell` and `.wb/setup` get these (print them with `wb env <name>`):

| var | example | |
|---|---|---|
| `PORT`, `WB_PORT` | `3100` | first port of this workbench's block |
| `WB_PORTS` | `3100-3109` | the whole block, for monorepos with several apps |
| `COMPOSE_PROJECT_NAME` | `myapp-login-fix` | keeps docker compose containers and volumes apart |
| `WB_NAME`, `WB_PROJECT` | `login-fix`, `myapp` | |
| `WB_PATH`, `WB_SOURCE` | | this copy, and the original folder |

`wb run` runs the command as given, never through a shell. Frameworks that read `PORT` (Next.js, Rails, Express...) just work. Ones that don't (Vite) need the port as an argument: `wb run x -- sh -c 'vite --port $PORT --strictPort'`. Use single quotes, or your own shell expands `$PORT` before `wb` sees it.

`COMPOSE_PROJECT_NAME` is `<project>-<name>` when that's already a valid Compose name (lowercase letters, digits, `-`, `_`). Otherwise it's cleaned up to one and a short hash of the real name is appended, so `fix.a`, `fix-a` and `Fix-a` never share containers.

## `.wb/setup` (optional)

An executable `.wb/setup` in your repo runs inside every new workbench with the env above. On Windows it's `setup.cmd`, `setup.bat` or `setup.ps1`. If it fails, `wb new` still succeeds and keeps the workbench, and tells you how to re-run the hook. Use it for what can't be generic, like a separate database:

```sh
#!/bin/sh
createdb -T myapp_dev "myapp_$WB_NAME"
echo "DATABASE_URL=postgres://localhost/myapp_$WB_NAME" >> .env.local
```

## Several repos in one folder

Run `wb new x` from a folder that isn't a repo itself but holds several (e.g. `frontend/` and `backend/`). The whole folder gets copied and every repo in it gets branch `x`.

## Where things live

Copies go to `~/.workbenches/<project>/<name>`, and `WB_HOME` changes that. Nothing is written into your repo.

- **Same disk.** Copy-on-write only works on one disk, so keep `WB_HOME` on the same disk as your repos.
- **Filesystems without copy-on-write** (ext4, NTFS): `wb` refuses rather than silently copying gigabytes. Pass `--copy` or set `WB_COPY=1`.
- **Deleting is instant.** `wb rm` moves the folder to `~/.workbenches/.trash` and deletes it in the background.
- **macOS:** `~/.workbenches` is excluded from Spotlight and Time Machine.

## Known limits

- Build folders that store absolute paths inside binary files will rebuild from cold in the copy (CMake, Gradle's configuration cache). Bazel and Xcode's DerivedData key their caches by folder path, so each copy starts cold.
- Folders synced by iCloud Drive or Dropbox can have files that aren't downloaded. Keep repos outside them.
- `wb rm` checks git's view of the copy. Ignored files (an edited `.env`, a local database file) aren't part of it and go with the folder.
- `wb land` brings back each repo's branch, not commits made inside submodules. Push those; `wb rm` refuses until you do.
- Tested on macOS (APFS) and Linux. Windows compiles (CI checks it) but hasn't been run yet.

## Tests

```
cargo test
```

- `tests/use_cases.rs`: one test per use case, written from this README and `wb --help` alone, black-box against the real binary. The list of use cases is at the top of the file. Each test runs in its own sandbox (temp `HOME`, `WB_HOME` and git config), so nothing touches your real setup.
- Unit tests next to the code cover the few rules worth pinning down directly: whole-path matching when rewriting, name validation, compose names and port blocks.
