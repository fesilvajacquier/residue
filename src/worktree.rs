use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Verdict {
    Main,
    Active,
    Local,
    Unstarted,
    Gone,
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
            Verdict::Merged => "✓",
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
            Verdict::Merged => "the remote branch was deleted and every commit is in the default branch",
            Verdict::Missing => "git still lists it but the directory is gone",
            Verdict::DeadDb => "no worktree matches these databases any more",
        }
    }

    pub fn is_leftover(self) -> bool {
        matches!(
            self,
            Verdict::Gone | Verdict::Merged | Verdict::Missing | Verdict::DeadDb
        )
    }

    /// Removable without an explicit force: nothing that exists only here would be lost.
    pub fn is_safe(self) -> bool {
        matches!(self, Verdict::Merged | Verdict::Missing | Verdict::DeadDb)
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
    pub bytes: u64,
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
    pub ahead: usize,
    pub untracked: usize,
    pub last_commit: Option<SystemTime>,
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

    pub fn db_bytes(&self) -> u64 {
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
}

struct RawWorktree {
    path: PathBuf,
    head: String,
    branch: Option<String>,
    locked: bool,
}

impl WorktreeScanner {
    pub fn new(code_dir: PathBuf) -> Self {
        let here = std::env::current_dir()
            .and_then(|d| d.canonicalize())
            .unwrap_or_default();
        Self { code_dir, here }
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
                if repo.path().join(".git").exists() {
                    out.push(repo.path());
                }
            }
        }
        out.sort();
        out
    }

    pub fn fetch(&self) -> String {
        let repos = self.repos();
        let mut failed = Vec::new();
        for repo in &repos {
            // Only remotes some branch tracks: a deploy remote like heroku has no readable
            // branches and would abort the whole fetch on credentials.
            let remotes = git(repo, &["for-each-ref", "--format=%(upstream:remotename)", "refs/heads"])
                .unwrap_or_default();
            let mut seen = Vec::new();
            for remote in remotes.lines().filter(|r| !r.is_empty()) {
                if seen.contains(&remote) {
                    continue;
                }
                seen.push(remote);
                if git(repo, &["fetch", "--prune", "--quiet", remote]).is_none() {
                    failed.push(format!("{}:{remote}", repo_label(repo)));
                }
            }
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
            Ok(dbs) => (dbs, None),
            Err(e) => (Vec::new(), Some(e)),
        };
        let repos = self.repos();
        let mut rows: Vec<Worktree> = std::thread::scope(|scope| {
            let handles: Vec<_> = repos
                .iter()
                .map(|r| scope.spawn(move || self.scan_repo(r)))
                .collect();
            handles
                .into_iter()
                .map(|h| h.join().unwrap_or_default())
                // A repo with only its main checkout has nothing to clean up; listing it is noise.
                .filter(|per_repo| per_repo.len() > 1)
                .flatten()
                .collect()
        });
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
                ahead: 0,
                untracked: 0,
                last_commit: None,
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

    fn scan_repo(&self, repo: &Path) -> Vec<Worktree> {
        let label = repo_label(repo);
        let default = default_branch(repo);
        let upstreams = upstream_state(repo);
        list_worktrees(repo)
            .into_iter()
            .enumerate()
            .map(|(i, raw)| {
                let is_main = i == 0;
                let exists = raw.path.is_dir();
                let (has_upstream, gone) = raw
                    .branch
                    .as_ref()
                    .and_then(|b| upstreams.get(b))
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
                    git(repo, &["rev-list", "--count", &format!("{default}..{reference}")])
                        .and_then(|s| s.trim().parse().ok())
                        .unwrap_or(1)
                };
                let last_commit = git(repo, &["log", "-1", "--format=%ct", &reference])
                    .and_then(|s| s.trim().parse::<u64>().ok())
                    .map(|secs| UNIX_EPOCH + Duration::from_secs(secs));
                let verdict = if is_main {
                    Verdict::Main
                } else if !exists {
                    Verdict::Missing
                } else if has_upstream && !gone {
                    Verdict::Active
                } else if gone {
                    if ahead == 0 {
                        Verdict::Merged
                    } else {
                        Verdict::Gone
                    }
                } else if ahead == 0 {
                    Verdict::Unstarted
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
                    repo_label: label.clone(),
                    key: db_key(&raw.path),
                    path: Some(raw.path),
                    branch: raw.branch,
                    verdict,
                    hold,
                    upstream_gone: gone,
                    ahead,
                    untracked,
                    last_commit,
                    dbs: Vec::new(),
                }
            })
            .collect()
    }
}

pub fn remove_worktree(repo: &Path, path: &Path) -> Result<(), String> {
    git_checked(repo, &["worktree", "remove", "--force", &path.to_string_lossy()])
}

pub fn prune_worktrees(repo: &Path) -> Result<(), String> {
    git_checked(repo, &["worktree", "prune"])
}

pub fn delete_branch(repo: &Path, branch: &str) -> Result<(), String> {
    // -D, not -d: a squash-merged branch is not an ancestor of the default branch, so -d would
    // reject exactly the branches that are safest to drop.
    git_checked(repo, &["branch", "-D", branch])
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
    let out = git(path, &["status", "--porcelain", "--untracked-files=all"]).unwrap_or_default();
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

fn worktree_dbs() -> Result<Vec<Db>, String> {
    let out = Command::new("psql")
        .args([
            "-X",
            "-d",
            "postgres",
            "-tA",
            "-F|",
            "-c",
            "SELECT datname, pg_database_size(datname) FROM pg_database \
             WHERE NOT datistemplate AND datname LIKE '%\\_wt\\_%' ORDER BY datname",
        ])
        .output()
        .map_err(|e| format!("psql: {e}"))?;
    if !out.status.success() {
        return Err(first_line(&String::from_utf8_lossy(&out.stderr)));
    }
    Ok(String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|line| {
            let (name, bytes) = line.split_once('|')?;
            Some(Db {
                name: name.to_string(),
                bytes: bytes.trim().parse().ok()?,
            })
        })
        .collect())
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
