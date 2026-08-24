use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::path::PathBuf;
use std::process::Command;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use chrono::NaiveDateTime;
use ratatui::crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind, MouseButton,
    MouseEventKind,
};
use ratatui::crossterm::execute;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, List, ListItem, ListState, Paragraph};
use ratatui::Frame;
use serde::{Deserialize, Serialize};

// Vercel/Geist palette
const BG: Color = Color::Rgb(0, 0, 0);
const FG: Color = Color::Rgb(237, 237, 237);
const GRAY: Color = Color::Rgb(136, 136, 136);
const DIM: Color = Color::Rgb(102, 102, 102);
const BORDER: Color = Color::Rgb(51, 51, 51);
const SEL_BG: Color = Color::Rgb(26, 26, 26);
const GREEN: Color = Color::Rgb(80, 226, 92);
const AMBER: Color = Color::Rgb(255, 179, 71);
const RED: Color = Color::Rgb(255, 86, 86);
const BLUE: Color = Color::Rgb(82, 168, 255);

#[derive(Deserialize, Serialize, Default, Clone)]
struct FleetEntry {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    tab_id: Option<String>,
    #[serde(default)]
    commander: Option<String>,
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    reported_status: Option<String>,
    #[serde(default)]
    note: Option<String>,
    #[serde(default)]
    updated_at: Option<String>,
}

#[derive(Deserialize, Clone)]
struct AgentInfo {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    agent: Option<String>,
    #[serde(default)]
    agent_status: Option<String>,
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default)]
    pane_id: Option<String>,
    #[serde(default)]
    terminal_title_stripped: Option<String>,
}

#[derive(Deserialize, Default)]
struct RuntimeFile {
    #[serde(default)]
    connected: bool,
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    updated_at: Option<String>,
    #[serde(default)]
    agents: Vec<AgentInfo>,
}

#[derive(Deserialize, Clone)]
struct DecisionEntry {
    #[serde(default)]
    id: String,
    #[serde(default)]
    project: String,
    #[serde(default)]
    state: String,
    #[serde(default)]
    question: String,
    #[serde(default)]
    options: Vec<String>,
    #[serde(default)]
    created_at: String,
}

#[derive(Deserialize, Clone)]
struct InboxEntry {
    #[serde(default)]
    ts: String,
    #[serde(default)]
    project: String,
    #[serde(default)]
    status: String,
    #[serde(default)]
    summary: String,
}

#[derive(Deserialize, Serialize, Default)]
struct DispatchFile {
    #[serde(default)]
    cursor: usize,
    #[serde(default)]
    last_agent: Option<String>,
    #[serde(default)]
    last_ts: Option<String>,
}

struct DispatchState {
    cursor: usize,
    last_agent: Option<String>,
    last_ts: Option<String>,
    enabled: bool,
    cooldown_until: HashMap<String, Instant>,
}

struct Member {
    name: String,
    role: String,
    status: String,
}

struct Project {
    name: String,
    entry: FleetEntry,
    members: Vec<Member>,
    commander_offline: bool,
}

struct App {
    projects: Vec<Project>,
    decisions: Vec<DecisionEntry>,
    tower_staff: Vec<(String, String, Option<String>)>,
    orphans: Vec<(String, String)>,
    inbox: Vec<InboxEntry>,
    cursor: usize,
    totals: (usize, usize, usize),
    selected: ListState,
    expanded: HashSet<String>,
    error: Option<String>,
    synced_at: Option<String>,
    proj_area: Rect,
    item_heights: Vec<u16>,
    inbox_area: Rect,
    summaries: HashMap<String, (String, String)>, // name -> (hash, summary)
    sum_rx: mpsc::Receiver<(String, String, String)>,
    sum_tx: mpsc::Sender<(String, String, String)>,
    sum_worker: Option<thread::JoinHandle<()>>,
    dispatch: DispatchState,
}

fn data_dir() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_default();
    PathBuf::from(home).join(".herdr-coordinator")
}

fn now_str() -> String {
    chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string()
}

fn status_color(s: &str) -> Color {
    match s {
        "working" => GREEN,
        "idle" => GRAY,
        "done" => BLUE,
        "blocked" => RED,
        "need_decision" => AMBER,
        _ => DIM,
    }
}

fn status_label(s: &str) -> &'static str {
    match s {
        "working" => "干活中",
        "idle" => "空闲",
        "done" => "完成",
        "blocked" => "卡住",
        "need_decision" => "等拍板",
        _ => "未知",
    }
}

fn ago(ts: &str) -> String {
    if let Ok(t) = NaiveDateTime::parse_from_str(ts, "%Y-%m-%d %H:%M:%S") {
        let sec = (chrono::Local::now().naive_local() - t)
            .num_seconds()
            .max(0);
        return match sec {
            0..=59 => format!("{}s", sec),
            60..=3599 => format!("{}m", sec / 60),
            3600..=86399 => format!("{}h", sec / 3600),
            _ => format!("{}d", sec / 86400),
        };
    }
    "?".into()
}

fn load(app: &mut App) {
    let dir = data_dir();
    let mut registry: BTreeMap<String, FleetEntry> = fs::read_to_string(dir.join("fleets.json"))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();

    let runtime: RuntimeFile = fs::read_to_string(dir.join("runtime.json"))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    let agents = if runtime.connected {
        runtime.agents
    } else {
        Vec::new()
    };
    app.error = if runtime.connected {
        None
    } else {
        Some(
            runtime
                .error
                .unwrap_or_else(|| "事件监听器未连接 Herdr".into()),
        )
    };
    app.synced_at = runtime
        .updated_at
        .as_deref()
        .and_then(|s| s.get(11..19))
        .map(str::to_string);

    app.decisions = fs::read_to_string(dir.join("decisions.json"))
        .ok()
        .and_then(|s| serde_json::from_str::<Vec<DecisionEntry>>(&s).ok())
        .unwrap_or_default()
        .into_iter()
        .filter(|d| d.state == "open")
        .collect();

    // 项目状态来自机长汇报；实时 Agent 状态只用于成员状态和离线提示。
    for (name, entry) in registry.iter_mut() {
        if app.decisions.iter().any(|d| d.project == *name) {
            entry.status = Some("need_decision".into());
        } else if let Some(reported) = &entry.reported_status {
            entry.status = Some(reported.clone());
        }
    }

    app.inbox = fs::read_to_string(dir.join("inbox.jsonl"))
        .unwrap_or_default()
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect();
    app.cursor = fs::read_to_string(dir.join("inbox.cursor"))
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0);

    let total = agents.len();
    let working = agents
        .iter()
        .filter(|a| a.agent_status.as_deref() == Some("working"))
        .count();
    let blocked = agents
        .iter()
        .filter(|a| a.agent_status.as_deref() == Some("blocked"))
        .count();
    app.totals = (total, working, blocked);

    // 两遍认领：先按机长名/名字前缀（精确），再按 cwd 兜底。
    // cwd 被多个项目共用时不做兜底认领，避免不同机组混到一起。
    let mut cwd_count: BTreeMap<&str, usize> = BTreeMap::new();
    for entry in registry.values() {
        if let Some(c) = entry.cwd.as_deref() {
            *cwd_count.entry(c).or_insert(0) += 1;
        }
    }
    let mut claimed: Vec<usize> = Vec::new();
    let mut claims: BTreeMap<String, Vec<usize>> = BTreeMap::new();
    for (name, entry) in &registry {
        for (i, a) in agents.iter().enumerate() {
            if claimed.contains(&i) {
                continue;
            }
            let Some(aname) = a.name.as_deref() else {
                continue;
            };
            let is_cmd = Some(aname) == entry.commander.as_deref();
            let by_prefix = aname.starts_with(&format!("{}-", name));
            if is_cmd || by_prefix {
                claimed.push(i);
                claims.entry(name.clone()).or_default().push(i);
            }
        }
    }
    for (name, entry) in &registry {
        let Some(cwd) = entry.cwd.as_deref() else {
            continue;
        };
        if cwd_count.get(cwd).copied().unwrap_or(0) != 1 {
            continue;
        }
        for (i, a) in agents.iter().enumerate() {
            if claimed.contains(&i) || a.name.is_none() {
                continue;
            }
            if a.cwd.as_deref() == Some(cwd) {
                claimed.push(i);
                claims.entry(name.clone()).or_default().push(i);
            }
        }
    }

    let mut projects = Vec::new();
    for (name, entry) in &registry {
        let mut members = Vec::new();
        let mut commander_online = false;
        for &i in claims.get(name).map(|v| v.as_slice()).unwrap_or(&[]) {
            let a = &agents[i];
            let aname = a.name.as_deref().unwrap_or("");
            let is_cmd = Some(aname) == entry.commander.as_deref();
            let role = if is_cmd {
                commander_online = true;
                "机长".to_string()
            } else if a.agent.as_deref() == Some("codex") || aname.contains("rev") {
                "副机长".to_string()
            } else {
                let short = aname.strip_prefix(&format!("{}-", name)).unwrap_or(aname);
                format!("乘务-{}", short)
            };
            members.push(Member {
                name: aname.to_string(),
                role,
                status: a.agent_status.clone().unwrap_or_else(|| "unknown".into()),
            });
        }
        members.sort_by_key(|m| match m.role.as_str() {
            "机长" => 0,
            "副机长" => 1,
            _ => 2,
        });
        projects.push(Project {
            name: name.clone(),
            entry: entry.clone(),
            members,
            commander_offline: !commander_online,
        });
    }

    let staff_or_orphan = |a: &AgentInfo| {
        let label = a
            .name
            .clone()
            .or_else(|| a.terminal_title_stripped.clone())
            .unwrap_or_else(|| a.agent.clone().unwrap_or_else(|| "?".into()));
        (
            label,
            a.agent_status.clone().unwrap_or_else(|| "unknown".into()),
            a.name.clone().or_else(|| a.pane_id.clone()),
        )
    };
    app.tower_staff = agents
        .iter()
        .enumerate()
        .filter(|(i, a)| {
            !claimed.contains(i)
                && a.cwd
                    .as_deref()
                    .is_some_and(|c| c.ends_with("herdr-coordinator"))
        })
        .map(|(_, a)| staff_or_orphan(a))
        .collect();
    app.orphans = agents
        .iter()
        .enumerate()
        .filter(|(i, a)| {
            !claimed.contains(i)
                && !a
                    .cwd
                    .as_deref()
                    .is_some_and(|c| c.ends_with("herdr-coordinator"))
        })
        .map(|(_, a)| {
            let (l, s, _) = staff_or_orphan(a);
            (l, s)
        })
        .collect();

    app.projects = projects;
    let len = app.projects.len();
    if len == 0 {
        app.selected.select(None);
    } else if app.selected.selected().is_none_or(|s| s >= len) {
        app.selected.select(Some(0));
    }
}

fn dot(color: Color) -> Span<'static> {
    Span::styled("●", Style::default().fg(color))
}

fn section(title: &str) -> Block<'_> {
    Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(BORDER))
        .title(Span::styled(
            format!(" {} ", title),
            Style::default().fg(GRAY).add_modifier(Modifier::BOLD),
        ))
        .style(Style::default().bg(BG))
}

fn draw(f: &mut Frame, app: &mut App) {
    let unread = app.inbox.len().saturating_sub(app.cursor);
    let inbox_h = (app.inbox.len().clamp(1, 5) + 2) as u16;
    let orphan_h = if app.orphans.is_empty() {
        0
    } else {
        (app.orphans.len().min(4) + 2) as u16
    };
    let staff_h = if app.tower_staff.is_empty() {
        0
    } else {
        (app.tower_staff.len().min(4) + 2) as u16
    };
    let dec_h = if app.decisions.is_empty() {
        0
    } else {
        (app.decisions.len().min(5) + 2) as u16
    };

    let rows = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(dec_h),
        Constraint::Min(6),
        Constraint::Length(staff_h),
        Constraint::Length(orphan_h),
        Constraint::Length(inbox_h),
        Constraint::Length(1),
    ])
    .split(f.area());

    f.render_widget(Block::default().style(Style::default().bg(BG)), f.area());

    // header
    let (total, working, blocked) = app.totals;
    let now = chrono::Local::now().format("%H:%M:%S").to_string();
    let mut header = vec![
        Span::styled(
            "TOWER",
            Style::default().fg(FG).add_modifier(Modifier::BOLD),
        ),
        Span::styled(format!("  {}", now), Style::default().fg(DIM)),
        Span::styled(format!("   {} agents", total), Style::default().fg(GRAY)),
        Span::raw("   "),
        dot(GREEN),
        Span::styled(format!(" {} working", working), Style::default().fg(GRAY)),
        Span::raw("   "),
        dot(RED),
        Span::styled(format!(" {} blocked", blocked), Style::default().fg(GRAY)),
    ];
    if let Some(t) = &app.synced_at {
        header.push(Span::styled(
            format!("   event {}", t),
            Style::default().fg(DIM),
        ));
    }
    if let Some(e) = &app.error {
        header.push(Span::styled(
            format!("   ⚠ {}", e),
            Style::default().fg(RED),
        ));
    }
    f.render_widget(Paragraph::new(Line::from(header)), rows[0]);

    // decisions
    if !app.decisions.is_empty() {
        let lines: Vec<Line> = app
            .decisions
            .iter()
            .take(5)
            .map(|decision| {
                let mut spans = vec![
                    Span::styled(
                        format!("{}  ", decision.project),
                        Style::default().fg(AMBER).add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(decision.question.clone(), Style::default().fg(FG)),
                    Span::styled(
                        format!("  {}", ago(&decision.created_at)),
                        Style::default().fg(DIM),
                    ),
                    Span::styled(format!("  [{}]", decision.id), Style::default().fg(DIM)),
                ];
                if !decision.options.is_empty() {
                    spans.push(Span::styled(
                        format!("  选项: {}", decision.options.join(" / ")),
                        Style::default().fg(GRAY),
                    ));
                }
                Line::from(spans)
            })
            .collect();
        let block = section("等你拍板").border_style(Style::default().fg(AMBER));
        f.render_widget(Paragraph::new(lines).block(block), rows[1]);
    }

    // projects
    let items: Vec<ListItem> = app
        .projects
        .iter()
        .map(|p| {
            let st = p.entry.status.as_deref().unwrap_or("unknown");
            let mut l1 = vec![
                Span::styled(
                    format!("{:<14}", p.name),
                    Style::default().fg(FG).add_modifier(Modifier::BOLD),
                ),
                dot(status_color(st)),
                Span::styled(format!(" {}", status_label(st)), Style::default().fg(GRAY)),
                Span::styled(
                    format!("  {}", ago(p.entry.updated_at.as_deref().unwrap_or(""))),
                    Style::default().fg(DIM),
                ),
            ];
            if let Some(note) = &p.entry.note {
                if !note.is_empty() && st != "need_decision" {
                    l1.push(Span::styled(
                        format!("  — {}", note),
                        Style::default().fg(DIM),
                    ));
                }
            }
            let mut lines = vec![Line::from(l1)];
            if p.commander_offline {
                lines.push(Line::from(Span::styled(
                    format!(
                        "    ⚠ 机长 {} 离线",
                        p.entry.commander.as_deref().unwrap_or("?")
                    ),
                    Style::default().fg(RED),
                )));
            }
            if app.expanded.contains(&p.name) {
                for m in &p.members {
                    let summary = app
                        .summaries
                        .get(&m.name)
                        .map(|(_, s)| s.as_str())
                        .unwrap_or("");
                    let role_w: usize = m
                        .role
                        .chars()
                        .map(|c| if c.is_ascii() { 1 } else { 2 })
                        .sum();
                    let role_pad = " ".repeat(16usize.saturating_sub(role_w));
                    lines.push(Line::from(vec![
                        Span::raw("    "),
                        dot(status_color(&m.status)),
                        Span::styled(format!(" {}{}", m.role, role_pad), Style::default().fg(FG)),
                        Span::styled(
                            format!("{:<8}", status_label(&m.status)),
                            Style::default().fg(GRAY),
                        ),
                        Span::styled(summary.to_string(), Style::default().fg(DIM)),
                    ]));
                }
            } else {
                let mut spans = vec![Span::raw("    ")];
                for m in &p.members {
                    spans.push(Span::styled(m.role.clone(), Style::default().fg(GRAY)));
                    spans.push(Span::raw(" "));
                    spans.push(dot(status_color(&m.status)));
                    spans.push(Span::raw("   "));
                }
                if p.members.is_empty() {
                    spans.push(Span::styled("（无存活成员）", Style::default().fg(DIM)));
                }
                lines.push(Line::from(spans));
                if let Some((_, s)) = app.summaries.get(&format!("proj:{}", p.name)) {
                    lines.push(Line::from(vec![
                        Span::raw("    "),
                        Span::styled(s.clone(), Style::default().fg(DIM)),
                    ]));
                }
            }
            lines.push(Line::raw(""));
            ListItem::new(lines)
        })
        .collect();
    app.item_heights = items.iter().map(|it| it.height() as u16).collect();
    app.proj_area = rows[2];
    let list = List::new(items)
        .block(section("项目"))
        .highlight_style(Style::default().bg(SEL_BG));
    f.render_stateful_widget(list, rows[2], &mut app.selected);

    // tower staff
    if !app.tower_staff.is_empty() {
        let lines: Vec<Line> = app
            .tower_staff
            .iter()
            .take(4)
            .map(|(label, st, name)| {
                let summary = name
                    .as_deref()
                    .and_then(|n| app.summaries.get(n))
                    .map(|(_, s)| s.as_str())
                    .unwrap_or("");
                Line::from(vec![
                    dot(status_color(st)),
                    Span::styled(format!(" {}", label), Style::default().fg(FG)),
                    Span::styled(format!("  {}", status_label(st)), Style::default().fg(DIM)),
                    Span::styled(format!("  {}", summary), Style::default().fg(DIM)),
                ])
            })
            .collect();
        let mut staff_title = format!(
            "塔台管制员 · 派单:{}",
            if app.dispatch.enabled { "开" } else { "关" }
        );
        let pending = dispatch_pending(app);
        if pending > 0 {
            staff_title.push_str(&format!(" · 待派 {}", pending));
        }
        if let (Some(a), Some(t)) = (&app.dispatch.last_agent, &app.dispatch.last_ts) {
            staff_title.push_str(&format!(" · 上次→{} {}", a, ago(t)));
        }
        f.render_widget(Paragraph::new(lines).block(section(&staff_title)), rows[3]);
    }

    // orphans
    if !app.orphans.is_empty() {
        let lines: Vec<Line> = app
            .orphans
            .iter()
            .take(4)
            .map(|(label, st)| {
                Line::from(vec![
                    dot(status_color(st)),
                    Span::styled(format!(" {}", label), Style::default().fg(GRAY)),
                    Span::styled(format!("  {}", status_label(st)), Style::default().fg(DIM)),
                ])
            })
            .collect();
        let title = format!("未登记 {}", app.orphans.len());
        f.render_widget(Paragraph::new(lines).block(section(&title)), rows[4]);
    }

    // inbox
    let title = if unread > 0 {
        format!("收件箱 · {} 未读", unread)
    } else {
        "收件箱".to_string()
    };
    let lines: Vec<Line> = if app.inbox.is_empty() {
        vec![Line::from(Span::styled(
            "（暂无汇报）",
            Style::default().fg(DIM),
        ))]
    } else {
        app.inbox
            .iter()
            .enumerate()
            .rev()
            .take(5)
            .map(|(i, e)| {
                let is_unread = i >= app.cursor;
                let fg = if is_unread { FG } else { DIM };
                Line::from(vec![
                    Span::styled(
                        if is_unread { "● " } else { "  " },
                        Style::default().fg(BLUE),
                    ),
                    Span::styled(
                        e.ts.get(11..16).unwrap_or("--:--").to_string(),
                        Style::default().fg(DIM),
                    ),
                    Span::styled(
                        format!("  {}", e.project),
                        Style::default().fg(fg).add_modifier(Modifier::BOLD),
                    ),
                    Span::raw("  "),
                    dot(status_color(&e.status)),
                    Span::styled(format!(" {}", e.summary), Style::default().fg(fg)),
                ])
            })
            .collect()
    };
    f.render_widget(Paragraph::new(lines).block(section(&title)), rows[5]);
    app.inbox_area = rows[5];

    // footer
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            "点击 展开/折叠   Enter 全部展开/折叠   滚轮 切换   点击收件箱 标记已读   d 派单开关   q 退出",
            Style::default().fg(DIM),
        ))),
        rows[6],
    );
}

fn item_at(app: &App, y: u16) -> Option<usize> {
    let inner_top = app.proj_area.y + 1;
    let inner_bottom = app.proj_area.y + app.proj_area.height.saturating_sub(1);
    if y < inner_top || y >= inner_bottom {
        return None;
    }
    let mut cur = inner_top;
    for (i, h) in app
        .item_heights
        .iter()
        .enumerate()
        .skip(app.selected.offset())
    {
        if y < cur + h {
            return Some(i);
        }
        cur += h;
    }
    None
}

fn main() -> std::io::Result<()> {
    let fleet = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."))
        .join("fleet");
    let _ = Command::new(fleet).arg("watch-start").output();
    let mut terminal = ratatui::init();
    execute!(std::io::stdout(), EnableMouseCapture)?;
    let (tx, rx) = mpsc::channel();
    let mut app = App {
        projects: Vec::new(),
        decisions: Vec::new(),
        tower_staff: Vec::new(),
        orphans: Vec::new(),
        inbox: Vec::new(),
        cursor: 0,
        totals: (0, 0, 0),
        selected: ListState::default(),
        expanded: HashSet::new(),
        error: None,
        synced_at: None,
        proj_area: Rect::default(),
        item_heights: Vec::new(),
        inbox_area: Rect::default(),
        summaries: load_summaries(),
        sum_rx: rx,
        sum_tx: tx,
        sum_worker: None,
        dispatch: load_dispatch(),
    };
    load(&mut app);
    app.expanded = app.projects.iter().map(|p| p.name.clone()).collect();
    let mut last = Instant::now();
    let mut last_sum = Instant::now() - Duration::from_secs(30);
    loop {
        terminal.draw(|f| draw(f, &mut app))?;
        if event::poll(Duration::from_millis(250))? {
            match event::read()? {
                Event::Key(k) if k.kind == KeyEventKind::Press => match k.code {
                    KeyCode::Char('q') | KeyCode::Esc => break,
                    KeyCode::Up | KeyCode::Char('k') => app.selected.select_previous(),
                    KeyCode::Down | KeyCode::Char('j') => app.selected.select_next(),
                    KeyCode::Enter => {
                        if app.projects.iter().all(|p| app.expanded.contains(&p.name)) {
                            app.expanded.clear();
                        } else {
                            app.expanded = app.projects.iter().map(|p| p.name.clone()).collect();
                        }
                    }
                    KeyCode::Char('r') => mark_read(&mut app),
                    KeyCode::Char('d') => app.dispatch.enabled = !app.dispatch.enabled,
                    _ => {}
                },
                Event::Mouse(m) => match m.kind {
                    MouseEventKind::ScrollUp => app.selected.select_previous(),
                    MouseEventKind::ScrollDown => app.selected.select_next(),
                    MouseEventKind::Down(MouseButton::Left) => {
                        let (col, row) = (m.column, m.row);
                        if let Some(i) = item_at(&app, row) {
                            app.selected.select(Some(i));
                            if let Some(p) = app.projects.get(i) {
                                if !app.expanded.remove(&p.name) {
                                    app.expanded.insert(p.name.clone());
                                }
                            }
                        } else if app.inbox_area.contains((col, row).into()) {
                            mark_read(&mut app);
                        }
                    }
                    _ => {}
                },
                _ => {}
            }
        }
        if last.elapsed() >= Duration::from_secs(5) {
            load(&mut app);
            try_dispatch(&mut app);
            last = Instant::now();
        }
        // 每 30 秒总结一轮成员进展；画面哈希没变的成员跳过调用
        let worker_free = app.sum_worker.as_ref().is_none_or(|h| h.is_finished());
        if last_sum.elapsed() >= Duration::from_secs(30) && worker_free {
            let snapshot: Vec<(String, Option<String>)> = app
                .projects
                .iter()
                .flat_map(|p| p.members.iter().map(|m| m.name.clone()))
                .chain(
                    app.tower_staff
                        .iter()
                        .filter_map(|(_, _, name)| name.clone()),
                )
                .map(|name| {
                    let prev = app.summaries.get(&name).map(|(h, _)| h.clone());
                    (name, prev)
                })
                .collect();
            // 项目级概况：由各成员的一句话进展汇总生成
            let proj_jobs: Vec<(String, String, String)> = app
                .projects
                .iter()
                .filter_map(|p| {
                    let mut input = String::new();
                    for m in &p.members {
                        if let Some((_, s)) = app.summaries.get(&m.name) {
                            input.push_str(&format!("{}: {}\n", m.role, s));
                        }
                    }
                    if input.is_empty() {
                        return None;
                    }
                    let key = format!("proj:{}", p.name);
                    let h = hash_of(&input);
                    if app.summaries.get(&key).map(|(ph, _)| ph.as_str()) == Some(h.as_str()) {
                        return None;
                    }
                    Some((key, h, input))
                })
                .collect();
            let tx = app.sum_tx.clone();
            app.sum_worker = Some(thread::spawn(move || {
                for (name, prev) in snapshot {
                    let Some(tail) = read_tail(&name) else {
                        continue;
                    };
                    let h = hash_of(&tail);
                    if prev.as_deref() == Some(h.as_str()) {
                        continue;
                    }
                    if let Some(s) = llm_summarize(&tail, SUM_SYS) {
                        let _ = tx.send((name, h, s));
                    }
                }
                for (key, h, input) in proj_jobs {
                    if let Some(s) = llm_summarize(&input, SUM_SYS_PROJ) {
                        let _ = tx.send((key, h, s));
                    }
                }
            }));
            last_sum = Instant::now();
        }
        let mut changed = false;
        while let Ok((n, h, s)) = app.sum_rx.try_recv() {
            app.summaries.insert(n, (h, s));
            changed = true;
        }
        if changed {
            save_summaries(&app.summaries);
        }
    }
    let _ = execute!(std::io::stdout(), DisableMouseCapture);
    ratatui::restore();
    Ok(())
}

fn mark_read(app: &mut App) {
    let _ = fs::write(data_dir().join("inbox.cursor"), app.inbox.len().to_string());
    app.cursor = app.inbox.len();
}

// ── 自动派单：新收件箱消息派给空闲塔台管制员 ──

fn dispatch_path() -> PathBuf {
    data_dir().join("dispatch.json")
}

fn load_dispatch() -> DispatchState {
    let f: DispatchFile = fs::read_to_string(dispatch_path())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    DispatchState {
        cursor: f.cursor,
        last_agent: f.last_agent,
        last_ts: f.last_ts,
        enabled: true,
        cooldown_until: HashMap::new(),
    }
}

fn save_dispatch(d: &DispatchState) {
    let f = DispatchFile {
        cursor: d.cursor,
        last_agent: d.last_agent.clone(),
        last_ts: d.last_ts.clone(),
    };
    if let Ok(s) = serde_json::to_string_pretty(&f) {
        let _ = fs::write(dispatch_path(), s);
    }
}

fn dispatch_prompt(entries: &[InboxEntry]) -> String {
    let mut s = format!(
        "【塔台自动派单】收件箱有 {} 条新汇报待处理：\n",
        entries.len()
    );
    for e in entries {
        s.push_str(&format!(
            "- [{}] {} · {} · {}\n",
            e.ts, e.project, e.status, e.summary
        ));
    }
    s.push_str(
        "请按塔台管制员标准流程处理：先运行 ./fleet inbox 确认消化这些消息，\
         再 ./fleet list 核对各项目状态，需要跟进的按流程转发给对应机长，\
         需要用户拍板的用大白话汇报。",
    );
    s
}

fn dispatch_pending(app: &App) -> usize {
    app.inbox
        .len()
        .saturating_sub(app.dispatch.cursor.max(app.cursor))
}

fn try_dispatch(app: &mut App) {
    if !app.dispatch.enabled {
        return;
    }
    // inbox.jsonl 被截断/重写时回收游标
    if app.dispatch.cursor > app.inbox.len() {
        app.dispatch.cursor = app.inbox.len();
        save_dispatch(&app.dispatch);
    }
    // 已读消息不再派：起点取派单游标与已读游标的较大者
    let start = app.dispatch.cursor.max(app.cursor);
    if start >= app.inbox.len() {
        return;
    }
    let now = Instant::now();
    let Some(target) = app.tower_staff.iter().find_map(|(_, st, name)| {
        let name = name.as_deref()?;
        if st != "idle" {
            return None;
        }
        if app
            .dispatch
            .cooldown_until
            .get(name)
            .is_some_and(|&t| t > now)
        {
            return None;
        }
        Some(name.to_string())
    }) else {
        return; // 无空闲管制员：排队等待，下个 tick 再试
    };
    let text = dispatch_prompt(&app.inbox[start..]);
    let ok = Command::new("herdr")
        .args(["agent", "prompt", &target, &text])
        .output()
        .is_ok_and(|o| o.status.success());
    if ok {
        app.dispatch.cursor = app.inbox.len();
        app.dispatch.last_agent = Some(target.clone());
        app.dispatch.last_ts = Some(now_str());
        // herdr 状态更新有延迟，冷却期内不再派给同一人
        app.dispatch
            .cooldown_until
            .insert(target, now + Duration::from_secs(60));
        save_dispatch(&app.dispatch);
    } else {
        app.error = Some(format!("派单给 {} 失败", target));
    }
}

// ── 成员进展总结（qwen3.7-flash，按画面哈希缓存） ──

// 系统提示词保持字节级固定，以命中服务端前缀缓存
const SUM_SYS: &str = "你是终端看板的进展总结器。根据给出的终端输出，用一句不超过24个字的中文概括该 agent 正在做的事情；只输出这一句话。";
const SUM_SYS_PROJ: &str = "你是终端看板的进展总结器。根据给出的项目各成员进展，用一句不超过24个字的中文概括该项目整体正在做的事情；只输出这一句话。";

fn sum_path() -> PathBuf {
    data_dir().join("summaries.json")
}

fn load_summaries() -> HashMap<String, (String, String)> {
    fs::read_to_string(sum_path())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn save_summaries(m: &HashMap<String, (String, String)>) {
    if let Ok(s) = serde_json::to_string(m) {
        let _ = fs::write(sum_path(), s);
    }
}

fn read_tail(name: &str) -> Option<String> {
    let out = Command::new("herdr")
        .args([
            "agent",
            "read",
            name,
            "--source",
            "recent-unwrapped",
            "--lines",
            "25",
        ])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    let chars: Vec<char> = text.chars().collect();
    let start = chars.len().saturating_sub(800);
    Some(chars[start..].iter().collect())
}

fn hash_of(s: &str) -> String {
    let mut h = DefaultHasher::new();
    s.hash(&mut h);
    format!("{:x}", h.finish())
}

fn llm_summarize(tail: &str, sys: &str) -> Option<String> {
    let key = std::env::var("DASHSCOPE_API_KEY").ok()?;
    let model = std::env::var("TOWER_SUMMARY_MODEL").unwrap_or_else(|_| "qwen3.7-flash".into());
    let body = serde_json::json!({
        "model": model,
        "messages": [
            {"role": "system", "content": sys},
            {"role": "user", "content": tail}
        ],
        "max_tokens": 64,
        "enable_thinking": false
    })
    .to_string();
    let out = Command::new("curl")
        .args([
            "-s",
            "--max-time",
            "20",
            "https://dashscope.aliyuncs.com/compatible-mode/v1/chat/completions",
            "-H",
            &format!("Authorization: Bearer {}", key),
            "-H",
            "Content-Type: application/json",
            "-d",
            &body,
        ])
        .output()
        .ok()?;
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).ok()?;
    let s = v
        .pointer("/choices/0/message/content")?
        .as_str()?
        .trim()
        .to_string();
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}
