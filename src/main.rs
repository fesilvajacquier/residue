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
    list_only: bool,
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
        list_only: false,
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
            "--list" => opts.list_only = true,
            "-h" | "--help" => {
                println!(
                    "server-monitor [--interval SECS] [--stale-after HOURS] [--code-dir DIR] [--list]"
                );
                println!("  --code-dir  where repos live as DIR/<org>/<repo> (default $PGWT_CODE_DIR or ~/code)");
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
    if opts.list_only {
        print_list(&Scanner::new(opts.stale_after).scan());
        print_worktrees(&WorktreeScanner::new(opts.code_dir).scan());
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
        let scanner = WorktreeScanner::new(opts.code_dir);
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
                format::bytes(w.db_bytes())
            },
            w.hold.map(|h| h.label()).unwrap_or(""),
        );
    }
}
