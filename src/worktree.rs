use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Verdict {
    Main,
    Active,
    Local,
    Unstarted,
    Gone,
    Abandoned,
    Merged,
    Missing,
    DeadDb,
}

impl Verdict {
    pub fn symbol(self) -> &'static str {
        match self {
            Verdict::Main | Verdict::Active | Verdict::Local => "●",
            Verdict::Unstarted => "·",
            Verdict::Gone => "?",
            Verdict::Merged | Verdict::Abandoned => "✓",
            Verdict::Missing | Verdict::DeadDb => "✗",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Verdict::Main => "main",
            Verdict::Active => "tracking",
            Verdict::Local => "local",
            Verdict::Unstarted => "no commits",
            Verdict::Gone => "gone",
            Verdict::Merged => "merged",
            Verdict::Abandoned => "abandoned",
            Verdict::Missing => "dir missing",
            Verdict::DeadDb => "db only",
        }
    }

    pub fn explain(self) -> &'static str {
        match self {
            Verdict::Main => "the repository's main checkout, never removed from here",
            Verdict::Active => "its branch still exists on the remote, someone may be working on it",
            Verdict::Local => "never pushed and holds commits the default branch lacks",
            Verdict::Unstarted => "no commits of its own; a session may still be working in it",
            Verdict::Gone => "the remote branch was deleted but local commits are not in the default branch",
            Verdict::Merged => "its pull request was merged, or its remote branch was deleted with every commit in the default branch",
            Verdict::Abandoned => "never pushed, nothing of its own, and untouched for longer than the idle threshold",
            Verdict::Missing => "git still lists it but the directory is gone",
            Verdict::DeadDb => "no worktree matches these databases any more",
        }
    }

    pub fn is_leftover(self) -> bool {
        matches!(
            self,
            Verdict::Gone
                | Verdict::Merged
                | Verdict::Abandoned
                | Verdict::Missing
                | Verdict::DeadDb
        )
    }

    /// Removable without an explicit force: nothing that exists only here would be lost.
    pub fn is_safe(self) -> bool {
        matches!(
            self,
            Verdict::Merged | Verdict::Abandoned | Verdict::Missing | Verdict::DeadDb
        )
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Hold {
    Locked,
    Here,
    Dirty,
}

impl Hold {
    pub fn label(self) -> &'static str {
        match self {
            Hold::Locked => "worktree is locked",
            Hold::Here => "you are standing in it",
            Hold::Dirty => "uncommitted changes",
        }
    }

    pub fn is_hard(self) -> bool {
        !matches!(self, Hold::Dirty)
    }
}

#[derive(Clone, Debug)]
pub struct Db {
    pub name: String,
    pub bytes: Option<u64>,
}

#[derive(Clone, Debug)]
pub struct Worktree {
    pub repo: PathBuf,
    pub repo_label: String,
    pub path: Option<PathBuf>,
    pub branch: Option<String>,
    pub key: String,
    pub verdict: Verdict,
    pub hold: Option<Hold>,
    pub upstream_gone: bool,
    pub pr_merged: bool,
    pub ahead: usize,
    pub untracked: usize,
    pub last_commit: Option<SystemTime>,
    pub touched: Option<SystemTime>,
    pub dbs: Vec<Db>,
}

impl Worktree {
    pub fn dir_label(&self) -> String {
        match (&self.path, self.verdict) {
            (_, Verdict::Main) => "main".to_string(),
            (Some(p), _) => p
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default(),
            (None, _) => self.key.clone(),
        }
    }

    pub fn label(&self) -> String {
        format!("{}/{}", self.repo_label, self.dir_label())
    }

    pub fn db_bytes(&self) -> Option<u64> {
        self.dbs.iter().map(|d| d.bytes).sum()
    }

    pub fn age(&self) -> Option<Duration> {
        self.last_commit
            .and_then(|t| SystemTime::now().duration_since(t).ok())
    }

    pub fn owns(&self, cwd: Option<&Path>) -> bool {
        let Some(cwd) = cwd else { return false };
        match &self.path {
            Some(p) => cwd.starts_with(p),
            None => db_key(cwd) == self.key,
        }
    }
}

#[derive(Clone, Debug)]
pub struct WorktreeSnapshot {
    pub rows: Vec<Worktree>,
    pub repos: usize,
    pub db_error: Option<String>,
    pub note: Option<String>,
    pub scan_time: Duration,
}

pub struct WorktreeScanner {
    code_dir: PathBuf,
    here: PathBuf,
    idle_after: Duration,
    merged_prs: Mutex<HashMap<PathBuf, (Instant, MergedPrs)>>,
    db_sizes: Arc<Mutex<HashMap<String, u64>>>,
    sizing: Arc<AtomicBool>,
}

type MergedPrs = HashMap<String, Vec<String>>;

const MERGED_PRS_TTL: Duration = Duration::from_secs(600);

const SCAN_WORKERS: usize = 16;

struct RawWorktree {
    path: PathBuf,
    head: String,
    branch: Option<String>,
    locked: bool,
}

struct RepoContext {
    repo: PathBuf,
    label: String,
    default: String,
    upstreams: HashMap<String, (bool, bool)>,
    merged_prs: MergedPrs,
}

impl WorktreeScanner {
    pub fn new(code_dir: PathBuf, idle_after: Duration) -> Self {
        let here = std::env::current_dir()
            .and_then(|d| d.canonicalize())
            .unwrap_or_default();
        Self {
            code_dir,
            here,
            idle_after,
            merged_prs: Mutex::new(HashMap::new()),
            db_sizes: Arc::new(Mutex::new(HashMap::new())),
            sizing: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn repos(&self) -> Vec<PathBuf> {
        let mut out = Vec::new();
        let Ok(orgs) = std::fs::read_dir(&self.code_dir) else {
            return out;
        };
        for org in orgs.flatten() {
            let Ok(repos) = std::fs::read_dir(org.path()) else {
                continue;
            };
            for repo in repos.flatten() {
                // A linked worktree has a .git file, not a directory; its repo is scanned already.
                if repo.path().join(".git").is_dir() {
                    out.push(repo.path());
                }
            }
        }
        out.sort();
        out
    }

    pub fn fetch(&self) -> String {
        let repos: Vec<PathBuf> = self
            .repos()
            .into_iter()
            .filter(|r| list_worktrees(r).len() > 1)
            .collect();
        let failed: Vec<String> = par_map(repos.clone(), SCAN_WORKERS, |repo| {
            // Only remotes some branch tracks: a deploy remote like heroku has no readable
            // branches and would abort the whole fetch on credentials.
            let remotes = git(&repo, &["for-each-ref", "--format=%(upstream:remotename)", "refs/heads"])
                .unwrap_or_default();
            let mut seen = Vec::new();
            let mut failed = Vec::new();
            for remote in remotes.lines().filter(|r| !r.is_empty()) {
                if seen.contains(&remote) {
                    continue;
                }
                seen.push(remote);
                let args = ["fetch", "--prune", "--quiet", "--no-recurse-submodules", remote];
                if git(&repo, &args).is_none() {
                    failed.push(format!("{}:{remote}", repo_label(&repo)));
                }
            }
            failed
        })
        .into_iter()
        .flatten()
        .collect();
        if let Ok(mut cache) = self.merged_prs.lock() {
            cache.clear();
        }
        if failed.is_empty() {
            format!("fetched {} repos", repos.len())
        } else {
            format!("fetched {} repos, failed: {}", repos.len(), failed.join(", "))
        }
    }

    pub fn scan(&self) -> WorktreeSnapshot {
        let started = Instant::now();
        let (mut dbs, db_error) = match worktree_dbs() {
            Ok(names) => (self.sized(names), None),
            Err(e) => (Vec::new(), Some(e)),
        };
        let repos = self.repos();
        let contexts: Vec<(RepoContext, Vec<RawWorktree>)> = par_map(repos.clone(), SCAN_WORKERS, |repo| {
            let list = list_worktrees(&repo);
            // A repo with only its main checkout has nothing to clean up; listing it is noise.
            let merged_prs = if list.len() > 1 {
                self.merged_prs(&repo)
            } else {
                MergedPrs::new()
            };
            let context = RepoContext {
                label: repo_label(&repo),
                default: default_branch(&repo),
                upstreams: upstream_state(&repo),
                merged_prs,
                repo,
            };
            (context, list)
        })
        .into_iter()
        .filter(|(_, list)| list.len() > 1)
        .collect();
        let jobs: Vec<(&RepoContext, usize, &RawWorktree)> = contexts
            .iter()
            .flat_map(|(ctx, list)| list.iter().enumerate().map(move |(i, raw)| (ctx, i, raw)))
            .collect();
        let mut rows: Vec<Worktree> =
            par_map(jobs, SCAN_WORKERS, |(ctx, i, raw)| self.classify(ctx, i == 0, raw));
        for row in &mut rows {
            let (mine, rest): (Vec<Db>, Vec<Db>) = dbs
                .drain(..)
                .partition(|db| db_suffix(&db.name) == Some(row.key.as_str()));
            row.dbs = mine;
            dbs = rest;
        }
        let mut dead: BTreeMap<String, Vec<Db>> = BTreeMap::new();
        for db in dbs {
            if let Some(suffix) = db_suffix(&db.name) {
                dead.entry(suffix.to_string()).or_default().push(db);
            }
        }
        for (key, dbs) in dead {
            rows.push(Worktree {
                repo: PathBuf::new(),
                repo_label: label_from_db(&dbs[0].name),
                path: None,
                branch: None,
                key,
                verdict: Verdict::DeadDb,
                hold: None,
                upstream_gone: false,
                pr_merged: false,
                ahead: 0,
                untracked: 0,
                last_commit: None,
                touched: None,
                dbs,
            });
        }
        rows.sort_by(|a, b| {
            a.repo_label
                .cmp(&b.repo_label)
                .then(a.verdict.cmp(&b.verdict))
                .then(a.dir_label().cmp(&b.dir_label()))
        });
        WorktreeSnapshot {
            rows,
            repos: repos.len(),
            db_error,
            note: None,
            scan_time: started.elapsed(),
        }
    }

    /// Sizing thousands of databases takes minutes, so sizes fill in from a background thread.
    fn sized(&self, names: Vec<String>) -> Vec<Db> {
        let Ok(mut sizes) = self.db_sizes.lock() else {
            return Vec::new();
        };
        let current: std::collections::HashSet<&String> = names.iter().collect();
        sizes.retain(|name, _| current.contains(name));
        let dbs: Vec<Db> = names
            .into_iter()
            .map(|name| Db {
                bytes: sizes.get(&name).copied(),
                name,
            })
            .collect();
        let unsized_names: Vec<String> = dbs
            .iter()
            .filter(|d| d.bytes.is_none())
            .map(|d| d.name.clone())
            .collect();
        if !unsized_names.is_empty() && !self.sizing.swap(true, Ordering::SeqCst) {
            let cache = Arc::clone(&self.db_sizes);
            let sizing = Arc::clone(&self.sizing);
            std::thread::spawn(move || {
                for chunk in unsized_names.chunks(100) {
                    let Ok(measured) = measure_dbs(chunk) else { break };
                    if let Ok(mut sizes) = cache.lock() {
                        sizes.extend(measured);
                    }
                }
                sizing.store(false, Ordering::SeqCst);
            });
        }
        dbs
    }

    fn merged_prs(&self, repo: &Path) -> MergedPrs {
        if let Ok(cache) = self.merged_prs.lock() {
            if let Some((at, prs)) = cache.get(repo) {
                if at.elapsed() < MERGED_PRS_TTL {
                    return prs.clone();
                }
            }
        }
        let prs = fetch_merged_prs(repo);
        if let Ok(mut cache) = self.merged_prs.lock() {
            cache.insert(repo.to_path_buf(), (Instant::now(), prs.clone()));
        }
        prs
    }

    fn classify(&self, ctx: &RepoContext, is_main: bool, raw: &RawWorktree) -> Worktree {
        let repo = ctx.repo.as_path();
        let exists = raw.path.is_dir();
        let (has_upstream, gone) = raw
            .branch
            .as_ref()
            .and_then(|b| ctx.upstreams.get(b))
            .copied()
            .unwrap_or((false, false));
        let (dirty, untracked) = if exists && !is_main {
            status(&raw.path)
        } else {
            (false, 0)
        };
        let reference = raw.branch.clone().unwrap_or_else(|| raw.head.clone());
        // Unknown counts as "has commits": the safe direction is to keep it.
        let ahead = if is_main {
            0
        } else {
            git(repo, &["rev-list", "--count", &format!("{}..{reference}", ctx.default)])
                .and_then(|s| s.trim().parse().ok())
                .unwrap_or(1)
        };
        let pr_merged = !is_main
            && ahead > 0
            && raw
                .branch
                .as_ref()
                .and_then(|b| ctx.merged_prs.get(b))
                .is_some_and(|heads| heads.iter().any(|h| contains(repo, h, &raw.head)));
        let last_commit = git(repo, &["log", "-1", "--format=%ct", &reference])
            .and_then(|s| s.trim().parse::<u64>().ok())
            .map(|secs| UNIX_EPOCH + Duration::from_secs(secs));
        let touched = if exists && !is_main {
            last_touched(&raw.path)
        } else {
            None
        };
        let idle = touched
            .and_then(|t| SystemTime::now().duration_since(t).ok())
            .is_some_and(|d| d >= self.idle_after);
        let verdict = if is_main {
            Verdict::Main
        } else if !exists {
            Verdict::Missing
        } else if pr_merged {
            Verdict::Merged
        } else if has_upstream && !gone {
            Verdict::Active
        } else if gone {
            if ahead == 0 {
                Verdict::Merged
            } else {
                Verdict::Gone
            }
        } else if ahead == 0 {
            if idle && !dirty && untracked == 0 {
                Verdict::Abandoned
            } else {
                Verdict::Unstarted
            }
        } else {
            Verdict::Local
        };
        let hold = if is_main {
            None
        } else if raw.locked {
            Some(Hold::Locked)
        } else if self.here.starts_with(&raw.path) {
            Some(Hold::Here)
        } else if dirty {
            Some(Hold::Dirty)
        } else {
            None
        };
        Worktree {
            repo: repo.to_path_buf(),
            repo_label: ctx.label.clone(),
            key: db_key(&raw.path),
            path: Some(raw.path.clone()),
            branch: raw.branch.clone(),
            verdict,
            hold,
            upstream_gone: gone,
            pr_merged,
            ahead,
            untracked,
            last_commit,
            touched,
            dbs: Vec::new(),
        }
    }
}

pub fn par_map<T: Send, R: Send>(
    items: Vec<T>,
    workers: usize,
    f: impl Fn(T) -> R + Sync,
) -> Vec<R> {
    let workers = workers.min(items.len()).max(1);
    let queue = Mutex::new(items.into_iter().enumerate());
    let f = &f;
    let queue = &queue;
    let mut results: Vec<(usize, R)> = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..workers)
            .map(|_| {
                scope.spawn(move || {
                    let mut done = Vec::new();
                    loop {
                        let next = queue.lock().ok().and_then(|mut q| q.next());
                        let Some((i, item)) = next else { break };
                        done.push((i, f(item)));
                    }
                    done
                })
            })
            .collect();
        handles
            .into_iter()
            .flat_map(|h| h.join().unwrap_or_default())
            .collect()
    });
    results.sort_by_key(|(i, _)| *i);
    results.into_iter().map(|(_, r)| r).collect()
}

fn fetch_merged_prs(repo: &Path) -> MergedPrs {
    let mut prs = MergedPrs::new();
    let Ok(out) = Command::new("gh")
        .current_dir(repo)
        .args([
            "pr",
            "list",
            "--state",
            "merged",
            "--limit",
            "1000",
            "--json",
            "headRefName,headRefOid",
            "--template",
            "{{range .}}{{.headRefName}}|{{.headRefOid}}\n{{end}}",
        ])
        .output()
    else {
        return prs;
    };
    if !out.status.success() {
        return prs;
    }
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        if let Some((branch, oid)) = line.split_once('|') {
            prs.entry(branch.to_string()).or_default().push(oid.to_string());
        }
    }
    prs
}

fn contains(repo: &Path, merged_head: &str, tip: &str) -> bool {
    merged_head == tip || git(repo, &["merge-base", "--is-ancestor", tip, merged_head]).is_some()
}

/// Not the index: our own `git status` refreshes it, which would make every worktree look fresh.
fn last_touched(path: &Path) -> Option<SystemTime> {
    let mut candidates = vec![path.to_path_buf()];
    if let Ok(link) = std::fs::read_to_string(path.join(".git")) {
        if let Some(dir) = link.trim().strip_prefix("gitdir: ") {
            let gitdir = path.join(dir);
            candidates.push(gitdir.join("HEAD"));
            candidates.push(gitdir.join("logs").join("HEAD"));
        }
    }
    candidates
        .iter()
        .filter_map(|p| std::fs::metadata(p).and_then(|m| m.modified()).ok())
        .max()
}

/// Without force, git refuses a worktree that gained changes since the scan that called it safe.
pub fn remove_worktree(repo: &Path, path: &Path, force: bool) -> Result<(), String> {
    let path = path.to_string_lossy();
    let mut args = vec!["worktree", "remove"];
    if force {
        args.push("--force");
    }
    args.push(&path);
    git_checked(repo, &args)
}

pub fn prune_worktrees(repo: &Path) -> Result<(), String> {
    git_checked(repo, &["worktree", "prune"])
}

pub fn delete_branches(repo: &Path, branches: &[String]) -> Result<(), String> {
    // -D, not -d: a squash-merged branch is not an ancestor of the default branch, so -d would
    // reject exactly the branches that are safest to drop.
    let mut args = vec!["branch", "-D"];
    args.extend(branches.iter().map(String::as_str));
    git_checked(repo, &args)
}

pub fn drop_db(name: &str) -> Result<(), String> {
    // --force kicks any connection a dead worktree's server left behind.
    let out = Command::new("dropdb")
        .args(["--force", name])
        .output()
        .map_err(|e| e.to_string())?;
    if out.status.success() {
        Ok(())
    } else {
        Err(first_line(&String::from_utf8_lossy(&out.stderr)))
    }
}

fn repo_label(repo: &Path) -> String {
    let parts: Vec<String> = repo
        .components()
        .rev()
        .take(2)
        .filter_map(|c| c.as_os_str().to_str().map(str::to_string))
        .collect();
    parts.into_iter().rev().collect::<Vec<_>>().join("/")
}

fn default_branch(repo: &Path) -> String {
    if let Some(head) = git(repo, &["symbolic-ref", "--quiet", "--short", "refs/remotes/origin/HEAD"]) {
        if let Some(branch) = head.trim().strip_prefix("origin/") {
            return branch.to_string();
        }
    }
    for candidate in ["main", "master", "trunk", "develop"] {
        let reference = format!("refs/heads/{candidate}");
        if git(repo, &["show-ref", "--verify", "--quiet", &reference]).is_some() {
            return candidate.to_string();
        }
    }
    git(repo, &["rev-parse", "--abbrev-ref", "HEAD"])
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| "HEAD".to_string())
}

fn upstream_state(repo: &Path) -> HashMap<String, (bool, bool)> {
    let out = git(
        repo,
        &["for-each-ref", "--format=%(refname:short)|%(upstream)|%(upstream:track)", "refs/heads"],
    )
    .unwrap_or_default();
    out.lines()
        .filter_map(|line| {
            let mut parts = line.splitn(3, '|');
            let branch = parts.next()?.to_string();
            let upstream = parts.next().unwrap_or("");
            let track = parts.next().unwrap_or("");
            Some((branch, (!upstream.is_empty(), track.contains("[gone]"))))
        })
        .collect()
}

fn list_worktrees(repo: &Path) -> Vec<RawWorktree> {
    let out = git(repo, &["worktree", "list", "--porcelain"]).unwrap_or_default();
    let mut list = Vec::new();
    let mut current: Option<RawWorktree> = None;
    for line in out.lines() {
        if let Some(path) = line.strip_prefix("worktree ") {
            if let Some(done) = current.take() {
                list.push(done);
            }
            current = Some(RawWorktree {
                path: PathBuf::from(path),
                head: String::new(),
                branch: None,
                locked: false,
            });
            continue;
        }
        let Some(wt) = current.as_mut() else { continue };
        if let Some(head) = line.strip_prefix("HEAD ") {
            wt.head = head.to_string();
        } else if let Some(branch) = line.strip_prefix("branch refs/heads/") {
            wt.branch = Some(branch.to_string());
        } else if line.starts_with("locked") {
            wt.locked = true;
        }
    }
    if let Some(done) = current {
        list.push(done);
    }
    list
}

fn status(path: &Path) -> (bool, usize) {
    let out = git(path, &["-c", "core.untrackedCache=true", "status", "--porcelain", "--untracked-files=normal"]).unwrap_or_default();
    let mut dirty = false;
    let mut untracked = 0;
    for line in out.lines() {
        if line.starts_with("??") {
            untracked += 1;
        } else if !line.is_empty() {
            dirty = true;
        }
    }
    (dirty, untracked)
}

fn worktree_dbs() -> Result<Vec<String>, String> {
    Ok(psql(
        "SELECT datname FROM pg_database \
         WHERE NOT datistemplate AND datname LIKE '%\\_wt\\_%' ORDER BY datname",
    )?
    .lines()
    .map(str::to_string)
    .collect())
}

fn measure_dbs(names: &[String]) -> Result<Vec<(String, u64)>, String> {
    let list = names
        .iter()
        .map(|n| format!("'{}'", n.replace('\'', "''")))
        .collect::<Vec<_>>()
        .join(",");
    Ok(psql(&format!(
        "SELECT datname || '|' || pg_database_size(datname) FROM pg_database WHERE datname IN ({list})"
    ))?
    .lines()
    .filter_map(|line| {
        let (name, bytes) = line.split_once('|')?;
        Some((name.to_string(), bytes.trim().parse().ok()?))
    })
    .collect())
}

fn psql(query: &str) -> Result<String, String> {
    let out = Command::new("psql")
        .args(["-X", "-d", "postgres", "-tA", "-c", query])
        .output()
        .map_err(|e| format!("psql: {e}"))?;
    if !out.status.success() {
        return Err(first_line(&String::from_utf8_lossy(&out.stderr)));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn db_suffix(name: &str) -> Option<&str> {
    let (_, suffix) = name.split_once("_wt_")?;
    // parallel_tests appends -N to the test database, so the shards belong to the same worktree.
    Some(match suffix.rsplit_once('-') {
        Some((base, shard)) if !shard.is_empty() && shard.bytes().all(|b| b.is_ascii_digit()) => base,
        _ => suffix,
    })
}

fn label_from_db(name: &str) -> String {
    let prefix = name.split_once("_wt_").map(|(p, _)| p).unwrap_or(name);
    ["_test", "_development", "_dev"]
        .iter()
        .find_map(|env| prefix.strip_suffix(env))
        .unwrap_or(prefix)
        .to_string()
}

/// Must mirror the DB_SUFFIX derivation in the worktree hooks, or live DBs look dead.
pub fn db_key(path: &Path) -> String {
    let text = path.to_string_lossy();
    let raw = match text.split_once("/.claude/worktrees/") {
        Some((_, rest)) => rest.to_string(),
        None => path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default(),
    };
    raw.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .take(30)
        .collect()
}

fn git(dir: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git").arg("-C").arg(dir).args(args).output().ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim_end().to_string())
}

fn git_checked(dir: &Path, args: &[&str]) -> Result<(), String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .map_err(|e| e.to_string())?;
    if out.status.success() {
        Ok(())
    } else {
        Err(first_line(&String::from_utf8_lossy(&out.stderr)))
    }
}

fn first_line(text: &str) -> String {
    text.lines().next().unwrap_or("failed").trim().to_string()
}
