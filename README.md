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
wb ls                             # this repo's copies: branch, port, what's serving, changes
wb land login-fix                 # fetch its branch into the original repo
rm -rf "$(wb path login-fix)"     # delete it when you're done (stop its servers first)
```

`--branch <b>` checks out an existing branch instead of creating one, including a branch that only exists on a remote (like `git switch`). `--from <path>` copies a repo you're not in.

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
| `COMPOSE_PROJECT_NAME` | `myapp-login-fix-3f9a1c2e` | Keeps docker compose containers and volumes apart |
| `WB_NAME`, `WB_PROJECT` | `login-fix`, `myapp` | |
| `WB_PATH`, `WB_SOURCE` | | The copy, and the original |

`COMPOSE_PROJECT_NAME` is `<project>-<name>`, lowercased, plus a short hash, so two workbenches don't end up sharing containers.

Settings: `WB_HOME` moves the copies (default `~/.workbenches`; keep it on the same disk as your repos, clones don't cross disks). `WB_COPY=1` is the same as `--copy`.

## What `wb new` does

1. **Refuses sources a copy can't be independent from.** A linked worktree or submodule, a repo that borrows objects from another (`clone --shared`), one in the middle of a merge, rebase, cherry-pick or bisect, or one with a git command running.
2. **Copies the folder.** One `clonefile` call on macOS, a reflink per file on Linux. On a disk without clones it refuses rather than silently using gigabytes; `--copy` does a real copy.
3. **Makes the copied `.git` its own.** Drops the original's worktree list and stale lock files, fixes relative remotes, keeps your git identity (including one set by `includeIf "gitdir:..."`), and switches to the branch. Your repo's git hooks don't run during these steps (a copied hook can still point at the original); run anything a hook would do, like a post-checkout step, from `.wb/setup`.
4. **Fixes paths in three known places.** Some tools write the repo's absolute path into files they generate, and a copy of those would run or write into the original. `wb` rewrites the original's path to the copy's in:
   - Python virtualenvs: `pyvenv.cfg`, the scripts in `bin/`, `.pth` files (otherwise `pip` installs into the original's venv)
   - `node_modules/.bin` (package manager shims)
   - `.git/hooks`

   Nothing else is touched. It also deletes untracked `*.pid` files and `.next/dev/lock`, left by servers running in the original.
5. **Runs `.wb/setup`** if the repo has one.

Until the copy is done, `wb ls` shows it as `creating` and other commands refuse it. If `wb new` is interrupted, delete the folder; a creation that hasn't finished in an hour stops counting anyway.

## `.wb/setup`

An executable `.wb/setup` runs inside every new copy with the variables above. Use it for what `wb` doesn't do, like a separate database, or a config file with an absolute path:

```sh
#!/bin/sh
createdb -T myapp_dev "myapp_$WB_NAME"
echo "DATABASE_URL=postgres://localhost/myapp_$WB_NAME" >> .env.local
```

If it fails, the copy is kept and `wb new` tells you how to re-run it. `--no-setup` skips it.

## Removing a workbench

`wb` doesn't remove workbenches: you delete the folder, and that's permanent. Which changes are work and which are noise depends on the project, so that call stays with you (or your agent). Inside the workbench:

```sh
git status --short                          # uncommitted files: commit or discard
git log --oneline HEAD --not --remotes      # commits no remote has: push or wb land
git stash list --format='%gd %cr: %s'       # only entries younger than the workbench are new
```

The copy starts with every stash the original had, so a long stash list is normal. `wb ls` saying `clean` only means no uncommitted files.

Then stop what runs from it. Servers leave children behind, so look for both kinds:

```sh
D="$(wb path login-fix)"
lsof -d cwd 2>/dev/null | grep -F "$D"          # started inside it
ps -eo pid,args | grep -F "$D" | grep -v grep   # its path in their command line
```

Kill the ones you started, then `rm -rf "$D"`. Once the folder is gone, `wb ls` stops listing it, and its name and ports can be used again.

## Speed

On an Apple Silicon Mac, `wb new` on a repo with 100k files (391 MB) takes about 1.5s. Most of it is the copy, which grows with the number of files, not their size.

## Limits

- **Windows support was written by AI and has never been run.** It compiles in CI, nothing more. Treat it as untested.
- Paths are fixed only in the three places above. Any other file or symlink holding the original's absolute path still points at it; fix those in `.wb/setup`.
- `wb land` lands the repo's branch, not commits made inside submodules. Push those first.
- Caches that store absolute paths in binary files (CMake, Gradle's configuration cache) or key on the folder path (Bazel, Xcode DerivedData) start cold in each copy.
- Files that iCloud Drive or Dropbox hasn't downloaded can't be copied. Keep repos outside synced folders.

## Tests

```
cargo test
```

`tests/use_cases.rs` has one test per use case, run against the real binary in a sandbox (its own `HOME`, `WB_HOME` and git config). Most of them were written from this README and `wb --help` before reading the code, so they check what's promised, not what's implemented. A few unit tests next to the code cover rules easier to pin down directly: path matching, name rules, compose names and port blocks.

## License

MIT
