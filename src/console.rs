//! `tiered-memory console` — a read-only terminal dashboard over the memory
//! store: layer gauges, projects, L2 groups (with their per-topic docs), the
//! installed agent-skill copies across harnesses, and a project↔group graph.
//!
//! Rendering is ratatui; the data collection ([`collect_state`]) is plain
//! structs so it is testable without a terminal.

use crate::harnesses::{summaries, SkillInstall};
use crate::store::{normalize_topic, record_group, LayeredDirStore, MemoryStore, DEFAULT_TOPIC};
use crate::types::{Level, UserDb};
use crate::{default_data_dir, EngineConfig};
use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::symbols;
use ratatui::text::{Line, Span};
use ratatui::widgets::canvas::{self, Circle, Line as ShapeLine};
use ratatui::widgets::{
    Block, Gauge, List, ListItem, ListState, Paragraph, Row, Table, TableState, Tabs,
};
use ratatui::Frame;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

// -- state -------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct LayerView {
    pub label: &'static str,
    pub count: usize,
    pub capacity: usize,
}

#[derive(Debug, Clone)]
pub struct ProjectView {
    pub id: String,
    pub name: String,
    pub group: Option<String>,
    pub descriptor: String,
    pub l1: usize,
    pub l2: usize,
    pub similar: usize,
}

#[derive(Debug, Clone)]
pub struct GroupView {
    pub name: String,
    pub members: Vec<String>,
    /// (topic file, record count), most records first.
    pub topics: Vec<(String, usize)>,
}

/// Everything the console renders, gathered in one pass — no terminal needed.
#[derive(Debug, Clone)]
pub struct ConsoleState {
    pub user: String,
    pub data_dir: PathBuf,
    pub version: &'static str,
    pub embedder: String,
    pub dims: usize,
    pub layers: Vec<LayerView>,
    pub param_keys: Vec<String>,
    pub projects: Vec<ProjectView>,
    pub groups: Vec<GroupView>,
    pub ungrouped: Vec<String>,
    pub installs: Vec<SkillInstall>,
}

impl ConsoleState {
    pub fn total_records(&self) -> usize {
        self.layers.iter().map(|l| l.count).sum()
    }
}

fn data_root() -> PathBuf {
    std::env::var("TM_DATA_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| default_data_dir())
}

fn home_dir() -> PathBuf {
    std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("."))
}

/// Gather the console's view of the world. `root` is the data dir;
/// `home`/`cwd` anchor the harness skill paths.
pub fn collect_state(root: &Path, user: &str, home: &Path, cwd: &Path) -> ConsoleState {
    let db = LayeredDirStore::new(root)
        .ok()
        .and_then(|s| s.load(user).ok())
        .flatten();
    let capacities = EngineConfig::from_env();
    let fallback = UserDb::new("—".into(), 0);
    let db = db.as_ref().unwrap_or(&fallback);

    let count_in = |lvl: Level| db.count_in(lvl);
    let layers = vec![
        LayerView {
            label: "L1 · hot (this project)",
            count: count_in(Level::L1),
            capacity: capacities.l1_capacity,
        },
        LayerView {
            label: "L2 · warm (related scopes)",
            count: count_in(Level::L2),
            capacity: capacities.l2_capacity,
        },
        LayerView {
            label: "L3 · cold (global traits)",
            count: count_in(Level::L3),
            capacity: capacities.l3_capacity,
        },
    ];

    let mut param_keys = std::collections::BTreeSet::new();
    for r in &db.records {
        param_keys.extend(r.params.keys().cloned());
    }

    let projects: Vec<ProjectView> = db
        .projects
        .values()
        .map(|p| ProjectView {
            id: p.project_id.clone(),
            name: p.name.clone(),
            group: crate::engine::effective_group(p.group.as_deref()).map(str::to_string),
            descriptor: p.descriptor.clone(),
            l1: db
                .records
                .iter()
                .filter(|r| {
                    r.level == Level::L1 && r.project_id.as_deref() == Some(p.project_id.as_str())
                })
                .count(),
            l2: db
                .records
                .iter()
                .filter(|r| {
                    r.level == Level::L2
                        && r.project_id.as_deref() == Some(p.project_id.as_str())
                        && r.group.is_none()
                })
                .count(),
            similar: p.similar.len(),
        })
        .collect();

    // groups: members from the registry, topic files from the same bucketing
    // the store's mirrors use
    let mut by_name: BTreeMap<String, GroupView> = BTreeMap::new();
    for p in &projects {
        if let Some(g) = &p.group {
            by_name
                .entry(g.clone())
                .or_insert_with(|| GroupView {
                    name: g.clone(),
                    members: Vec::new(),
                    topics: Vec::new(),
                })
                .members
                .push(p.id.clone());
        }
    }
    let mut topic_counts: BTreeMap<String, BTreeMap<String, usize>> = BTreeMap::new();
    for r in db.records.iter().filter(|r| r.level == Level::L2) {
        if let Some(g) = record_group(r, &db.projects) {
            let topic = r
                .topic
                .as_deref()
                .and_then(normalize_topic)
                .unwrap_or_else(|| DEFAULT_TOPIC.to_string());
            *topic_counts.entry(g).or_default().entry(topic).or_default() += 1;
        }
    }
    for (g, topics) in topic_counts {
        if let Some(view) = by_name.get_mut(&g) {
            let mut t: Vec<(String, usize)> = topics.into_iter().collect();
            t.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
            view.topics = t;
        }
    }
    let ungrouped = projects
        .iter()
        .filter(|p| p.group.is_none())
        .map(|p| p.id.clone())
        .collect();

    ConsoleState {
        user: user.to_string(),
        data_dir: root.to_path_buf(),
        version: env!("CARGO_PKG_VERSION"),
        embedder: db.embedder.clone(),
        dims: db.dims,
        layers,
        param_keys: param_keys.into_iter().collect(),
        projects,
        groups: by_name.into_values().collect(),
        ungrouped,
        installs: summaries(home, cwd),
    }
}

// -- terminal app ------------------------------------------------------------

const TABS: [&str; 5] = ["Overview", "Projects", "Groups", "Skills", "Graph"];

pub fn run(user: &str) -> crate::error::Result<()> {
    let mut state = collect_state(
        &data_root(),
        user,
        &home_dir(),
        &std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
    );
    let mut terminal = ratatui::init();
    let result = event_loop(&mut terminal, &mut state);
    ratatui::restore();
    result
}

fn event_loop(
    terminal: &mut ratatui::DefaultTerminal,
    state: &mut ConsoleState,
) -> crate::error::Result<()> {
    let mut tab = 0usize;
    let mut selected = 0usize;
    loop {
        let cap = selection_cap(state, tab);
        selected = selected.min(cap);
        terminal
            .draw(|f| draw(f, state, tab, selected))
            .map_err(|e| crate::error::MemoryError::invalid(format!("console: {e}")))?;
        if !event::poll(Duration::from_millis(250))
            .map_err(|e| crate::error::MemoryError::invalid(format!("console: {e}")))?
        {
            continue;
        }
        let Event::Key(k) = event::read()
            .map_err(|e| crate::error::MemoryError::invalid(format!("console: {e}")))?
        else {
            continue;
        };
        if k.kind != KeyEventKind::Press {
            continue;
        }
        match k.code {
            KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
            KeyCode::Char('r') => {
                *state = collect_state(
                    &data_root(),
                    &state.user,
                    &home_dir(),
                    &std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
                );
            }
            KeyCode::Tab | KeyCode::Right => {
                tab = (tab + 1) % TABS.len();
                selected = 0;
            }
            KeyCode::Left | KeyCode::BackTab => {
                tab = (tab + TABS.len() - 1) % TABS.len();
                selected = 0;
            }
            KeyCode::Down => selected = selected.saturating_add(1).min(cap),
            KeyCode::Up => selected = selected.saturating_sub(1),
            _ => {}
        }
    }
}

fn selection_cap(state: &ConsoleState, tab: usize) -> usize {
    match tab {
        1 => state.projects.len().saturating_sub(1),
        2 => state.groups.len().saturating_sub(1),
        3 => state.installs.len().saturating_sub(1),
        _ => 0,
    }
}

fn draw(f: &mut Frame, state: &ConsoleState, tab: usize, selected: usize) {
    let [header, body, footer] = Layout::vertical([
        Constraint::Length(3),
        Constraint::Min(1),
        Constraint::Length(1),
    ])
    .areas(f.area());

    let titles: Vec<Line> = TABS.iter().map(|t| Line::from(*t)).collect();
    f.render_widget(
        Tabs::new(titles)
            .block(Block::bordered().title(format!(
                " tiered-memory v{} · user `{}` · {} ",
                state.version,
                state.user,
                state.data_dir.display()
            )))
            .select(tab)
            .highlight_style(
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
        header,
    );

    match tab {
        0 => draw_overview(f, state, body),
        1 => draw_projects(f, state, body, selected),
        2 => draw_groups(f, state, body, selected),
        3 => draw_skills(f, state, body, selected),
        _ => draw_graph(f, state, body, selected),
    }

    let hints = match tab {
        1 => "↑/↓ select · r refresh · tab next panel · q quit",
        2 => "↑/↓ select group · r refresh · tab next panel · q quit",
        3 => "↑/↓ select · install more: tiered-memory install-skill · q quit",
        4 => "↑/↓ scroll nodes · r refresh · q quit",
        _ => "r refresh · tab/←→ switch panels · q quit",
    };
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            format!(" {hints} "),
            Style::default().dim(),
        ))),
        footer,
    );
}

// -- overview ----------------------------------------------------------------

fn draw_overview(f: &mut Frame, state: &ConsoleState, area: Rect) {
    let [info, gauges, flow, skills] = Layout::vertical([
        Constraint::Length(7),
        Constraint::Length(11),
        Constraint::Length(4),
        Constraint::Min(1),
    ])
    .areas(area);

    let keys = if state.param_keys.is_empty() {
        String::from("(none yet)")
    } else {
        state.param_keys.join(", ")
    };
    f.render_widget(
        Paragraph::new(vec![
            Line::from(vec![
                Span::styled("records: ", Style::default().dim()),
                Span::raw(format!("{}", state.total_records())),
                Span::styled("   embedder: ", Style::default().dim()),
                Span::raw(format!("{} ({} dims)", state.embedder, state.dims)),
                Span::styled("   projects: ", Style::default().dim()),
                Span::raw(format!("{}", state.projects.len())),
            ]),
            Line::from(""),
            Line::from(vec![
                Span::styled("learned parameters: ", Style::default().dim()),
                Span::raw(keys),
            ]),
            Line::from(""),
            Line::from(Span::styled(
                "recall probes L1 → L2 → L3, nearest layer wins · hot L2/L3 memories are promoted into L1",
                Style::default().dim(),
            )),
            Line::from(Span::styled(
                "consolidation lifts parameters that agree across projects into L3 traits",
                Style::default().dim(),
            )),
        ])
        .block(Block::bordered().title(" memory engine ")),
        info,
    );

    let gauge_rows: Vec<Rect> = Layout::vertical(
        state
            .layers
            .iter()
            .map(|_| Constraint::Length(3))
            .collect::<Vec<_>>(),
    )
    .split(gauges)
    .to_vec();
    for (layer, row) in state.layers.iter().zip(gauge_rows) {
        let ratio = if layer.capacity == 0 {
            0.0
        } else {
            (layer.count as f64 / layer.capacity as f64).clamp(0.0, 1.0)
        };
        f.render_widget(
            Gauge::default()
                .block(Block::bordered().title(format!(" {} ", layer.label)))
                .ratio(ratio)
                .label(format!("{} / {}", layer.count, layer.capacity))
                .gauge_style(Style::default().fg(Color::Cyan)),
            row,
        );
    }

    let short = |i: usize| {
        state.layers[i]
            .label
            .split(' ')
            .next()
            .unwrap_or("?")
            .to_string()
    };
    f.render_widget(
        Paragraph::new(Line::from(format!(
            "  {}  ──evict──▶  {}  ──lift──▶  {}",
            format_args!("{} ({})", short(0), state.layers[0].count),
            format_args!("{} ({})", short(1), state.layers[1].count),
            format_args!("{} ({})", short(2), state.layers[2].count),
        )))
        .alignment(Alignment::Center)
        .block(Block::bordered().title(" layer flow ")),
        flow,
    );

    let installed: Vec<&SkillInstall> = state.installs.iter().filter(|i| i.installed).collect();
    let lines = if installed.is_empty() {
        vec![
            Line::from("no agent harness has the skill yet"),
            Line::from(Span::styled(
                "run `tiered-memory install-skill` (or `tiered-memory setup` in a project) to install it",
                Style::default().fg(Color::Yellow),
            )),
        ]
    } else {
        installed
            .iter()
            .map(|i| {
                Line::from(vec![
                    Span::styled("✓ ", Style::default().fg(Color::Green)),
                    Span::raw(i.label.clone()),
                    Span::styled(format!("  —  {}", i.path.display()), Style::default().dim()),
                ])
            })
            .collect()
    };
    f.render_widget(
        Paragraph::new(lines).block(Block::bordered().title(" agent skills ")),
        skills,
    );
}

// -- projects ----------------------------------------------------------------

fn draw_projects(f: &mut Frame, state: &ConsoleState, area: Rect, selected: usize) {
    if state.projects.is_empty() {
        render_empty(f, area, "no projects — run `tiered-memory init` inside one");
        return;
    }
    let [table_area, detail] =
        Layout::vertical([Constraint::Min(3), Constraint::Length(4)]).areas(area);
    let header = Row::new(["project", "name", "L2 group", "L1", "L2", "similar"])
        .style(Style::default().bold());
    let rows = state.projects.iter().map(|p| {
        Row::new(vec![
            p.id.clone(),
            p.name.clone(),
            p.group.clone().unwrap_or_else(|| "—".into()),
            p.l1.to_string(),
            p.l2.to_string(),
            p.similar.to_string(),
        ])
    });
    let widths = [
        Constraint::Percentage(22),
        Constraint::Percentage(24),
        Constraint::Percentage(18),
        Constraint::Length(6),
        Constraint::Length(6),
        Constraint::Length(9),
    ];
    let mut ts = TableState::default().with_selected(Some(selected));
    f.render_stateful_widget(
        Table::new(rows, widths)
            .header(header)
            .block(Block::bordered().title(" projects "))
            .row_highlight_style(Style::default().bg(Color::DarkGray))
            .column_spacing(1),
        table_area,
        &mut ts,
    );
    let detail_text = match state.projects.get(selected) {
        Some(p) => Line::from(vec![
            Span::styled(p.id.clone(), Style::default().bold()),
            Span::styled(format!(" — {}  ", p.name), Style::default().dim()),
            Span::raw(p.descriptor.clone()),
        ]),
        None => Line::from(""),
    };
    f.render_widget(
        Paragraph::new(detail_text).block(Block::bordered().title(" descriptor ")),
        detail,
    );
}

// -- groups ------------------------------------------------------------------

fn draw_groups(f: &mut Frame, state: &ConsoleState, area: Rect, selected: usize) {
    if state.groups.is_empty() {
        render_empty(
            f,
            area,
            "no L2 groups yet — assign one with `tiered-memory group set <name>` (the /tiered-memory skill asks once per project)",
        );
        return;
    }
    let [list, detail] =
        Layout::horizontal([Constraint::Percentage(34), Constraint::Min(1)]).areas(area);
    let items: Vec<ListItem> = state
        .groups
        .iter()
        .map(|g| {
            ListItem::new(Line::from(format!(
                "{} ({} members)",
                g.name,
                g.members.len()
            )))
        })
        .collect();
    let mut ls = ListState::default().with_selected(Some(selected));
    f.render_stateful_widget(
        List::new(items)
            .block(Block::bordered().title(" L2 groups "))
            .highlight_style(
                Style::default()
                    .bg(Color::DarkGray)
                    .add_modifier(Modifier::BOLD),
            ),
        list,
        &mut ls,
    );
    let Some(g) = state.groups.get(selected) else {
        return;
    };
    let mut lines = vec![
        Line::from(vec![
            Span::styled("members: ", Style::default().dim()),
            Span::raw(if g.members.is_empty() {
                "(none)".into()
            } else {
                g.members.join(", ")
            }),
        ]),
        Line::from(""),
        Line::from(Span::styled(
            "per-topic docs (cache/L2/groups/…):",
            Style::default().dim(),
        )),
    ];
    if g.topics.is_empty() {
        lines.push(Line::from("  (no L2 memories filed yet)"));
    } else {
        for (topic, n) in &g.topics {
            lines.push(Line::from(format!("  {topic}.md — {n}")));
        }
    }
    f.render_widget(
        Paragraph::new(lines).block(Block::bordered().title(format!(" group {} ", g.name))),
        detail,
    );
}

// -- skills ------------------------------------------------------------------

fn draw_skills(f: &mut Frame, state: &ConsoleState, area: Rect, selected: usize) {
    let [table_area, detail] =
        Layout::vertical([Constraint::Min(3), Constraint::Length(4)]).areas(area);
    let header =
        Row::new(["", "harness", "target", "skill summary"]).style(Style::default().bold());
    let rows = state.installs.iter().map(|i| {
        Row::new(vec![
            if i.installed {
                "✓".to_string()
            } else {
                " ".to_string()
            },
            i.label.clone(),
            i.path.display().to_string(),
            truncate(&i.description, 70),
        ])
    });
    let widths = [
        Constraint::Length(2),
        Constraint::Percentage(28),
        Constraint::Percentage(36),
        Constraint::Min(20),
    ];
    let mut ts = TableState::default().with_selected(Some(selected));
    f.render_stateful_widget(
        Table::new(rows, widths)
            .header(header)
            .block(Block::bordered().title(" skill installs "))
            .row_highlight_style(Style::default().bg(Color::DarkGray))
            .column_spacing(1),
        table_area,
        &mut ts,
    );
    let detail_text = match state.installs.get(selected) {
        Some(i) if i.installed => Line::from(vec![
            Span::styled("installed at ", Style::default().dim()),
            Span::raw(i.path.display().to_string()),
            Span::styled(
                " — reinstall after upgrading: tiered-memory install-skill",
                Style::default().dim(),
            ),
        ]),
        Some(i) => Line::from(Span::styled(
            format!(
                "not installed — install with: tiered-memory install-skill --harness {}",
                i.harness
            ),
            Style::default().fg(Color::Yellow),
        )),
        None => Line::from(""),
    };
    f.render_widget(
        Paragraph::new(detail_text).block(Block::bordered().title(" status ")),
        detail,
    );
}

// -- graph -------------------------------------------------------------------

fn draw_graph(f: &mut Frame, state: &ConsoleState, area: Rect, scroll: usize) {
    let [pipeline, canvas_area] =
        Layout::vertical([Constraint::Length(3), Constraint::Min(1)]).areas(area);
    f.render_widget(
        Paragraph::new(Line::from(format!(
            "  L1 {}  ──evict──▶  L2 {}  ──lift──▶  L3 {}      (projects ○ left · L2 groups ○ right · a line = membership)",
            state.layers[0].count, state.layers[1].count, state.layers[2].count
        )))
        .block(Block::bordered().title(" layers ")),
        pipeline,
    );

    if state.projects.is_empty() {
        render_empty(
            f,
            canvas_area,
            "nothing to graph yet — register a project with `tiered-memory init`",
        );
        return;
    }

    // a scrolling window of projects on the left; all groups on the right
    const MAX_NODES: usize = 10;
    let start = scroll.min(state.projects.len().saturating_sub(1));
    let shown: Vec<&ProjectView> = state.projects.iter().skip(start).take(MAX_NODES).collect();
    let n_p = shown.len() as f64;
    let n_g = state.groups.len() as f64;

    let project_y = |i: usize| -> f64 {
        if n_p <= 1.0 {
            50.0
        } else {
            90.0 - 80.0 * i as f64 / (n_p - 1.0)
        }
    };
    let group_y = |i: usize| -> f64 {
        if n_g <= 1.0 {
            50.0
        } else {
            90.0 - 80.0 * i as f64 / (n_g - 1.0)
        }
    };
    let group_index = |name: &str| state.groups.iter().position(|g| g.name == name);

    let title_suffix = if state.projects.len() > MAX_NODES {
        format!(
            " — showing {}–{} of {} (↑/↓ scroll)",
            start + 1,
            start + shown.len(),
            state.projects.len()
        )
    } else {
        String::new()
    };

    let canvas = canvas::Canvas::default()
        .block(Block::bordered().title(format!(" projects ↔ L2 groups{title_suffix} ")))
        .x_bounds([0.0, 100.0])
        .y_bounds([0.0, 100.0])
        .marker(symbols::marker::Marker::Braille)
        .paint(|ctx| {
            for (i, p) in shown.iter().enumerate() {
                let y = project_y(i);
                let grouped = p.group.as_deref().and_then(group_index);
                if let Some(gi) = grouped {
                    ctx.draw(&ShapeLine {
                        x1: 30.0,
                        y1: y,
                        x2: 70.0,
                        y2: group_y(gi),
                        color: Color::DarkGray,
                    });
                    ctx.draw(&Circle {
                        x: 30.0,
                        y,
                        radius: 1.5,
                        color: Color::Cyan,
                    });
                } else {
                    ctx.draw(&Circle {
                        x: 30.0,
                        y,
                        radius: 1.5,
                        color: Color::DarkGray,
                    });
                }
                let label = match grouped {
                    Some(_) => p.id.clone(),
                    None => format!("{} (no group)", p.id),
                };
                ctx.print(2.0, y + 0.5, label);
            }
            for (i, g) in state.groups.iter().enumerate() {
                let y = group_y(i);
                ctx.draw(&Circle {
                    x: 70.0,
                    y,
                    radius: 2.0,
                    color: Color::Magenta,
                });
                ctx.print(
                    74.0,
                    y + 0.5,
                    format!(
                        "{} ({} members · {} topics)",
                        g.name,
                        g.members.len(),
                        g.topics.len()
                    ),
                );
            }
        });
    f.render_widget(canvas, canvas_area);
}

// -- helpers -----------------------------------------------------------------

fn render_empty(f: &mut Frame, area: Rect, text: &str) {
    f.render_widget(
        Paragraph::new(Span::styled(text.to_string(), Style::default().dim()))
            .alignment(Alignment::Center),
        area,
    );
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        s.chars().take(max.saturating_sub(1)).collect::<String>() + "…"
    }
}
