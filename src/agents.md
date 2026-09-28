---
name: workbenches
description: Use `wb` (workbenches) to work on a git repo in an isolated copy with its own branch and ports, so several agents and the human can edit, run dev servers and test at the same time without touching each other. Use when asked to work in parallel, in isolation, in a workbench, on a separate branch without disturbing the current checkout, or to run a second copy of an app. Covers creating, running, verifying, landing and deleting workbenches, and the mistakes to avoid.
---

# workbenches (`wb`)

`wb` makes independent copies of a git repo, called **workbenches**. Each one is a full copy of the repo folder, including its own `.git`, `.env`, `node_modules`, build caches and any uncommitted changes. It sits on its own branch and owns a block of 10 ports. Several can run dev servers at the same time as the original.

A workbench is **not** a git worktree. Never use `git worktree` commands with it.

## The golden rules

1. **One task, one workbench.** Name it after the task: `wb new fix-login`. The branch gets the same name.
2. **Work only inside the workbench folder.** Get it with `wb path <name>`. Never edit files in the original repo (`WB_SOURCE`) while working in a workbench.
3. **Never hardcode ports.** Start servers with `wb run <name> -- <cmd>` so `PORT` is set. To pass the port as an argument, wrap the command in `sh -c` with single quotes so `$PORT` expands inside the workbench: `wb run <name> -- sh -c 'npx vite --port $PORT --strictPort'`.
4. **Commit inside the workbench**, then `wb land <name>` or `git push`. Uncommitted work is never landed.
5. **Save work before `wb rm`, and never `rm -rf` a workbench yourself.** `wb rm` doesn't block: it moves the copy to the trash for 3 days. Deciding what's work is your job (see "Removing a workbench").
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
| `wb rm <name>` | Stop processes running inside it and move it to the trash (kept 3 days); lists what wasn't saved |

When two repos have workbenches with the same name, use `project/name`. `wb ls --all` shows them.

## Environment inside a workbench

`wb run`, `wb shell` and `.wb/setup` get:

| Var | Example | Meaning |
|---|---|---|
| `PORT` / `WB_PORT` | `3100` | First port of this workbench's block. Use it for the main dev server. |
| `WB_PORTS` | `3100-3109` | All 10 ports. Use `WB_PORT+1`, `+2`, ... for extra services (API, worker, storybook). |
| `COMPOSE_PROJECT_NAME` | `myapp-fix-login-3f9a1c2e` | Makes `docker compose` containers, networks and volumes separate |
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
    fixed   12 venv/shim/hook file(s) that pointed at the original
    removed 1 pid/lock file(s) of servers running in the original
    note    kept your git identity from the original (user.email)
```

- **`fixed`**: `wb` rewrote the original's path in Python venvs, `node_modules/.bin` and `.git/hooks`. It fixes only those. Any other file or symlink holding the original's absolute path (a config, an IDE setting) still points at `WB_SOURCE`: if one matters for your task, tell the user rather than editing a committed file.
- **`.wb/setup failed`**: the workbench exists, but its setup script didn't complete. Read the error, fix it, then re-run with `wb run <name> -- sh .wb/setup`.

## When `wb new` refuses

| Message | Meaning | What to do |
|---|---|---|
| linked worktree or submodule | You're in a git worktree or submodule; a copy would share its branch | Run from the main checkout, or use `--from <main repo path>` |
| middle of a merge / rebase / cherry-pick / bisect | The repo has an operation in progress | Tell the user; don't finish or abort it on your own |
| borrows objects from another repo | It was made with `clone --shared` or `--reference` | Tell the user; the message has the command that makes it self-contained |
| a git command is running (index.lock) | Another git process is active, or crashed | Wait and retry. Delete `.git/index.lock` only if the user confirms nothing is running. |
| can't make free copy-on-write copies ... would use X of real space | This disk has no copy-on-write | Ask the user before using `--copy` (it costs X of real disk) |
| workbench 'x' already exists | The name is taken | Pick another name, or `wb ls` to see if it's yours to reuse |
| bad name | Bad workbench name | Use letters, digits, `-`, `_`, `.` (not starting with `.` or `-`) |
| isn't a valid git branch name | Bad `--branch` | Use letters, digits, `-`, `_`, `.`, `/` |

## Removing a workbench

`wb rm` doesn't check whether the work is safe to remove; you do, because only you know which changes are real work in this project. Before `wb rm <name>`:

1. `git status` in the workbench. Commit real work. Changes a tool made on its own (a dev server rewriting `AGENTS.md`, a regenerated lockfile, build output) can be discarded with `git checkout -- <file>` or left; say which in your report.
2. `git push` the branch, or `wb land <name>`. Check `git log origin/<branch>..<branch>` is empty if you pushed.
3. Unsure whether something matters? Ask the user before removing.
4. `wb rm <name>`. Read its output: it lists anything that wasn't saved elsewhere (uncommitted files, stash entries, unpushed branches). If it lists something you didn't expect, tell the user where the copy went.

A removed copy stays in `~/.workbenches/.trash` for at least 3 days: each `wb rm` deletes trashed copies older than that, and nothing else does. To recover it, move its folder out of the trash; it's a normal git repo. If `wb rm` can't move the copy to the trash, it deletes nothing and says so.

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

If the repo contains an executable `.wb/setup`, it runs inside every new workbench with the env above. Use it for things that can't be copied, and suggest adding one if the user hits a shared-database problem:

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
- **`creating` in `wb ls`** means a `wb new` is still copying, or was interrupted. Other commands refuse that workbench until it's ready. If it stays that way, ask the user before `wb rm <name>`.
- **`origin` and your git identity came along,** so `git push` and commits work normally.
- **Copies live in `~/.workbenches/<project>/<name>`** (or `$WB_HOME`). Nothing is written into the original repo.
- **`wb rm` returns at once.** It moves the folder to `~/.workbenches/.trash`. Later `wb rm` runs delete trashed copies older than 3 days.
- **Installing packages in a workbench** (`npm install`, `pip install`) affects only that workbench.

## Install this guide as a skill

```sh
mkdir -p ~/.claude/skills/workbenches && wb --agents > ~/.claude/skills/workbenches/SKILL.md
```
