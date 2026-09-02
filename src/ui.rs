use crate::model::{AgentState, CoverageLevel, SessionState, TurnStatus};
use crate::rollout::{PollOutcome, SelectedReader, SessionGroup};
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use crossterm::{
    cursor::{Hide, Show},
    event::{self, Event, KeyCode, KeyEventKind},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap},
    Frame, Terminal,
};
use std::{
    collections::{HashMap, HashSet},
    io::{self, Stdout},
    time::{Duration, Instant},
};

const EVENT_POLL: Duration = Duration::from_millis(250);
const UPDATE_INTERVAL: Duration = Duration::from_secs(1);
const RENDER_TEXT_LIMIT: usize = 256;
const _: fn(&SessionGroup, &SessionState) -> Vec<String> = crate::rollout::tree_lines;

pub struct TerminalGuard {
    raw: bool,
    alternate: bool,
    cursor_hidden: bool,
}

impl TerminalGuard {
    fn enter() -> Result<Self> {
        enable_raw_mode().context("enable terminal raw mode")?;
        let mut guard = Self {
            raw: true,
            alternate: false,
            cursor_hidden: false,
        };
        execute!(io::stdout(), EnterAlternateScreen).context("enter terminal alternate screen")?;
        guard.alternate = true;
        execute!(io::stdout(), Hide).context("hide terminal cursor")?;
        guard.cursor_hidden = true;
        Ok(guard)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        if self.cursor_hidden {
            let _ = execute!(io::stdout(), Show);
        }
        if self.alternate {
            let _ = execute!(io::stdout(), LeaveAlternateScreen);
        }
        if self.raw {
            let _ = disable_raw_mode();
        }
    }
}

#[derive(Default)]
struct UiState {
    selected_thread: Option<String>,
    catching_up: bool,
    last_change: Option<Instant>,
}

#[derive(Clone)]
struct TreeRow {
    thread_id: String,
    depth: usize,
}

impl UiState {
    fn synchronise(&mut self, rows: &[TreeRow]) {
        if rows.is_empty() {
            self.selected_thread = None;
        } else if !rows
            .iter()
            .any(|row| Some(&row.thread_id) == self.selected_thread.as_ref())
        {
            self.selected_thread = Some(rows[0].thread_id.clone());
        }
    }

    fn move_selection(&mut self, rows: &[TreeRow], delta: isize) {
        self.synchronise(rows);
        let Some(selected) = &self.selected_thread else {
            return;
        };
        let current = rows
            .iter()
            .position(|row| &row.thread_id == selected)
            .unwrap_or(0);
        let next = current
            .saturating_add_signed(delta)
            .min(rows.len().saturating_sub(1));
        self.selected_thread = Some(rows[next].thread_id.clone());
    }
}

pub fn run(reader: &mut SelectedReader) -> Result<()> {
    let _guard = TerminalGuard::enter()?;
    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend).context("initialise terminal")?;
    terminal.clear().context("clear terminal")?;

    let result = event_loop(&mut terminal, reader);
    let cursor_result = terminal.show_cursor().context("restore terminal cursor");
    result.and(cursor_result)
}

fn event_loop(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    reader: &mut SelectedReader,
) -> Result<()> {
    let mut ui = UiState::default();
    let mut dirty = true;
    let mut last_update = Instant::now();
    loop {
        let rows = flatten(&reader.group, &reader.state);
        ui.synchronise(&rows);
        if dirty {
            terminal
                .draw(|frame| draw(frame, &reader.group, &reader.state, &rows, &ui))
                .context("draw terminal UI")?;
            dirty = false;
        }

        if event::poll(EVENT_POLL).context("poll terminal events")? {
            match event::read().context("read terminal event")? {
                Event::Key(key) if key.kind == KeyEventKind::Press => match key.code {
                    KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
                    KeyCode::Up | KeyCode::Char('k') => {
                        ui.move_selection(&rows, -1);
                        dirty = true;
                    }
                    KeyCode::Down | KeyCode::Char('j') => {
                        ui.move_selection(&rows, 1);
                        dirty = true;
                    }
                    KeyCode::Char('r') => {
                        let outcome = reader.poll().context("rescan selected session")?;
                        note_poll(&mut ui, outcome);
                        last_update = Instant::now();
                        dirty = true;
                    }
                    _ => {}
                },
                Event::Resize(_, _) => dirty = true,
                _ => {}
            }
        }

        if last_update.elapsed() >= UPDATE_INTERVAL {
            let outcome = reader.poll().context("update selected session")?;
            last_update = Instant::now();
            note_poll(&mut ui, outcome);
            dirty = true;
        }
    }
}

fn note_poll(ui: &mut UiState, outcome: PollOutcome) -> bool {
    let changed = match outcome {
        PollOutcome::Updated { records, admitted } => records > 0 || admitted > 0,
        PollOutcome::Rebuilt => true,
    };
    if changed {
        ui.catching_up = true;
        ui.last_change = Some(Instant::now());
        return true;
    }
    if ui.catching_up
        && ui
            .last_change
            .is_some_and(|instant| instant.elapsed() >= UPDATE_INTERVAL)
    {
        ui.catching_up = false;
        return true;
    }
    false
}

fn flatten(group: &SessionGroup, state: &SessionState) -> Vec<TreeRow> {
    fn local_activity(agent: &AgentState) -> Option<DateTime<Utc>> {
        agent.last_activity_at.or(agent.latest_turn.started_at)
    }

    fn label(id: &str, state: &SessionState) -> String {
        let agent = &state.agents[id];
        agent
            .agent_path
            .as_deref()
            .or(agent.agent_nickname.as_deref())
            .unwrap_or(id)
            .to_owned()
    }

    fn subtree_activity(
        id: &str,
        children: &HashMap<String, Vec<String>>,
        state: &SessionState,
        visiting: &mut HashSet<String>,
        memo: &mut HashMap<String, Option<DateTime<Utc>>>,
    ) -> Option<DateTime<Utc>> {
        if let Some(activity) = memo.get(id) {
            return *activity;
        }
        let agent = state.agents.get(id)?;
        if !visiting.insert(id.to_owned()) {
            return local_activity(agent);
        }

        let mut newest = local_activity(agent);
        if let Some(ids) = children.get(id) {
            for child in ids {
                newest = newest.max(subtree_activity(child, children, state, visiting, memo));
            }
        }
        visiting.remove(id);
        memo.insert(id.to_owned(), newest);
        newest
    }

    fn visit(
        id: &str,
        depth: usize,
        children: &HashMap<String, Vec<String>>,
        state: &SessionState,
        seen: &mut HashSet<String>,
        rows: &mut Vec<TreeRow>,
    ) {
        if !seen.insert(id.to_owned()) || !state.agents.contains_key(id) {
            return;
        }
        rows.push(TreeRow {
            thread_id: id.to_owned(),
            depth,
        });
        if let Some(ids) = children.get(id) {
            for child in ids {
                visit(child, depth + 1, children, state, seen, rows);
            }
        }
    }

    let mut children: HashMap<String, Vec<String>> = HashMap::new();
    for agent in state.agents.values() {
        if let Some(parent) = &agent.parent_thread_id {
            children
                .entry(parent.clone())
                .or_default()
                .push(agent.thread_id.clone());
        }
    }

    let mut memo = HashMap::new();
    let ids = children
        .values()
        .flat_map(|ids| ids.iter())
        .cloned()
        .collect::<Vec<_>>();
    for id in ids {
        subtree_activity(&id, &children, state, &mut HashSet::new(), &mut memo);
    }
    for ids in children.values_mut() {
        ids.sort_by(|left, right| {
            memo.get(right)
                .copied()
                .flatten()
                .cmp(&memo.get(left).copied().flatten())
                .then_with(|| label(left, state).cmp(&label(right, state)))
                .then_with(|| left.cmp(right))
        });
    }

    let root = &group.rollouts[group.root].thread_id;
    let mut rows = Vec::new();
    visit(root, 0, &children, state, &mut HashSet::new(), &mut rows);
    rows
}

fn draw(
    frame: &mut Frame,
    group: &SessionGroup,
    state: &SessionState,
    rows: &[TreeRow],
    ui: &UiState,
) {
    let area = frame.area();
    if area.width < 30 || area.height < 8 {
        frame.render_widget(
            Paragraph::new("agentop\nterminal too small\nq / Esc: quit")
                .block(Block::default().borders(Borders::ALL))
                .wrap(Wrap { trim: true }),
            area,
        );
        return;
    }

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2),
            Constraint::Percentage(42),
            Constraint::Min(3),
            Constraint::Length(1),
        ])
        .split(area);

    let root = &group.rollouts[group.root];
    let header = format!(
        "agentop · {} · {} · {} agents · started {}{}",
        abbreviate(&state.session_id),
        root.cwd.as_deref().or(state.cwd.as_deref()).map_or_else(
            || "(unknown project)".into(),
            |path| { render_text(&path.display().to_string()) }
        ),
        state.agents.len(),
        timestamp(state.started_at),
        if ui.catching_up {
            " · catching up…"
        } else {
            ""
        }
    );
    frame.render_widget(
        Paragraph::new(header).block(Block::default().borders(Borders::BOTTOM)),
        chunks[0],
    );

    let items = rows
        .iter()
        .map(|row| {
            let agent = &state.agents[&row.thread_id];
            ListItem::new(tree_line(row, agent))
        })
        .collect::<Vec<_>>();
    let mut list_state = ListState::default();
    list_state.select(
        ui.selected_thread
            .as_ref()
            .and_then(|id| rows.iter().position(|row| &row.thread_id == id)),
    );
    frame.render_stateful_widget(
        List::new(items)
            .block(Block::default().title(" agents ").borders(Borders::ALL))
            .highlight_style(Style::default().add_modifier(Modifier::REVERSED))
            .highlight_symbol("› "),
        chunks[1],
        &mut list_state,
    );

    let selected = ui
        .selected_thread
        .as_ref()
        .and_then(|id| state.agents.get(id));
    frame.render_widget(
        Paragraph::new(detail_lines(selected, state, group))
            .block(Block::default().title(" details ").borders(Borders::ALL))
            .wrap(Wrap { trim: false }),
        chunks[2],
    );
    frame.render_widget(
        Paragraph::new("↑/↓ or j/k select   r rescan   q/Esc quit"),
        chunks[3],
    );
}

fn tree_line(row: &TreeRow, agent: &AgentState) -> Line<'static> {
    let label = render_text(
        agent
            .agent_path
            .as_deref()
            .or(agent.agent_nickname.as_deref())
            .unwrap_or(&agent.thread_id),
    );
    let role = render_text(agent.agent_role.as_deref().unwrap_or(if row.depth == 0 {
        "orchestrator"
    } else {
        "unknown role"
    }));
    let status = status(agent.latest_turn.status);
    let age = agent
        .last_activity_at
        .map_or_else(|| "no activity".into(), age);
    Line::from(vec![
        Span::raw(format!("{}{}  ", "  ".repeat(row.depth), label)),
        Span::raw(format!("{role}  {status}  {age}")),
    ])
}

fn detail_lines<'a>(
    agent: Option<&'a AgentState>,
    state: &SessionState,
    group: &SessionGroup,
) -> Vec<Line<'a>> {
    let Some(agent) = agent else {
        return vec![Line::from("No agent selected")];
    };
    let health = &state.data_health;
    let metadata = group
        .rollouts
        .iter()
        .find(|metadata| metadata.thread_id == agent.thread_id);
    let schema = if agent.schema_catalogued {
        "catalogued"
    } else {
        "missing"
    };
    let mut lines = vec![
        Line::from(format!(
            "{} · thread {} · parent {}",
            render_text(agent.agent_path.as_deref().unwrap_or("(unnamed)")),
            render_text(&agent.thread_id),
            render_text(agent.parent_thread_id.as_deref().unwrap_or("none"))
        )),
        Line::from(format!(
            "role {} · nickname {} · Codex {} · schema {} · compatibility {}",
            render_text(agent.agent_role.as_deref().unwrap_or("unknown")),
            render_text(agent.agent_nickname.as_deref().unwrap_or("unknown")),
            render_text(&agent.cli_version),
            schema,
            coverage(agent.coverage)
        )),
        Line::from(format!(
            "lifecycle {} · turn {} · depth {} · started {} · completed {} · last activity {}",
            status(agent.latest_turn.status),
            render_text(agent.latest_turn.turn_id.as_deref().unwrap_or("unknown")),
            metadata
                .and_then(|metadata| metadata.depth)
                .map_or_else(|| "unknown".into(), |depth| depth.to_string()),
            timestamp(agent.latest_turn.started_at),
            timestamp(agent.latest_turn.completed_at),
            timestamp(agent.last_activity_at)
        )),
        Line::from(format!(
            "activity: {}{}",
            render_text(agent.current_activity().unwrap_or("none")),
            agent
                .active_call_evidence()
                .map(|(started, ordinal)| format!(
                    " · active call started {} · ordinal {}",
                    timestamp(started),
                    ordinal.map_or_else(|| "unknown".into(), |value| value.to_string())
                ))
                .unwrap_or_default()
        )),
        Line::from(format!(
            "reasoning: {}",
            render_text(agent.last_reasoning_summary.as_deref().unwrap_or("none"))
        )),
        Line::from(format!(
            "message: {}",
            render_text(agent.last_message.as_deref().unwrap_or("none"))
        )),
        Line::from(format!(
            "final: {}",
            render_text(agent.final_message.as_deref().unwrap_or("none"))
        )),
        Line::from(format!(
            "untrusted result claim: {}",
            render_text(agent.result_status_claim.as_deref().unwrap_or("none"))
        )),
        Line::from(format!(
            "health: unknown records {} · unknown events {} · malformed {} · oversized {}",
            health.unknown_records,
            health.unknown_events,
            health.malformed_records,
            health.oversized_records
        )),
    ];
    if let Some(diagnostic) = health.recent_diagnostics.back() {
        lines.push(Line::from(format!(
            "latest diagnostic: {}:{} ordinal {} · {} {}",
            render_text(&diagnostic.rollout_path.display().to_string()),
            diagnostic.byte_offset,
            diagnostic
                .ordinal
                .map_or_else(|| "unknown".into(), |value| value.to_string()),
            render_text(&diagnostic.kind),
            render_text(diagnostic.detail.as_deref().unwrap_or(""))
        )));
    }
    lines
}

fn status(value: TurnStatus) -> &'static str {
    match value {
        TurnStatus::Pending => "PENDING",
        TurnStatus::Running => "RUNNING",
        TurnStatus::Completed => "COMPLETED",
        TurnStatus::Interrupted => "INTERRUPTED",
        TurnStatus::Errored => "ERRORED",
    }
}

fn coverage(value: CoverageLevel) -> &'static str {
    match value {
        CoverageLevel::Unknown => "unknown",
        CoverageLevel::Ingestable => "ingestable",
        CoverageLevel::SemanticallyCovered => "semantically covered",
        CoverageLevel::LiveVerified => "live verified",
    }
}

fn timestamp(value: Option<DateTime<Utc>>) -> String {
    value.map_or_else(|| "unknown".into(), |time| time.to_rfc3339())
}

fn age(time: DateTime<Utc>) -> String {
    let seconds = Utc::now().signed_duration_since(time).num_seconds().max(0);
    if seconds < 60 {
        format!("{seconds}s ago")
    } else if seconds < 3_600 {
        format!("{}m ago", seconds / 60)
    } else if seconds < 86_400 {
        format!("{}h ago", seconds / 3_600)
    } else {
        format!("{}d ago", seconds / 86_400)
    }
}

fn render_text(value: &str) -> String {
    let mut characters = value
        .chars()
        .filter(|character| !character.is_control())
        .take(RENDER_TEXT_LIMIT + 1)
        .collect::<Vec<_>>();
    if characters.len() > RENDER_TEXT_LIMIT {
        characters.truncate(RENDER_TEXT_LIMIT - 1);
        characters.push('…');
    }
    characters.into_iter().collect()
}

fn abbreviate(value: &str) -> String {
    let safe = render_text(value);
    let prefix = safe.chars().take(8).collect::<String>();
    if safe.chars().count() > 8 {
        format!("{prefix}…")
    } else {
        prefix
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rollout::RolloutMetadata;
    use ratatui::backend::TestBackend;
    use std::path::PathBuf;

    fn fixture() -> (SessionGroup, SessionState) {
        let root = RolloutMetadata {
            path: PathBuf::from("root"),
            session_id: "session".into(),
            thread_id: "root".into(),
            parent_thread_id: None,
            cwd: Some(PathBuf::from("/project")),
            timestamp: None,
            cli_version: "0.152.1".into(),
            agent_path: Some("/root".into()),
            agent_role: None,
            agent_nickname: None,
            depth: None,
            history_start: None,
            consumed_offset: 0,
        };
        let child = RolloutMetadata {
            path: PathBuf::from("child"),
            session_id: "session".into(),
            thread_id: "child".into(),
            parent_thread_id: Some("root".into()),
            cwd: Some(PathBuf::from("/project")),
            timestamp: None,
            cli_version: "0.152.1".into(),
            agent_path: Some("/root/child".into()),
            agent_role: Some("map_implementer".into()),
            agent_nickname: None,
            depth: Some(1),
            history_start: None,
            consumed_offset: 0,
        };
        let group = SessionGroup {
            session_id: "session".into(),
            rollouts: vec![root, child],
            root: 0,
        };
        let mut state = SessionState {
            session_id: "session".into(),
            cwd: Some(PathBuf::from("/project")),
            ..SessionState::default()
        };
        let mut root_state = AgentState::new("root".into(), "0.152.1".into());
        root_state.agent_path = Some("/root".into());
        let mut child_state = AgentState::new("child".into(), "0.152.1".into());
        child_state.parent_thread_id = Some("root".into());
        child_state.agent_path = Some("/root/child".into());
        child_state.agent_role = Some("map_implementer".into());
        state.agents.insert("root".into(), root_state);
        state.agents.insert("child".into(), child_state);
        (group, state)
    }

    fn add_agent(state: &mut SessionState, id: &str, parent: &str, path: &str, activity: i64) {
        let mut agent = AgentState::new(id.into(), "0.152.1".into());
        agent.parent_thread_id = Some(parent.into());
        agent.agent_path = Some(path.into());
        agent.last_activity_at = DateTime::from_timestamp(activity, 0);
        state.agents.insert(id.into(), agent);
    }

    #[test]
    fn newest_subtree_activity_orders_hierarchy_with_root_first() {
        let (group, mut state) = fixture();
        state.agents.get_mut("child").unwrap().last_activity_at = DateTime::from_timestamp(10, 0);
        add_agent(
            &mut state,
            "grandchild",
            "child",
            "/root/child/grandchild",
            100,
        );
        add_agent(&mut state, "sibling", "root", "/root/sibling", 50);

        let ids = flatten(&group, &state)
            .into_iter()
            .map(|row| row.thread_id)
            .collect::<Vec<_>>();
        assert_eq!(ids, ["root", "child", "grandchild", "sibling"]);
    }

    #[test]
    fn direct_siblings_are_newest_first_with_deterministic_ties() {
        let (group, mut state) = fixture();
        state.agents.get_mut("child").unwrap().last_activity_at = DateTime::from_timestamp(10, 0);
        add_agent(&mut state, "newer", "root", "/root/newer", 30);
        add_agent(&mut state, "tie-b", "root", "/root/tie", 20);
        add_agent(&mut state, "tie-a", "root", "/root/tie", 20);

        let ids = flatten(&group, &state)
            .into_iter()
            .map(|row| row.thread_id)
            .collect::<Vec<_>>();
        assert_eq!(ids, ["root", "newer", "tie-a", "tie-b", "child"]);
    }

    #[test]
    fn selection_survives_tree_reordering_and_navigation_is_bounded() {
        let (group, mut state) = fixture();
        let mut ui = UiState::default();
        let rows = flatten(&group, &state);
        ui.synchronise(&rows);
        ui.move_selection(&rows, 1);
        assert_eq!(ui.selected_thread.as_deref(), Some("child"));

        add_agent(&mut state, "newer", "root", "/root/newer", 100);
        let rows = flatten(&group, &state);
        ui.synchronise(&rows);
        assert_eq!(ui.selected_thread.as_deref(), Some("child"));
        ui.move_selection(&rows, 99);
        assert_eq!(ui.selected_thread.as_deref(), Some("child"));
        ui.move_selection(&rows, -99);
        assert_eq!(ui.selected_thread.as_deref(), Some("root"));
    }

    #[test]
    fn tiny_terminal_renders_without_panicking() {
        let (group, state) = fixture();
        let backend = TestBackend::new(20, 4);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                draw(
                    frame,
                    &group,
                    &state,
                    &flatten(&group, &state),
                    &UiState::default(),
                )
            })
            .unwrap();
        let text = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(text.contains("terminal too small"));
    }

    #[test]
    fn render_boundary_removes_controls_and_bounds_metadata() {
        let unsafe_text = format!("prefix\u{1b}[31m\n{}", "x".repeat(RENDER_TEXT_LIMIT * 2));
        let rendered = render_text(&unsafe_text);
        assert!(!rendered.chars().any(char::is_control));
        assert!(rendered.chars().count() <= RENDER_TEXT_LIMIT);
        assert!(rendered.ends_with('…'));

        let (mut group, mut state) = fixture();
        group.rollouts[0].cwd = Some(PathBuf::from(&unsafe_text));
        state.session_id = unsafe_text.clone();
        state.agents.get_mut("root").unwrap().cli_version = unsafe_text;
        let backend = TestBackend::new(100, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                draw(
                    frame,
                    &group,
                    &state,
                    &flatten(&group, &state),
                    &UiState::default(),
                )
            })
            .unwrap();
        let text = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(!text.chars().any(char::is_control));
    }
}
