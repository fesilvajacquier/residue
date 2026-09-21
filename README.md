# server-monitor

A lazygit-style TUI that shows every dev server listening on this machine, flags the ones
left behind by deleted worktrees, and lets you stop them without hunting for PIDs.

Built for a workflow of many agents and many git worktrees, where a `puma`, `sidekiq`,
`vite` or headless Chrome keeps running for days after its worktree is gone.

## What it shows

```
┏ [1] Servers (5) ✗2 !1 ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━┓┌ Details ───────────────────────────────┐
┃   PORT   PROJECT             BRANCH     AGE   MEM    ┃│Port     3001  bound on 127.0.0.1       │
┃ ! 3000   shop/wt-a1b2c3      feat/cart  10d   200M   ┃│URL      http://localhost:3001          │
┃▶✗ 3001   shop/wt-d4e5f6      (deleted)  6d    92M    ┃│Status   ORPHAN — dir no longer exists  │
┃ ● 3002   shop/wt-778899      fix/login  13m   332M   ┃│Project  shop/wt-d4e5f6                 │
┃ ✗ *5000  api/wt-0a1b2c       (deleted)  11d   153M   ┃│Branch   unknown                        │
┃ ● 5173   shop/wt-778899/docs fix/login  13m   31M    ┃│Dir      ~/wt/shop/wt-d4e5f6  [DELETED] │
┗━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━┛│Started  2026-09-14 17:45  (6d18h ago)  │
┌ [2] Strays: processes in deleted dirs (2) ───────────┐│Memory   92M across 1 process(es)       │
│ PID     PROGRAM    PROCS  DIR             AGE   MEM  ││                                        │
│▶93544   sidekiq    1      api/wt-0a1b2c   11d   84M  ││Processes holding the port              │
│ 48861   node sass  1      shop/wt-d4e5f6  2d    48M  ││2575648 puma  6d18h  92M  puma 8.0.2    │
└──────────────────────────────────────────────────────┘└────────────────────────────────────────┘
 j/k move  tab panel  d stop  D force kill  K kill all orphans  o problems only  / filter  ? help  q quit
```

- **Servers**: one row per listening TCP port owned by you. Puma workers, Chrome helpers and
  other processes in the same directory are folded into the row.
- **Strays**: processes that are not listening on anything but whose working directory has been
  deleted (Sidekiq, Sass watchers, crashpad handlers left behind by a removed worktree).
- **Details**: bind address, URL, project, git branch, start time, memory, CPU, and the process tree.

Status markers:

| Marker | Meaning |
| --- | --- |
| `✗ ORPHAN` | the working directory no longer exists, typically a deleted worktree |
| `! STALE` | running longer than the stale threshold (24h by default) |
| `● OK` | nothing suspicious |
| `*PORT` | bound on all interfaces (`0.0.0.0` or `::`) |

Everything comes from `/proc`: listening sockets from `/proc/net/tcp{,6}`, socket owners from
`/proc/<pid>/fd`, and the deleted-directory signal from the `(deleted)` suffix the kernel puts
on `/proc/<pid>/cwd`.

## Keys

| Key | Action |
| --- | --- |
| `j` / `k`, arrows, `g` / `G` | move |
| `tab`, `h` / `l`, `1` / `2` | switch panel |
| `d` | stop: `SIGTERM` to the root process(es) of the selection |
| `D` | force kill: `SIGKILL` to every process in the selection's tree |
| `K` | stop every orphan in the current panel |
| `o` | show only ORPHAN / STALE servers |
| `s` | cycle sort: port, status, age, mem, cpu |
| `/` | filter by port, project, branch, dir or command |
| `r` | rescan now (auto-rescans every 2s) |
| `?` | help |
| `q` | quit |

Every kill asks for confirmation and lists the exact PIDs first.

## Install

```sh
cargo install --path .
server-monitor
```

Options:

```
server-monitor [--interval SECS] [--stale-after HOURS] [--list]
```

`--list` prints the same information as plain text and exits, which is handy from scripts or
from an agent that wants to check for leftovers.

## Layout

- `src/scan.rs` reads `/proc` and builds a `Snapshot` of servers and strays.
- `src/app.rs` holds the UI state, filtering, sorting and the kill requests.
- `src/ui.rs` renders the panels with ratatui.
- `src/main.rs` runs the scanner on a background thread and the event loop on the main one.
