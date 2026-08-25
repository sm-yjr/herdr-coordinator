use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::io;
use std::time::{Duration, Instant};

use anyhow::Result;
use ratatui::crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind, MouseButton,
    MouseEventKind,
};
use ratatui::crossterm::execute;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::Frame;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::commands::snapshot_from_cli;
use crate::state::{age_seconds, effective_status, now, State};

const BG: Color = Color::Rgb(0, 0, 0);
const FG: Color = Color::Rgb(237, 237, 237);
const GRAY: Color = Color::Rgb(150, 150, 150);
const DIM: Color = Color::Rgb(92, 92, 92);
const BORDER: Color = Color::Rgb(48, 48, 48);
const SEL_BG: Color = Color::Rgb(25, 25, 25);
const GREEN: Color = Color::Rgb(80, 226, 92);
const AMBER: Color = Color::Rgb(255, 179, 71);
const RED: Color = Color::Rgb(255, 86, 86);
const BLUE: Color = Color::Rgb(82, 168, 255);
const CYAN: Color = Color::Rgb(89, 213, 224);

#[derive(Clone, Deserialize, Serialize, Default)]
struct FleetEntry {
    #[serde(default)]
    commander: String,
    #[serde(default)]
    cwd: String,
    #[serde(default)]
    status: String,
    #[serde(default)]
    reported_status: String,
    #[serde(default)]
    note: String,
    #[serde(default)]
    updated_at: String,
    #[serde(default)]
    claim_state: Option<String>,
}

#[derive(Clone, Deserialize, Default)]
struct Agent {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    agent_status: String,
    #[serde(default)]
    cwd: Option<String>,
}

#[derive(Clone, Deserialize, Default)]
struct Decision {
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

#[derive(Clone, Deserialize, Default)]
struct Attention {
    #[serde(default)]
    project: String,
    #[serde(default)]
    priority: i64,
    #[serde(default)]
    reason: String,
    #[serde(default)]
    required_actor: String,
    #[serde(default)]
    action: String,
}

#[derive(Clone, Deserialize, Default)]
struct Inbox {
    #[serde(default)]
    ts: String,
    #[serde(default)]
    project: String,
    #[serde(default)]
    status: String,
    #[serde(default)]
    summary: String,
}

struct Member {
    name: String,
    role: String,
    status: String,
}
struct Project {
    name: String,
    entry: FleetEntry,
    status: String,
    members: Vec<Member>,
    commander_offline: bool,
    live_status: String,
}

struct App {
    state: State,
    projects: Vec<Project>,
    decisions: Vec<Decision>,
    attention: Vec<Attention>,
    inbox: Vec<Inbox>,
    cursor: usize,
    agents: Vec<Agent>,
    connected: bool,
    error: Option<String>,
    synced_at: String,
    selected: ListState,
    expanded: HashSet<String>,
    project_area: Rect,
    project_heights: Vec<u16>,
    inbox_area: Rect,
    message: String,
}

impl App {
    fn new(state: State) -> Self {
        Self {
            state,
            projects: vec![],
            decisions: vec![],
            attention: vec![],
            inbox: vec![],
            cursor: 0,
            agents: vec![],
            connected: false,
            error: None,
            synced_at: String::new(),
            selected: ListState::default(),
            expanded: HashSet::new(),
            project_area: Rect::default(),
            project_heights: vec![],
            inbox_area: Rect::default(),
            message: String::new(),
        }
    }

    fn load(&mut self) {
        let registry: BTreeMap<String, FleetEntry> =
            serde_json::from_value(self.state.load("fleets.json", serde_json::json!({})))
                .unwrap_or_default();
        self.decisions = serde_json::from_value::<Vec<Decision>>(
            self.state.load("decisions.json", serde_json::json!([])),
        )
        .unwrap_or_default()
        .into_iter()
        .filter(|item| item.state == "open")
        .collect();
        self.attention =
            serde_json::from_value(self.state.load("attention.json", serde_json::json!([])))
                .unwrap_or_default();
        self.inbox = self
            .state
            .inbox_entries()
            .into_iter()
            .filter_map(|item| serde_json::from_value(item).ok())
            .collect();
        self.cursor = fs::read_to_string(self.state.path("inbox.cursor"))
            .ok()
            .and_then(|text| text.trim().parse().ok())
            .unwrap_or(0);
        let runtime = self.state.load("runtime.json", serde_json::json!({}));
        self.connected = runtime
            .get("connected")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        self.error = runtime
            .get("error")
            .and_then(Value::as_str)
            .map(str::to_string);
        self.synced_at = runtime
            .get("updated_at")
            .and_then(Value::as_str)
            .and_then(|value| value.get(11..19))
            .unwrap_or("--:--:--")
            .to_string();
        self.agents = if self.connected {
            serde_json::from_value(
                runtime
                    .get("agents")
                    .cloned()
                    .unwrap_or_else(|| serde_json::json!([])),
            )
            .unwrap_or_default()
        } else {
            vec![]
        };

        let decisions = self.state.decisions();
        let runtime_projects = runtime.get("projects").and_then(Value::as_object);
        let mut claimed = HashSet::new();
        self.projects = registry
            .into_iter()
            .map(|(name, entry)| {
                let entry_value = serde_json::to_value(&entry).unwrap_or_default();
                let status = effective_status(&entry_value, &decisions, &name);
                let mut commander_online = false;
                let mut members = vec![];
                for agent in &self.agents {
                    let Some(agent_name) = agent.name.as_deref() else {
                        continue;
                    };
                    let belongs = agent_name == entry.commander
                        || agent_name.starts_with(&format!("{name}-"))
                        || (!entry.cwd.is_empty() && agent.cwd.as_deref() == Some(&entry.cwd));
                    if !belongs || claimed.contains(agent_name) {
                        continue;
                    }
                    claimed.insert(agent_name.to_string());
                    let role = if agent_name == entry.commander {
                        commander_online = true;
                        "机长".into()
                    } else if agent_name.contains("-rev") {
                        "副机长".into()
                    } else {
                        format!(
                            "乘务 {}",
                            agent_name
                                .strip_prefix(&format!("{name}-"))
                                .unwrap_or(agent_name)
                        )
                    };
                    members.push(Member {
                        name: agent_name.into(),
                        role,
                        status: if agent.agent_status.is_empty() {
                            "unknown".into()
                        } else {
                            agent.agent_status.clone()
                        },
                    });
                }
                members.sort_by_key(|member| {
                    if member.role == "机长" {
                        0
                    } else if member.role == "副机长" {
                        1
                    } else {
                        2
                    }
                });
                let live_status = runtime_projects
                    .and_then(|values| values.get(&name))
                    .and_then(|value| value.get("live_status"))
                    .and_then(Value::as_str)
                    .unwrap_or("unknown")
                    .into();
                Project {
                    name,
                    entry,
                    status,
                    members,
                    commander_offline: !commander_online,
                    live_status,
                }
            })
            .collect();
        if self.projects.is_empty() {
            self.selected.select(None);
        } else if self
            .selected
            .selected()
            .is_none_or(|index| index >= self.projects.len())
        {
            self.selected.select(Some(0));
        }
    }

    fn reconcile(&mut self) {
        self.message = match snapshot_from_cli().and_then(|snapshot| {
            self.state
                .write_runtime(&snapshot, true, None, "tower_manual", None)?;
            self.state.refresh_attention()?;
            Ok(())
        }) {
            Ok(()) => "已重新对账".into(),
            Err(error) => format!("对账失败：{error}"),
        };
        self.load();
    }

    fn mark_read(&mut self) {
        if fs::write(
            self.state.path("inbox.cursor"),
            self.inbox.len().to_string(),
        )
        .is_ok()
        {
            self.cursor = self.inbox.len();
            self.message = "收件箱已读".into();
        }
    }
}

fn status_color(status: &str) -> Color {
    match status {
        "working" => GREEN,
        "done" | "verified" | "accepted" => BLUE,
        "blocked" | "offline" => RED,
        "need_decision" | "reported" => AMBER,
        "idle" => GRAY,
        _ => DIM,
    }
}

fn status_label(status: &str) -> &'static str {
    match status {
        "working" => "进行中",
        "idle" => "空闲",
        "done" => "已完成",
        "blocked" => "受阻",
        "need_decision" => "等你拍板",
        "offline" => "离线",
        "reported" => "机长声明",
        "verified" => "机器已验证",
        "accepted" => "用户已验收",
        _ => "未知",
    }
}

fn dot(status: &str) -> Span<'static> {
    Span::styled("●", Style::default().fg(status_color(status)))
}
fn section(title: impl Into<String>) -> Block<'static> {
    Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(BORDER))
        .title(Span::styled(
            format!(" {} ", title.into()),
            Style::default().fg(GRAY).add_modifier(Modifier::BOLD),
        ))
        .style(Style::default().bg(BG))
}
fn ago(value: &str) -> String {
    match age_seconds(Some(value)) {
        seconds @ 0..=59 => format!("{seconds}秒前"),
        seconds @ 60..=3599 => format!("{}分钟前", seconds / 60),
        seconds @ 3600..=86399 => format!("{}小时前", seconds / 3600),
        seconds => format!("{}天前", seconds / 86400),
    }
}

fn claim_path(current: Option<&str>) -> Vec<Span<'static>> {
    let stages = [
        ("reported", "机长声明"),
        ("verified", "机器验证"),
        ("accepted", "用户验收"),
    ];
    let active = stages.iter().position(|(state, _)| Some(*state) == current);
    let mut spans = vec![];
    for (index, (state, label)) in stages.into_iter().enumerate() {
        if index > 0 {
            spans.push(Span::styled("  →  ", Style::default().fg(DIM)));
        }
        let reached = active.is_some_and(|position| index <= position);
        spans.push(Span::styled(
            label.to_string(),
            Style::default()
                .fg(if reached { status_color(state) } else { DIM })
                .add_modifier(if active == Some(index) {
                    Modifier::BOLD
                } else {
                    Modifier::empty()
                }),
        ));
    }
    spans
}

fn draw(frame: &mut Frame, app: &mut App) {
    frame.render_widget(
        Block::default().style(Style::default().bg(BG)),
        frame.area(),
    );
    let decision_h = if app.decisions.is_empty() {
        0
    } else {
        (app.decisions.len().min(3) as u16 * 2 + 2).min(8)
    };
    let attention_h = if app.attention.is_empty() {
        3
    } else {
        (app.attention.len().min(4) as u16 * 2 + 2).min(10)
    };
    let inbox_h = (app.inbox.len().min(4) as u16 + 2).max(3);
    let rows = Layout::vertical([
        Constraint::Length(2),
        Constraint::Length(decision_h),
        Constraint::Length(attention_h),
        Constraint::Min(7),
        Constraint::Length(inbox_h),
        Constraint::Length(1),
    ])
    .split(frame.area());

    let working = app
        .agents
        .iter()
        .filter(|agent| agent.agent_status == "working")
        .count();
    let blocked = app
        .agents
        .iter()
        .filter(|agent| agent.agent_status == "blocked")
        .count();
    let mut header = vec![
        Span::styled(
            "HERDR COORDINATOR",
            Style::default().fg(FG).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("  {}", now().get(11..19).unwrap_or("--:--:--")),
            Style::default().fg(DIM),
        ),
        Span::styled(
            format!("   {} 个项目", app.projects.len()),
            Style::default().fg(GRAY),
        ),
        Span::styled(format!("   ● {working} 进行中"), Style::default().fg(GREEN)),
        Span::styled(format!("   ● {blocked} 受阻"), Style::default().fg(RED)),
        Span::styled(
            format!("   {} 项待介入", app.attention.len()),
            Style::default().fg(if app.attention.is_empty() {
                GREEN
            } else {
                AMBER
            }),
        ),
    ];
    if app.connected {
        header.push(Span::styled(
            format!("   已对账 {}", app.synced_at),
            Style::default().fg(DIM),
        ));
    } else {
        header.push(Span::styled(
            format!("   ⚠ {}", app.error.as_deref().unwrap_or("尚未对账")),
            Style::default().fg(RED),
        ));
    }
    frame.render_widget(Paragraph::new(Line::from(header)), rows[0]);
    frame.render_widget(
        Paragraph::new(format!("状态目录  {}", app.state.dir.display()))
            .style(Style::default().fg(DIM)),
        Rect {
            y: rows[0].y + 1,
            height: 1,
            ..rows[0]
        },
    );

    if decision_h > 0 {
        let lines: Vec<Line> = app
            .decisions
            .iter()
            .take(3)
            .flat_map(|decision| {
                let options = if decision.options.is_empty() {
                    "未提供预设选项".into()
                } else {
                    decision.options.join("  /  ")
                };
                vec![
                    Line::from(vec![
                        Span::styled(
                            format!("{}  ", decision.project),
                            Style::default().fg(AMBER).add_modifier(Modifier::BOLD),
                        ),
                        Span::styled(decision.question.clone(), Style::default().fg(FG)),
                        Span::styled(
                            format!("  {}", ago(&decision.created_at)),
                            Style::default().fg(DIM),
                        ),
                    ]),
                    Line::from(vec![
                        Span::styled(format!("    {options}"), Style::default().fg(GRAY)),
                        Span::styled(format!("   [{}]", decision.id), Style::default().fg(DIM)),
                    ]),
                ]
            })
            .collect();
        frame.render_widget(
            Paragraph::new(lines)
                .block(section("等你拍板").border_style(Style::default().fg(AMBER))),
            rows[1],
        );
    }

    let attention_lines: Vec<Line> = if app.attention.is_empty() {
        vec![Line::from(vec![
            dot("idle"),
            Span::styled(" 当前无需人工介入", Style::default().fg(GRAY)),
        ])]
    } else {
        app.attention
            .iter()
            .take(4)
            .flat_map(|item| {
                vec![
                    Line::from(vec![
                        Span::styled(
                            format!("P{:<3}", item.priority),
                            Style::default()
                                .fg(if item.priority >= 90 { RED } else { AMBER })
                                .add_modifier(Modifier::BOLD),
                        ),
                        Span::styled(
                            format!("{:<14}", item.project),
                            Style::default().fg(FG).add_modifier(Modifier::BOLD),
                        ),
                        Span::styled(item.reason.clone(), Style::default().fg(FG)),
                        Span::styled(
                            format!(
                                "  → {}",
                                if item.required_actor == "user" {
                                    "你"
                                } else {
                                    "塔台"
                                }
                            ),
                            Style::default().fg(CYAN),
                        ),
                    ]),
                    Line::from(Span::styled(
                        format!("      {}", item.action),
                        Style::default().fg(DIM),
                    )),
                ]
            })
            .collect()
    };
    frame.render_widget(
        Paragraph::new(attention_lines)
            .block(section(format!("需要介入 · {}", app.attention.len())))
            .wrap(Wrap { trim: true }),
        rows[2],
    );

    let project_items: Vec<ListItem> = if app.projects.is_empty() {
        vec![ListItem::new(Line::from(Span::styled(
            "尚未登记项目。使用 fleet register 绑定现有项目和唯一机长。",
            Style::default().fg(DIM),
        )))]
    } else {
        app.projects
            .iter()
            .map(|project| {
                let mut headline = vec![
                    Span::styled(
                        format!("{:<16}", project.name),
                        Style::default().fg(FG).add_modifier(Modifier::BOLD),
                    ),
                    dot(&project.status),
                    Span::styled(
                        format!(" {}", status_label(&project.status)),
                        Style::default().fg(status_color(&project.status)),
                    ),
                    Span::styled(
                        format!("   机长 {}", project.entry.commander),
                        Style::default().fg(GRAY),
                    ),
                    Span::styled(
                        format!("   实时 {}", status_label(&project.live_status)),
                        Style::default().fg(status_color(&project.live_status)),
                    ),
                    Span::styled(
                        format!("   {}", ago(&project.entry.updated_at)),
                        Style::default().fg(DIM),
                    ),
                ];
                if !project.entry.note.is_empty() {
                    headline.push(Span::styled(
                        format!("   — {}", project.entry.note),
                        Style::default().fg(DIM),
                    ));
                }
                let mut path = vec![Span::raw("    完成路径  ")];
                path.extend(claim_path(project.entry.claim_state.as_deref()));
                let mut lines = vec![Line::from(headline), Line::from(path)];
                if project.commander_offline {
                    lines.push(Line::from(Span::styled(
                        format!("    ⚠ 机长 {} 当前离线", project.entry.commander),
                        Style::default().fg(RED),
                    )));
                }
                if app.expanded.contains(&project.name) {
                    for member in &project.members {
                        lines.push(Line::from(vec![
                            Span::raw("    "),
                            dot(&member.status),
                            Span::styled(format!(" {:<12}", member.role), Style::default().fg(FG)),
                            Span::styled(
                                format!("{:<8}", status_label(&member.status)),
                                Style::default().fg(status_color(&member.status)),
                            ),
                            Span::styled(member.name.clone(), Style::default().fg(DIM)),
                        ]));
                    }
                } else {
                    let mut members = vec![Span::raw("    ")];
                    for member in &project.members {
                        members.push(Span::styled(
                            format!("{} ", member.role),
                            Style::default().fg(GRAY),
                        ));
                        members.push(dot(&member.status));
                        members.push(Span::raw("    "));
                    }
                    if project.members.is_empty() {
                        members.push(Span::styled("没有存活成员", Style::default().fg(DIM)));
                    }
                    lines.push(Line::from(members));
                }
                lines.push(Line::raw(""));
                ListItem::new(lines)
            })
            .collect()
    };
    app.project_heights = project_items
        .iter()
        .map(|item| item.height() as u16)
        .collect();
    app.project_area = rows[3];
    frame.render_stateful_widget(
        List::new(project_items)
            .block(section("项目"))
            .highlight_style(Style::default().bg(SEL_BG)),
        rows[3],
        &mut app.selected,
    );

    let unread = app.inbox.len().saturating_sub(app.cursor);
    let inbox_lines: Vec<Line> = if app.inbox.is_empty() {
        vec![Line::from(Span::styled(
            "暂无汇报",
            Style::default().fg(DIM),
        ))]
    } else {
        app.inbox
            .iter()
            .enumerate()
            .rev()
            .take(4)
            .map(|(index, item)| {
                Line::from(vec![
                    Span::styled(
                        if index >= app.cursor { "● " } else { "  " },
                        Style::default().fg(BLUE),
                    ),
                    Span::styled(
                        item.ts.get(11..16).unwrap_or("--:--").to_string(),
                        Style::default().fg(DIM),
                    ),
                    Span::styled(
                        format!("  {:<14}", item.project),
                        Style::default().fg(FG).add_modifier(Modifier::BOLD),
                    ),
                    dot(&item.status),
                    Span::styled(
                        format!(" {}", item.summary),
                        Style::default().fg(if index >= app.cursor { FG } else { GRAY }),
                    ),
                ])
            })
            .collect()
    };
    app.inbox_area = rows[4];
    frame.render_widget(
        Paragraph::new(inbox_lines).block(section(if unread > 0 {
            format!("最近事件 · {unread} 未读")
        } else {
            "最近事件".into()
        })),
        rows[4],
    );
    frame.render_widget(
        Paragraph::new(format!(
            "↑↓/jk 选择   Enter 展开   r 对账   a 重建注意力   m 全部已读   q 关闭{}",
            if app.message.is_empty() {
                String::new()
            } else {
                format!("   │ {}", app.message)
            }
        ))
        .style(Style::default().fg(DIM)),
        rows[5],
    );
}

fn item_at(app: &App, y: u16) -> Option<usize> {
    let mut current = app.project_area.y + 1;
    if y < current || y >= app.project_area.y + app.project_area.height.saturating_sub(1) {
        return None;
    }
    for (index, height) in app
        .project_heights
        .iter()
        .enumerate()
        .skip(app.selected.offset())
    {
        if y < current + height {
            return Some(index);
        }
        current += height;
    }
    None
}

pub fn run(state: &State) -> Result<()> {
    state.import_legacy_once()?;
    state.init()?;
    let mut app = App::new(state.clone());
    app.reconcile();
    app.expanded = app
        .projects
        .iter()
        .map(|project| project.name.clone())
        .collect();
    let mut terminal = ratatui::init();
    execute!(io::stdout(), EnableMouseCapture)?;
    let result = event_loop(&mut terminal, &mut app);
    let _ = execute!(io::stdout(), DisableMouseCapture);
    ratatui::restore();
    result.map_err(Into::into)
}

fn event_loop(terminal: &mut ratatui::DefaultTerminal, app: &mut App) -> io::Result<()> {
    let mut last_refresh = Instant::now();
    loop {
        terminal.draw(|frame| draw(frame, app))?;
        if event::poll(Duration::from_millis(200))? {
            match event::read()? {
                Event::Key(key) if key.kind == KeyEventKind::Press => match key.code {
                    KeyCode::Char('q') | KeyCode::Esc => break,
                    KeyCode::Up | KeyCode::Char('k') => app.selected.select_previous(),
                    KeyCode::Down | KeyCode::Char('j') => app.selected.select_next(),
                    KeyCode::Enter => toggle(app),
                    KeyCode::Char('r') => app.reconcile(),
                    KeyCode::Char('a') => {
                        app.message = match app.state.refresh_attention() {
                            Ok(_) => "注意力队列已重建".into(),
                            Err(error) => format!("重建失败：{error}"),
                        };
                        app.load();
                    }
                    KeyCode::Char('m') => app.mark_read(),
                    _ => {}
                },
                Event::Mouse(mouse) => match mouse.kind {
                    MouseEventKind::ScrollUp => app.selected.select_previous(),
                    MouseEventKind::ScrollDown => app.selected.select_next(),
                    MouseEventKind::Down(MouseButton::Left) => {
                        if let Some(index) = item_at(app, mouse.row) {
                            app.selected.select(Some(index));
                            toggle(app);
                        } else if app.inbox_area.contains((mouse.column, mouse.row).into()) {
                            app.mark_read();
                        }
                    }
                    _ => {}
                },
                _ => {}
            }
        }
        if last_refresh.elapsed() >= Duration::from_secs(1) {
            app.load();
            last_refresh = Instant::now();
        }
    }
    Ok(())
}

fn toggle(app: &mut App) {
    let Some(index) = app.selected.selected() else {
        return;
    };
    let Some(project) = app.projects.get(index) else {
        return;
    };
    if !app.expanded.remove(&project.name) {
        app.expanded.insert(project.name.clone());
    }
}
