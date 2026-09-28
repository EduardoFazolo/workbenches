# workbenches

`wb` makes full copies of a git repo, each on its own branch with its own block of ports, so you (or several AI agents) can work on and run a few versions of the same app at once.

```
~/code/myapp                        main          :3000
~/.workbenches/myapp/login-fix      login-fix     :3100
~/.workbenches/myapp/new-dashboard  new-dashboard :3110
```

## Why not `git worktree`

A worktree shares one `.git` with the original. That's why git won't check out the same branch twice, and why `.env`, `node_modules` and build caches don't come along: you reinstall and reconfigure every time.

A workbench is a copy of the whole folder, `.git` included. Any branch can be checked out in any copy, everything untracked comes along, and deleting one is deleting a folder. On filesystems with copy-on-write clones (APFS, btrfs, XFS) the copy shares disk with the original until files change.

## Install

```
cargo install --git https://github.com/EduardoFazolo/workbenches
```

## Use

```
wb new login-fix                  # copy this repo: branch login-fix, ports 3100-3109
wb run login-fix -- npm run dev   # run a command inside it with PORT=3100
cd "$(wb path login-fix)"         # or work in it directly (wb shell login-fix opens a shell)
wb ls                             # every copy: branch, port, what's running, changes
wb land login-fix                 # fetch its branch into the original repo
wb rm login-fix                   # stop what runs inside it and delete it
```

`--branch <b>` checks out an existing branch instead of creating one, including a branch that only exists on a remote. `--from <path>` copies a repo you're not in.

`wb run` runs the command as given, not through a shell, so `$PORT` in its arguments would be expanded by your own shell first. Wrap those in `sh -c` with single quotes:

```
wb run login-fix -- sh -c 'vite --port $PORT --strictPort'
```

For AI coding agents, `wb --agents` prints a usage guide in skill format:

```
mkdir -p ~/.claude/skills/workbenches && wb --agents > ~/.claude/skills/workbenches/SKILL.md
```

## Environment

`wb run`, `wb shell` and `.wb/setup` get these variables (`wb env <name>` prints them):

| Variable | Example | |
|---|---|---|
| `PORT`, `WB_PORT` | `3100` | First port of the copy's block of 10 |
| `WB_PORTS` | `3100-3109` | The whole block, for extra services |
| `COMPOSE_PROJECT_NAME` | `myapp-login-fix` | Keeps docker compose containers and volumes apart |
| `WB_NAME`, `WB_PROJECT` | `login-fix`, `myapp` | |
| `WB_PATH`, `WB_SOURCE` | | The copy, and the original |

`COMPOSE_PROJECT_NAME` is `<project>-<name>` when that's already a valid compose name. Otherwise it's cleaned up to one and gets a short hash, so `fix.a` and `fix-a` stay apart.

Settings: `WB_HOME` moves the copies (default `~/.workbenches`; keep it on the same disk as your repos, clones don't cross disks). `WB_COPY=1` is the same as `--copy`.

## What `wb new` does

1. **Refuses unsafe sources.** A linked worktree or submodule (the copy would share state with the original), a repo in the middle of a merge, rebase, cherry-pick or bisect, or one with a git command running.
2. **Copies the folder.** One `clonefile` call on macOS, a reflink per file on Linux. On a disk without clones it refuses rather than silently using gigabytes; `--copy` does a real copy.
3. **Makes the copied `.git` independent.** Drops the original's worktree list and stale lock files, fixes relative remotes and alternates, keeps your git identity (including one set by `includeIf "gitdir:..."`), and switches to the branch.
4. **Rewrites leftover paths.** Untracked text files that contain the original folder's path get the copy's path instead: Python venv scripts (otherwise `pip` installs into the original's venv), pnpm shims, git hooks, absolute symlinks. Committed files are never changed, only listed, including committed symlinks and files in submodules. Binary files are skipped. Pid and lock files left by processes running in the original are removed.
5. **Runs `.wb/setup`** if the repo has one.

## `.wb/setup`

An executable `.wb/setup` runs inside every new copy with the variables above. Use it for what a copy can't give you, like a separate database:

```sh
#!/bin/sh
createdb -T myapp_dev "myapp_$WB_NAME"
echo "DATABASE_URL=postgres://localhost/myapp_$WB_NAME" >> .env.local
```

If it fails, the copy is kept and `wb new` tells you how to re-run it. `--no-setup` skips it.

## `wb rm` doesn't lose work

It refuses when deleting would lose:

- uncommitted changes, untracked files included
- commits the original can't reach and no remote has: on a branch or tag, in the stash, or only in the reflog after a detached HEAD or a reset
- any of the above inside a submodule

If git can't answer, it refuses too. `wb land` or push, then `rm` again; `--force` deletes anyway.

Before deleting, it stops the processes running inside the copy. Shells are left alone, so a terminal tab that `cd`'d into it stays open. The folder is moved to `~/.workbenches/.trash` and deleted in the background, so `rm` returns at once.

## Speed

The copy itself is one fast call. Most of the time goes to step 4, which reads every small file to find leftover paths, so the time grows with the number of files, not their size. On an Apple Silicon Mac:

| Repo | Copy | Git fixes | Path scan | Total |
|---|---|---|---|---|
| 100k files, 391 MB | 1.0s | 0.4s | 2.1s | 3.5s |

A large `node_modules` (300k to 500k files) takes several seconds more.

## Limits

- **Windows support was written by AI and has never been run.** It compiles in CI, nothing more. Treat it as untested.
- `wb rm` checks what git knows about. Changes to ignored files (an edited `.env`, a local database file) aren't checked and go with the folder.
- `wb land` lands the repo's branch, not commits made inside submodules. Push those; `wb rm` refuses until you do.
- `wb rm` tells shells apart from dev servers by process name (`sh`, `bash`, `zsh`...). A server started as a shell loop (`sh -c 'while ...'`) keeps running.
- Caches that store absolute paths in binary files (CMake, Gradle's configuration cache) or key on the folder path (Bazel, Xcode DerivedData) start cold in each copy.
- Files that iCloud Drive or Dropbox hasn't downloaded can't be copied. Keep repos outside synced folders.

## Tests

```
cargo test
```

`tests/use_cases.rs` has one test per use case, run against the real binary in a sandbox (its own `HOME`, `WB_HOME` and git config). Most of them were written from this README and `wb --help` before reading the code, so they check what's promised, not what's implemented. A few unit tests next to the code cover rules easier to pin down directly: whole-path matching, name rules, compose names and port blocks.

## License

MIT
