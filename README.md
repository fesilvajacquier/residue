# server-monitor

A lazygit-style TUI for the residue that agents and git worktrees leave behind: dev servers
still listening after their worktree was deleted, stray processes in deleted directories, and
worktrees, branches and per-worktree databases that outlived their pull request.

Built for a workflow of many agents and many git worktrees. One view shows, per worktree, what
is running in it and which databases it owns, so removing it stops the servers, removes the
directory and branch, and drops the databases in one confirmed step.

## What it shows

```
┏ [1] Servers (5) ✗2 !1 ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━┓┌ Details ───────────────────────────────────┐
┃   PORT   PROJECT             BRANCH     AGE   MEM   CPU   ┃│Port     3001  bound on 127.0.0.1           │
┃ ! 3000   shop/wt-a1b2c3      feat/cart  10d   200M  0%    ┃│Status   ORPHAN — dir no longer exists      │
┃▶✗ 3001   shop/wt-d4e5f6      (deleted)  6d    92M   0%    ┃│Project  shop/wt-d4e5f6                     │
┃ ● 3002   shop/wt-778899      fix/login  13m   332M  2%    ┃│Dir      ~/wt/shop/wt-d4e5f6  [DELETED]     │
┃ ✗ *5000  api/wt-0a1b2c       (deleted)  11d   153M  0%    ┃│Worktree gone, git no longer lists it       │
┗━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━┛│Started  2026-09-14 17:45  (6d18h ago)      │
┌ [2] Strays: processes in deleted dirs (1) ────────────────┐│Memory   92M across 1 process(es)           │
│ PID     PROGRAM    PROCS  DIR             AGE   MEM       ││                                            │
│ 93544   sidekiq    1      api/wt-0a1b2c   11d   84M       ││Processes holding the port                  │
└───────────────────────────────────────────────────────────┘│2575648 puma  6d18h  92M  puma 8.0.2        │
┌ [3] Worktrees (6) ✓2 ?1 ──────────────────────────────────┐│                                            │
│   REPO   DIR         BRANCH      STATE     PORTS  DB  AGE ││                                            │
│ ● shop   main        main        main             -   2h  ││                                            │
│ ● shop   wt-778899   fix/login   tracking  3002   40M 13m ││                                            │
│ ✓ shop   wt-a1b2c3   feat/cart   merged    3000   40M 10d ││                                            │
│ ! shop   wt-0f9e8d   feat/tax    gone             38M 3d  ││                                            │
│ ✗ shop   wt-d4e5f6   -           db only          40M -   ││                                            │
│ ✗ api    wt-0a1b2c   -           db only   +1     22M -   ││                                            │
└───────────────────────────────────────────────────────────┘└────────────────────────────────────────────┘
 j/k move  tab panel  x remove  X force remove  d stop its procs  K clean all safe  f fetch  ? help  q quit
```

- **Servers**: one row per listening TCP port owned by you. Puma workers, Chrome helpers and
  other processes in the same directory are folded into the row.
- **Strays**: processes that are not listening on anything but whose working directory has been
  deleted (Sidekiq, Sass watchers, crashpad handlers left behind by a removed worktree).
- **Worktrees**: every git worktree under `~/code/<org>/<repo>`, with what is running in it, the
  databases named after it, and whether it can go. Databases whose worktree no longer exists show
  up as `db only` rows so they can be dropped from the same place.
- **Details**: everything known about the selection, including the worktree a server belongs to
  and, for a worktree, exactly what `x` would do to it.

Server markers:

| Marker | Meaning |
| --- | --- |
| `✗ ORPHAN` | the working directory no longer exists, typically a deleted worktree |
| `! STALE` | running longer than the stale threshold (24h by default) |
| `● OK` | nothing suspicious |
| `*PORT` | bound on all interfaces (`0.0.0.0` or `::`) |

Worktree states:

| State | Meaning | `x` |
| --- | --- | --- |
| `✓ merged` | remote branch deleted and every commit is in the default branch | removes |
| `✗ dir missing` | git still lists the worktree but the directory is gone | prunes |
| `✗ db only` | databases whose worktree no longer exists | drops them |
| `? gone` | remote branch deleted but local commits are not in the default branch | needs `X` |
| `● local` | never pushed, holds its own commits | needs `X` |
| `· no commits` | never pushed, nothing of its own; a session may be working in it | needs `X` |
| `● tracking` | its branch still exists on the remote | needs `X` |
| `!` | held: uncommitted changes (`X` overrides), locked, or the directory you launched from | blocked |

Removing a worktree stops the processes running in it, runs `git worktree remove --force`,
deletes the branch only when its remote is gone, and drops its databases with `dropdb --force`.
The confirmation lists every step and what would be lost.

Everything about processes comes from `/proc`: listening sockets from `/proc/net/tcp{,6}`, socket
owners from `/proc/<pid>/fd`, and the deleted-directory signal from the `(deleted)` suffix the
kernel puts on `/proc/<pid>/cwd`. Worktrees come from `git worktree list`, `git status` and
`git rev-list`; databases from `psql` on the `postgres` database, matched by the `_wt_<name>`
suffix that worktree setup hooks give them.

## Keys

| Key | Action |
| --- | --- |
| `j` / `k`, arrows, `g` / `G` | move |
| `tab`, `h` / `l`, `1` / `2` / `3` | switch panel |
| `d` | stop: `SIGTERM` to the root process(es) of the selection, or everything running in the worktree |
| `D` | force kill: `SIGKILL` to every process in the selection's tree |
| `K` | servers and strays: stop every orphan · worktrees: clean every safe leftover |
| `x` | worktree: stop its processes, remove it, drop its databases |
| `X` | same, but also for dirty, local, gone or unstarted worktrees |
| `f` | `git fetch --prune` every repo, so deleted remote branches show as gone |
| `o` | show only problems: orphan and stale servers, leftover worktrees |
| `s` | cycle server sort: port, status, age, mem, cpu |
| `/` | filter by port, project, branch, dir, command or database name |
| `r` | rescan now (processes every 2s, worktrees every 30s) |
| `?` | help |
| `q` | quit |

Every kill or removal asks for confirmation and lists the exact PIDs, paths and databases first.

## Install

```sh
cargo install --path .
server-monitor
```

Options:

```
server-monitor [--interval SECS] [--stale-after HOURS] [--code-dir DIR] [--list]
```

`--code-dir` is where repositories live as `DIR/<org>/<repo>` (default `$PGWT_CODE_DIR` or
`~/code`). `--list` prints the servers, strays and leftover worktrees as plain text and exits,
which is handy from scripts or from an agent that wants to check for leftovers.

## Layout

- `src/scan.rs` reads `/proc` and builds a `Snapshot` of servers and strays.
- `src/worktree.rs` lists worktrees and their databases, classifies them, and performs removals.
- `src/app.rs` holds the UI state, filtering, cross-links between panels, and the confirmations.
- `src/ui.rs` renders the panels with ratatui.
- `src/main.rs` runs the two scanners on background threads and the event loop on the main one.
