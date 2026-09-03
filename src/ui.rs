use crate::model::{AgentState, CoverageLevel, DataHealth, SessionState, TurnStatus};
use crate::rollout::{self, Discovery, PollOutcome, SelectedReader, SessionGroup};
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
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap},
    Frame, Terminal,
};
use std::{
    collections::{HashMap, HashSet},
    io::{self, Stdout},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

const EVENT_POLL: Duration = Duration::from_millis(250);
const UPDATE_INTERVAL: Duration = Duration::from_secs(1);
const INITIAL_UPDATE_INTERVAL: Duration = Duration::from_millis(50);
const RENDER_TEXT_LIMIT: usize = 256;
const _: fn(&SessionGroup, &SessionState) -> Vec<String> = crate::rollout::tree_lines;

fn update_interval(loading: bool) -> Duration {
    if loading {
        INITIAL_UPDATE_INTERVAL
    } else {
        UPDATE_INTERVAL
    }
}

fn event_wait(elapsed: Duration, loading: bool) -> Duration {
    EVENT_POLL.min(update_interval(loading).saturating_sub(elapsed))
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, clap::ValueEnum)]
pub enum ColorMode {
    None,
    #[default]
    Auto,
}

#[derive(Clone, Copy)]
struct Palette {
    enabled: bool,
}

impl Palette {
    fn new(mode: ColorMode) -> Self {
        Self {
            enabled: mode == ColorMode::Auto,
        }
    }

    fn fg(self, color: Color) -> Style {
        if self.enabled {
            Style::default().fg(color)
        } else {
            Style::default()
        }
    }

    fn title(self) -> Style {
        self.fg(Color::Cyan).add_modifier(Modifier::BOLD)
    }
    fn metadata(self) -> Style {
        self.fg(Color::DarkGray)
    }
    fn role(self) -> Style {
        self.fg(Color::Blue)
    }
    fn good(self) -> Style {
        self.fg(Color::Green)
    }
    fn warning(self) -> Style {
        self.fg(Color::Yellow)
    }
    fn error(self) -> Style {
        self.fg(Color::Red).add_modifier(Modifier::BOLD)
    }
    fn selection(self) -> Style {
        Style::default().add_modifier(Modifier::REVERSED | Modifier::BOLD)
    }
}
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

#[derive(Default)]
struct PickerState {
    selected_session: Option<String>,
}

impl PickerState {
    fn synchronise(&mut self, groups: &[SessionGroup]) {
        if groups.is_empty() {
            self.selected_session = None;
        } else if !groups
            .iter()
            .any(|group| Some(&group.session_id) == self.selected_session.as_ref())
        {
            self.selected_session = Some(groups[0].session_id.clone());
        }
    }

    fn move_selection(&mut self, groups: &[SessionGroup], delta: isize) {
        self.synchronise(groups);
        let Some(selected) = &self.selected_session else {
            return;
        };
        let current = groups
            .iter()
            .position(|group| &group.session_id == selected)
            .unwrap_or(0);
        let next = current
            .saturating_add_signed(delta)
            .min(groups.len().saturating_sub(1));
        self.selected_session = Some(groups[next].session_id.clone());
    }
}

#[derive(PartialEq, Eq)]
enum TreeExit {
    Quit,
    Back,
}

enum PickerAction {
    Quit,
    Escape,
    Refresh,
    Open(String),
}

#[derive(Debug, PartialEq, Eq)]
enum BrowserMode {
    Picker,
    Tree(String),
    Exit,
}

impl BrowserMode {
    fn after_picker(action: PickerAction) -> Self {
        match action {
            PickerAction::Quit | PickerAction::Escape => Self::Exit,
            PickerAction::Refresh => Self::Picker,
            PickerAction::Open(session_id) => Self::Tree(session_id),
        }
    }

    fn after_tree(exit: TreeExit) -> Self {
        match exit {
            TreeExit::Quit => Self::Exit,
            TreeExit::Back => Self::Picker,
        }
    }
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

pub fn run(reader: &mut SelectedReader, color: ColorMode) -> Result<()> {
    let _guard = TerminalGuard::enter()?;
    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend).context("initialise terminal")?;
    terminal.clear().context("clear terminal")?;
    let palette = Palette::new(color);

    let result = event_loop(&mut terminal, reader, palette).map(|_| ());
    let cursor_result = terminal.show_cursor().context("restore terminal cursor");
    result.and(cursor_result)
}

pub fn run_browser(sessions_dir: PathBuf, repo_root: PathBuf, color: ColorMode) -> Result<()> {
    let _guard = TerminalGuard::enter()?;
    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend).context("initialise terminal")?;
    terminal.clear().context("clear terminal")?;
    let result = browser_loop(
        &mut terminal,
        &sessions_dir,
        &repo_root,
        Palette::new(color),
    );
    let cursor_result = terminal.show_cursor().context("restore terminal cursor");
    result.and(cursor_result)
}

fn event_loop(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    reader: &mut SelectedReader,
    palette: Palette,
) -> Result<TreeExit> {
    let mut ui = UiState::default();
    let mut dirty = true;
    let mut last_update = Instant::now();
    loop {
        let rows = flatten(&reader.group, &reader.state);
        ui.synchronise(&rows);
        if dirty {
            let loading = reader.is_loading();
            terminal
                .draw(|frame| {
                    draw(
                        frame,
                        &reader.group,
                        &reader.state,
                        &rows,
                        &ui,
                        loading,
                        palette,
                    )
                })
                .context("draw terminal UI")?;
            dirty = false;
        }

        let loading = reader.is_loading();
        if event::poll(event_wait(last_update.elapsed(), loading))
            .context("poll terminal events")?
        {
            match event::read().context("read terminal event")? {
                Event::Key(key) if key.kind == KeyEventKind::Press => match key.code {
                    KeyCode::Char('q') => return Ok(TreeExit::Quit),
                    KeyCode::Esc => return Ok(TreeExit::Back),
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

        let update_interval = update_interval(reader.is_loading());
        if last_update.elapsed() >= update_interval {
            let outcome = reader.poll().context("update selected session")?;
            last_update = Instant::now();
            note_poll(&mut ui, outcome);
            dirty = true;
        }
    }
}

fn browser_loop(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    sessions_dir: &Path,
    repo_root: &Path,
    palette: Palette,
) -> Result<()> {
    let mut picker = PickerState::default();
    loop {
        let discovery = rollout::discover(sessions_dir)
            .with_context(|| format!("discover sessions under {}", sessions_dir.display()))?;
        let Discovery {
            admitted,
            pending,
            health,
        } = discovery;
        let groups = rollout::group(admitted);
        picker.synchronise(&groups);
        match BrowserMode::after_picker(picker_loop(
            terminal,
            &groups,
            &health,
            &mut picker,
            palette,
        )?) {
            BrowserMode::Exit => return Ok(()),
            BrowserMode::Picker => continue,
            BrowserMode::Tree(session_id) => {
                let selected = rollout::select(&groups, Some(&session_id))?.clone();
                let mut reader = SelectedReader::new(
                    selected,
                    pending,
                    sessions_dir.to_owned(),
                    repo_root.to_owned(),
                )?;
                match BrowserMode::after_tree(event_loop(terminal, &mut reader, palette)?) {
                    BrowserMode::Exit => return Ok(()),
                    BrowserMode::Picker => continue,
                    BrowserMode::Tree(_) => unreachable!("tree exit cannot open another tree"),
                }
            }
        }
    }
}

fn picker_loop(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    groups: &[SessionGroup],
    health: &DataHealth,
    picker: &mut PickerState,
    palette: Palette,
) -> Result<PickerAction> {
    loop {
        terminal
            .draw(|frame| draw_picker(frame, groups, health, picker, palette))
            .context("draw session picker")?;
        match event::read().context("read terminal event")? {
            Event::Key(key) if key.kind == KeyEventKind::Press => match key.code {
                KeyCode::Char('q') => return Ok(PickerAction::Quit),
                KeyCode::Esc => return Ok(PickerAction::Escape),
                KeyCode::Up | KeyCode::Char('k') => picker.move_selection(groups, -1),
                KeyCode::Down | KeyCode::Char('j') => picker.move_selection(groups, 1),
                KeyCode::Enter => {
                    if let Some(session_id) = picker.selected_session.clone() {
                        return Ok(PickerAction::Open(session_id));
                    }
                }
                KeyCode::Char('r') => return Ok(PickerAction::Refresh),
                _ => {}
            },
            Event::Resize(_, _) => {}
            _ => {}
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

fn project_label(group: &SessionGroup) -> String {
    let root = &group.rollouts[group.root];
    if let Some(url) = root.repository_url.as_deref() {
        let trimmed = url.trim_end_matches('/').trim_end_matches(".git");
        if let Some(name) = trimmed
            .rsplit(['/', ':'])
            .next()
            .filter(|name| !name.is_empty())
        {
            return render_text(name);
        }
    }
    root.cwd
        .as_deref()
        .and_then(Path::file_name)
        .map(|name| render_text(&name.to_string_lossy()))
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "(unknown project)".into())
}

fn draw_picker(
    frame: &mut Frame,
    groups: &[SessionGroup],
    health: &DataHealth,
    picker: &PickerState,
    palette: Palette,
) {
    let area = frame.area();
    if area.width < 30 || area.height < 8 {
        frame.render_widget(
            Paragraph::new("agentop\nterminal too small\nq / Esc: quit")
                .style(palette.warning())
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .border_style(palette.title()),
                )
                .wrap(Wrap { trim: true }),
            area,
        );
        return;
    }
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2),
            Constraint::Min(3),
            Constraint::Length(5),
            Constraint::Length(1),
        ])
        .split(area);
    frame.render_widget(
        Paragraph::new(format!(
            "agentop · {} sessions · newest observed update first",
            groups.len()
        ))
        .style(palette.title())
        .block(
            Block::default()
                .borders(Borders::BOTTOM)
                .border_style(palette.title()),
        ),
        chunks[0],
    );
    let items = groups
        .iter()
        .map(|group| {
            ListItem::new(Line::from(vec![
                Span::styled(project_label(group), palette.title()),
                Span::styled(
                    format!("  {}  ", abbreviate(&group.session_id)),
                    palette.metadata(),
                ),
                Span::raw(format!("{} rollouts  ", group.rollouts.len())),
                Span::styled(
                    format!(
                        "updated {}",
                        rollout::session_updated_at(group).map_or_else(|| "unknown".into(), age)
                    ),
                    palette.metadata(),
                ),
            ]))
        })
        .collect::<Vec<_>>();
    let mut state = ListState::default();
    state.select(
        picker
            .selected_session
            .as_ref()
            .and_then(|id| groups.iter().position(|group| &group.session_id == id)),
    );
    frame.render_stateful_widget(
        List::new(items)
            .block(
                Block::default()
                    .title(" sessions ")
                    .title_style(palette.title())
                    .borders(Borders::ALL)
                    .border_style(palette.title()),
            )
            .highlight_style(palette.selection())
            .highlight_symbol("› "),
        chunks[1],
        &mut state,
    );
    let detail = picker.selected_session.as_ref().and_then(|id| groups.iter().find(|group| &group.session_id == id)).map_or_else(
        || "No session selected".into(),
        |group| {
            let root = &group.rollouts[group.root];
            format!("session {}\nproject path: {}\nrepository: {}\narchive health: unknown {} · malformed {} · oversized {}",
                render_text(&group.session_id),
                root.cwd.as_deref().map_or_else(|| "unknown".into(), |path| render_text(&path.display().to_string())),
                root.repository_url.as_deref().map_or_else(|| "unknown".into(), render_text),
                health.unknown_records, health.malformed_records, health.oversized_records)
        },
    );
    frame.render_widget(
        Paragraph::new(detail)
            .style(palette.metadata())
            .block(
                Block::default()
                    .title(" details ")
                    .title_style(palette.title())
                    .borders(Borders::ALL)
                    .border_style(palette.title()),
            )
            .wrap(Wrap { trim: false }),
        chunks[2],
    );
    frame.render_widget(
        Paragraph::new("↑/↓ or j/k select   Enter open   r refresh   q/Esc quit"),
        chunks[3],
    );
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
    loading: bool,
    palette: Palette,
) {
    let area = frame.area();
    if area.width < 30 || area.height < 8 {
        frame.render_widget(
            Paragraph::new("agentop\nterminal too small\nq / Esc: quit")
                .style(palette.warning())
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .border_style(palette.title()),
                )
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
        if loading {
            " · loading history…"
        } else if ui.catching_up {
            " · catching up…"
        } else {
            ""
        }
    );
    frame.render_widget(
        Paragraph::new(header)
            .style(if loading || ui.catching_up {
                palette.warning()
            } else {
                palette.title()
            })
            .block(
                Block::default()
                    .borders(Borders::BOTTOM)
                    .border_style(palette.title()),
            ),
        chunks[0],
    );

    let items = rows
        .iter()
        .map(|row| {
            let agent = &state.agents[&row.thread_id];
            ListItem::new(tree_line(row, agent, palette))
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
            .block(
                Block::default()
                    .title(" agents ")
                    .title_style(palette.title())
                    .borders(Borders::ALL)
                    .border_style(palette.title()),
            )
            .highlight_style(palette.selection())
            .highlight_symbol("› "),
        chunks[1],
        &mut list_state,
    );

    let selected = ui
        .selected_thread
        .as_ref()
        .and_then(|id| state.agents.get(id));
    frame.render_widget(
        Paragraph::new(detail_lines(selected, state, group, palette))
            .block(
                Block::default()
                    .title(" details ")
                    .title_style(palette.title())
                    .borders(Borders::ALL)
                    .border_style(palette.title()),
            )
            .wrap(Wrap { trim: false }),
        chunks[2],
    );
    frame.render_widget(
        Paragraph::new("↑/↓ or j/k select   r rescan   q/Esc quit"),
        chunks[3],
    );
}

fn tree_line(row: &TreeRow, agent: &AgentState, palette: Palette) -> Line<'static> {
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
    let status = lifecycle_status(agent);
    let status_style = match agent.latest_turn.status {
        TurnStatus::Completed => palette.good(),
        TurnStatus::Interrupted | TurnStatus::Errored => palette.error(),
        TurnStatus::Pending => palette.warning(),
        TurnStatus::Running => palette.title(),
    };
    let age = agent
        .last_activity_at
        .map_or_else(|| "no activity".into(), age);
    Line::from(vec![
        Span::raw(format!("{}{}  ", "  ".repeat(row.depth), label)),
        Span::styled(format!("{role}  "), palette.role()),
        Span::styled(format!("{status}  "), status_style),
        Span::styled(age, palette.metadata()),
    ])
}

fn detail_lines<'a>(
    agent: Option<&'a AgentState>,
    state: &SessionState,
    group: &SessionGroup,
    palette: Palette,
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
    let health_style = if health.malformed_records > 0 || health.oversized_records > 0 {
        palette.error()
    } else if health.unknown_records > 0 || health.unknown_events > 0 {
        palette.warning()
    } else {
        palette.good()
    };
    let schema_style = if agent.schema_catalogued {
        palette.good()
    } else {
        palette.warning()
    };
    let mut lines = vec![
        Line::from(vec![
            Span::styled("agent: ", palette.title()),
            Span::raw(format!(
                "{} · thread {} · parent {}",
                render_text(agent.agent_path.as_deref().unwrap_or("(unnamed)")),
                render_text(&agent.thread_id),
                render_text(agent.parent_thread_id.as_deref().unwrap_or("none"))
            )),
        ]),
        Line::from(vec![
            Span::styled("metadata: ", palette.metadata()),
            Span::styled(
                format!(
                    "role {} · nickname {} · Codex {} · schema {} · compatibility {}",
                    render_text(agent.agent_role.as_deref().unwrap_or("unknown")),
                    render_text(agent.agent_nickname.as_deref().unwrap_or("unknown")),
                    render_text(&agent.cli_version),
                    schema,
                    coverage(agent.coverage)
                ),
                schema_style,
            ),
        ]),
        Line::from(vec![
            Span::styled("lifecycle: ", palette.title()),
            Span::raw(format!(
                "{} · turn {} · depth {} · started {} · completed {} · last activity {}",
                lifecycle_status(agent),
                render_text(agent.latest_turn.turn_id.as_deref().unwrap_or("unknown")),
                metadata
                    .and_then(|metadata| metadata.depth)
                    .map_or_else(|| "unknown".into(), |depth| depth.to_string()),
                timestamp(agent.latest_turn.started_at),
                timestamp(agent.latest_turn.completed_at),
                timestamp(agent.last_activity_at)
            )),
        ]),
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
        Line::from(Span::styled(
            format!(
                "health: unknown records {} · unknown events {} · malformed {} · oversized {}",
                health.unknown_records,
                health.unknown_events,
                health.malformed_records,
                health.oversized_records
            ),
            health_style,
        )),
    ];
    if let Some(diagnostic) = health.recent_diagnostics.back() {
        lines.push(Line::from(Span::styled(
            format!(
                "latest diagnostic: {}:{} ordinal {} · {} {}",
                render_text(&diagnostic.rollout_path.display().to_string()),
                diagnostic.byte_offset,
                diagnostic
                    .ordinal
                    .map_or_else(|| "unknown".into(), |value| value.to_string()),
                render_text(&diagnostic.kind),
                render_text(diagnostic.detail.as_deref().unwrap_or(""))
            ),
            palette.warning(),
        )));
    }
    lines
}

fn lifecycle_status(agent: &AgentState) -> &'static str {
    if agent.is_waiting_on_agent() {
        "WAITING ON AGENT ↓"
    } else {
        status(agent.latest_turn.status)
    }
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

    fn line_text(line: &Line<'_>) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect()
    }

    #[test]
    fn loading_update_interval_and_event_wait_are_deadline_bounded() {
        assert_eq!(update_interval(true), Duration::from_millis(50));
        assert_eq!(update_interval(false), Duration::from_secs(1));
        assert_eq!(event_wait(Duration::ZERO, true), Duration::from_millis(50));
        assert_eq!(
            event_wait(Duration::from_millis(20), true),
            Duration::from_millis(30)
        );
        assert_eq!(event_wait(Duration::from_millis(50), true), Duration::ZERO);
        assert_eq!(
            event_wait(Duration::ZERO, false),
            Duration::from_millis(250)
        );
        assert_eq!(
            event_wait(Duration::from_millis(900), false),
            Duration::from_millis(100)
        );
        assert_eq!(event_wait(Duration::from_secs(1), false), Duration::ZERO);
    }
    fn rendered_agent(
        agent: &AgentState,
        group: &SessionGroup,
        state: &SessionState,
    ) -> (String, String) {
        let row = TreeRow {
            thread_id: agent.thread_id.clone(),
            depth: 0,
        };
        let palette = Palette::new(ColorMode::None);
        let tree = line_text(&tree_line(&row, agent, palette));
        let detail = detail_lines(Some(agent), state, group, palette)
            .iter()
            .map(line_text)
            .collect::<Vec<_>>()
            .join("\n");
        (tree, detail)
    }

    #[test]
    fn waiting_status_renders_in_tree_and_detail_with_exact_precedence() {
        let (group, state) = fixture();
        let mut agent = state.agents["root"].clone();
        agent.latest_turn.status = TurnStatus::Running;
        agent.last_message = Some("text says wait_agent only".into());
        let (tree, detail) = rendered_agent(&agent, &group, &state);
        assert!(!tree.contains("WAITING ON AGENT"));
        assert!(!detail.contains("WAITING ON AGENT"));

        agent.in_flight_calls.insert(
            "wait".into(),
            crate::model::InFlightCall {
                tool_name: "wait_agent".into(),
                summary: "wait_agent".into(),
                started_at: None,
                ordinal: Some(1),
                sequence: 1,
            },
        );
        let (tree, detail) = rendered_agent(&agent, &group, &state);
        assert!(tree.contains("WAITING ON AGENT ↓"));
        assert!(detail.contains("lifecycle: WAITING ON AGENT ↓"));
        assert!(detail.contains("activity: wait_agent"));

        agent.in_flight_calls.insert(
            "newer".into(),
            crate::model::InFlightCall {
                tool_name: "exec".into(),
                summary: "running exec".into(),
                started_at: None,
                ordinal: Some(2),
                sequence: 2,
            },
        );
        let (tree, detail) = rendered_agent(&agent, &group, &state);
        assert!(!tree.contains("WAITING ON AGENT"));
        assert!(detail.contains("activity: running exec"));
        agent.in_flight_calls.remove("newer");
        agent.in_flight_calls.remove("wait");
        let (tree, detail) = rendered_agent(&agent, &group, &state);
        assert!(!tree.contains("WAITING ON AGENT"));
        assert!(!detail.contains("WAITING ON AGENT"));
    }

    #[test]
    fn partial_tree_loading_indicator_and_selection_render_in_none_mode() {
        let (group, mut state) = fixture();
        state.agents.remove("child");
        let mut ui = UiState::default();
        let rows = flatten(&group, &state);
        ui.synchronise(&rows);
        assert_eq!(ui.selected_thread.as_deref(), Some("root"));
        let backend = TestBackend::new(100, 28);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                draw(
                    frame,
                    &group,
                    &state,
                    &rows,
                    &ui,
                    true,
                    Palette::new(ColorMode::None),
                )
            })
            .unwrap();
        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(rendered.contains("loading history…"));
        assert!(rendered.contains("root"));
        assert!(!rendered.contains("child"));
        assert!(terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .all(|cell| cell.fg == Color::Reset && cell.bg == Color::Reset));

        let (_, complete) = fixture();
        let rows = flatten(&group, &complete);
        ui.synchronise(&rows);
        assert_eq!(ui.selected_thread.as_deref(), Some("root"));
        terminal
            .draw(|frame| {
                draw(
                    frame,
                    &group,
                    &complete,
                    &rows,
                    &ui,
                    false,
                    Palette::new(ColorMode::None),
                )
            })
            .unwrap();
        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(!rendered.contains("loading history…"));
        assert!(rendered.contains("child"));
    }

    fn fixture() -> (SessionGroup, SessionState) {
        let root = RolloutMetadata {
            path: PathBuf::from("root"),
            session_id: "session".into(),
            thread_id: "root".into(),
            parent_thread_id: None,
            cwd: Some(PathBuf::from("/project")),
            repository_url: None,
            timestamp: None,
            modified_at: None,
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
            repository_url: None,
            timestamp: None,
            modified_at: None,
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
                    false,
                    Palette::new(ColorMode::Auto),
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
    fn picker_keeps_session_selection_and_sanitises_project_information() {
        let (mut first, _) = fixture();
        first.session_id = "first-session-identifier".into();
        first.rollouts[first.root].session_id = first.session_id.clone();
        first.rollouts[first.root].repository_url =
            Some("git@host:organisation/repo\u{1b}[31m.git".into());
        let mut second = first.clone();
        second.session_id = "second-session-identifier".into();
        second.rollouts[second.root].session_id = second.session_id.clone();
        second.rollouts[second.root].repository_url = None;
        second.rollouts[second.root].cwd = Some(PathBuf::from("/work/fallback"));
        let groups = vec![first, second];
        let mut picker = PickerState::default();
        picker.synchronise(&groups);
        picker.move_selection(&groups, 1);
        assert_eq!(
            picker.selected_session.as_deref(),
            Some("second-session-identifier")
        );
        let reordered = vec![groups[1].clone(), groups[0].clone()];
        picker.synchronise(&reordered);
        assert_eq!(
            picker.selected_session.as_deref(),
            Some("second-session-identifier")
        );
        assert_eq!(project_label(&reordered[0]), "fallback");
        assert_eq!(project_label(&reordered[1]), "repo[31m");

        let mut terminal = Terminal::new(TestBackend::new(80, 14)).unwrap();
        terminal
            .draw(|frame| {
                draw_picker(
                    frame,
                    &reordered,
                    &DataHealth::default(),
                    &picker,
                    Palette::new(ColorMode::Auto),
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
        assert!(text.contains("fallback"));
        assert!(text.contains("second-s"));
        assert!(!text.contains('\u{1b}'));

        let mut tiny = Terminal::new(TestBackend::new(20, 4)).unwrap();
        tiny.draw(|frame| {
            draw_picker(
                frame,
                &reordered,
                &DataHealth::default(),
                &picker,
                Palette::new(ColorMode::Auto),
            )
        })
        .unwrap();
        assert!(tiny
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>()
            .contains("terminal too small"));
    }
    #[test]
    fn browser_mode_transitions_are_deterministic() {
        assert_eq!(
            BrowserMode::after_picker(PickerAction::Open("session-id".into())),
            BrowserMode::Tree("session-id".into())
        );
        assert_eq!(BrowserMode::after_tree(TreeExit::Back), BrowserMode::Picker);
        assert_eq!(
            BrowserMode::after_picker(PickerAction::Escape),
            BrowserMode::Exit
        );
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
                    false,
                    Palette::new(ColorMode::Auto),
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

    fn assert_reset_colours(terminal: &Terminal<TestBackend>) {
        assert!(terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .all(|cell| { cell.fg == Color::Reset && cell.bg == Color::Reset }));
    }

    #[test]
    fn auto_uses_semantic_colours_in_picker_and_tree() {
        let (group, state) = fixture();
        let groups = vec![group.clone()];
        let mut picker = PickerState::default();
        picker.synchronise(&groups);
        let palette = Palette::new(ColorMode::Auto);

        let mut picker_terminal = Terminal::new(TestBackend::new(80, 14)).unwrap();
        picker_terminal
            .draw(|frame| draw_picker(frame, &groups, &DataHealth::default(), &picker, palette))
            .unwrap();
        let picker_cells = picker_terminal.backend().buffer().content();
        for expected in [Color::Cyan, Color::DarkGray] {
            assert!(
                picker_cells.iter().any(|cell| cell.fg == expected),
                "picker should render semantic foreground {expected:?}"
            );
        }

        let rows = flatten(&group, &state);
        let mut ui = UiState::default();
        ui.synchronise(&rows);
        let mut tree_terminal = Terminal::new(TestBackend::new(100, 20)).unwrap();
        tree_terminal
            .draw(|frame| draw(frame, &group, &state, &rows, &ui, false, palette))
            .unwrap();
        let tree_cells = tree_terminal.backend().buffer().content();
        for expected in [Color::Cyan, Color::DarkGray, Color::Blue, Color::Yellow] {
            assert!(
                tree_cells.iter().any(|cell| cell.fg == expected),
                "tree should render semantic foreground {expected:?}"
            );
        }
    }

    #[test]
    fn none_resets_all_picker_tree_and_tiny_colours_with_selection_cues() {
        let (group, state) = fixture();
        let groups = vec![group.clone()];
        let mut picker = PickerState::default();
        picker.synchronise(&groups);
        let palette = Palette::new(ColorMode::None);

        let mut picker_terminal = Terminal::new(TestBackend::new(80, 14)).unwrap();
        picker_terminal
            .draw(|frame| draw_picker(frame, &groups, &DataHealth::default(), &picker, palette))
            .unwrap();
        assert_reset_colours(&picker_terminal);
        assert!(picker_terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .any(|cell| cell.symbol() == "›"));
        assert!(picker_terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .any(|cell| cell.modifier.contains(Modifier::REVERSED)));

        let rows = flatten(&group, &state);
        let mut ui = UiState::default();
        ui.synchronise(&rows);
        let mut tree_terminal = Terminal::new(TestBackend::new(100, 20)).unwrap();
        tree_terminal
            .draw(|frame| draw(frame, &group, &state, &rows, &ui, false, palette))
            .unwrap();
        assert_reset_colours(&tree_terminal);
        assert!(tree_terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .any(|cell| cell.symbol() == "›"));

        let mut tiny_picker = Terminal::new(TestBackend::new(20, 4)).unwrap();
        tiny_picker
            .draw(|frame| draw_picker(frame, &groups, &DataHealth::default(), &picker, palette))
            .unwrap();
        assert_reset_colours(&tiny_picker);
        let mut tiny_tree = Terminal::new(TestBackend::new(20, 4)).unwrap();
        tiny_tree
            .draw(|frame| draw(frame, &group, &state, &rows, &ui, false, palette))
            .unwrap();
        assert_reset_colours(&tiny_tree);
    }
}
