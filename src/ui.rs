use crate::model::{
    AgentInteraction, AgentState, CoverageLevel, DataHealth, InteractionKind, SessionState,
    StaleEvidence, ToolInteractionState, TurnStatus, CONTENT_TEXT_LIMIT,
};
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
const INITIAL_LOADING_WORK_WINDOW: Duration = Duration::from_millis(100);
const RENDER_TEXT_LIMIT: usize = 256;
const DETAIL_RENDER_TEXT_LIMIT: usize = CONTENT_TEXT_LIMIT;
const _: fn(&SessionGroup, &SessionState) -> Vec<String> = crate::rollout::tree_lines;

fn list_panel_height(total_height: u16, row_count: usize, max_percent: u16) -> u16 {
    const FIXED_HEIGHT: u16 = 3;
    const MIN_DETAIL_HEIGHT: u16 = 3;
    let space_cap = total_height.saturating_sub(FIXED_HEIGHT + MIN_DETAIL_HEIGHT);
    let percentage_cap = total_height.saturating_mul(max_percent) / 100;
    let cap = space_cap.min(percentage_cap.max(1));
    let desired = u16::try_from(row_count)
        .unwrap_or(u16::MAX)
        .saturating_add(2);
    desired.min(cap).max(1)
}

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

fn loading_work_window_open(loading: bool, elapsed: Duration) -> bool {
    loading && elapsed < INITIAL_LOADING_WORK_WINDOW
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, clap::ValueEnum)]
pub enum ColorMode {
    None,
    #[default]
    Auto,
}

const ORANGE: Color = Color::Rgb(255, 165, 0);
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
    fn model(self) -> Style {
        self.fg(Color::LightMagenta)
    }

    fn reasoning_effort(self, effort: &str) -> Style {
        let color = match effort {
            "none" | "minimal" => Color::LightGreen,
            "low" => Color::Green,
            "medium" => Color::Yellow,
            "high" => ORANGE,
            "xhigh" | "max" | "ultra" => Color::Red,
            _ => Color::LightYellow,
        };
        self.fg(color)
    }

    fn neutral(self) -> Style {
        self.fg(Color::Gray)
    }

    fn waiting(self) -> Style {
        self.fg(ORANGE)
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
    hide_completed: bool,
    history: Option<HistoryState>,
    catching_up: bool,
    last_change: Option<Instant>,
}

struct HistoryState {
    thread_id: String,
    selected_sequence: Option<u64>,
    follow_latest: bool,
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

    fn toggle_completed(&mut self) {
        self.hide_completed = !self.hide_completed;
    }

    fn open_history(&mut self, state: &SessionState) {
        let Some(thread_id) = self.selected_thread.clone() else {
            return;
        };
        let selected_sequence = state
            .agents
            .get(&thread_id)
            .and_then(|agent| agent.interactions.back())
            .map(|interaction| interaction.sequence);
        self.history = Some(HistoryState {
            thread_id,
            selected_sequence,
            follow_latest: true,
        });
    }

    fn close_history(&mut self) -> bool {
        self.history.take().is_some()
    }

    fn synchronise_history(&mut self, state: &SessionState) {
        let Some(history) = self.history.as_mut() else {
            return;
        };
        let Some(agent) = state.agents.get(&history.thread_id) else {
            self.history = None;
            return;
        };
        if agent.interactions.is_empty() {
            history.selected_sequence = None;
        } else if history.follow_latest {
            history.selected_sequence = agent
                .interactions
                .back()
                .map(|interaction| interaction.sequence);
        } else if !agent
            .interactions
            .iter()
            .any(|interaction| Some(interaction.sequence) == history.selected_sequence)
        {
            history.selected_sequence = agent
                .interactions
                .front()
                .map(|interaction| interaction.sequence);
        }
    }

    fn move_history(&mut self, state: &SessionState, delta: isize) {
        self.synchronise_history(state);
        let Some(history) = self.history.as_mut() else {
            return;
        };
        let Some(agent) = state.agents.get(&history.thread_id) else {
            return;
        };
        let Some(current) = agent
            .interactions
            .iter()
            .position(|interaction| Some(interaction.sequence) == history.selected_sequence)
        else {
            return;
        };
        let next = current
            .saturating_add_signed(delta)
            .min(agent.interactions.len().saturating_sub(1));
        history.selected_sequence = Some(agent.interactions[next].sequence);
        history.follow_latest = next + 1 == agent.interactions.len();
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

pub fn run_browser(sessions_dir: PathBuf, catalogue_dir: PathBuf, color: ColorMode) -> Result<()> {
    let _guard = TerminalGuard::enter()?;
    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend).context("initialise terminal")?;
    terminal.clear().context("clear terminal")?;
    let result = browser_loop(
        &mut terminal,
        &sessions_dir,
        &catalogue_dir,
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
    let mut last_update_started = Instant::now();
    loop {
        let rows = flatten(&reader.group, &reader.state, ui.hide_completed);
        ui.synchronise(&rows);
        ui.synchronise_history(&reader.state);
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
        if event::poll(event_wait(last_update_started.elapsed(), loading))
            .context("poll terminal events")?
        {
            match event::read().context("read terminal event")? {
                Event::Key(key) if key.kind == KeyEventKind::Press => match key.code {
                    KeyCode::Char('q') => return Ok(TreeExit::Quit),
                    KeyCode::Esc => {
                        if ui.close_history() {
                            dirty = true;
                        } else {
                            return Ok(TreeExit::Back);
                        }
                    }
                    KeyCode::Up | KeyCode::Char('k') => {
                        if ui.history.is_some() {
                            ui.move_history(&reader.state, -1);
                        } else {
                            ui.move_selection(&rows, -1);
                        }
                        dirty = true;
                    }
                    KeyCode::Down | KeyCode::Char('j') => {
                        if ui.history.is_some() {
                            ui.move_history(&reader.state, 1);
                        } else {
                            ui.move_selection(&rows, 1);
                        }
                        dirty = true;
                    }
                    KeyCode::Enter => {
                        if ui.history.is_none() {
                            ui.open_history(&reader.state);
                            dirty = true;
                        }
                    }
                    KeyCode::Char('r') => {
                        last_update_started = Instant::now();
                        let outcome = reader.poll().context("rescan selected session")?;
                        note_poll(&mut ui, outcome);
                        dirty = true;
                    }
                    KeyCode::Char('h') if ui.history.is_none() => {
                        ui.toggle_completed();
                        dirty = true;
                    }
                    _ => {}
                },
                Event::Resize(_, _) => dirty = true,
                _ => {}
            }
        }

        let loading = reader.is_loading();
        let update_interval = update_interval(loading);
        if last_update_started.elapsed() >= update_interval {
            let work_started = Instant::now();
            last_update_started = work_started;
            loop {
                let outcome = reader.poll().context("update selected session")?;
                note_poll(&mut ui, outcome);
                dirty = true;

                if !loading_work_window_open(reader.is_loading(), work_started.elapsed()) {
                    break;
                }
                if event::poll(Duration::ZERO)
                    .context("poll terminal events during initial loading")?
                {
                    break;
                }
            }
        }
    }
}

fn browser_loop(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    sessions_dir: &Path,
    catalogue_dir: &Path,
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
                    catalogue_dir.to_owned(),
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

fn agent_label(agent: &AgentState) -> &str {
    if agent.parent_thread_id.is_none() {
        return agent.agent_nickname.as_deref().unwrap_or(&agent.thread_id);
    }
    agent
        .agent_path
        .as_deref()
        .and_then(|path| path.rsplit('/').find(|segment| !segment.is_empty()))
        .or(agent.agent_nickname.as_deref())
        .unwrap_or(&agent.thread_id)
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
fn flatten(group: &SessionGroup, state: &SessionState, hide_completed: bool) -> Vec<TreeRow> {
    fn local_activity(agent: &AgentState) -> Option<DateTime<Utc>> {
        agent.last_activity_at.or(agent.latest_turn.started_at)
    }

    fn label(id: &str, state: &SessionState) -> String {
        agent_label(&state.agents[id]).to_owned()
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
        hide_completed: bool,
        seen: &mut HashSet<String>,
        rows: &mut Vec<TreeRow>,
    ) {
        if !seen.insert(id.to_owned()) || !state.agents.contains_key(id) {
            return;
        }
        let hidden = hide_completed && state.agents[id].latest_turn.status == TurnStatus::Completed;
        if !hidden {
            rows.push(TreeRow {
                thread_id: id.to_owned(),
                depth,
            });
        }
        let child_depth = if hidden { depth } else { depth + 1 };
        if let Some(ids) = children.get(id) {
            for child in ids {
                visit(
                    child,
                    child_depth,
                    children,
                    state,
                    hide_completed,
                    seen,
                    rows,
                );
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
    visit(
        root,
        0,
        &children,
        state,
        hide_completed,
        &mut HashSet::new(),
        &mut rows,
    );
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
            Paragraph::new("agentop\nterminal too small\nq quit · Esc back")
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
    if let Some(history) = ui.history.as_ref() {
        draw_history(frame, state, history, loading, ui.catching_up, palette);
        return;
    }

    let agent_panel_height = list_panel_height(area.height, rows.len(), 42);
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2),
            Constraint::Length(agent_panel_height),
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
            let stale_evidence = stale_evidence_for_display(state, agent, loading);
            ListItem::new(tree_line(row, agent, stale_evidence, palette))
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
    let selected_stale_evidence =
        selected.and_then(|agent| stale_evidence_for_display(state, agent, loading));
    frame.render_widget(
        Paragraph::new(detail_lines(
            selected,
            group,
            selected_stale_evidence,
            palette,
        ))
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
        Paragraph::new(format!(
            "↑/↓ or j/k select   Enter interactions   r rescan   h {} completed   Esc back   q quit",
            if ui.hide_completed { "show" } else { "hide" }
        )),
        chunks[3],
    );
}

fn interaction_kind(kind: InteractionKind) -> &'static str {
    match kind {
        InteractionKind::Lifecycle => "lifecycle",
        InteractionKind::Context => "context",
        InteractionKind::Tool => "tool",
        InteractionKind::Reasoning => "reasoning",
        InteractionKind::Message => "message",
        InteractionKind::Communication => "communication",
    }
}

fn interaction_style(kind: InteractionKind, palette: Palette) -> Style {
    match kind {
        InteractionKind::Lifecycle => palette.title(),
        InteractionKind::Context => palette.warning(),
        InteractionKind::Tool => palette.warning(),
        InteractionKind::Reasoning => palette.model(),
        InteractionKind::Message => palette.good(),
        InteractionKind::Communication => palette.role(),
    }
}

fn elapsed(start: DateTime<Utc>, end: DateTime<Utc>) -> String {
    let milliseconds = end.signed_duration_since(start).num_milliseconds().max(0);
    if milliseconds < 1_000 {
        format!("{milliseconds}ms")
    } else {
        let seconds = milliseconds / 1_000;
        if seconds < 60 {
            format!("{}.{:01}s", seconds, (milliseconds % 1_000) / 100)
        } else if seconds < 3_600 {
            format!("{}m {}s", seconds / 60, seconds % 60)
        } else if seconds < 86_400 {
            format!("{}h {}m", seconds / 3_600, (seconds % 3_600) / 60)
        } else {
            format!("{}d {}h", seconds / 86_400, (seconds % 86_400) / 3_600)
        }
    }
}

fn tool_state_text(interaction: &AgentInteraction, now: DateTime<Utc>) -> Option<String> {
    let state = interaction.tool_state?;
    let duration = match state {
        ToolInteractionState::Open => interaction.timestamp.map(|started| elapsed(started, now)),
        ToolInteractionState::Yielded
        | ToolInteractionState::Returned
        | ToolInteractionState::EndedWithoutReturn => interaction
            .timestamp
            .zip(interaction.finished_at)
            .map(|(started, finished)| elapsed(started, finished)),
    };
    Some(match (state, duration) {
        (ToolInteractionState::Open, Some(duration)) => format!("open for {duration}"),
        (ToolInteractionState::Open, None) => "open".into(),
        (ToolInteractionState::Yielded, Some(duration)) => {
            format!("yielded after {duration}")
        }
        (ToolInteractionState::Yielded, None) => "yielded".into(),
        (ToolInteractionState::Returned, Some(duration)) => {
            format!("returned after {duration}")
        }
        (ToolInteractionState::Returned, None) => "returned".into(),
        (ToolInteractionState::EndedWithoutReturn, Some(duration)) => {
            format!("ended without return after {duration}")
        }
        (ToolInteractionState::EndedWithoutReturn, None) => "ended without return".into(),
    })
}

fn tool_state_style(state: ToolInteractionState, palette: Palette) -> Style {
    match state {
        ToolInteractionState::Open => palette.warning(),
        ToolInteractionState::Yielded => palette.metadata(),
        ToolInteractionState::Returned => palette.good(),
        ToolInteractionState::EndedWithoutReturn => palette.error(),
    }
}

fn interaction_line(
    interaction: &AgentInteraction,
    now: DateTime<Utc>,
    palette: Palette,
) -> Line<'static> {
    let mut spans = Vec::new();
    if let Some(timestamp) = interaction.timestamp {
        spans.push(Span::styled(
            format!("{}  ", utc_date_time(timestamp)),
            palette.metadata(),
        ));
    } else if let Some(ordinal) = interaction.ordinal {
        spans.push(Span::styled(
            format!("ordinal {ordinal}  "),
            palette.metadata(),
        ));
    }
    spans.push(Span::styled(
        format!("{}: ", interaction_kind(interaction.kind)),
        interaction_style(interaction.kind, palette),
    ));
    spans.push(Span::raw(render_text(&interaction.summary)));
    if let (Some(state), Some(state_text)) =
        (interaction.tool_state, tool_state_text(interaction, now))
    {
        spans.push(Span::raw(" · "));
        spans.push(Span::styled(state_text, tool_state_style(state, palette)));
    }
    Line::from(spans)
}

fn interaction_detail_lines(
    interaction: Option<(&AgentInteraction, usize)>,
    total: usize,
    now: DateTime<Utc>,
    palette: Palette,
) -> Vec<Line<'static>> {
    let Some((interaction, index)) = interaction else {
        return vec![Line::from("No interactions retained for this agent")];
    };
    let mut lines = vec![
        labelled_line("position", format!("{}/{}", index + 1, total), palette),
        labelled_line("type", interaction_kind(interaction.kind).into(), palette),
    ];
    if interaction.kind == InteractionKind::Tool {
        lines.push(labelled_line(
            "tool",
            render_detail_text(&interaction.summary),
            palette,
        ));
        if let Some(state_text) = tool_state_text(interaction, now) {
            lines.push(labelled_line("state", state_text, palette));
        }
        if let Some(timestamp) = interaction.timestamp {
            lines.push(labelled_line("started", timestamp.to_rfc3339(), palette));
        }
        if let Some(finished_at) = interaction.finished_at {
            let label = match interaction.tool_state {
                Some(ToolInteractionState::Yielded) => "yielded",
                Some(ToolInteractionState::EndedWithoutReturn) => "ended",
                Some(ToolInteractionState::Open | ToolInteractionState::Returned) | None => {
                    "returned"
                }
            };
            lines.push(labelled_line(label, finished_at.to_rfc3339(), palette));
        }
    } else {
        if let Some(timestamp) = interaction.timestamp {
            lines.push(labelled_line("timestamp", timestamp.to_rfc3339(), palette));
        }
        lines.push(labelled_line(
            "content",
            render_detail_text(&interaction.summary),
            palette,
        ));
    }
    if let Some(ordinal) = interaction.ordinal {
        lines.push(labelled_line("ordinal", ordinal.to_string(), palette));
    }
    lines
}

fn history_header(
    agent: &AgentState,
    retained: usize,
    progress: &str,
    stale_evidence: Option<StaleEvidence>,
    palette: Palette,
) -> Line<'static> {
    let mut spans = vec![
        Span::styled("agentop · interactions · ", palette.title()),
        Span::styled(render_text(agent_label(agent)), palette.title()),
    ];
    let role = agent
        .agent_role
        .as_deref()
        .or_else(|| agent.parent_thread_id.is_none().then_some("orchestrator"));
    for (value, style) in [
        (role, palette.role()),
        (agent.model.as_deref(), palette.model()),
    ] {
        if let Some(value) = value {
            spans.push(Span::raw(" · "));
            spans.push(Span::styled(render_text(value), style));
        }
    }
    if let Some(effort) = agent.reasoning_effort.as_deref() {
        spans.push(Span::raw(" · "));
        spans.push(Span::styled(
            render_text(effort),
            palette.reasoning_effort(effort),
        ));
    }
    spans.push(Span::raw(" · "));
    spans.push(Span::styled(
        lifecycle_status(agent, stale_evidence),
        lifecycle_style(agent, stale_evidence, palette),
    ));
    if let Some(last_activity_at) = agent.last_activity_at {
        spans.push(Span::styled(
            format!(" · activity {}", age(last_activity_at)),
            palette.metadata(),
        ));
    }
    spans.push(Span::styled(
        format!(" · {retained} retained"),
        palette.metadata(),
    ));
    if !progress.is_empty() {
        spans.push(Span::styled(progress.to_owned(), palette.warning()));
    }
    Line::from(spans)
}

fn draw_history(
    frame: &mut Frame,
    state: &SessionState,
    history: &HistoryState,
    loading: bool,
    catching_up: bool,
    palette: Palette,
) {
    let area = frame.area();
    let agent = state
        .agents
        .get(&history.thread_id)
        .expect("history view references a selected agent");
    let interaction_panel_height = list_panel_height(area.height, agent.interactions.len(), 55);
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2),
            Constraint::Length(interaction_panel_height),
            Constraint::Min(3),
            Constraint::Length(1),
        ])
        .split(area);
    let progress = if loading {
        " · loading history…"
    } else if catching_up {
        " · catching up…"
    } else {
        ""
    };
    let stale_evidence = stale_evidence_for_display(state, agent, loading);
    frame.render_widget(
        Paragraph::new(history_header(
            agent,
            agent.interactions.len(),
            progress,
            stale_evidence,
            palette,
        ))
        .block(
            Block::default()
                .borders(Borders::BOTTOM)
                .border_style(palette.title()),
        ),
        chunks[0],
    );

    let now = Utc::now();
    let items = agent
        .interactions
        .iter()
        .map(|interaction| ListItem::new(interaction_line(interaction, now, palette)))
        .collect::<Vec<_>>();
    let selected_index = history.selected_sequence.and_then(|sequence| {
        agent
            .interactions
            .iter()
            .position(|interaction| interaction.sequence == sequence)
    });
    let mut list_state = ListState::default();
    list_state.select(selected_index);
    frame.render_stateful_widget(
        List::new(items)
            .block(
                Block::default()
                    .title(" interactions · UTC · oldest → newest ")
                    .title_style(palette.title())
                    .borders(Borders::ALL)
                    .border_style(palette.title()),
            )
            .highlight_style(palette.selection())
            .highlight_symbol("› "),
        chunks[1],
        &mut list_state,
    );

    let selected = selected_index.and_then(|index| {
        agent
            .interactions
            .get(index)
            .map(|interaction| (interaction, index))
    });
    frame.render_widget(
        Paragraph::new(interaction_detail_lines(
            selected,
            agent.interactions.len(),
            now,
            palette,
        ))
        .block(
            Block::default()
                .title(" selected interaction ")
                .title_style(palette.title())
                .borders(Borders::ALL)
                .border_style(palette.title()),
        )
        .wrap(Wrap { trim: false }),
        chunks[2],
    );
    frame.render_widget(
        Paragraph::new("↑/k older   ↓/j newer   r rescan   Esc back   q quit"),
        chunks[3],
    );
}

fn exact_tokens(value: u64) -> String {
    let digits = value.to_string();
    let mut formatted = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            formatted.push(',');
        }
        formatted.push(digit);
    }
    formatted
}

fn context_pressure_style(percent: u16, palette: Palette) -> Style {
    if percent >= 85 {
        palette.error()
    } else if percent >= 70 {
        palette.warning()
    } else {
        palette.metadata()
    }
}

fn context_change(current: u64, previous: u64) -> String {
    if current > previous {
        format!(
            "+{} since previous observation",
            exact_tokens(current - previous)
        )
    } else if current < previous {
        format!(
            "-{} since previous observation",
            exact_tokens(previous - current)
        )
    } else {
        "unchanged since previous observation".into()
    }
}

fn context_usage_is_actionable(agent: &AgentState, stale_evidence: Option<StaleEvidence>) -> bool {
    stale_evidence.is_none() && agent.latest_turn.status != TurnStatus::Completed
}

fn tree_line(
    row: &TreeRow,
    agent: &AgentState,
    stale_evidence: Option<StaleEvidence>,
    palette: Palette,
) -> Line<'static> {
    let label = render_text(agent_label(agent));
    let status = lifecycle_status(agent, stale_evidence);
    let status_style = lifecycle_style(agent, stale_evidence, palette);
    let branch = agent.parent_thread_id.as_ref().map_or("", |_| "↪ ");
    let mut spans = vec![Span::raw(format!(
        "{}{branch}{label}  ",
        "  ".repeat(row.depth)
    ))];
    if let Some(role) = agent
        .agent_role
        .as_deref()
        .or_else(|| agent.parent_thread_id.is_none().then_some("orchestrator"))
    {
        spans.push(Span::styled(
            format!("{}  ", render_text(role)),
            palette.role(),
        ));
    }
    if let Some(model) = agent.model.as_deref() {
        spans.push(Span::styled(
            format!("{}  ", render_text(model)),
            palette.model(),
        ));
    }
    if let Some(effort) = agent.reasoning_effort.as_deref() {
        spans.push(Span::styled(
            format!("{}  ", render_text(effort)),
            palette.reasoning_effort(effort),
        ));
    }
    spans.push(Span::styled(format!("{status}  "), status_style));
    if let Some(usage) = agent
        .context_usage
        .as_ref()
        .filter(|_| context_usage_is_actionable(agent, stale_evidence))
    {
        let percent = usage.percent();
        spans.push(Span::styled(
            format!("CTX {percent}%  "),
            context_pressure_style(percent, palette),
        ));
    }
    if let Some(last_activity) = agent.last_activity_at {
        spans.push(Span::styled(age(last_activity), palette.metadata()));
    }
    Line::from(spans)
}

fn labelled_line(label: &str, value: String, palette: Palette) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("{label}: "), palette.title()),
        Span::raw(value),
    ])
}

fn push_detail_value(spans: &mut Vec<Span<'static>>, value: String, style: Style) {
    if spans.len() > 1 {
        spans.push(Span::raw(" · "));
    }
    spans.push(Span::styled(value, style));
}

fn detail_lines(
    agent: Option<&AgentState>,
    group: &SessionGroup,
    stale_evidence: Option<StaleEvidence>,
    palette: Palette,
) -> Vec<Line<'static>> {
    let Some(agent) = agent else {
        return vec![Line::from("No agent selected")];
    };
    let metadata = group
        .rollouts
        .iter()
        .find(|metadata| metadata.thread_id == agent.thread_id);

    let displayed_agent_path = agent
        .agent_path
        .as_deref()
        .map(|path| path.strip_prefix("/root/").unwrap_or(path))
        .unwrap_or("(unnamed)");
    let mut identity = format!(
        "{} · thread {}",
        render_text(displayed_agent_path),
        render_text(&agent.thread_id),
    );
    if let Some(parent) = agent.parent_thread_id.as_deref() {
        identity.push_str(&format!(" · parent {}", render_text(parent)));
    }
    let mut lines = vec![labelled_line("agent", identity, palette)];

    let mut metadata_spans = vec![Span::styled("metadata: ", palette.title())];
    if let Some(role) = agent.agent_role.as_deref() {
        push_detail_value(
            &mut metadata_spans,
            format!("role {}", render_text(role)),
            Style::default(),
        );
    }
    if let Some(nickname) = agent.agent_nickname.as_deref() {
        push_detail_value(
            &mut metadata_spans,
            format!("nickname {}", render_text(nickname)),
            Style::default(),
        );
    }
    if let Some(model) = agent.model.as_deref() {
        push_detail_value(
            &mut metadata_spans,
            render_text(model),
            palette.model(),
        );
    }
    if let Some(effort) = agent.reasoning_effort.as_deref() {
        push_detail_value(
            &mut metadata_spans,
            render_text(effort),
            palette.reasoning_effort(effort),
        );
    }
    push_detail_value(
        &mut metadata_spans,
        format!("Codex {}", render_text(&agent.cli_version)),
        Style::default(),
    );
    if !agent.schema_catalogued {
        push_detail_value(
            &mut metadata_spans,
            "schema missing".into(),
            palette.error(),
        );
    }
    if agent.coverage == CoverageLevel::Unknown {
        push_detail_value(
            &mut metadata_spans,
            format!("compatibility {}", coverage(agent.coverage)),
            palette.error(),
        );
    }
    lines.push(Line::from(metadata_spans));

    let mut lifecycle = vec![lifecycle_status(agent, stale_evidence).to_owned()];
    if let Some(turn_id) = agent.latest_turn.turn_id.as_deref() {
        lifecycle.push(format!("turn {}", render_text(turn_id)));
    }
    if let Some(depth) = metadata.and_then(|metadata| metadata.depth) {
        lifecycle.push(format!("depth {depth}"));
    }
    if let Some(started_at) = agent.latest_turn.started_at {
        lifecycle.push(format!("started {}", timestamp(Some(started_at))));
    }
    if let Some(completed_at) = agent.latest_turn.completed_at {
        lifecycle.push(format!("completed {}", timestamp(Some(completed_at))));
    }
    if let Some(last_activity) = agent.last_activity_at {
        lifecycle.push(format!("last activity {}", timestamp(Some(last_activity))));
    }
    lines.push(labelled_line("lifecycle", lifecycle.join(" · "), palette));
    if let Some(evidence) = stale_evidence {
        let explanation = match evidence {
            StaleEvidence::LaterAgentListSnapshot => {
                "a later complete list_agents snapshot no longer included this agent; completion unknown"
            }
            StaleEvidence::LaterSessionActivity => {
                "session activity continued for at least 2h after this agent's last activity; completion unknown"
            }
        };
        lines.push(Line::from(vec![
            Span::styled("stale: ", palette.title()),
            Span::styled(explanation, palette.warning()),
        ]));
    }

    if let Some(usage) = agent
        .context_usage
        .as_ref()
        .filter(|_| context_usage_is_actionable(agent, stale_evidence))
    {
        let percent = usage.percent();
        let mut context = vec![Span::styled("context: ", palette.title())];
        push_detail_value(
            &mut context,
            format!(
                "{} / {} input tokens ({percent}%)",
                exact_tokens(usage.input_tokens),
                exact_tokens(usage.model_context_window)
            ),
            context_pressure_style(percent, palette),
        );
        if let Some(previous) = usage.previous_input_tokens {
            push_detail_value(
                &mut context,
                context_change(usage.input_tokens, previous),
                palette.metadata(),
            );
        }
        if let Some(observed_at) = usage.observed_at {
            push_detail_value(
                &mut context,
                format!("observed {}", timestamp(Some(observed_at))),
                palette.metadata(),
            );
        }
        lines.push(Line::from(context));

        let mut counters = Vec::new();
        if let Some(cached) = usage.cached_input_tokens {
            counters.push(format!("{} cached input", exact_tokens(cached)));
        }
        if let Some(output) = usage.output_tokens {
            counters.push(format!("{} output", exact_tokens(output)));
        }
        if let Some(reasoning) = usage.reasoning_output_tokens {
            counters.push(format!("{} reasoning output", exact_tokens(reasoning)));
        }
        if let Some(cumulative) = usage.cumulative_tokens {
            counters.push(format!("{} cumulative", exact_tokens(cumulative)));
        }
        if !counters.is_empty() {
            lines.push(labelled_line("token usage", counters.join(" · "), palette));
        }

        let guidance = if percent >= 85 {
            Some((
                "context pressure is high; finish or hand off bounded work",
                palette.error(),
            ))
        } else if percent >= 70 {
            Some((
                "context pressure is rising; plan a bounded handoff",
                palette.warning(),
            ))
        } else {
            None
        };
        if let Some((guidance, style)) = guidance {
            lines.push(Line::from(vec![
                Span::styled("guidance: ", palette.title()),
                Span::styled(guidance, style),
            ]));
        }
    }

    let message = agent.last_message.as_deref();
    let final_message = agent.final_message.as_deref();
    if let Some(activity) = agent
        .current_activity()
        .filter(|activity| agent.active_call_evidence().is_some() || message != Some(*activity))
    {
        let mut activity = render_detail_text(activity);
        if let Some((started, ordinal)) = agent.active_call_evidence() {
            if let Some(started) = started {
                activity.push_str(&format!(
                    " · active call started {}",
                    timestamp(Some(started))
                ));
            }
            if let Some(ordinal) = ordinal {
                activity.push_str(&format!(" · ordinal {ordinal}"));
            }
        }
        lines.push(labelled_line("activity", activity, palette));
    }
    if let Some(summary) = agent.last_reasoning_summary.as_deref() {
        lines.push(labelled_line(
            "reasoning summary",
            render_detail_text(summary),
            palette,
        ));
    }
    if let Some(message) = message {
        lines.push(labelled_line(
            "message",
            render_detail_text(message),
            palette,
        ));
    }
    if let Some(final_message) =
        final_message.filter(|final_message| message != Some(*final_message))
    {
        lines.push(labelled_line(
            "final",
            render_detail_text(final_message),
            palette,
        ));
    }
    if let Some(claim) = agent.result_status_claim.as_deref() {
        lines.push(labelled_line("result", render_detail_text(claim), palette));
    }

    lines
}

fn stale_evidence_for_display(
    state: &SessionState,
    agent: &AgentState,
    loading: bool,
) -> Option<StaleEvidence> {
    if loading {
        None
    } else {
        state.stale_evidence(agent)
    }
}

fn lifecycle_status(agent: &AgentState, stale_evidence: Option<StaleEvidence>) -> &'static str {
    if stale_evidence.is_some() {
        "STALE"
    } else if agent.is_waiting_on_agent() {
        "WAITING ON AGENT ↓"
    } else {
        status(agent.latest_turn.status)
    }
}

fn lifecycle_style(
    agent: &AgentState,
    stale_evidence: Option<StaleEvidence>,
    palette: Palette,
) -> Style {
    if stale_evidence.is_some() {
        return palette.warning();
    }
    if agent.is_waiting_on_agent() {
        return palette.waiting();
    }
    match agent.latest_turn.status {
        TurnStatus::Completed => palette.neutral(),
        TurnStatus::Interrupted | TurnStatus::Errored => palette.error(),
        TurnStatus::Pending => palette.warning(),
        TurnStatus::Running => palette.good(),
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

fn utc_date_time(time: DateTime<Utc>) -> String {
    time.format("%Y-%m-%d %H:%M:%S").to_string()
}

fn timestamp(value: Option<DateTime<Utc>>) -> String {
    value.map_or_else(
        || "unknown".into(),
        |time| format!("{} UTC", utc_date_time(time)),
    )
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

fn render_text_with_limit(value: &str, limit: usize) -> String {
    let mut characters = value
        .chars()
        .filter(|character| !character.is_control())
        .take(limit + 1)
        .collect::<Vec<_>>();
    if characters.len() > limit {
        characters.truncate(limit - 1);
        characters.push('…');
    }
    characters.into_iter().collect()
}

fn render_text(value: &str) -> String {
    render_text_with_limit(value, RENDER_TEXT_LIMIT)
}

fn render_detail_text(value: &str) -> String {
    render_text_with_limit(value, DETAIL_RENDER_TEXT_LIMIT)
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
    use crate::model::STALE_AFTER_SESSION_PROGRESS_SECONDS;
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
    fn loading_update_cadence_counts_elapsed_work_and_bounds_event_wait() {
        assert_eq!(update_interval(true), Duration::from_millis(50));
        assert_eq!(update_interval(false), Duration::from_secs(1));
        assert_eq!(event_wait(Duration::ZERO, true), Duration::from_millis(50));
        assert_eq!(
            event_wait(Duration::from_millis(20), true),
            Duration::from_millis(30)
        );
        assert_eq!(event_wait(Duration::from_millis(50), true), Duration::ZERO);
        assert_eq!(event_wait(Duration::from_millis(75), true), Duration::ZERO);
        assert_eq!(
            event_wait(Duration::ZERO, false),
            Duration::from_millis(250)
        );
        assert_eq!(
            event_wait(Duration::from_millis(900), false),
            Duration::from_millis(100)
        );
        assert_eq!(event_wait(Duration::from_secs(1), false), Duration::ZERO);

        assert!(loading_work_window_open(true, Duration::ZERO));
        assert!(loading_work_window_open(true, Duration::from_millis(99)));
        assert!(!loading_work_window_open(true, Duration::from_millis(100)));
        assert!(!loading_work_window_open(false, Duration::ZERO));
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
        let stale_evidence = state.stale_evidence(agent);
        let tree = line_text(&tree_line(&row, agent, stale_evidence, palette));
        let detail = detail_lines(Some(agent), group, stale_evidence, palette)
            .iter()
            .map(line_text)
            .collect::<Vec<_>>()
            .join("\n");
        (tree, detail)
    }

    #[test]
    fn details_use_root_relative_agent_paths() {
        let (group, mut state) = fixture();
        let child = state.agents.get_mut("child").unwrap();
        child.agent_path = Some("/root/owner/child".into());

        let details = detail_lines(
            Some(child),
            &group,
            None,
            Palette::new(ColorMode::None),
        );
        assert_eq!(
            line_text(&details[0]),
            "agent: owner/child · thread child · parent root"
        );
        assert_eq!(child.agent_path.as_deref(), Some("/root/owner/child"));
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
                interaction_sequence: 0,
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
                interaction_sequence: 1,
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
    fn context_usage_renders_pressure_details_and_compaction_history() {
        let (group, mut state) = fixture();
        let root = state.agents.get_mut("root").unwrap();
        for record in [
            serde_json::json!({
                "timestamp": "2026-09-03T10:00:00Z",
                "type": "event_msg",
                "payload": {
                    "type": "token_count",
                    "info": {
                        "last_token_usage": {"input_tokens": 38_000},
                        "model_context_window": 258_400
                    }
                }
            }),
            serde_json::json!({
                "timestamp": "2026-09-03T10:01:00Z",
                "type": "event_msg",
                "payload": {
                    "type": "token_count",
                    "info": {
                        "last_token_usage": {
                            "input_tokens": 39_773,
                            "cached_input_tokens": 39_424,
                            "output_tokens": 59,
                            "reasoning_output_tokens": 12
                        },
                        "total_token_usage": {"total_tokens": 1_313_563},
                        "model_context_window": 258_400
                    }
                }
            }),
        ] {
            assert!(crate::model::reduce(root, &record));
        }

        let rows = flatten(&group, &state, false);
        let palette = Palette::new(ColorMode::Auto);
        let root = state.agents.get("root").unwrap();
        let tree = tree_line(&rows[0], root, None, palette);
        assert!(line_text(&tree).contains("CTX 15%"));
        let details = detail_lines(Some(root), &group, None, palette);
        let details = details.iter().map(line_text).collect::<Vec<_>>().join("\n");
        assert!(details.contains(
            "context: 39,773 / 258,400 input tokens (15%) · +1,773 since previous observation · observed 2026-09-03 10:01:00 UTC"
        ));
        assert!(details.contains(
            "token usage: 39,424 cached input · 59 output · 12 reasoning output · 1,313,563 cumulative"
        ));

        state.agents.get_mut("root").unwrap().latest_turn.status = TurnStatus::Completed;
        let root = state.agents.get("root").unwrap();
        assert!(root.context_usage.is_some());
        assert!(!line_text(&tree_line(&rows[0], root, None, palette)).contains("CTX "));
        let completed_details = detail_lines(Some(root), &group, None, palette)
            .iter()
            .map(line_text)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(!completed_details.contains("context:"));
        assert!(!completed_details.contains("token usage:"));
        assert!(!completed_details.contains("context pressure"));

        state.agents.get_mut("root").unwrap().latest_turn.status = TurnStatus::Running;
        let root = state.agents.get("root").unwrap();
        let stale_evidence = Some(StaleEvidence::LaterSessionActivity);
        assert!(!line_text(&tree_line(&rows[0], root, stale_evidence, palette)).contains("CTX "));
        let stale_details = detail_lines(Some(root), &group, stale_evidence, palette)
            .iter()
            .map(line_text)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(!stale_details.contains("context:"));
        assert!(!stale_details.contains("token usage:"));
        assert!(!stale_details.contains("context pressure"));

        let root = state.agents.get_mut("root").unwrap();
        assert!(crate::model::reduce(
            root,
            &serde_json::json!({
                "timestamp": "2026-09-03T10:02:00Z",
                "type": "event_msg",
                "payload": {
                    "type": "token_count",
                    "info": {
                        "last_token_usage": {"input_tokens": 200_000},
                        "model_context_window": 258_400
                    }
                }
            })
        ));
        let warning_tree = tree_line(&rows[0], root, None, palette);
        assert_eq!(
            warning_tree
                .spans
                .iter()
                .find(|span| span.content.contains("CTX 77%"))
                .unwrap()
                .style
                .fg,
            Some(Color::Yellow)
        );

        assert!(crate::model::reduce(
            root,
            &serde_json::json!({
                "timestamp": "2026-09-03T10:03:00Z",
                "type": "event_msg",
                "payload": {
                    "type": "token_count",
                    "info": {
                        "last_token_usage": {"input_tokens": 230_000},
                        "model_context_window": 258_400
                    }
                }
            })
        ));
        let error_tree = tree_line(&rows[0], root, None, palette);
        assert_eq!(
            error_tree
                .spans
                .iter()
                .find(|span| span.content.contains("CTX 89%"))
                .unwrap()
                .style
                .fg,
            Some(Color::Red)
        );

        assert!(crate::model::reduce(
            root,
            &serde_json::json!({
                "timestamp": "2026-09-03T10:04:00Z",
                "type": "event_msg",
                "payload": {"type": "context_compacted"}
            })
        ));
        assert!(root.context_usage.is_none());
        let compacted = root.interactions.back().unwrap();
        assert_eq!(
            line_text(&interaction_line(
                compacted,
                Utc::now(),
                Palette::new(ColorMode::None)
            )),
            "2026-09-03 10:04:00  context: COMPACTED"
        );
    }

    #[test]
    fn partial_tree_loading_indicator_and_selection_render_in_none_mode() {
        let (group, mut state) = fixture();
        state.agents.remove("child");
        let mut ui = UiState::default();
        let rows = flatten(&group, &state, false);
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
        assert!(rendered.contains("Esc back"));
        assert!(rendered.contains("q quit"));
        assert!(!rendered.contains("q/Esc quit"));
        assert!(terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .all(|cell| cell.fg == Color::Reset && cell.bg == Color::Reset));

        let (_, complete) = fixture();
        let rows = flatten(&group, &complete, false);
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
        root_state.model = Some("gpt-5.6-sol".into());
        root_state.reasoning_effort = Some("high".into());
        root_state.schema_catalogued = true;
        let mut child_state = AgentState::new("child".into(), "0.152.1".into());
        child_state.parent_thread_id = Some("root".into());
        child_state.agent_path = Some("/root/child".into());
        child_state.agent_role = Some("map_implementer".into());
        child_state.model = Some("gpt-5.6-luna".into());
        child_state.reasoning_effort = Some("medium".into());
        child_state.schema_catalogued = true;
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

        let ids = flatten(&group, &state, false)
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

        let ids = flatten(&group, &state, false)
            .into_iter()
            .map(|row| row.thread_id)
            .collect::<Vec<_>>();
        assert_eq!(ids, ["root", "newer", "tie-a", "tie-b", "child"]);
    }

    #[test]
    fn selection_survives_tree_reordering_and_navigation_is_bounded() {
        let (group, mut state) = fixture();
        let mut ui = UiState::default();
        let rows = flatten(&group, &state, false);
        ui.synchronise(&rows);
        ui.move_selection(&rows, 1);
        assert_eq!(ui.selected_thread.as_deref(), Some("child"));

        add_agent(&mut state, "newer", "root", "/root/newer", 100);
        let rows = flatten(&group, &state, false);
        ui.synchronise(&rows);
        assert_eq!(ui.selected_thread.as_deref(), Some("child"));
        ui.move_selection(&rows, 99);
        assert_eq!(ui.selected_thread.as_deref(), Some("child"));
        ui.move_selection(&rows, -99);
        assert_eq!(ui.selected_thread.as_deref(), Some("root"));
    }

    #[test]
    fn compact_tree_labels_and_completed_filter_preserve_live_descendants() {
        let (group, mut state) = fixture();
        state.agents.get_mut("child").unwrap().latest_turn.status = TurnStatus::Completed;
        add_agent(
            &mut state,
            "grandchild",
            "child",
            "/root/child/grandchild",
            100,
        );

        let rows = flatten(&group, &state, false);
        let child_row = rows.iter().find(|row| row.thread_id == "child").unwrap();
        let child_line = line_text(&tree_line(
            child_row,
            &state.agents["child"],
            None,
            Palette::new(ColorMode::None),
        ));
        assert!(child_line.starts_with("  ↪ child"));
        assert!(!child_line.contains("/root/"));

        let visible = flatten(&group, &state, true);
        assert_eq!(
            visible
                .iter()
                .map(|row| row.thread_id.as_str())
                .collect::<Vec<_>>(),
            ["root", "grandchild"]
        );
        let grandchild = visible
            .iter()
            .find(|row| row.thread_id == "grandchild")
            .unwrap();
        assert_eq!(grandchild.depth, 1);
        let grandchild_line = line_text(&tree_line(
            grandchild,
            &state.agents["grandchild"],
            None,
            Palette::new(ColorMode::None),
        ));
        assert!(grandchild_line.starts_with("  ↪ grandchild"));

        let mut ui = UiState::default();
        ui.toggle_completed();
        ui.synchronise(&visible);
        let mut terminal = Terminal::new(TestBackend::new(120, 24)).unwrap();
        terminal
            .draw(|frame| {
                draw(
                    frame,
                    &group,
                    &state,
                    &visible,
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
        assert!(rendered.contains("h show completed"));
        assert!(!rendered.contains("COMPLETED"));
    }

    #[test]
    fn stale_requires_later_session_evidence_and_survives_live_updates() {
        let (group, mut state) = fixture();
        let agent_activity = DateTime::from_timestamp(0, 0).unwrap();
        let before_threshold =
            DateTime::from_timestamp(STALE_AFTER_SESSION_PROGRESS_SECONDS - 1, 0).unwrap();
        let at_threshold =
            DateTime::from_timestamp(STALE_AFTER_SESSION_PROGRESS_SECONDS, 0).unwrap();

        {
            let child = state.agents.get_mut("child").unwrap();
            child.latest_turn.status = TurnStatus::Running;
            child.last_activity_at = Some(agent_activity);
        }

        let child = state.agents.get("child").unwrap();
        assert_eq!(state.stale_evidence(child), None);
        state.agents.get_mut("root").unwrap().last_activity_at = Some(before_threshold);
        assert_eq!(state.stale_evidence(&state.agents["child"]), None);
        state.agents.get_mut("root").unwrap().last_activity_at = Some(at_threshold);
        assert_eq!(
            state.stale_evidence(&state.agents["child"]),
            Some(StaleEvidence::LaterSessionActivity)
        );
        assert_eq!(
            stale_evidence_for_display(&state, &state.agents["child"], true),
            None
        );
        let mut updating_ui = UiState::default();
        assert!(note_poll(
            &mut updating_ui,
            PollOutcome::Updated {
                records: 1,
                admitted: 0,
            }
        ));
        assert!(updating_ui.catching_up);
        let stale_evidence = stale_evidence_for_display(&state, &state.agents["child"], false);
        assert_eq!(
            lifecycle_status(&state.agents["child"], stale_evidence),
            "STALE"
        );
        assert_eq!(
            lifecycle_style(
                &state.agents["child"],
                stale_evidence,
                Palette::new(ColorMode::Auto)
            )
            .fg,
            Some(Color::Yellow)
        );
        let visible = flatten(&group, &state, true);
        assert!(visible.iter().any(|row| row.thread_id == "child"));

        let child = state.agents.get("child").unwrap();
        let row = visible.iter().find(|row| row.thread_id == "child").unwrap();
        let tree = tree_line(row, child, stale_evidence, Palette::new(ColorMode::Auto));
        let stale_status = tree
            .spans
            .iter()
            .find(|span| span.content.contains("STALE"))
            .unwrap();
        assert_eq!(stale_status.style.fg, Some(Color::Yellow));

        let details = detail_lines(
            Some(child),
            &group,
            stale_evidence,
            Palette::new(ColorMode::Auto),
        );
        let stale_detail = details
            .iter()
            .find(|line| line_text(line).starts_with("stale: "))
            .unwrap();
        assert_eq!(stale_detail.spans[0].style.fg, Some(Color::Cyan));
        assert_eq!(stale_detail.spans[1].style.fg, Some(Color::Yellow));
        assert!(line_text(stale_detail).contains("at least 2h"));
        assert!(line_text(stale_detail).contains("completion unknown"));

        let snapshot_details = detail_lines(
            Some(child),
            &group,
            Some(StaleEvidence::LaterAgentListSnapshot),
            Palette::new(ColorMode::Auto),
        );
        assert!(snapshot_details
            .iter()
            .any(|line| line_text(line).contains("later complete list_agents snapshot")));

        let root = state.agents.get_mut("root").unwrap();
        root.latest_turn.status = TurnStatus::Running;
        root.last_activity_at = Some(agent_activity);
        assert_eq!(state.stale_evidence(&state.agents["root"]), None);

        let child = state.agents.get_mut("child").unwrap();
        child.latest_turn.status = TurnStatus::Completed;
        assert_eq!(state.stale_evidence(&state.agents["child"]), None);
    }

    #[test]
    fn yielded_tool_interactions_show_handoff_state() {
        let started = "2026-09-04T03:44:43Z".parse::<DateTime<Utc>>().unwrap();
        let yielded = "2026-09-04T03:45:14Z".parse::<DateTime<Utc>>().unwrap();
        let interaction = AgentInteraction {
            sequence: 1,
            kind: InteractionKind::Tool,
            summary: "Poll terminal — ops/dev/verify.sh".into(),
            timestamp: Some(started),
            ordinal: Some(12),
            tool_state: Some(ToolInteractionState::Yielded),
            finished_at: Some(yielded),
        };

        assert_eq!(
            tool_state_text(&interaction, yielded).as_deref(),
            Some("yielded after 31.0s")
        );
        let lines = interaction_detail_lines(
            Some((&interaction, 0)),
            1,
            yielded,
            Palette::new(ColorMode::None),
        );
        assert!(lines
            .iter()
            .any(|line| line_text(line) == "state: yielded after 31.0s"));
        assert!(lines
            .iter()
            .any(|line| line_text(line) == "yielded: 2026-09-04T03:45:14+00:00"));
        assert!(!lines
            .iter()
            .any(|line| line_text(line).starts_with("returned:")));
    }

    #[test]
    fn interaction_history_opens_at_latest_and_preserves_scrolled_position() {
        let (group, mut state) = fixture();
        {
            let root = state.agents.get_mut("root").unwrap();
            for record in [
                serde_json::json!({
                    "ordinal": 10,
                    "timestamp": "2026-09-03T10:00:00Z",
                    "type": "event_msg",
                    "payload": {"type": "task_started", "turn_id": "turn"}
                }),
                serde_json::json!({
                    "ordinal": 11,
                    "timestamp": "2026-09-03T10:00:01Z",
                    "type": "event_msg",
                    "payload": {"type": "agent_message", "message": "working"}
                }),
                serde_json::json!({
                    "ordinal": 12,
                    "timestamp": "2026-09-03T10:00:02Z",
                    "type": "response_item",
                    "payload": {"type": "function_call", "call_id": "call", "name": "exec"}
                }),
                serde_json::json!({
                    "ordinal": 13,
                    "timestamp": "2026-09-03T10:00:05Z",
                    "type": "response_item",
                    "payload": {"type": "function_call_output", "call_id": "call"}
                }),
            ] {
                assert!(crate::model::reduce(root, &record));
            }
        }

        let rows = flatten(&group, &state, false);
        let mut ui = UiState::default();
        ui.synchronise(&rows);
        ui.open_history(&state);
        assert_eq!(
            ui.history.as_ref().unwrap().selected_sequence,
            state.agents["root"]
                .interactions
                .back()
                .map(|interaction| interaction.sequence)
        );

        let mut terminal = Terminal::new(TestBackend::new(120, 24)).unwrap();
        terminal
            .draw(|frame| {
                draw(
                    frame,
                    &group,
                    &state,
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
        assert!(
            rendered.contains("interactions · root · orchestrator · gpt-5.6-sol · high · RUNNING")
        );
        assert!(rendered.contains("tool: exec · returned after 3.0s"));
        assert!(rendered.contains("state: returned after 3.0s"));
        assert!(rendered.contains("↑/k older"));
        assert!(!rendered.contains("h hide completed"));

        ui.move_history(&state, -1);
        let selected = ui.history.as_ref().unwrap().selected_sequence;
        let selected_summary = state.agents["root"]
            .interactions
            .iter()
            .find(|interaction| Some(interaction.sequence) == selected)
            .unwrap()
            .summary
            .as_str();
        assert_eq!(selected_summary, "working");

        let root = state.agents.get_mut("root").unwrap();
        assert!(crate::model::reduce(
            root,
            &serde_json::json!({
                "ordinal": 14,
                "type": "event_msg",
                "payload": {"type": "agent_reasoning", "text": "new live event"}
            }),
        ));
        ui.synchronise_history(&state);
        assert_eq!(ui.history.as_ref().unwrap().selected_sequence, selected);
        assert!(ui.close_history());
        assert!(ui.history.is_none());
    }

    #[test]
    fn details_and_interaction_content_use_available_space() {
        let (group, mut state) = fixture();
        let message = format!("{}VISIBLE_TAIL", "detail ".repeat(80));
        assert!(message.chars().count() > RENDER_TEXT_LIMIT);
        assert!(message.chars().count() < DETAIL_RENDER_TEXT_LIMIT);
        assert!(crate::model::reduce(
            state.agents.get_mut("root").unwrap(),
            &serde_json::json!({
                "ordinal": 20,
                "timestamp": "2026-09-03T10:05:00Z",
                "type": "event_msg",
                "payload": {"type": "agent_message", "message": message}
            }),
        ));

        let rows = flatten(&group, &state, false);
        let mut ui = UiState::default();
        ui.synchronise(&rows);
        assert_eq!(list_panel_height(30, rows.len(), 42), 4);
        let mut terminal = Terminal::new(TestBackend::new(120, 30)).unwrap();
        terminal
            .draw(|frame| {
                draw(
                    frame,
                    &group,
                    &state,
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
        assert!(rendered.contains("VISIBLE_TAIL"));

        ui.open_history(&state);
        assert_eq!(list_panel_height(30, 1, 55), 3);
        terminal
            .draw(|frame| {
                draw(
                    frame,
                    &group,
                    &state,
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
        assert!(rendered.contains("VISIBLE_TAIL"));
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
                    &flatten(&group, &state, false),
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
        assert!(text.contains("q/Esc quit"));
        assert!(!text.contains("Esc back"));
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
    fn timestamps_use_clean_utc_date_times() {
        let time = DateTime::parse_from_rfc3339("2026-09-03T09:04:39.564+00:00")
            .unwrap()
            .with_timezone(&Utc);
        assert_eq!(utc_date_time(time), "2026-09-03 09:04:39");
        assert_eq!(timestamp(Some(time)), "2026-09-03 09:04:39 UTC");
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
                    &flatten(&group, &state, false),
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

    #[test]
    fn tree_and_details_prioritise_known_actionable_information() {
        let (group, mut state) = fixture();
        let rows = flatten(&group, &state, false);
        let palette = Palette::new(ColorMode::Auto);

        {
            let root = state.agents.get("root").unwrap();
            let line = tree_line(&rows[0], root, None, palette);
            let text = line
                .spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<Vec<_>>()
                .concat();

            let model_position = text.find("gpt-5.6-sol").unwrap();
            let effort_position = text.find("high").unwrap();
            let status_position = text.find("PENDING").unwrap();
            assert!(model_position < effort_position);
            assert!(effort_position < status_position);

            let model_span = line
                .spans
                .iter()
                .find(|span| span.content.contains("gpt-5.6-sol"))
                .unwrap();
            let effort_span = line
                .spans
                .iter()
                .find(|span| span.content.contains("high"))
                .unwrap();
            assert_eq!(model_span.style.fg, Some(Color::LightMagenta));
            assert_eq!(effort_span.style.fg, Some(ORANGE));

            let details = detail_lines(Some(root), &group, None, palette);
            let details_text = details
                .iter()
                .flat_map(|line| line.spans.iter())
                .map(|span| span.content.as_ref())
                .collect::<Vec<_>>()
                .concat();
            assert!(details_text.contains("model gpt-5.6-sol"));
            assert!(details_text.contains("reasoning high"));
            for absent in [
                "schema catalogued",
                "compatibility ingestable",
                "final:",
                "untrusted result claim:",
                "unknown",
                "session health:",
            ] {
                assert!(
                    !details_text.contains(absent),
                    "unavailable or healthy detail should be omitted: {absent}"
                );
            }
            for line in &details {
                let label = line.spans.first().unwrap();
                assert!(label.content.ends_with(": "));
                assert_eq!(label.style.fg, Some(Color::Cyan));
            }
        }

        {
            let root = state.agents.get_mut("root").unwrap();
            root.schema_catalogued = false;
            root.coverage = CoverageLevel::Unknown;
        }
        state.data_health.unknown_records = 31_098;
        state.data_health.unknown_events = 33_270;

        let root = state.agents.get("root").unwrap();
        let details = detail_lines(Some(root), &group, None, palette);
        let details_text = details
            .iter()
            .flat_map(|line| line.spans.iter())
            .map(|span| span.content.as_ref())
            .collect::<Vec<_>>()
            .concat();
        assert!(details_text.contains("schema missing"));
        assert!(details_text.contains("compatibility unknown"));
        assert!(!details_text.contains("unknown records"));
        assert!(!details_text.contains("unknown events"));
        assert!(!details_text.contains("latest diagnostic"));
        assert!(details
            .iter()
            .flat_map(|line| line.spans.iter())
            .filter(|span| {
                span.content.contains("schema missing")
                    || span.content.contains("compatibility unknown")
            })
            .all(|span| span.style.fg == Some(Color::Red)));
    }

    #[test]
    fn details_hide_unactionable_session_health_counts() {
        let (group, mut state) = fixture();
        state.data_health.malformed_records = 8;
        state.data_health.oversized_records = 2;

        let root = state.agents.get("root").unwrap();
        let details = detail_lines(
            Some(root),
            &group,
            None,
            Palette::new(ColorMode::Auto),
        );
        let details_text = details
            .iter()
            .flat_map(|line| line.spans.iter())
            .map(|span| span.content.as_ref())
            .collect::<Vec<_>>()
            .concat();

        assert!(!details_text.contains("session health:"));
        assert!(!details_text.contains("malformed 8"));
        assert!(!details_text.contains("oversized 2"));
    }

    #[test]
    fn duplicate_completed_output_is_rendered_once_as_message() {
        let (group, mut state) = fixture();
        {
            let root = state.agents.get_mut("root").unwrap();
            root.latest_turn.status = TurnStatus::Completed;
            root.last_message = Some("shared completion".into());
            root.final_message = Some("shared completion".into());
        }

        let root = state.agents.get("root").unwrap();
        let details = detail_lines(
            Some(root),
            &group,
            None,
            Palette::new(ColorMode::Auto),
        );
        let details_text = details.iter().map(line_text).collect::<Vec<_>>().join("\n");
        assert!(details_text.contains("message: shared completion"));
        assert!(!details_text.contains("activity: shared completion"));
        assert!(!details_text.contains("final: shared completion"));

        state.agents.get_mut("root").unwrap().final_message = Some("distinct final".into());
        let root = state.agents.get("root").unwrap();
        let details = detail_lines(
            Some(root),
            &group,
            None,
            Palette::new(ColorMode::Auto),
        );
        let details_text = details.iter().map(line_text).collect::<Vec<_>>().join("\n");
        assert!(details_text.contains("message: shared completion"));
        assert!(details_text.contains("final: distinct final"));
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

        let rows = flatten(&group, &state, false);
        let mut ui = UiState::default();
        ui.synchronise(&rows);
        let mut tree_terminal = Terminal::new(TestBackend::new(100, 20)).unwrap();
        tree_terminal
            .draw(|frame| draw(frame, &group, &state, &rows, &ui, false, palette))
            .unwrap();
        let tree_cells = tree_terminal.backend().buffer().content();
        for expected in [Color::Cyan, Color::Blue, Color::LightMagenta, ORANGE] {
            assert!(
                tree_cells.iter().any(|cell| cell.fg == expected),
                "tree should render semantic foreground {expected:?}"
            );
        }
    }

    #[test]
    fn reasoning_and_lifecycle_colours_encode_operational_state() {
        let palette = Palette::new(ColorMode::Auto);
        for (effort, expected) in [
            ("none", Color::LightGreen),
            ("minimal", Color::LightGreen),
            ("low", Color::Green),
            ("medium", Color::Yellow),
            ("high", ORANGE),
            ("xhigh", Color::Red),
            ("max", Color::Red),
            ("ultra", Color::Red),
            ("future", Color::LightYellow),
        ] {
            assert_eq!(palette.reasoning_effort(effort).fg, Some(expected));
        }

        let (group, state) = fixture();
        let mut agent = state.agents["root"].clone();
        agent.latest_turn.status = TurnStatus::Running;
        assert_eq!(
            lifecycle_style(&agent, None, palette).fg,
            Some(Color::Green)
        );

        agent.in_flight_calls.insert(
            "wait".into(),
            crate::model::InFlightCall {
                tool_name: "wait_agent".into(),
                summary: "wait_agent".into(),
                started_at: None,
                ordinal: Some(1),
                sequence: 1,
                interaction_sequence: 0,
            },
        );
        assert_eq!(lifecycle_style(&agent, None, palette).fg, Some(ORANGE));

        agent.latest_turn.status = TurnStatus::Completed;
        assert_eq!(lifecycle_style(&agent, None, palette).fg, Some(Color::Gray));
        assert_eq!(
            Palette::new(ColorMode::None).reasoning_effort("high").fg,
            None
        );

        let details = detail_lines(Some(&agent), &group, None, palette);
        let metadata = &details[1];
        assert!(line_text(metadata).contains("model gpt-5.6-sol"));
        assert!(line_text(metadata).contains("reasoning high"));
        assert_eq!(
            metadata
                .spans
                .iter()
                .find(|span| span.content.contains("reasoning high"))
                .unwrap()
                .style
                .fg,
            Some(ORANGE)
        );
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

        let rows = flatten(&group, &state, false);
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
