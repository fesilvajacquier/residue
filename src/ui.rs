use ratatui::layout::{Constraint, Flex, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Cell, Clear, Paragraph, Row, Table, TableState, Wrap};
use ratatui::Frame;

use crate::app::{App, Mode, Panel};
use crate::format;
use crate::scan::{ProcInfo, Server, Status, Stray};

const FOCUS: Color = Color::Cyan;
const DIM: Color = Color::DarkGray;

pub fn draw(frame: &mut Frame, app: &App) {
    let [body, footer] =
        Layout::vertical([Constraint::Min(5), Constraint::Length(1)]).areas(frame.area());
    let [left, right] =
        Layout::horizontal([Constraint::Percentage(52), Constraint::Percentage(48)]).areas(body);
    let stray_height = (app.stray_rows.len() as u16 + 3).clamp(4, body.height / 3);
    let [servers_area, strays_area] =
        Layout::vertical([Constraint::Min(4), Constraint::Length(stray_height)]).areas(left);

    draw_servers(frame, app, servers_area);
    draw_strays(frame, app, strays_area);
    draw_details(frame, app, right);
    draw_footer(frame, app, footer);

    match &app.mode {
        Mode::Confirm(req) => draw_confirm(frame, req),
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

    let header = Row::new(["", "PORT", "PROJECT", "BRANCH", "AGE", "MEM", "CPU"])
        .style(Style::new().fg(DIM).add_modifier(Modifier::UNDERLINED));
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
        .row_highlight_style(Style::new().bg(Color::Rgb(40, 44, 52)).bold())
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
    let header = Row::new(["PID", "PROGRAM", "PROCS", "DIR", "AGE", "MEM"])
        .style(Style::new().fg(DIM).add_modifier(Modifier::UNDERLINED));
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
        .row_highlight_style(Style::new().bg(Color::Rgb(40, 44, 52)).bold())
        .highlight_symbol("▶");
    let mut state = TableState::default().with_selected(app.selected_index(Panel::Strays));
    frame.render_stateful_widget(table, area, &mut state);
}

fn draw_details(frame: &mut Frame, app: &App, area: Rect) {
    let block = panel_block(Line::from(" Details "), false);
    let lines = match (&app.snapshot, app.panel) {
        (None, _) => vec![Line::from("scanning /proc…")],
        (Some(_), Panel::Servers) => match app.selected_server() {
            Some(s) => server_details(s),
            None => vec![Line::from("no server matches").fg(DIM)],
        },
        (Some(_), Panel::Strays) => match app.selected_stray() {
            Some(s) => stray_details(s),
            None => vec![Line::from("no stray processes, nice").fg(DIM)],
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

fn server_details(s: &Server) -> Vec<Line<'_>> {
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
    lines.push(Line::from(Span::styled(
        "Processes holding the port",
        Style::new().fg(DIM).underlined(),
    )));
    let roots: Vec<i32> = s.roots().iter().map(|r| r.pid).collect();
    for m in &s.members {
        let indent = if roots.contains(&m.pid) { "" } else { "  ↳ " };
        lines.push(process_line(indent, m));
    }
    if !s.children.is_empty() {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "Child processes (no port)",
            Style::new().fg(DIM).underlined(),
        )));
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

fn stray_details(s: &Stray) -> Vec<Line<'_>> {
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
    lines.push(Line::from(Span::styled(
        "Command line",
        Style::new().fg(DIM).underlined(),
    )));
    lines.push(Line::from(p.cmdline.clone()));
    if !s.children.is_empty() {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "Child processes",
            Style::new().fg(DIM).underlined(),
        )));
        for c in &s.children {
            lines.push(process_line("  ↳ ", c));
        }
    }
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
            let mut spans = hint_spans(&[
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
            ]);
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

fn draw_confirm(frame: &mut Frame, req: &crate::app::KillRequest) {
    const MAX_LISTED: usize = 12;
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
    lines.push(Line::from(vec![
        Span::raw("  send "),
        Span::styled(req.signal.as_str(), Style::new().fg(Color::Red).bold()),
        Span::raw("?   "),
        Span::styled("y", Style::new().fg(FOCUS).bold()),
        Span::raw(" yes   "),
        Span::styled("n", Style::new().fg(FOCUS).bold()),
        Span::raw(" no"),
    ]));
    let width = frame.area().width.min(90);
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

fn draw_help(frame: &mut Frame) {
    let rows = [
        ("j / k, ↑ / ↓", "move selection"),
        ("g / G", "jump to first / last"),
        ("tab, h / l, 1 / 2", "switch panel"),
        (
            "d",
            "stop: SIGTERM to the root process(es) of the selection",
        ),
        ("D", "force kill: SIGKILL to every process holding the port"),
        ("K", "stop every orphan in the current panel"),
        ("o", "toggle showing only ORPHAN / STALE servers"),
        ("s", "cycle sort: port, status, age, mem, cpu"),
        ("/", "filter by port, project, branch, dir or command"),
        ("r", "rescan now (auto-rescans every 2s)"),
        ("q, ctrl-c", "quit"),
        ("", ""),
        (
            "✗ ORPHAN",
            "working directory was deleted (a removed worktree)",
        ),
        ("! STALE", "older than the stale threshold (--stale-after)"),
        ("● OK", "nothing suspicious"),
        ("*PORT", "bound on all interfaces (0.0.0.0 / ::)"),
    ];
    let lines: Vec<Line> = rows
        .iter()
        .map(|(k, v)| {
            Line::from(vec![
                Span::styled(format!("  {k:<20}"), Style::new().fg(FOCUS).bold()),
                Span::raw(v.to_string()),
            ])
        })
        .collect();
    let area = popup(frame, 84.min(frame.area().width), lines.len() as u16 + 2);
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(FOCUS))
        .title(" Keys (esc to close) ");
    frame.render_widget(Paragraph::new(lines).block(block), area);
}
