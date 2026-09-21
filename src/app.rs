use std::path::{Path, PathBuf};
use std::sync::mpsc::Sender;
use std::time::{Duration, Instant};

use nix::sys::signal::{kill, Signal};
use nix::unistd::Pid;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::format;
use crate::scan::{ProcInfo, Server, Snapshot, Status, Stray};
use crate::worktree::{self, Verdict, Worktree, WorktreeSnapshot};

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Panel {
    Servers,
    Strays,
    Worktrees,
}

impl Panel {
    fn index(self) -> usize {
        match self {
            Panel::Servers => 0,
            Panel::Strays => 1,
            Panel::Worktrees => 2,
        }
    }

    fn next(self) -> Panel {
        match self {
            Panel::Servers => Panel::Strays,
            Panel::Strays => Panel::Worktrees,
            Panel::Worktrees => Panel::Servers,
        }
    }

    fn prev(self) -> Panel {
        self.next().next()
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum SortBy {
    Port,
    Status,
    Age,
    Memory,
    Cpu,
}

impl SortBy {
    pub fn label(self) -> &'static str {
        match self {
            SortBy::Port => "port",
            SortBy::Status => "status",
            SortBy::Age => "age",
            SortBy::Memory => "mem",
            SortBy::Cpu => "cpu",
        }
    }

    fn next(self) -> SortBy {
        match self {
            SortBy::Port => SortBy::Status,
            SortBy::Status => SortBy::Age,
            SortBy::Age => SortBy::Memory,
            SortBy::Memory => SortBy::Cpu,
            SortBy::Cpu => SortBy::Port,
        }
    }
}

pub enum WtCommand {
    Rescan,
    Fetch,
}

#[derive(Clone)]
pub struct Target {
    pub pid: i32,
    pub describe: String,
}

#[derive(Clone)]
pub struct KillRequest {
    pub title: String,
    pub signal: Signal,
    pub targets: Vec<Target>,
}

#[derive(Clone)]
pub struct RemoveItem {
    pub summary: String,
    pub steps: Vec<String>,
    pub warning: Option<String>,
    pub kill: Vec<Target>,
    pub repo: PathBuf,
    pub path: Option<PathBuf>,
    pub prune_only: bool,
    pub branch: Option<String>,
    pub dbs: Vec<String>,
}

#[derive(Clone)]
pub struct RemoveRequest {
    pub title: String,
    pub items: Vec<RemoveItem>,
}

#[derive(Clone)]
pub enum Action {
    Kill(KillRequest),
    Remove(RemoveRequest),
}

pub enum Mode {
    Normal,
    Confirm(Action),
    Filter,
    Help,
}

pub struct App {
    pub snapshot: Option<Snapshot>,
    pub worktrees: Option<WorktreeSnapshot>,
    pub panel: Panel,
    pub mode: Mode,
    pub filter: String,
    pub only_problems: bool,
    pub sort: SortBy,
    pub server_rows: Vec<usize>,
    pub stray_rows: Vec<usize>,
    pub worktree_rows: Vec<usize>,
    selected: [usize; 3],
    pub toast: Option<(String, Instant)>,
    pub should_quit: bool,
    refresh: Sender<()>,
    refresh_wt: Sender<WtCommand>,
}

const TOAST_TTL: Duration = Duration::from_secs(4);

impl App {
    pub fn new(refresh: Sender<()>, refresh_wt: Sender<WtCommand>) -> Self {
        Self {
            snapshot: None,
            worktrees: None,
            panel: Panel::Servers,
            mode: Mode::Normal,
            filter: String::new(),
            only_problems: false,
            sort: SortBy::Port,
            server_rows: Vec::new(),
            stray_rows: Vec::new(),
            worktree_rows: Vec::new(),
            selected: [0, 0, 0],
            toast: None,
            should_quit: false,
            refresh,
            refresh_wt,
        }
    }

    pub fn apply_snapshot(&mut self, snapshot: Snapshot) {
        let keep_port = self.selected_server().map(|s| s.port);
        let keep_pid = self.selected_stray().map(|p| p.root.pid);
        self.snapshot = Some(snapshot);
        self.rebuild_rows();
        if let Some(port) = keep_port {
            if let Some(i) = self
                .server_rows
                .iter()
                .position(|&i| self.servers()[i].port == port)
            {
                self.selected[0] = i;
            }
        }
        if let Some(pid) = keep_pid {
            if let Some(i) = self
                .stray_rows
                .iter()
                .position(|&i| self.strays()[i].root.pid == pid)
            {
                self.selected[1] = i;
            }
        }
        self.clamp_selection();
    }

    pub fn apply_worktrees(&mut self, snapshot: WorktreeSnapshot) {
        let keep = self
            .selected_worktree()
            .map(|w| (w.repo_label.clone(), w.key.clone()));
        if let Some(note) = &snapshot.note {
            self.notify(note);
        }
        self.worktrees = Some(snapshot);
        self.rebuild_rows();
        if let Some((repo, key)) = keep {
            if let Some(i) = self.worktree_rows.iter().position(|&i| {
                let w = &self.worktree_list()[i];
                w.repo_label == repo && w.key == key
            }) {
                self.selected[2] = i;
            }
        }
        self.clamp_selection();
    }

    pub fn tick(&mut self) {
        if let Some((_, at)) = &self.toast {
            if at.elapsed() > TOAST_TTL {
                self.toast = None;
            }
        }
    }

    fn servers(&self) -> &[Server] {
        self.snapshot
            .as_ref()
            .map(|s| s.servers.as_slice())
            .unwrap_or(&[])
    }

    fn strays(&self) -> &[Stray] {
        self.snapshot
            .as_ref()
            .map(|s| s.strays.as_slice())
            .unwrap_or(&[])
    }

    fn worktree_list(&self) -> &[Worktree] {
        self.worktrees
            .as_ref()
            .map(|s| s.rows.as_slice())
            .unwrap_or(&[])
    }

    pub fn visible_servers(&self) -> Vec<&Server> {
        self.server_rows
            .iter()
            .map(|&i| &self.servers()[i])
            .collect()
    }

    pub fn visible_strays(&self) -> Vec<&Stray> {
        self.stray_rows.iter().map(|&i| &self.strays()[i]).collect()
    }

    pub fn visible_worktrees(&self) -> Vec<&Worktree> {
        self.worktree_rows
            .iter()
            .map(|&i| &self.worktree_list()[i])
            .collect()
    }

    fn row_count(&self, panel: Panel) -> usize {
        match panel {
            Panel::Servers => self.server_rows.len(),
            Panel::Strays => self.stray_rows.len(),
            Panel::Worktrees => self.worktree_rows.len(),
        }
    }

    pub fn selected_index(&self, panel: Panel) -> Option<usize> {
        let len = self.row_count(panel);
        (len > 0).then(|| self.selected[panel.index()].min(len - 1))
    }

    pub fn selected_server(&self) -> Option<&Server> {
        let i = self.selected_index(Panel::Servers)?;
        Some(&self.servers()[self.server_rows[i]])
    }

    pub fn selected_stray(&self) -> Option<&Stray> {
        let i = self.selected_index(Panel::Strays)?;
        Some(&self.strays()[self.stray_rows[i]])
    }

    pub fn selected_worktree(&self) -> Option<&Worktree> {
        let i = self.selected_index(Panel::Worktrees)?;
        Some(&self.worktree_list()[self.worktree_rows[i]])
    }

    pub fn problem_count(&self) -> (usize, usize) {
        let orphans = self
            .servers()
            .iter()
            .filter(|s| s.status == Status::Orphan)
            .count();
        let stale = self
            .servers()
            .iter()
            .filter(|s| s.status == Status::Stale)
            .count();
        (orphans, stale)
    }

    pub fn leftover_count(&self) -> (usize, usize) {
        let rows = self.worktree_list();
        let safe = rows.iter().filter(|w| w.verdict.is_safe()).count();
        let check = rows.iter().filter(|w| w.verdict == Verdict::Gone).count();
        (safe, check)
    }

    /// The worktree a directory belongs to: the longest matching path wins, so a nested
    /// `.claude/worktrees/x` beats the main checkout that contains it.
    pub fn worktree_for(&self, cwd: Option<&Path>) -> Option<&Worktree> {
        let cwd = cwd?;
        let rows = self.worktree_list();
        let by_path = rows
            .iter()
            .filter(|w| w.path.is_some() && w.owns(Some(cwd)))
            .max_by_key(|w| w.path.as_ref().map(|p| p.as_os_str().len()));
        by_path.or_else(|| {
            rows.iter()
                .find(|w| w.verdict == Verdict::DeadDb && w.owns(Some(cwd)))
        })
    }

    pub fn servers_for(&self, wt: &Worktree) -> Vec<&Server> {
        self.servers()
            .iter()
            .filter(|s| self.owner_is(wt, s.root().cwd.as_deref()))
            .collect()
    }

    pub fn strays_for(&self, wt: &Worktree) -> Vec<&Stray> {
        self.strays()
            .iter()
            .filter(|s| self.owner_is(wt, s.root.cwd.as_deref()))
            .collect()
    }

    fn owner_is(&self, wt: &Worktree, cwd: Option<&Path>) -> bool {
        self.worktree_for(cwd)
            .map(|w| w.repo_label == wt.repo_label && w.key == wt.key)
            .unwrap_or(false)
    }

    fn rebuild_rows(&mut self) {
        let needle = self.filter.to_lowercase();
        let matches = |hay: &[&str]| {
            needle.is_empty() || hay.iter().any(|h| h.to_lowercase().contains(&needle))
        };

        let mut rows: Vec<usize> = self
            .servers()
            .iter()
            .enumerate()
            .filter(|(_, s)| !self.only_problems || s.status.is_problem())
            .filter(|(_, s)| {
                let port = s.port.to_string();
                let branch = s.branch.as_deref().unwrap_or("");
                let cwd = path_text(s.root().cwd.as_deref());
                matches(&[&port, &s.project, branch, &s.root().cmdline, &cwd])
            })
            .map(|(i, _)| i)
            .collect();
        let servers = self.servers();
        match self.sort {
            SortBy::Port => rows.sort_by_key(|&i| servers[i].port),
            SortBy::Status => rows.sort_by_key(|&i| (servers[i].status, servers[i].port)),
            SortBy::Age => rows.sort_by(|&a, &b| servers[b].root().age.cmp(&servers[a].root().age)),
            SortBy::Memory => {
                rows.sort_by(|&a, &b| servers[b].total_rss().cmp(&servers[a].total_rss()))
            }
            SortBy::Cpu => rows.sort_by(|&a, &b| {
                servers[b]
                    .total_cpu()
                    .partial_cmp(&servers[a].total_cpu())
                    .unwrap_or(std::cmp::Ordering::Equal)
            }),
        }
        self.server_rows = rows;

        self.stray_rows = self
            .strays()
            .iter()
            .enumerate()
            .filter(|(_, s)| {
                let p = &s.root;
                let pid = p.pid.to_string();
                let cwd = path_text(p.cwd.as_deref());
                matches(&[&pid, &p.comm, &p.cmdline, &cwd])
            })
            .map(|(i, _)| i)
            .collect();

        self.worktree_rows = self
            .worktree_list()
            .iter()
            .enumerate()
            .filter(|(_, w)| !self.only_problems || w.verdict.is_leftover())
            .filter(|(_, w)| {
                let path = path_text(w.path.as_deref());
                let branch = w.branch.as_deref().unwrap_or("");
                let dbs: Vec<&str> = w.dbs.iter().map(|d| d.name.as_str()).collect();
                let mut hay = vec![w.repo_label.as_str(), branch, &path, &w.key, w.verdict.label()];
                hay.extend(dbs);
                matches(&hay)
            })
            .map(|(i, _)| i)
            .collect();
        self.clamp_selection();
    }

    fn clamp_selection(&mut self) {
        for panel in [Panel::Servers, Panel::Strays, Panel::Worktrees] {
            let len = self.row_count(panel);
            let i = &mut self.selected[panel.index()];
            *i = (*i).min(len.saturating_sub(1));
        }
    }

    fn move_selection(&mut self, delta: isize) {
        let len = self.row_count(self.panel);
        if len == 0 {
            return;
        }
        let i = &mut self.selected[self.panel.index()];
        *i = (*i as isize + delta).clamp(0, len as isize - 1) as usize;
    }

    fn jump_to(&mut self, end: bool) {
        let len = self.row_count(self.panel);
        self.selected[self.panel.index()] = if end { len.saturating_sub(1) } else { 0 };
    }

    pub fn handle_key(&mut self, key: KeyEvent) {
        match &self.mode {
            Mode::Normal => self.handle_normal(key),
            Mode::Confirm(_) => self.handle_confirm(key),
            Mode::Filter => self.handle_filter(key),
            Mode::Help => {
                if matches!(
                    key.code,
                    KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('?')
                ) {
                    self.mode = Mode::Normal;
                }
            }
        }
    }

    fn handle_normal(&mut self, key: KeyEvent) {
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            self.should_quit = true;
            return;
        }
        match key.code {
            KeyCode::Char('q') => self.should_quit = true,
            KeyCode::Char('j') | KeyCode::Down => self.move_selection(1),
            KeyCode::Char('k') | KeyCode::Up => self.move_selection(-1),
            KeyCode::PageDown => self.move_selection(10),
            KeyCode::PageUp => self.move_selection(-10),
            KeyCode::Char('g') | KeyCode::Home => self.jump_to(false),
            KeyCode::Char('G') | KeyCode::End => self.jump_to(true),
            KeyCode::Tab | KeyCode::Char('l') => self.panel = self.panel.next(),
            KeyCode::BackTab | KeyCode::Char('h') => self.panel = self.panel.prev(),
            KeyCode::Char('1') => self.panel = Panel::Servers,
            KeyCode::Char('2') => self.panel = Panel::Strays,
            KeyCode::Char('3') => self.panel = Panel::Worktrees,
            KeyCode::Char('d') => self.request_kill_selected(Signal::SIGTERM),
            KeyCode::Char('D') => self.request_kill_selected(Signal::SIGKILL),
            KeyCode::Char('K') => self.request_clean_all(),
            KeyCode::Char('x') => self.request_remove(false),
            KeyCode::Char('X') => self.request_remove(true),
            KeyCode::Char('o') => {
                self.only_problems = !self.only_problems;
                self.rebuild_rows();
            }
            KeyCode::Char('s') => {
                self.sort = self.sort.next();
                self.rebuild_rows();
            }
            KeyCode::Char('/') => self.mode = Mode::Filter,
            KeyCode::Esc if !self.filter.is_empty() => {
                self.filter.clear();
                self.rebuild_rows();
            }
            KeyCode::Char('r') => {
                self.refresh_all();
                self.notify("refreshing…");
            }
            KeyCode::Char('f') => {
                let _ = self.refresh_wt.send(WtCommand::Fetch);
                self.notify("fetching every repo (prune)…");
            }
            KeyCode::Char('?') => self.mode = Mode::Help,
            _ => {}
        }
    }

    fn handle_filter(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => {
                self.filter.clear();
                self.mode = Mode::Normal;
            }
            KeyCode::Enter => self.mode = Mode::Normal,
            KeyCode::Backspace => {
                self.filter.pop();
            }
            KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.filter.push(c)
            }
            _ => return,
        }
        self.rebuild_rows();
    }

    fn handle_confirm(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Char('y') | KeyCode::Enter => {
                let Mode::Confirm(action) = std::mem::replace(&mut self.mode, Mode::Normal) else {
                    return;
                };
                match action {
                    Action::Kill(req) => self.execute_kill(req),
                    Action::Remove(req) => self.execute_remove(req),
                }
            }
            KeyCode::Char('n') | KeyCode::Esc | KeyCode::Char('q') => self.mode = Mode::Normal,
            _ => {}
        }
    }

    fn request_kill_selected(&mut self, signal: Signal) {
        let request = match self.panel {
            Panel::Servers => self.selected_server().map(|s| server_request(s, signal)),
            Panel::Strays => self.selected_stray().map(|s| stray_request(s, signal)),
            Panel::Worktrees => self
                .selected_worktree()
                .map(|w| self.worktree_kill_request(w, signal)),
        };
        match request {
            Some(req) if req.targets.is_empty() => self.notify("nothing is running there"),
            Some(req) => self.mode = Mode::Confirm(Action::Kill(req)),
            None => self.notify("nothing selected"),
        }
    }

    fn worktree_kill_request(&self, wt: &Worktree, signal: Signal) -> KillRequest {
        let mut targets = Vec::new();
        for s in self.servers_for(wt) {
            match signal {
                Signal::SIGKILL => targets.extend(s.all().map(target)),
                _ => targets.extend(s.roots().into_iter().map(target)),
            }
        }
        for s in self.strays_for(wt) {
            match signal {
                Signal::SIGKILL => targets.extend(s.all().map(target)),
                _ => targets.push(target(&s.root)),
            }
        }
        KillRequest {
            title: format!(
                "{} everything running in {}",
                signal_verb(signal),
                wt.label()
            ),
            signal,
            targets,
        }
    }

    fn request_clean_all(&mut self) {
        if self.panel == Panel::Worktrees {
            let items: Vec<RemoveItem> = self
                .visible_worktrees()
                .into_iter()
                .filter(|w| w.verdict.is_safe() && w.hold.is_none())
                .map(|w| self.remove_item(w))
                .collect();
            if items.is_empty() {
                self.notify("no provably safe leftovers in this panel");
                return;
            }
            self.mode = Mode::Confirm(Action::Remove(RemoveRequest {
                title: format!("Clean every merged worktree and dead database ({})", items.len()),
                items,
            }));
            return;
        }
        let signal = Signal::SIGTERM;
        let targets: Vec<Target> = match self.panel {
            Panel::Servers => self
                .visible_servers()
                .iter()
                .filter(|s| s.status == Status::Orphan)
                .flat_map(|s| s.roots().into_iter().map(target).collect::<Vec<_>>())
                .collect(),
            _ => self
                .visible_strays()
                .iter()
                .map(|s| target(&s.root))
                .collect(),
        };
        if targets.is_empty() {
            self.notify("no orphans in this panel");
            return;
        }
        let what = match self.panel {
            Panel::Servers => "every orphaned server",
            _ => "every stray process",
        };
        self.mode = Mode::Confirm(Action::Kill(KillRequest {
            title: format!(
                "{} {} ({} processes)",
                signal_verb(signal),
                what,
                targets.len()
            ),
            signal,
            targets,
        }));
    }

    fn request_remove(&mut self, force: bool) {
        if self.panel != Panel::Worktrees {
            self.notify("x removes worktrees: switch to panel 3");
            return;
        }
        let Some(wt) = self.selected_worktree() else {
            self.notify("nothing selected");
            return;
        };
        if wt.verdict == Verdict::Main {
            self.notify("the main checkout is never removed from here");
            return;
        }
        if let Some(hold) = wt.hold {
            if hold.is_hard() || !force {
                let hint = if hold.is_hard() { "" } else { " (X to force)" };
                self.notify(&format!("blocked: {}{hint}", hold.label()));
                return;
            }
        }
        if !force && !wt.verdict.is_safe() {
            self.notify(&format!(
                "{}: {} (X to force)",
                wt.verdict.label(),
                wt.verdict.explain()
            ));
            return;
        }
        let item = self.remove_item(wt);
        let title = format!(
            "{} {}",
            if force { "Force remove" } else { "Remove" },
            wt.label()
        );
        self.mode = Mode::Confirm(Action::Remove(RemoveRequest {
            title,
            items: vec![item],
        }));
    }

    fn remove_item(&self, wt: &Worktree) -> RemoveItem {
        let mut kill: Vec<Target> = self
            .servers_for(wt)
            .iter()
            .flat_map(|s| s.roots().into_iter().map(target).collect::<Vec<_>>())
            .collect();
        kill.extend(self.strays_for(wt).iter().map(|s| target(&s.root)));
        let prune_only = wt.verdict == Verdict::Missing;
        // Only a branch whose remote is gone goes with the worktree; anything else may hold
        // the only copy of its commits.
        let branch = wt
            .branch
            .clone()
            .filter(|_| wt.upstream_gone && (wt.ahead == 0 || wt.verdict == Verdict::Gone));
        let dbs: Vec<String> = wt.dbs.iter().map(|d| d.name.clone()).collect();

        let mut steps = Vec::new();
        if !kill.is_empty() {
            steps.push(format!("stop {} process(es) running in it", kill.len()));
        }
        if let Some(p) = &wt.path {
            if prune_only {
                steps.push("prune the missing worktree from git".to_string());
            } else {
                steps.push(format!("remove {}", p.to_string_lossy()));
            }
        }
        match (&wt.branch, &branch) {
            (Some(_), Some(b)) => steps.push(format!("delete branch {b}")),
            (Some(b), None) => steps.push(format!("keep branch {b}")),
            (None, _) => {}
        }
        for db in &wt.dbs {
            steps.push(format!("drop database {} ({})", db.name, format::bytes(db.bytes)));
        }
        let mut losses = Vec::new();
        if wt.hold == Some(worktree::Hold::Dirty) {
            losses.push("uncommitted changes".to_string());
        }
        if wt.untracked > 0 {
            losses.push(format!("{} untracked file(s)", wt.untracked));
        }
        if wt.verdict == Verdict::Gone || wt.verdict == Verdict::Local {
            losses.push(format!("{} commit(s) not in the default branch", wt.ahead));
        }
        let warning = (!losses.is_empty()).then(|| format!("loses {}", losses.join(", ")));
        let mut compact = Vec::new();
        if !kill.is_empty() {
            compact.push(format!("{} proc(s)", kill.len()));
        }
        if wt.path.is_some() {
            compact.push(if prune_only { "prune" } else { "dir" }.to_string());
        }
        if branch.is_some() {
            compact.push("branch".to_string());
        }
        if !dbs.is_empty() {
            compact.push(format!("{} db(s) {}", dbs.len(), format::bytes(wt.db_bytes())));
        }
        let summary = format!(
            "{:<34} {:<11} {}",
            format::truncate(&wt.label(), 34),
            wt.verdict.label(),
            compact.join(" · ")
        );
        RemoveItem {
            summary,
            steps,
            warning,
            kill,
            repo: wt.repo.clone(),
            path: wt.path.clone(),
            prune_only,
            branch,
            dbs,
        }
    }

    fn execute_kill(&mut self, req: KillRequest) {
        let (sent, failed) = send_signal(&req.targets, req.signal);
        let _ = self.refresh.send(());
        if failed.is_empty() {
            self.notify(&format!(
                "sent {} to {sent} process(es)",
                req.signal.as_str()
            ));
        } else {
            self.notify(&format!("sent to {sent}, failed: {}", failed.join(", ")));
        }
    }

    fn execute_remove(&mut self, req: RemoveRequest) {
        let mut removed = 0;
        let mut dropped = 0;
        let mut errors = Vec::new();
        for item in &req.items {
            send_signal(&item.kill, Signal::SIGTERM);
            let result = match (&item.path, item.prune_only) {
                (Some(_), true) => worktree::prune_worktrees(&item.repo),
                (Some(p), false) => worktree::remove_worktree(&item.repo, p),
                (None, _) => Ok(()),
            };
            match result {
                Ok(()) => {
                    if item.path.is_some() {
                        removed += 1;
                    }
                    if let Some(b) = &item.branch {
                        if let Err(e) = worktree::delete_branch(&item.repo, b) {
                            errors.push(format!("{b}: {e}"));
                        }
                    }
                }
                Err(e) => {
                    errors.push(format!("{}: {e}", item.summary.trim_end()));
                    continue;
                }
            }
            for db in &item.dbs {
                match worktree::drop_db(db) {
                    Ok(()) => dropped += 1,
                    Err(e) => errors.push(format!("{db}: {e}")),
                }
            }
        }
        self.refresh_all();
        let mut done = Vec::new();
        if removed > 0 {
            done.push(format!("removed {removed} worktree(s)"));
        }
        if dropped > 0 {
            done.push(format!("dropped {dropped} database(s)"));
        }
        if done.is_empty() {
            done.push("nothing removed".to_string());
        }
        if !errors.is_empty() {
            done.push(format!("failed: {}", errors.join("; ")));
        }
        self.notify(&done.join(", "));
    }

    fn refresh_all(&mut self) {
        let _ = self.refresh.send(());
        let _ = self.refresh_wt.send(WtCommand::Rescan);
    }

    pub fn notify(&mut self, text: &str) {
        self.toast = Some((text.to_string(), Instant::now()));
    }
}

fn send_signal(targets: &[Target], signal: Signal) -> (usize, Vec<String>) {
    let mut sent = 0;
    let mut failed = Vec::new();
    for t in targets {
        match kill(Pid::from_raw(t.pid), signal) {
            Ok(()) => sent += 1,
            Err(e) => failed.push(format!("{}: {e}", t.pid)),
        }
    }
    (sent, failed)
}

fn path_text(path: Option<&Path>) -> String {
    path.map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default()
}

fn server_request(server: &Server, signal: Signal) -> KillRequest {
    // SIGTERM goes to the roots only: a well-behaved master (puma, foreman) tears down
    // its own workers. SIGKILL cannot rely on that, so it hits every holder of the port.
    let targets: Vec<Target> = match signal {
        Signal::SIGKILL => server.all().map(target).collect(),
        _ => server.roots().into_iter().map(target).collect(),
    };
    KillRequest {
        title: format!(
            "{} port {} ({})",
            signal_verb(signal),
            server.port,
            server.project
        ),
        signal,
        targets,
    }
}

fn stray_request(stray: &Stray, signal: Signal) -> KillRequest {
    let targets: Vec<Target> = match signal {
        Signal::SIGKILL => stray.all().map(target).collect(),
        _ => vec![target(&stray.root)],
    };
    KillRequest {
        title: format!(
            "{} stray {} (pid {})",
            signal_verb(signal),
            stray.root.program(),
            stray.root.pid
        ),
        signal,
        targets,
    }
}

fn target(p: &ProcInfo) -> Target {
    Target {
        pid: p.pid,
        describe: format!(
            "{:<14} {}",
            p.program(),
            format::truncate(p.short_command(), 60)
        ),
    }
}

fn signal_verb(signal: Signal) -> &'static str {
    match signal {
        Signal::SIGKILL => "Force kill",
        _ => "Stop",
    }
}
