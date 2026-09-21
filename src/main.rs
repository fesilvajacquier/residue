mod app;
mod format;
mod scan;
mod ui;

use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread;
use std::time::Duration;

use ratatui::crossterm::event::{self, Event, KeyEventKind};

use app::App;
use scan::Scanner;

struct Options {
    interval: Duration,
    stale_after: Duration,
    list_only: bool,
}

fn parse_options() -> Options {
    let mut opts = Options {
        interval: Duration::from_secs(2),
        stale_after: Duration::from_secs(24 * 3600),
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
            "--list" => opts.list_only = true,
            "-h" | "--help" => {
                println!("server-monitor [--interval SECS] [--stale-after HOURS] [--list]");
                println!("  --list   print the servers and strays as plain text and exit");
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

fn main() -> std::io::Result<()> {
    let opts = parse_options();
    if opts.list_only {
        print_list(&Scanner::new(opts.stale_after).scan());
        return Ok(());
    }
    let (snapshot_tx, snapshot_rx) = mpsc::channel();
    let (refresh_tx, refresh_rx) = mpsc::channel::<()>();

    thread::spawn(move || {
        let mut scanner = Scanner::new(opts.stale_after);
        loop {
            if snapshot_tx.send(scanner.scan()).is_err() {
                break;
            }
            if let Err(RecvTimeoutError::Disconnected) = refresh_rx.recv_timeout(opts.interval) {
                break;
            }
        }
    });

    let mut terminal = ratatui::init();
    let mut app = App::new(refresh_tx);
    let result = run(&mut terminal, &mut app, snapshot_rx);
    ratatui::restore();
    result
}

fn run(
    terminal: &mut ratatui::DefaultTerminal,
    app: &mut App,
    snapshot_rx: mpsc::Receiver<scan::Snapshot>,
) -> std::io::Result<()> {
    while !app.should_quit {
        while let Ok(snapshot) = snapshot_rx.try_recv() {
            app.apply_snapshot(snapshot);
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
