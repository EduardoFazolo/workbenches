---
name: workbenches
description: Use `wb` (workbenches) to work on a git repo in an isolated copy with its own branch and ports, so several agents and the human can edit, run dev servers and test at the same time without touching each other. Use when asked to work in parallel, in isolation, in a workbench, on a separate branch without disturbing the current checkout, or to run a second copy of an app. Covers creating, running, verifying, landing and deleting workbenches, and the mistakes to avoid.
---

# workbenches (`wb`)

`wb` makes instant, independent copies of a git repo called **workbenches**. Each one is a full copy of the repo folder, including its own `.git`, `.env`, `node_modules`, build caches and any uncommitted changes. It sits on its own branch and owns a block of 10 ports. Several can run dev servers at the same time as the original.

A workbench is **not** a git worktree. Never use `git worktree` commands with it.

## The golden rules

1. **One task, one workbench.** Name it after the task: `wb new fix-login`. The branch gets the same name.
2. **Work only inside the workbench folder.** Get it with `wb path <name>`. Never edit files in the original repo (`WB_SOURCE`) while working in a workbench.
3. **Never hardcode ports.** Start servers with `wb run <name> -- <cmd>` so `PORT` is set. To pass the port as an argument, wrap the command in `sh -c` with single quotes so `$PORT` expands inside the workbench: `wb run <name> -- sh -c 'npx vite --port $PORT --strictPort'`.
4. **Commit inside the workbench**, then `wb land <name>` or `git push`. Uncommitted work is never landed.
5. **Never `wb rm --force` and never `rm -rf` a workbench yourself.** If `wb rm` refuses, work would be lost. Report that to the user and let them decide.
6. **Read `wb` output and errors in full.** They say exactly what happened and what to do next.

## Commands

| Command | What it does |
|---|---|
| `wb new <name>` | Copy the repo you're in into a new workbench on branch `<name>` (created from current HEAD) |
| `wb new <name> --branch <b>` | Same, but check out branch `<b>`. An existing branch is checked out, not recreated, including one that only exists on a remote (it then tracks it). |
| `wb new <name> --from <path>` | Copy a repo you're not currently in |
| `wb ls` | This repo's workbenches: branch, first port, status (`serving :3100` / `N processes` / `idle`), uncommitted files |
| `wb ls --all` | Every workbench of every repo (shown as `project/name`) |
| `wb path <name>` | Print the folder path |
| `wb run <name> -- <cmd...>` | Run a command in the workbench folder with its env; returns the command's exit code |
| `wb run <name> -- sh -c '<cmd with $PORT>'` | `$PORT` and the other `WB_*` vars expand to the workbench's values (the command itself never goes through a shell) |
| `wb shell <name>` | Interactive shell in the workbench (humans; agents should use `wb run` or `cd`) |
| `wb env <name>` | Print the env as `export` lines (`eval "$(wb env <name>)"`) |
| `wb land <name>` | Fetch the workbench's current branch into the original repo |
| `wb rm <name>` | Stop processes running inside it and delete it; refuses if work would be lost |

When two repos have workbenches with the same name, use `project/name`. `wb ls --all` shows them.

## Environment inside a workbench

`wb run`, `wb shell` and `.wb/setup` get:

| Var | Example | Meaning |
|---|---|---|
| `PORT` / `WB_PORT` | `3100` | First port of this workbench's block. Use it for the main dev server. |
| `WB_PORTS` | `3100-3109` | All 10 ports. Use `WB_PORT+1`, `+2`, ... for extra services (API, worker, storybook). |
| `COMPOSE_PROJECT_NAME` | `myapp-fix-login` | Makes `docker compose` containers, networks and volumes separate |
| `WB_NAME` / `WB_PROJECT` | `fix-login` / `myapp` | Identity |
| `WB_PATH` | | This workbench's folder |
| `WB_SOURCE` | | The original repo. Read it for reference only; don't write to it. |

## Standard workflow

```sh
wb ls                                   # 1. see what exists; don't reuse another agent's workbench
wb new fix-login                        # 2. create; read the whole output
cd "$(wb path fix-login)"               # 3. every edit and git command happens here
# ... edit, test ...
git add -A && git commit -m "Fix login redirect"   # 4. commit inside the workbench
wb land fix-login                       # 5a. bring the branch into the original repo
# or: git push -u origin fix-login      # 5b. push and open a PR (origin came along)
wb rm fix-login                         # 6. only when the user is done with it
```

Keep the workbench after landing if the user may want to review or run it. Removing it is the user's call unless they asked you to clean up.

## Running a dev server

Servers block, so start them in the background and log to a file **outside** the repo, or to a gitignored path:

```sh
wb run fix-login -- npm run dev > /tmp/wb-fix-login.log 2>&1 &
PORT=$(wb env fix-login | sed -n "s/^export PORT='\(.*\)'/\1/p")
for i in $(seq 60); do curl -sf "localhost:$PORT" >/dev/null && break; sleep 1; done
curl -s "localhost:$PORT" | head      # verify it's YOUR server
wb ls                                 # STATUS should say: serving :<port>
```

- **Next.js, Rails, Express, CRA, Remix, Astro** read `PORT` directly.
- **Vite ignores `PORT`**, and when the port is busy it silently moves to the next one. Use `wb run <name> -- sh -c 'npx vite --port $PORT --strictPort'`. For other tools, check how they take a port and always pass it explicitly.
- **Monorepos:** give each app its own port from the block: `wb run <name> -- sh -c 'pnpm --filter api dev --port $((WB_PORT+1))'`, and so on.
- **Scripts with a hardcoded port** (`next dev -p 3000`): don't edit tracked config just for this. Call the underlying command with `$PORT` instead: `wb run <name> -- sh -c 'npx next dev -p $PORT'`.
- **Quoting matters.** `wb run x -- npx vite --port $PORT` passes YOUR shell's `$PORT`, which is usually empty or wrong. Use `sh -c '...'` with single quotes.
- **Never test against `localhost:3000`** or any port outside your block. That's the user's main server or another agent's.
- **Stopping:** stop your server (kill its pid, or let `wb rm` stop it) when you're done.

## Things a workbench does NOT isolate

Files, git state and ports are isolated. External services are **shared** unless the repo's `.wb/setup` provisions separate ones:

- **Databases.** Postgres, MySQL, Redis or Mongo named in `.env` point at the same dev database as the original. Before running migrations, seeds or destructive scripts, check `DATABASE_URL` (and similar) in the workbench.
  - If it's shared, tell the user before changing schema or data.
  - A `.wb/setup` can create a per-workbench database (see below).
  - SQLite files inside the repo are copied, so they're already separate.
- **Docker named volumes and external services.** `COMPOSE_PROJECT_NAME` separates compose projects, but anything outside compose (a globally running service, cloud resources, queues) is shared.
- **Browser cookies.** `localhost:3000` and `localhost:3100` share cookies, because cookies ignore the port. Logging in on one can log out the other. When testing two instances in a browser, use separate profiles or an incognito window for one.
- **Caches outside the repo.** Global package stores, `~/.cache`, and Xcode DerivedData or Bazel output (keyed by folder, so the first build in a workbench is cold).

## Reading `wb new` output

```
✓ fix-login ready in 1.4s (copy-on-write, uses almost no disk)
    path    ~/.workbenches/myapp/fix-login
    branch  fix-login
    ports   3100-3109  (PORT=3100)
    fixed   12 files/links that pointed at the original, removed 1 stale pid/lock/socket files
    note    kept your git identity from the original (user.email)
  ! 1 committed file(s) mention the original folder's path and were left as-is: .vscode/settings.json
```

- **`fixed`**: `wb` rewrote untracked files (venv scripts, shims, symlinks, hooks) that pointed at the original folder, so the workbench never runs or writes into the original. No action needed.
- **`!` committed files mention the original path**: those tracked files still point at the original folder. If one matters for your task (for example a config path), be aware that it references `WB_SOURCE`. Don't "fix" it in a commit unless the task calls for it.
- **`.wb/setup failed`**: the workbench exists, but its setup script didn't complete. Read the error, fix it, then re-run with `wb run <name> -- sh .wb/setup`.

## When `wb new` refuses

| Message | Meaning | What to do |
|---|---|---|
| linked worktree or submodule | You're in a git worktree or submodule; a copy would share its branch | Run from the main checkout, or use `--from <main repo path>` |
| middle of a merge / rebase / cherry-pick / bisect | The repo has an operation in progress | Tell the user; don't finish or abort it on your own |
| a git command is running (index.lock) | Another git process is active, or crashed | Wait and retry. Delete `.git/index.lock` only if the user confirms nothing is running. |
| can't make free copy-on-write copies ... would use X of real space | This disk has no copy-on-write | Ask the user before using `--copy` (it costs X of real disk) |
| workbench 'x' already exists | The name is taken | Pick another name, or `wb ls` to see if it's yours to reuse |
| bad name | Bad workbench name | Use letters, digits, `-`, `_`, `.` (not starting with `.` or `-`) |
| isn't a valid git branch name | Bad `--branch` | Use letters, digits, `-`, `_`, `.`, `/` |

## When `wb rm` refuses

It lists what would be lost: uncommitted changes (untracked files included), or commits the original repo can't reach and no remote has, whether on a branch, a tag, in the stash, or only in the reflog (after a detached HEAD or a reset). Submodules are checked too. If it says it couldn't check, git failed: report the error to the user.

1. Commit what matters inside the workbench.
2. Then `wb land <name>` or `git push`.
3. Then `wb rm <name>` again.

Only use `--force` when the user explicitly says to throw the work away.

## Landing

- **`wb land <name>`** fetches the workbench's **current branch** into the original repo under the same name. Only commits move.
- **If that branch is checked out in the original**, git won't move it under the user. `wb` fetches it and prints the `git merge FETCH_HEAD` command to run in the original. Ask the user before merging into their checked-out branch.
- **If it refuses because the original's branch has commits the workbench lacks**, update the workbench first:
  1. `git fetch "$WB_SOURCE" <branch>`, then `git merge FETCH_HEAD` (or rebase), inside the workbench.
  2. Land again.
- **Landing never pushes.** To publish, `git push` from the workbench or from the original.

## Several agents at once

- Each agent gets its own workbench and name. Check `wb ls` first, and never work inside another agent's workbench.
- Each workbench has its own branch, so there are no git conflicts while working. Conflicts only appear when landing or merging. Land one at a time, and run the tests after each.
- Keep the user informed of which workbench, branch and port you're using. `wb ls` is the shared status board.

## Optional per-repo setup: `.wb/setup`

If the repo contains an executable `.wb/setup` (Windows: `.wb/setup.cmd`, `.bat` or `.ps1`), it runs inside every new workbench with the env above. Use it for things that can't be copied, and suggest adding one if the user hits a shared-database problem:

```sh
#!/bin/sh
# .wb/setup: separate database per workbench
createdb -T myapp_dev "myapp_$WB_NAME" 2>/dev/null || true
printf 'DATABASE_URL=postgres://localhost/myapp_%s\nPORT=%s\n' "$WB_NAME" "$WB_PORT" >> .env.local
```

`wb new --no-setup` skips it.

## Facts that prevent wrong assumptions

- **The workbench starts as an exact copy** of the original, including its uncommitted changes and untracked files at that moment. Check `git status` in the workbench before committing, so you don't commit someone else's in-progress edits by accident.
- **Same branch name, different branches.** A branch named `main` in the workbench and in the original are separate after the copy. Commits in one appear in the other only through `wb land`, fetch, push or pull.
- **`origin` and your git identity came along,** so `git push` and commits work normally.
- **Copies live in `~/.workbenches/<project>/<name>`** (or `$WB_HOME`). Nothing is written into the original repo.
- **Deleting is instant.** `wb rm` moves the folder to a trash area and deletes it in the background.
- **Installing packages in a workbench** (`npm install`, `pip install`) affects only that workbench.

## Install this guide as a skill

```sh
mkdir -p ~/.claude/skills/workbenches && wb --agents > ~/.claude/skills/workbenches/SKILL.md
```
