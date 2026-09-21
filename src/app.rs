use std::sync::mpsc::Sender;
use std::time::{Duration, Instant};

use nix::sys::signal::{kill, Signal};
use nix::unistd::Pid;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::scan::{ProcInfo, Server, Snapshot, Status, Stray};

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Panel {
    Servers,
    Strays,
}

impl Panel {
    fn index(self) -> usize {
        match self {
            Panel::Servers => 0,
            Panel::Strays => 1,
        }
    }

    fn next(self) -> Panel {
        match self {
            Panel::Servers => Panel::Strays,
            Panel::Strays => Panel::Servers,
        }
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

pub enum Mode {
    Normal,
    Confirm(KillRequest),
    Filter,
    Help,
}

pub struct App {
    pub snapshot: Option<Snapshot>,
    pub panel: Panel,
    pub mode: Mode,
    pub filter: String,
    pub only_problems: bool,
    pub sort: SortBy,
    pub server_rows: Vec<usize>,
    pub stray_rows: Vec<usize>,
    selected: [usize; 2],
    pub toast: Option<(String, Instant)>,
    pub should_quit: bool,
    refresh: Sender<()>,
}

const TOAST_TTL: Duration = Duration::from_secs(4);

impl App {
    pub fn new(refresh: Sender<()>) -> Self {
        Self {
            snapshot: None,
            panel: Panel::Servers,
            mode: Mode::Normal,
            filter: String::new(),
            only_problems: false,
            sort: SortBy::Port,
            server_rows: Vec::new(),
            stray_rows: Vec::new(),
            selected: [0, 0],
            toast: None,
            should_quit: false,
            refresh,
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

    pub fn visible_servers(&self) -> Vec<&Server> {
        self.server_rows
            .iter()
            .map(|&i| &self.servers()[i])
            .collect()
    }

    pub fn visible_strays(&self) -> Vec<&Stray> {
        self.stray_rows.iter().map(|&i| &self.strays()[i]).collect()
    }

    pub fn selected_index(&self, panel: Panel) -> Option<usize> {
        let len = match panel {
            Panel::Servers => self.server_rows.len(),
            Panel::Strays => self.stray_rows.len(),
        };
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
                let cwd = s
                    .root()
                    .cwd
                    .as_ref()
                    .map(|c| c.to_string_lossy().into_owned())
                    .unwrap_or_default();
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
                let cwd = p
                    .cwd
                    .as_ref()
                    .map(|c| c.to_string_lossy().into_owned())
                    .unwrap_or_default();
                matches(&[&pid, &p.comm, &p.cmdline, &cwd])
            })
            .map(|(i, _)| i)
            .collect();
        self.clamp_selection();
    }

    fn clamp_selection(&mut self) {
        for panel in [Panel::Servers, Panel::Strays] {
            let len = match panel {
                Panel::Servers => self.server_rows.len(),
                Panel::Strays => self.stray_rows.len(),
            };
            let i = &mut self.selected[panel.index()];
            *i = (*i).min(len.saturating_sub(1));
        }
    }

    fn move_selection(&mut self, delta: isize) {
        let len = match self.panel {
            Panel::Servers => self.server_rows.len(),
            Panel::Strays => self.stray_rows.len(),
        };
        if len == 0 {
            return;
        }
        let i = &mut self.selected[self.panel.index()];
        *i = (*i as isize + delta).clamp(0, len as isize - 1) as usize;
    }

    fn jump_to(&mut self, end: bool) {
        let len = match self.panel {
            Panel::Servers => self.server_rows.len(),
            Panel::Strays => self.stray_rows.len(),
        };
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
            KeyCode::Tab | KeyCode::BackTab | KeyCode::Char('h') | KeyCode::Char('l') => {
                self.panel = self.panel.next()
            }
            KeyCode::Char('1') => self.panel = Panel::Servers,
            KeyCode::Char('2') => self.panel = Panel::Strays,
            KeyCode::Char('d') => self.request_kill_selected(Signal::SIGTERM),
            KeyCode::Char('D') => self.request_kill_selected(Signal::SIGKILL),
            KeyCode::Char('K') => self.request_kill_problems(),
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
                let _ = self.refresh.send(());
                self.notify("refreshing…");
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
                let Mode::Confirm(req) = std::mem::replace(&mut self.mode, Mode::Normal) else {
                    return;
                };
                self.execute(req);
            }
            KeyCode::Char('n') | KeyCode::Esc | KeyCode::Char('q') => self.mode = Mode::Normal,
            _ => {}
        }
    }

    fn request_kill_selected(&mut self, signal: Signal) {
        let request = match self.panel {
            Panel::Servers => self.selected_server().map(|s| server_request(s, signal)),
            Panel::Strays => self.selected_stray().map(|s| stray_request(s, signal)),
        };
        match request {
            Some(req) => self.mode = Mode::Confirm(req),
            None => self.notify("nothing selected"),
        }
    }

    fn request_kill_problems(&mut self) {
        let signal = Signal::SIGTERM;
        let targets: Vec<Target> = match self.panel {
            Panel::Servers => self
                .visible_servers()
                .iter()
                .filter(|s| s.status == Status::Orphan)
                .flat_map(|s| s.roots().into_iter().map(target).collect::<Vec<_>>())
                .collect(),
            Panel::Strays => self
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
            Panel::Strays => "every stray process",
        };
        self.mode = Mode::Confirm(KillRequest {
            title: format!(
                "{} {} ({} processes)",
                signal_verb(signal),
                what,
                targets.len()
            ),
            signal,
            targets,
        });
    }

    fn execute(&mut self, req: KillRequest) {
        let mut sent = 0;
        let mut failed = Vec::new();
        for t in &req.targets {
            match kill(Pid::from_raw(t.pid), req.signal) {
                Ok(()) => sent += 1,
                Err(e) => failed.push(format!("{}: {e}", t.pid)),
            }
        }
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

    pub fn notify(&mut self, text: &str) {
        self.toast = Some((text.to_string(), Instant::now()));
    }
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
            crate::format::truncate(p.short_command(), 60)
        ),
    }
}

fn signal_verb(signal: Signal) -> &'static str {
    match signal {
        Signal::SIGKILL => "Force kill",
        _ => "Stop",
    }
}
