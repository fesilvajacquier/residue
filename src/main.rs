mod app;
mod format;
mod scan;
mod ui;
mod worktree;

use std::path::PathBuf;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread;
use std::time::Duration;

use ratatui::crossterm::event::{self, Event, KeyEventKind};

use app::{App, WtCommand};
use scan::Scanner;
use worktree::WorktreeScanner;

const WORKTREE_INTERVAL: Duration = Duration::from_secs(30);

struct Options {
    interval: Duration,
    stale_after: Duration,
    code_dir: PathBuf,
    idle_after: Duration,
    list_only: bool,
    clean: bool,
    dry_run: bool,
}

const DEFAULT_IDLE_DAYS: u64 = 15;

fn t3_idle_days() -> Option<u64> {
    let home = std::env::var("HOME").ok()?;
    let settings =
        std::fs::read_to_string(PathBuf::from(home).join(".t3/userdata/settings.json")).ok()?;
    let (_, rest) = settings.split_once("\"sidebarAutoSettleAfterDays\"")?;
    let digits: String = rest
        .trim_start_matches(|c: char| c == ':' || c.is_whitespace())
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect();
    digits.parse().ok()
}

fn days(n: u64) -> Duration {
    Duration::from_secs(n * 24 * 3600)
}

fn default_code_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("PGWT_CODE_DIR") {
        return PathBuf::from(dir);
    }
    PathBuf::from(std::env::var("HOME").unwrap_or_default()).join("code")
}

fn parse_options() -> Options {
    let mut opts = Options {
        interval: Duration::from_secs(2),
        stale_after: Duration::from_secs(24 * 3600),
        code_dir: default_code_dir(),
        idle_after: days(t3_idle_days().unwrap_or(DEFAULT_IDLE_DAYS)),
        list_only: false,
        clean: false,
        dry_run: false,
    };
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--interval" => {
                let secs: u64 = args.next().and_then(|v| v.parse().ok()).unwrap_or(2);
                opts.interval = Duration::from_secs(secs.max(1));
            }
            "--stale-after" => {
                let hours: u64 = args.next().and_then(|v| v.parse().ok()).unwrap_or(24);
                opts.stale_after = Duration::from_secs(hours * 3600);
            }
            "--code-dir" => {
                if let Some(dir) = args.next() {
                    opts.code_dir = PathBuf::from(dir);
                }
            }
            "--idle-after" => {
                let n: u64 = args.next().and_then(|v| v.parse().ok()).unwrap_or(DEFAULT_IDLE_DAYS);
                opts.idle_after = days(n);
            }
            "--list" => opts.list_only = true,
            "--clean" => opts.clean = true,
            "--dry-run" => opts.dry_run = true,
            "-h" | "--help" => {
                println!(
                    "residue [--interval SECS] [--stale-after HOURS] [--code-dir DIR] [--idle-after DAYS] [--list] [--clean [--dry-run]]"
                );
                println!("  --code-dir  where repos live as DIR/<org>/<repo> (default $PGWT_CODE_DIR or ~/code)");
                println!("  --idle-after  days before a worktree with nothing of its own counts as abandoned (default: T3 Code's sidebarAutoSettleAfterDays, else {DEFAULT_IDLE_DAYS})");
                println!("  --clean     fetch, then do what K does on every repo: remove each provably safe leftover, and exit");
                println!("  --dry-run   with --clean, print what would be removed without touching anything");
                println!("  --list      print servers, strays and leftover worktrees as plain text and exit");
                std::process::exit(0);
            }
            other => {
                eprintln!("unknown argument: {other}");
                std::process::exit(2);
            }
        }
    }
    opts
}

enum Message {
    Procs(scan::Snapshot),
    Worktrees(worktree::WorktreeSnapshot),
}

fn main() -> std::io::Result<()> {
    let opts = parse_options();
    if opts.clean {
        std::process::exit(clean(opts));
    }
    if opts.list_only {
        print_list(&Scanner::new(opts.stale_after).scan());
        print_worktrees(&WorktreeScanner::new(opts.code_dir, opts.idle_after).scan());
        return Ok(());
    }
    let (tx, rx) = mpsc::channel();
    let (refresh_tx, refresh_rx) = mpsc::channel::<()>();
    let (wt_tx, wt_rx) = mpsc::channel::<WtCommand>();

    let procs_tx = tx.clone();
    thread::spawn(move || {
        let mut scanner = Scanner::new(opts.stale_after);
        loop {
            if procs_tx.send(Message::Procs(scanner.scan())).is_err() {
                break;
            }
            if let Err(RecvTimeoutError::Disconnected) = refresh_rx.recv_timeout(opts.interval) {
                break;
            }
        }
    });

    thread::spawn(move || {
        let scanner = WorktreeScanner::new(opts.code_dir, opts.idle_after);
        let mut note = None;
        loop {
            let mut snapshot = scanner.scan();
            snapshot.note = note.take();
            if tx.send(Message::Worktrees(snapshot)).is_err() {
                break;
            }
            match wt_rx.recv_timeout(WORKTREE_INTERVAL) {
                Ok(WtCommand::Fetch) => note = Some(scanner.fetch()),
                Ok(WtCommand::Rescan) | Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break,
            }
        }
    });

    let mut terminal = ratatui::init();
    let mut app = App::new(refresh_tx, wt_tx);
    let result = run(&mut terminal, &mut app, rx);
    ratatui::restore();
    result
}

fn run(
    terminal: &mut ratatui::DefaultTerminal,
    app: &mut App,
    rx: mpsc::Receiver<Message>,
) -> std::io::Result<()> {
    while !app.should_quit {
        while let Ok(message) = rx.try_recv() {
            match message {
                Message::Procs(snapshot) => app.apply_snapshot(snapshot),
                Message::Worktrees(snapshot) => app.apply_worktrees(snapshot),
            }
        }
        app.tick();
        terminal.draw(|frame| ui::draw(frame, app))?;
        if event::poll(Duration::from_millis(200))? {
            if let Event::Key(key) = event::read()? {
                if key.kind == KeyEventKind::Press {
                    app.handle_key(key);
                }
            }
        }
    }
    Ok(())
}

fn clean(opts: Options) -> i32 {
    let started = chrono::Local::now();
    println!("residue --clean{}  {}", if opts.dry_run { " --dry-run" } else { "" }, started.format("%Y-%m-%d %H:%M:%S"));
    let scanner = WorktreeScanner::new(opts.code_dir, opts.idle_after);
    println!("{}", scanner.fetch());
    let (procs_tx, _procs_rx) = mpsc::channel();
    let (wt_tx, _wt_rx) = mpsc::channel();
    let mut app = App::new(procs_tx, wt_tx);
    app.apply_snapshot(Scanner::new(opts.stale_after).scan());
    let snapshot = scanner.scan();
    if let Some(err) = &snapshot.db_error {
        println!("no database info: {err}");
    }
    app.apply_worktrees(snapshot);
    let items = app.safe_leftovers();
    println!("{} safe leftover(s)", items.len());
    for item in &items {
        println!("  {}", item.summary.trim_end());
        for step in &item.steps {
            println!("      {step}");
        }
    }
    if opts.dry_run || items.is_empty() {
        return 0;
    }
    let (summary, errors) = app::remove_now(items);
    for e in &errors {
        println!("  failed: {e}");
    }
    let elapsed = (chrono::Local::now() - started).num_seconds();
    println!("{summary}  ({elapsed}s)");
    i32::from(!errors.is_empty())
}

fn print_list(snapshot: &scan::Snapshot) {
    println!(
        "{:<7} {:<6} {:<40} {:<24} {:>7} {:>6}  DIR",
        "STATUS", "PORT", "PROJECT", "BRANCH", "AGE", "MEM"
    );
    for s in &snapshot.servers {
        let root = s.root();
        println!(
            "{:<7} {:<6} {:<40} {:<24} {:>7} {:>6}  {}{}",
            s.status.label(),
            s.port,
            format::truncate(&s.project, 40),
            format::truncate(s.branch.as_deref().unwrap_or("-"), 24),
            format::age(root.age),
            format::bytes(s.total_rss()),
            root.cwd
                .as_ref()
                .map(|c| c.to_string_lossy().into_owned())
                .unwrap_or_default(),
            if root.cwd_deleted { " (deleted)" } else { "" },
        );
    }
    if !snapshot.strays.is_empty() {
        println!("\nSTRAYS (processes whose working directory was deleted)");
        for s in &snapshot.strays {
            let p = &s.root;
            println!(
                "{:>8} {:<16} {:>3} {:>7} {:>6}  {}  {}",
                p.pid,
                format::truncate(&p.program(), 16),
                s.children.len() + 1,
                format::age(p.age),
                format::bytes(s.total_rss()),
                p.cwd
                    .as_ref()
                    .map(|c| c.to_string_lossy().into_owned())
                    .unwrap_or_default(),
                format::truncate(p.short_command(), 60),
            );
        }
    }
}

fn print_worktrees(snapshot: &worktree::WorktreeSnapshot) {
    let leftovers: Vec<&worktree::Worktree> = snapshot
        .rows
        .iter()
        .filter(|w| w.verdict.is_leftover())
        .collect();
    if let Some(err) = &snapshot.db_error {
        println!("\n(no database info: {err})");
    }
    if leftovers.is_empty() {
        return;
    }
    println!(
        "\nLEFTOVER WORKTREES ({} of {} across {} repos)",
        leftovers.len(),
        snapshot.rows.len(),
        snapshot.repos
    );
    println!(
        "{:<12} {:<40} {:<40} {:>6}  {}",
        "STATE", "WORKTREE", "BRANCH", "DB", "HOLD"
    );
    for w in leftovers {
        println!(
            "{:<12} {:<40} {:<40} {:>6}  {}",
            w.verdict.label(),
            format::truncate(&w.label(), 40),
            format::truncate(w.branch.as_deref().unwrap_or("-"), 40),
            if w.dbs.is_empty() {
                "-".to_string()
            } else {
                format::maybe_bytes(w.db_bytes())
            },
            w.hold.map(|h| h.label()).unwrap_or(""),
        );
    }
}
