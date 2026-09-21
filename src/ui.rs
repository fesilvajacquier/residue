use ratatui::layout::{Constraint, Flex, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Cell, Clear, Paragraph, Row, Table, TableState, Wrap};
use ratatui::Frame;

use crate::app::{Action, App, KillRequest, Mode, Panel, RemoveRequest};
use crate::format;
use crate::scan::{ProcInfo, Server, Status, Stray};
use crate::worktree::{Verdict, Worktree};

const FOCUS: Color = Color::Cyan;
const DIM: Color = Color::DarkGray;
const SELECTED: Style = Style::new().bg(Color::Rgb(40, 44, 52)).add_modifier(Modifier::BOLD);
const HEADER: Style = Style::new().fg(DIM).add_modifier(Modifier::UNDERLINED);

pub fn draw(frame: &mut Frame, app: &App) {
    let [body, footer] =
        Layout::vertical([Constraint::Min(5), Constraint::Length(1)]).areas(frame.area());
    let [left, right] =
        Layout::horizontal([Constraint::Percentage(52), Constraint::Percentage(48)]).areas(body);
    let stray_height = (app.stray_rows.len() as u16 + 3).clamp(4, body.height / 5);
    let worktree_height = (app.worktree_rows.len() as u16 + 3).clamp(4, body.height * 2 / 5);
    let [servers_area, strays_area, worktrees_area] = Layout::vertical([
        Constraint::Min(4),
        Constraint::Length(stray_height),
        Constraint::Length(worktree_height),
    ])
    .areas(left);

    draw_servers(frame, app, servers_area);
    draw_strays(frame, app, strays_area);
    draw_worktrees(frame, app, worktrees_area);
    draw_details(frame, app, right);
    draw_footer(frame, app, footer);

    match &app.mode {
        Mode::Confirm(Action::Kill(req)) => draw_confirm_kill(frame, req),
        Mode::Confirm(Action::Remove(req)) => draw_confirm_remove(frame, req),
        Mode::Help => draw_help(frame),
        _ => {}
    }
}

fn panel_block(title: Line<'static>, focused: bool) -> Block<'static> {
    let color = if focused { FOCUS } else { DIM };
    Block::bordered()
        .border_type(if focused {
            BorderType::Thick
        } else {
            BorderType::Plain
        })
        .border_style(Style::new().fg(color))
        .title(title)
}

fn status_style(status: Status) -> Style {
    match status {
        Status::Orphan => Style::new().fg(Color::Red).bold(),
        Status::Stale => Style::new().fg(Color::Yellow),
        Status::Ok => Style::new().fg(Color::Green),
    }
}

fn verdict_style(verdict: Verdict) -> Style {
    match verdict {
        Verdict::Main => Style::new().fg(DIM),
        Verdict::Active => Style::new().fg(Color::Green),
        Verdict::Local => Style::new().fg(Color::Blue),
        Verdict::Unstarted => Style::new().fg(DIM),
        Verdict::Gone => Style::new().fg(Color::Yellow),
        Verdict::Merged => Style::new().fg(Color::Yellow).bold(),
        Verdict::Missing | Verdict::DeadDb => Style::new().fg(Color::Red).bold(),
    }
}

fn draw_servers(frame: &mut Frame, app: &App, area: Rect) {
    let servers = app.visible_servers();
    let (orphans, stale) = app.problem_count();
    let mut title = vec![Span::raw(format!(" [1] Servers ({}) ", servers.len()))];
    if orphans > 0 {
        title.push(Span::styled(
            format!("✗{orphans} "),
            Style::new().fg(Color::Red),
        ));
    }
    if stale > 0 {
        title.push(Span::styled(
            format!("!{stale} "),
            Style::new().fg(Color::Yellow),
        ));
    }
    let block = panel_block(Line::from(title), app.panel == Panel::Servers);

    let header = Row::new(["", "PORT", "PROJECT", "BRANCH", "AGE", "MEM", "CPU"]).style(HEADER);
    let rows = servers.iter().map(|s| server_row(s));
    let widths = [
        Constraint::Length(1),
        Constraint::Length(6),
        Constraint::Min(14),
        Constraint::Length(18),
        Constraint::Length(7),
        Constraint::Length(6),
        Constraint::Length(5),
    ];
    let table = Table::new(rows, widths)
        .header(header)
        .block(block)
        .column_spacing(1)
        .row_highlight_style(SELECTED)
        .highlight_symbol("▶");
    let mut state = TableState::default().with_selected(app.selected_index(Panel::Servers));
    frame.render_stateful_widget(table, area, &mut state);
}

fn server_row(s: &Server) -> Row<'_> {
    let root = s.root();
    let port = if s.is_public() {
        format!("*{}", s.port)
    } else {
        s.port.to_string()
    };
    let branch = match (&s.branch, root.cwd_deleted) {
        (Some(b), _) => Span::raw(b.clone()),
        (None, true) => Span::styled("(dir deleted)", Style::new().fg(Color::Red)),
        (None, false) => Span::styled("-", Style::new().fg(DIM)),
    };
    Row::new(vec![
        Cell::from(Span::styled(s.status.symbol(), status_style(s.status))),
        Cell::from(port),
        Cell::from(s.project.clone()),
        Cell::from(branch),
        Cell::from(format::age(root.age)),
        Cell::from(format::bytes(s.total_rss())),
        Cell::from(format!("{:.0}%", s.total_cpu())),
    ])
}

fn draw_strays(frame: &mut Frame, app: &App, area: Rect) {
    let strays = app.visible_strays();
    let title = Line::from(format!(
        " [2] Strays: processes in deleted dirs ({}) ",
        strays.len()
    ));
    let block = panel_block(title, app.panel == Panel::Strays);
    let header = Row::new(["PID", "PROGRAM", "PROCS", "DIR", "AGE", "MEM"]).style(HEADER);
    let rows = strays.iter().map(|s| {
        let p = &s.root;
        Row::new(vec![
            Cell::from(p.pid.to_string()),
            Cell::from(Span::styled(p.program(), Style::new().fg(Color::Magenta))),
            Cell::from((s.children.len() + 1).to_string()),
            Cell::from(crate::scan::project_label(p.cwd.as_deref())),
            Cell::from(format::age(p.age)),
            Cell::from(format::bytes(s.total_rss())),
        ])
    });
    let widths = [
        Constraint::Length(8),
        Constraint::Length(18),
        Constraint::Length(5),
        Constraint::Min(16),
        Constraint::Length(7),
        Constraint::Length(6),
    ];
    let table = Table::new(rows, widths)
        .header(header)
        .block(block)
        .column_spacing(1)
        .row_highlight_style(SELECTED)
        .highlight_symbol("▶");
    let mut state = TableState::default().with_selected(app.selected_index(Panel::Strays));
    frame.render_stateful_widget(table, area, &mut state);
}

fn draw_worktrees(frame: &mut Frame, app: &App, area: Rect) {
    let worktrees = app.visible_worktrees();
    let (safe, check) = app.leftover_count();
    let mut title = vec![Span::raw(format!(" [3] Worktrees ({}) ", worktrees.len()))];
    if safe > 0 {
        title.push(Span::styled(
            format!("✓{safe} "),
            Style::new().fg(Color::Yellow),
        ));
    }
    if check > 0 {
        title.push(Span::styled(
            format!("?{check} "),
            Style::new().fg(Color::Yellow),
        ));
    }
    if let Some(err) = app.worktrees.as_ref().and_then(|w| w.db_error.as_ref()) {
        title.push(Span::styled(
            format!("no db info: {} ", format::truncate(err, 30)),
            Style::new().fg(Color::Red),
        ));
    }
    let block = panel_block(Line::from(title), app.panel == Panel::Worktrees);
    let header =
        Row::new(["", "REPO", "DIR", "BRANCH", "STATE", "PORTS", "DB", "AGE"]).style(HEADER);
    let rows = worktrees.iter().map(|w| worktree_row(app, w));
    let widths = [
        Constraint::Length(1),
        Constraint::Length(11),
        Constraint::Length(24),
        Constraint::Min(12),
        Constraint::Length(11),
        Constraint::Length(10),
        Constraint::Length(6),
        Constraint::Length(7),
    ];
    let table = Table::new(rows, widths)
        .header(header)
        .block(block)
        .column_spacing(1)
        .row_highlight_style(SELECTED)
        .highlight_symbol("▶");
    let mut state = TableState::default().with_selected(app.selected_index(Panel::Worktrees));
    frame.render_stateful_widget(table, area, &mut state);
}

fn worktree_row<'a>(app: &'a App, w: &'a Worktree) -> Row<'a> {
    let marker = match w.hold {
        Some(_) => Span::styled("!", Style::new().fg(Color::Red)),
        None => Span::styled(w.verdict.symbol(), verdict_style(w.verdict)),
    };
    let repo = w
        .repo_label
        .rsplit('/')
        .next()
        .unwrap_or(&w.repo_label)
        .to_string();
    let branch = match &w.branch {
        Some(b) => Span::raw(b.clone()),
        None if w.verdict == Verdict::DeadDb => Span::styled("-", Style::new().fg(DIM)),
        None => Span::styled("(detached)", Style::new().fg(DIM)),
    };
    let mut ports: Vec<String> = app
        .servers_for(w)
        .iter()
        .map(|s| s.port.to_string())
        .collect();
    let strays = app.strays_for(w).len();
    if strays > 0 {
        ports.push(format!("+{strays}"));
    }
    let db = if w.dbs.is_empty() {
        Span::styled("-", Style::new().fg(DIM))
    } else {
        Span::raw(format::bytes(w.db_bytes()))
    };
    let age = w
        .age()
        .map(format::age)
        .unwrap_or_else(|| "-".to_string());
    Row::new(vec![
        Cell::from(marker),
        Cell::from(repo),
        Cell::from(w.dir_label()),
        Cell::from(branch),
        Cell::from(Span::styled(w.verdict.label(), verdict_style(w.verdict))),
        Cell::from(ports.join(",")),
        Cell::from(db),
        Cell::from(age),
    ])
}

fn draw_details(frame: &mut Frame, app: &App, area: Rect) {
    let block = panel_block(Line::from(" Details "), false);
    let lines = match app.panel {
        Panel::Servers => match (&app.snapshot, app.selected_server()) {
            (None, _) => vec![Line::from("scanning /proc…")],
            (_, Some(s)) => server_details(app, s),
            (_, None) => vec![Line::from("no server matches").fg(DIM)],
        },
        Panel::Strays => match (&app.snapshot, app.selected_stray()) {
            (None, _) => vec![Line::from("scanning /proc…")],
            (_, Some(s)) => stray_details(app, s),
            (_, None) => vec![Line::from("no stray processes, nice").fg(DIM)],
        },
        Panel::Worktrees => match (&app.worktrees, app.selected_worktree()) {
            (None, _) => vec![Line::from("scanning worktrees and databases…")],
            (_, Some(w)) => worktree_details(app, w),
            (_, None) => vec![Line::from("no worktree matches").fg(DIM)],
        },
    };
    frame.render_widget(
        Paragraph::new(lines)
            .block(block)
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn field<'a>(name: &'a str, value: impl Into<Span<'a>>) -> Line<'a> {
    Line::from(vec![
        Span::styled(format!("{name:<9}"), Style::new().fg(DIM)),
        value.into(),
    ])
}

fn heading(text: &str) -> Line<'static> {
    Line::from(Span::styled(
        text.to_string(),
        Style::new().fg(DIM).underlined(),
    ))
}

fn worktree_line<'a>(app: &'a App, cwd: Option<&std::path::Path>, deleted: bool) -> Line<'a> {
    match app.worktree_for(cwd) {
        Some(w) => {
            let mut text = format!("{}  {}", w.label(), w.verdict.label());
            if let Some(h) = w.hold {
                text.push_str(&format!(", {}", h.label()));
            }
            field("Worktree", Span::styled(text, verdict_style(w.verdict)))
        }
        None if deleted => field(
            "Worktree",
            Span::styled("gone, git no longer lists it", Style::new().fg(Color::Red)),
        ),
        None if app.worktrees.is_none() => field("Worktree", Span::styled("scanning…", Style::new().fg(DIM))),
        None => field("Worktree", Span::styled("not in a worktree", Style::new().fg(DIM))),
    }
}

fn server_details<'a>(app: &'a App, s: &'a Server) -> Vec<Line<'a>> {
    let root = s.root();
    let mut lines = vec![
        field(
            "Port",
            Span::raw(format!("{}  bound on {}", s.port, s.binds.join(", "))),
        ),
        field(
            "URL",
            Span::styled(
                format!("http://localhost:{}", s.port),
                Style::new().fg(Color::Blue),
            ),
        ),
        field(
            "Status",
            Span::styled(
                format!("{} {}", s.status.label(), status_reason(s)),
                status_style(s.status),
            ),
        ),
        field("Project", Span::raw(s.project.clone())),
        field(
            "Branch",
            s.branch
                .clone()
                .map(Span::raw)
                .unwrap_or_else(|| Span::styled("unknown", Style::new().fg(DIM))),
        ),
    ];
    lines.extend(dir_lines(root));
    lines.push(worktree_line(app, root.cwd.as_deref(), root.cwd_deleted));
    lines.push(field(
        "Started",
        Span::raw(format!(
            "{}  ({} ago)",
            format::timestamp(root.started_at),
            format::age(root.age)
        )),
    ));
    lines.push(field(
        "Memory",
        Span::raw(format!(
            "{} across {} process(es)",
            format::bytes(s.total_rss()),
            s.members.len() + s.children.len()
        )),
    ));
    lines.push(field("CPU", Span::raw(format!("{:.1}%", s.total_cpu()))));
    lines.push(Line::from(""));
    lines.push(heading("Processes holding the port"));
    let roots: Vec<i32> = s.roots().iter().map(|r| r.pid).collect();
    for m in &s.members {
        let indent = if roots.contains(&m.pid) { "" } else { "  ↳ " };
        lines.push(process_line(indent, m));
    }
    if !s.children.is_empty() {
        lines.push(Line::from(""));
        lines.push(heading("Child processes (no port)"));
        for c in &s.children {
            lines.push(process_line("  ↳ ", c));
        }
    }
    lines
}

fn status_reason(s: &Server) -> &'static str {
    match s.status {
        Status::Orphan => "— its working directory no longer exists",
        Status::Stale => "— running longer than the stale threshold",
        Status::Ok => "",
    }
}

fn dir_lines(p: &ProcInfo) -> Vec<Line<'_>> {
    let dir = p
        .cwd
        .as_ref()
        .map(|c| c.to_string_lossy().into_owned())
        .unwrap_or_else(|| "?".into());
    let mut spans = vec![Span::raw(dir)];
    if p.cwd_deleted {
        spans.push(Span::styled(
            "  [DELETED]",
            Style::new().fg(Color::Red).bold(),
        ));
    }
    vec![Line::from(
        std::iter::once(Span::styled(format!("{:<9}", "Dir"), Style::new().fg(DIM)))
            .chain(spans)
            .collect::<Vec<_>>(),
    )]
}

fn stray_details<'a>(app: &'a App, s: &'a Stray) -> Vec<Line<'a>> {
    let p = &s.root;
    let mut lines = vec![
        field("PID", Span::raw(format!("{}  (parent {})", p.pid, p.ppid))),
        field(
            "Program",
            Span::styled(p.program(), Style::new().fg(Color::Magenta)),
        ),
        field(
            "Status",
            Span::styled(
                "STRAY — its working directory no longer exists",
                status_style(Status::Orphan),
            ),
        ),
    ];
    lines.extend(dir_lines(p));
    lines.push(worktree_line(app, p.cwd.as_deref(), true));
    lines.push(field(
        "Started",
        Span::raw(format!(
            "{}  ({} ago)",
            format::timestamp(p.started_at),
            format::age(p.age)
        )),
    ));
    lines.push(field(
        "Memory",
        Span::raw(format!(
            "{} across {} process(es)",
            format::bytes(s.total_rss()),
            s.children.len() + 1
        )),
    ));
    lines.push(field("CPU", Span::raw(format!("{:.1}%", p.cpu_percent))));
    lines.push(Line::from(""));
    lines.push(heading("Command line"));
    lines.push(Line::from(p.cmdline.clone()));
    if !s.children.is_empty() {
        lines.push(Line::from(""));
        lines.push(heading("Child processes"));
        for c in &s.children {
            lines.push(process_line("  ↳ ", c));
        }
    }
    lines
}

fn worktree_details<'a>(app: &'a App, w: &'a Worktree) -> Vec<Line<'a>> {
    let mut lines = vec![field("Repo", Span::raw(w.repo_label.clone()))];
    match &w.path {
        Some(p) => {
            let mut spans = vec![
                Span::styled(format!("{:<9}", "Path"), Style::new().fg(DIM)),
                Span::raw(p.to_string_lossy().into_owned()),
            ];
            if w.verdict == Verdict::Missing {
                spans.push(Span::styled(
                    "  [MISSING]",
                    Style::new().fg(Color::Red).bold(),
                ));
            }
            lines.push(Line::from(spans));
        }
        None => lines.push(field(
            "Path",
            Span::styled("no worktree, only databases", Style::new().fg(Color::Red)),
        )),
    }
    if let Some(b) = &w.branch {
        let upstream = if w.upstream_gone {
            "  (remote branch deleted)"
        } else if w.verdict == Verdict::Active {
            "  (tracks a live remote branch)"
        } else {
            "  (never pushed)"
        };
        lines.push(field("Branch", Span::raw(format!("{b}{upstream}"))));
    }
    lines.push(field(
        "State",
        Span::styled(
            format!("{} — {}", w.verdict.label(), w.verdict.explain()),
            verdict_style(w.verdict),
        ),
    ));
    if let Some(h) = w.hold {
        lines.push(field(
            "Hold",
            Span::styled(h.label(), Style::new().fg(Color::Red).bold()),
        ));
    }
    if w.verdict != Verdict::DeadDb && w.verdict != Verdict::Main {
        let commits = if w.ahead == 0 {
            "none of its own".to_string()
        } else {
            format!("{} not in the default branch", w.ahead)
        };
        lines.push(field("Commits", Span::raw(commits)));
        let changes = match (w.hold == Some(crate::worktree::Hold::Dirty), w.untracked) {
            (false, 0) => "clean".to_string(),
            (true, 0) => "uncommitted changes".to_string(),
            (false, n) => format!("{n} untracked file(s)"),
            (true, n) => format!("uncommitted changes, {n} untracked file(s)"),
        };
        lines.push(field("Changes", Span::raw(changes)));
    }
    if let Some(t) = w.last_commit {
        lines.push(field(
            "Commit",
            Span::raw(format!(
                "{}  ({} ago)",
                format::timestamp(t),
                w.age().map(format::age).unwrap_or_default()
            )),
        ));
    }

    lines.push(Line::from(""));
    lines.push(heading("Databases"));
    if w.dbs.is_empty() {
        lines.push(Line::from("  none").fg(DIM));
    }
    for db in &w.dbs {
        lines.push(Line::from(format!(
            "  {:>6}  {}",
            format::bytes(db.bytes),
            db.name
        )));
    }

    lines.push(Line::from(""));
    lines.push(heading("Running in it"));
    let servers = app.servers_for(w);
    let strays = app.strays_for(w);
    if servers.is_empty() && strays.is_empty() {
        lines.push(Line::from("  nothing").fg(DIM));
    }
    for s in servers {
        let root = s.root();
        lines.push(Line::from(vec![
            Span::styled(format!("  {:<6}", s.port), Style::new().fg(Color::Blue)),
            Span::styled(
                format!("{} ", s.status.symbol()),
                status_style(s.status),
            ),
            Span::styled(
                format!("{:<14} ", format::truncate(&root.program(), 14)),
                Style::new().fg(Color::Magenta),
            ),
            Span::raw(format!(
                "{:>6} {:>5}",
                format::age(root.age),
                format::bytes(s.total_rss())
            )),
        ]));
    }
    for s in strays {
        lines.push(process_line("  stray ", &s.root));
    }

    lines.push(Line::from(""));
    lines.push(heading("Keys"));
    let removal = match (w.verdict, w.hold) {
        (Verdict::Main, _) => "x  never removes the main checkout".to_string(),
        (_, Some(h)) if h.is_hard() => format!("x  blocked: {}", h.label()),
        (_, Some(h)) => format!("x  blocked: {}  ·  X forces it", h.label()),
        (v, None) if v.is_safe() => "x  stop its processes, remove it, drop its databases".to_string(),
        (v, None) => format!("x  refuses ({})  ·  X forces it", v.label()),
    };
    lines.push(Line::from(format!("  {removal}")));
    lines.push(Line::from("  d  stop everything running in it"));
    lines
}

fn process_line<'a>(indent: &'a str, m: &'a ProcInfo) -> Line<'a> {
    Line::from(vec![
        Span::raw(format!("{indent}{:>7} ", m.pid)),
        Span::styled(
            format!("{:<14} ", format::truncate(&m.program(), 14)),
            Style::new().fg(Color::Magenta),
        ),
        Span::raw(format!(
            "{:>6} {:>5} {:>4.0}%  ",
            format::age(m.age),
            format::bytes(m.rss_bytes),
            m.cpu_percent
        )),
        Span::styled(
            format::truncate(m.short_command(), 90),
            Style::new().fg(DIM),
        ),
    ])
}

fn draw_footer(frame: &mut Frame, app: &App, area: Rect) {
    let line = match &app.mode {
        Mode::Filter => Line::from(vec![
            Span::styled(" filter: ", Style::new().fg(FOCUS)),
            Span::raw(app.filter.clone()),
            Span::styled("▏", Style::new().fg(FOCUS)),
            Span::styled("   enter keep · esc clear", Style::new().fg(DIM)),
        ]),
        _ if app.toast.is_some() => Line::from(Span::styled(
            format!(
                " {}",
                app.toast.as_ref().map(|t| t.0.as_str()).unwrap_or("")
            ),
            Style::new().fg(Color::Black).bg(Color::Yellow),
        )),
        _ => {
            let hints: &[(&str, &str)] = match app.panel {
                Panel::Worktrees => &[
                    ("j/k", "move"),
                    ("tab", "panel"),
                    ("x", "remove"),
                    ("X", "force remove"),
                    ("d", "stop its procs"),
                    ("K", "clean all safe"),
                    ("f", "fetch"),
                    ("o", "leftovers only"),
                    ("/", "filter"),
                    ("?", "help"),
                    ("q", "quit"),
                ],
                _ => &[
                    ("j/k", "move"),
                    ("tab", "panel"),
                    ("d", "stop"),
                    ("D", "force kill"),
                    ("K", "kill all orphans"),
                    ("o", "problems only"),
                    ("s", "sort"),
                    ("/", "filter"),
                    ("?", "help"),
                    ("q", "quit"),
                ],
            };
            let mut spans = hint_spans(hints);
            let mut state = Vec::new();
            state.push(format!("sort:{}", app.sort.label()));
            if app.only_problems {
                state.push("problems only".into());
            }
            if !app.filter.is_empty() {
                state.push(format!("filter:{}", app.filter));
            }
            if let Some(snap) = &app.snapshot {
                state.push(format!("scan {}ms", snap.scan_time.as_millis()));
            }
            if let Some(wt) = &app.worktrees {
                state.push(format!(
                    "{} repos {}ms",
                    wt.repos,
                    wt.scan_time.as_millis()
                ));
            }
            spans.push(Span::styled(
                format!("  [{}]", state.join(" · ")),
                Style::new().fg(DIM),
            ));
            Line::from(spans)
        }
    };
    frame.render_widget(Paragraph::new(line), area);
}

fn hint_spans(pairs: &[(&str, &str)]) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    for (key, desc) in pairs {
        spans.push(Span::styled(
            format!(" {key}"),
            Style::new().fg(FOCUS).bold(),
        ));
        spans.push(Span::styled(format!(" {desc}"), Style::new().fg(DIM)));
    }
    spans
}

fn popup(frame: &mut Frame, width: u16, height: u16) -> Rect {
    let [area] = Layout::horizontal([Constraint::Length(width)])
        .flex(Flex::Center)
        .areas(frame.area());
    let [area] = Layout::vertical([Constraint::Length(height)])
        .flex(Flex::Center)
        .areas(area);
    frame.render_widget(Clear, area);
    area
}

fn confirm_prompt(verb: Vec<Span<'static>>) -> Line<'static> {
    let mut spans = vec![Span::raw("  ")];
    spans.extend(verb);
    spans.extend([
        Span::raw("?   "),
        Span::styled("y", Style::new().fg(FOCUS).bold()),
        Span::raw(" yes   "),
        Span::styled("n", Style::new().fg(FOCUS).bold()),
        Span::raw(" no"),
    ]);
    Line::from(spans)
}

fn render_confirm(frame: &mut Frame, lines: Vec<Line<'_>>) {
    let width = frame.area().width.min(96);
    let height = (lines.len() as u16 + 2).min(frame.area().height);
    let area = popup(frame, width, height);
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(Color::Red))
        .title(" Confirm ");
    frame.render_widget(
        Paragraph::new(lines)
            .block(block)
            .wrap(Wrap { trim: false }),
        area,
    );
}

const MAX_LISTED: usize = 12;

fn draw_confirm_kill(frame: &mut Frame, req: &KillRequest) {
    let mut lines = vec![
        Line::from(""),
        Line::from(Span::styled(req.title.clone(), Style::new().bold())),
        Line::from(""),
    ];
    for t in req.targets.iter().take(MAX_LISTED) {
        lines.push(Line::from(vec![
            Span::styled(format!("  {:>8}  ", t.pid), Style::new().fg(Color::Magenta)),
            Span::raw(t.describe.clone()),
        ]));
    }
    if req.targets.len() > MAX_LISTED {
        lines.push(Line::from(format!("  … and {} more", req.targets.len() - MAX_LISTED)).fg(DIM));
    }
    lines.push(Line::from(""));
    lines.push(confirm_prompt(vec![
        Span::raw("send "),
        Span::styled(req.signal.as_str(), Style::new().fg(Color::Red).bold()),
    ]));
    render_confirm(frame, lines);
}

fn draw_confirm_remove(frame: &mut Frame, req: &RemoveRequest) {
    let mut lines = vec![
        Line::from(""),
        Line::from(Span::styled(req.title.clone(), Style::new().bold())),
        Line::from(""),
    ];
    if let [item] = req.items.as_slice() {
        for step in &item.steps {
            lines.push(Line::from(format!("  • {step}")));
        }
        for t in item.kill.iter().take(MAX_LISTED) {
            lines.push(Line::from(vec![
                Span::styled(format!("      {:>8}  ", t.pid), Style::new().fg(Color::Magenta)),
                Span::raw(t.describe.clone()),
            ]));
        }
        if let Some(w) = &item.warning {
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                format!("  ⚠ {w}"),
                Style::new().fg(Color::Red).bold(),
            )));
        }
    } else {
        for item in req.items.iter().take(MAX_LISTED) {
            lines.push(Line::from(format!("  {}", item.summary)));
        }
        if req.items.len() > MAX_LISTED {
            lines.push(Line::from(format!("  … and {} more", req.items.len() - MAX_LISTED)).fg(DIM));
        }
    }
    lines.push(Line::from(""));
    lines.push(confirm_prompt(vec![Span::styled(
        "go ahead",
        Style::new().fg(Color::Red).bold(),
    )]));
    render_confirm(frame, lines);
}

fn draw_help(frame: &mut Frame) {
    let rows = [
        ("j / k, ↑ / ↓", "move selection"),
        ("g / G", "jump to first / last"),
        ("tab, h / l, 1 / 2 / 3", "switch panel"),
        ("d", "stop: SIGTERM to the root process(es) of the selection"),
        ("D", "force kill: SIGKILL to every process in the selection"),
        ("K", "servers/strays: stop every orphan · worktrees: clean every safe leftover"),
        ("x", "worktree: stop its processes, remove it, drop its databases"),
        ("X", "same, but also for dirty, local, gone or unstarted worktrees"),
        ("f", "git fetch --prune every repo, so gone branches show as gone"),
        ("o", "show only problems (orphan/stale servers, leftover worktrees)"),
        ("s", "cycle server sort: port, status, age, mem, cpu"),
        ("/", "filter by port, project, branch, dir, command or db name"),
        ("r", "rescan now (processes every 2s, worktrees every 30s)"),
        ("q, ctrl-c", "quit"),
        ("", ""),
        ("✗ ORPHAN", "server whose working directory was deleted"),
        ("! STALE", "server older than the stale threshold (--stale-after)"),
        ("✓ merged", "worktree whose remote branch is gone and fully merged: safe"),
        ("? gone", "remote branch gone but local commits are not in the default branch"),
        ("✗ dir missing", "git lists the worktree but the directory is gone"),
        ("✗ db only", "worktree databases whose worktree no longer exists"),
        ("! (hold)", "locked, dirty, or the directory you launched from"),
    ];
    let lines: Vec<Line> = rows
        .iter()
        .map(|(k, v)| {
            Line::from(vec![
                Span::styled(format!("  {k:<22}"), Style::new().fg(FOCUS).bold()),
                Span::raw(v.to_string()),
            ])
        })
        .collect();
    let area = popup(frame, 100.min(frame.area().width), lines.len() as u16 + 2);
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(FOCUS))
        .title(" Keys (esc to close) ");
    frame.render_widget(Paragraph::new(lines).block(block), area);
}
