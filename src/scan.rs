use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use procfs::net::TcpState;
use procfs::process::{all_processes, FDTarget, Process};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Status {
    Orphan,
    Stale,
    Ok,
}

impl Status {
    pub fn symbol(self) -> &'static str {
        match self {
            Status::Orphan => "✗",
            Status::Stale => "!",
            Status::Ok => "●",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Status::Orphan => "ORPHAN",
            Status::Stale => "STALE",
            Status::Ok => "OK",
        }
    }

    pub fn is_problem(self) -> bool {
        self != Status::Ok
    }
}

#[derive(Clone, Debug)]
pub struct ProcInfo {
    pub pid: i32,
    pub ppid: i32,
    pub comm: String,
    pub argv: Vec<String>,
    pub cmdline: String,
    pub cwd: Option<PathBuf>,
    pub cwd_deleted: bool,
    pub started_at: SystemTime,
    pub age: Duration,
    pub rss_bytes: u64,
    pub cpu_percent: f64,
}

const INTERPRETERS: &[&str] = &[
    "node", "ruby", "python", "python3", "bun", "deno", "sh", "bash", "zsh", "bundle",
];

impl ProcInfo {
    pub fn short_command(&self) -> &str {
        if self.cmdline.is_empty() {
            &self.comm
        } else {
            &self.cmdline
        }
    }

    /// Something more telling than `comm`, which is only the thread name ("MainThread" for node).
    pub fn program(&self) -> String {
        // Some programs (chrome, puma, sidekiq) overwrite argv with one space-joined string.
        let tokens: Vec<&str> = match self.argv.as_slice() {
            [single] => single.split_whitespace().collect(),
            many => many.iter().map(String::as_str).collect(),
        };
        let Some(first) = tokens.first() else {
            return self.comm.clone();
        };
        let exe = basename(first);
        if !INTERPRETERS.contains(&exe) {
            return exe.to_string();
        }
        match tokens.iter().skip(1).find(|a| !a.starts_with('-')) {
            Some(script) => format!("{exe} {}", basename(script)),
            None => exe.to_string(),
        }
    }
}

fn basename(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

#[derive(Clone, Debug)]
pub struct Stray {
    pub root: ProcInfo,
    pub children: Vec<ProcInfo>,
}

impl Stray {
    pub fn total_rss(&self) -> u64 {
        self.root.rss_bytes + self.children.iter().map(|c| c.rss_bytes).sum::<u64>()
    }

    pub fn all(&self) -> impl Iterator<Item = &ProcInfo> {
        std::iter::once(&self.root).chain(self.children.iter())
    }
}

#[derive(Clone, Debug)]
pub struct Server {
    pub port: u16,
    pub binds: Vec<String>,
    pub members: Vec<ProcInfo>,
    pub children: Vec<ProcInfo>,
    pub project: String,
    pub branch: Option<String>,
    pub status: Status,
}

impl Server {
    pub fn root(&self) -> &ProcInfo {
        self.roots().into_iter().next().unwrap_or(&self.members[0])
    }

    pub fn roots(&self) -> Vec<&ProcInfo> {
        let pids: HashSet<i32> = self.members.iter().map(|m| m.pid).collect();
        let mut roots: Vec<&ProcInfo> = self
            .members
            .iter()
            .filter(|m| !pids.contains(&m.ppid))
            .collect();
        roots.sort_by_key(|p| p.started_at);
        roots
    }

    pub fn all(&self) -> impl Iterator<Item = &ProcInfo> {
        self.members.iter().chain(self.children.iter())
    }

    pub fn total_rss(&self) -> u64 {
        self.all().map(|m| m.rss_bytes).sum()
    }

    pub fn total_cpu(&self) -> f64 {
        self.all().map(|m| m.cpu_percent).sum()
    }

    pub fn is_public(&self) -> bool {
        self.binds.iter().any(|b| b == "0.0.0.0" || b == "::")
    }
}

#[derive(Clone, Debug)]
pub struct Snapshot {
    pub servers: Vec<Server>,
    pub strays: Vec<Stray>,
    pub scan_time: Duration,
}

pub struct Scanner {
    uid: u32,
    ticks_per_sec: u64,
    page_size: u64,
    boot_time: u64,
    stale_after: Duration,
    prev_cpu: HashMap<i32, (u64, Instant)>,
    branch_cache: HashMap<i32, Option<String>>,
}

struct RawProc {
    info: ProcInfo,
    sockets: Vec<u64>,
}

impl Scanner {
    pub fn new(stale_after: Duration) -> Self {
        Self {
            uid: nix::unistd::Uid::current().as_raw(),
            ticks_per_sec: procfs::ticks_per_second(),
            page_size: procfs::page_size(),
            boot_time: procfs::boot_time_secs().unwrap_or(0),
            stale_after,
            prev_cpu: HashMap::new(),
            branch_cache: HashMap::new(),
        }
    }

    pub fn scan(&mut self) -> Snapshot {
        let started = Instant::now();
        let listeners = listening_sockets();
        let procs = self.collect_processes();

        let mut by_port: HashMap<u16, (Vec<String>, Vec<i32>)> = HashMap::new();
        for raw in &procs {
            for inode in &raw.sockets {
                if let Some((addr, port)) = listeners.get(inode) {
                    let entry = by_port.entry(*port).or_default();
                    if !entry.0.contains(addr) {
                        entry.0.push(addr.clone());
                    }
                    if !entry.1.contains(&raw.info.pid) {
                        entry.1.push(raw.info.pid);
                    }
                }
            }
        }

        let by_pid: HashMap<i32, &RawProc> = procs.iter().map(|p| (p.info.pid, p)).collect();
        let mut children_of: HashMap<i32, Vec<i32>> = HashMap::new();
        for raw in &procs {
            children_of
                .entry(raw.info.ppid)
                .or_default()
                .push(raw.info.pid);
        }
        let mut claimed: HashSet<i32> = by_port
            .values()
            .flat_map(|(_, pids)| pids.iter().copied())
            .collect();
        let mut servers = Vec::new();
        for (port, (binds, pids)) in by_port {
            let mut members: Vec<ProcInfo> = pids.iter().map(|p| by_pid[p].info.clone()).collect();
            members.sort_by_key(|m| m.pid);
            let children = descendants(&members, &children_of, &by_pid, &mut claimed);
            let mut server = Server {
                port,
                binds,
                members,
                children,
                project: String::new(),
                branch: None,
                status: Status::Ok,
            };
            let root = server.root().clone();
            server.project = project_label(root.cwd.as_deref());
            server.branch = self.branch_for(&root);
            server.status = if server.roots().iter().any(|r| r.cwd_deleted) {
                Status::Orphan
            } else if root.age > self.stale_after {
                Status::Stale
            } else {
                Status::Ok
            };
            servers.push(server);
        }
        servers.sort_by_key(|s| s.port);

        let strays = group_strays(
            procs
                .iter()
                .filter(|p| p.info.cwd_deleted && !claimed.contains(&p.info.pid))
                .map(|p| p.info.clone())
                .collect(),
        );

        let live: HashSet<i32> = procs.iter().map(|p| p.info.pid).collect();
        self.branch_cache.retain(|pid, _| live.contains(pid));
        self.prev_cpu.retain(|pid, _| live.contains(pid));

        Snapshot {
            servers,
            strays,
            scan_time: started.elapsed(),
        }
    }

    fn collect_processes(&mut self) -> Vec<RawProc> {
        let now = Instant::now();
        let now_unix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let me = std::process::id() as i32;
        let Ok(iter) = all_processes() else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for proc in iter.flatten() {
            if proc.pid == me || proc.uid().map(|u| u != self.uid).unwrap_or(true) {
                continue;
            }
            let Some(raw) = self.read_process(&proc, now, now_unix) else {
                continue;
            };
            out.push(raw);
        }
        out
    }

    fn read_process(&mut self, proc: &Process, now: Instant, now_unix: u64) -> Option<RawProc> {
        let stat = proc.stat().ok()?;
        // Kernel threads have no command line; they are never something the user started.
        let cmdline = proc.cmdline().ok()?;
        if cmdline.is_empty() {
            return None;
        }
        let (cwd, cwd_deleted) = split_deleted(proc.cwd().ok());
        let started_unix = self.boot_time + stat.starttime / self.ticks_per_sec;
        let age = Duration::from_secs(now_unix.saturating_sub(started_unix));
        let cpu_ticks = stat.utime + stat.stime;
        let cpu_percent = match self.prev_cpu.get(&stat.pid) {
            Some((prev_ticks, prev_at)) => {
                let elapsed = now.duration_since(*prev_at).as_secs_f64();
                if elapsed > 0.0 {
                    (cpu_ticks.saturating_sub(*prev_ticks)) as f64
                        / self.ticks_per_sec as f64
                        / elapsed
                        * 100.0
                } else {
                    0.0
                }
            }
            None => 0.0,
        };
        self.prev_cpu.insert(stat.pid, (cpu_ticks, now));
        let sockets = proc
            .fd()
            .map(|fds| {
                fds.flatten()
                    .filter_map(|fd| match fd.target {
                        FDTarget::Socket(inode) => Some(inode),
                        _ => None,
                    })
                    .collect()
            })
            .unwrap_or_default();
        Some(RawProc {
            info: ProcInfo {
                pid: stat.pid,
                ppid: stat.ppid,
                comm: stat.comm.clone(),
                cmdline: cmdline.join(" "),
                argv: cmdline,
                cwd,
                cwd_deleted,
                started_at: UNIX_EPOCH + Duration::from_secs(started_unix),
                age,
                rss_bytes: stat.rss * self.page_size,
                cpu_percent,
            },
            sockets,
        })
    }

    fn branch_for(&mut self, root: &ProcInfo) -> Option<String> {
        if let Some(cached) = self.branch_cache.get(&root.pid) {
            return cached.clone();
        }
        let branch = match (&root.cwd, root.cwd_deleted) {
            (Some(dir), false) => git_branch(dir),
            _ => None,
        };
        self.branch_cache.insert(root.pid, branch.clone());
        branch
    }
}

fn descendants(
    members: &[ProcInfo],
    children_of: &HashMap<i32, Vec<i32>>,
    by_pid: &HashMap<i32, &RawProc>,
    claimed: &mut HashSet<i32>,
) -> Vec<ProcInfo> {
    // Only descendants still working in the same directory belong to the server. A tool like
    // An IDE or agent host spawns shells and agents all over the place; those are not "its" processes.
    let home_dirs: HashSet<Option<&PathBuf>> = members.iter().map(|m| m.cwd.as_ref()).collect();
    let mut queue: Vec<i32> = members.iter().map(|m| m.pid).collect();
    let mut found = Vec::new();
    while let Some(pid) = queue.pop() {
        for child in children_of.get(&pid).into_iter().flatten() {
            let info = &by_pid[child].info;
            if claimed.contains(child) || !home_dirs.contains(&info.cwd.as_ref()) {
                continue;
            }
            claimed.insert(*child);
            found.push(info.clone());
            queue.push(*child);
        }
    }
    found.sort_by_key(|p| p.pid);
    found
}

fn group_strays(mut procs: Vec<ProcInfo>) -> Vec<Stray> {
    let pids: HashSet<i32> = procs.iter().map(|p| p.pid).collect();
    let mut ancestor_of: HashMap<i32, i32> = HashMap::new();
    procs.sort_by_key(|p| p.pid);
    for p in &procs {
        if pids.contains(&p.ppid) {
            let root = ancestor_of.get(&p.ppid).copied().unwrap_or(p.ppid);
            ancestor_of.insert(p.pid, root);
        }
    }
    let mut groups: Vec<Stray> = procs
        .iter()
        .filter(|p| !ancestor_of.contains_key(&p.pid))
        .map(|p| Stray {
            root: p.clone(),
            children: Vec::new(),
        })
        .collect();
    for p in procs {
        if let Some(root) = ancestor_of.get(&p.pid) {
            if let Some(g) = groups.iter_mut().find(|g| g.root.pid == *root) {
                g.children.push(p);
            }
        }
    }
    groups.sort_by(|a, b| {
        a.root
            .cwd
            .cmp(&b.root.cwd)
            .then(b.root.age.cmp(&a.root.age))
    });
    groups
}

fn listening_sockets() -> HashMap<u64, (String, u16)> {
    let mut map = HashMap::new();
    let entries = procfs::net::tcp()
        .unwrap_or_default()
        .into_iter()
        .chain(procfs::net::tcp6().unwrap_or_default());
    for entry in entries {
        if entry.state == TcpState::Listen {
            let addr = entry.local_address;
            map.insert(entry.inode, (addr.ip().to_string(), addr.port()));
        }
    }
    map
}

fn split_deleted(cwd: Option<PathBuf>) -> (Option<PathBuf>, bool) {
    let Some(cwd) = cwd else { return (None, false) };
    let text = cwd.to_string_lossy();
    match text.strip_suffix(" (deleted)") {
        Some(clean) => (Some(PathBuf::from(clean)), true),
        None => (Some(cwd), false),
    }
}

const NOISE_COMPONENTS: &[&str] = &["worktrees", ".claude", ".t3", "code"];

pub fn project_label(cwd: Option<&Path>) -> String {
    let Some(cwd) = cwd else {
        return "?".to_string();
    };
    let home = std::env::var("HOME").unwrap_or_default();
    let relative = cwd.strip_prefix(&home).unwrap_or(cwd);
    let parts: Vec<&str> = relative
        .components()
        .filter_map(|c| c.as_os_str().to_str())
        .filter(|c| *c != "/" && !NOISE_COMPONENTS.contains(c))
        .collect();
    let n = parts.len();
    if n == 0 {
        return if relative.as_os_str().is_empty() {
            "~".to_string()
        } else {
            cwd.to_string_lossy().into_owned()
        };
    }
    parts[n.saturating_sub(2)..].join("/")
}

fn git_branch(dir: &Path) -> Option<String> {
    let output = Command::new("git")
        .args(["-C"])
        .arg(dir)
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let branch = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (!branch.is_empty()).then_some(branch)
}
