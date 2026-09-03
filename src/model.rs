use chrono::{DateTime, Utc};
use serde_json::Value;
use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;

const TEXT_LIMIT: usize = 512;
const DIAGNOSTIC_LIMIT: usize = 20;
const INTERACTION_LIMIT: usize = 256;
const AGENT_LIST_SNAPSHOT_LIMIT: usize = 64;
const AGENT_LIST_MEMBER_LIMIT: usize = 1_024;
const SPAWN_HINT_LIMIT: usize = 64;
pub const STALE_AFTER_SESSION_PROGRESS_SECONDS: i64 = 2 * 60 * 60;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CoverageLevel {
    Unknown,
    Ingestable,
    SemanticallyCovered,
    LiveVerified,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnStatus {
    Pending,
    Running,
    Completed,
    Interrupted,
    Errored,
}
#[derive(Debug, Clone)]
pub struct TurnState {
    pub turn_id: Option<String>,
    pub status: TurnStatus,
    pub started_at: Option<DateTime<Utc>>,
    pub completed_at: Option<DateTime<Utc>>,
}
impl Default for TurnState {
    fn default() -> Self {
        Self {
            turn_id: None,
            status: TurnStatus::Pending,
            started_at: None,
            completed_at: None,
        }
    }
}
#[derive(Debug, Clone)]
pub struct InFlightCall {
    pub tool_name: String,
    pub summary: String,
    pub started_at: Option<DateTime<Utc>>,
    pub ordinal: Option<u64>,
    pub sequence: u64,
    pub interaction_sequence: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InteractionKind {
    Lifecycle,
    Tool,
    Reasoning,
    Message,
    Communication,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolInteractionState {
    Open,
    Returned,
    EndedWithoutReturn,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StaleEvidence {
    LaterAgentListSnapshot,
    LaterSessionActivity,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum AgentListScope {
    All,
    PathPrefix(String),
}

impl AgentListScope {
    fn covers(&self, agent_path: &str) -> bool {
        match self {
            Self::All => true,
            Self::PathPrefix(prefix) => {
                agent_path == prefix
                    || agent_path
                        .strip_prefix(prefix)
                        .is_some_and(|suffix| suffix.starts_with('/'))
            }
        }
    }
}

#[derive(Debug, Clone)]
struct AgentListSnapshot {
    observed_at: DateTime<Utc>,
    scope: AgentListScope,
    agent_paths: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpawnedAgentHint {
    pub agent_path: String,
    pub observed_at: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub struct AgentInteraction {
    pub sequence: u64,
    pub kind: InteractionKind,
    pub summary: String,
    pub timestamp: Option<DateTime<Utc>>,
    pub ordinal: Option<u64>,
    pub tool_state: Option<ToolInteractionState>,
    pub finished_at: Option<DateTime<Utc>>,
}
#[derive(Debug, Clone)]
pub struct DiagnosticSample {
    pub rollout_path: PathBuf,
    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "required diagnostic evidence even though raw samples are hidden from the normal TUI"
        )
    )]
    pub byte_offset: u64,
    pub cli_version: Option<String>,
    pub kind: String,
    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "required diagnostic evidence even though raw samples are hidden from the normal TUI"
        )
    )]
    pub ordinal: Option<u64>,
    pub detail: Option<String>,
}
#[derive(Debug, Default, Clone)]
pub struct DataHealth {
    pub unknown_records: u64,
    pub unknown_events: u64,
    pub malformed_records: u64,
    pub oversized_records: u64,
    pub recent_diagnostics: VecDeque<DiagnosticSample>,
}
impl DataHealth {
    pub fn diagnostic(&mut self, mut sample: DiagnosticSample) {
        sample.rollout_path = PathBuf::from(sanitise(&sample.rollout_path.to_string_lossy()));
        sample.cli_version = sample.cli_version.as_deref().map(sanitise);
        sample.kind = sanitise(&sample.kind);
        sample.detail = sample.detail.as_deref().map(sanitise);
        if self.recent_diagnostics.len() == DIAGNOSTIC_LIMIT {
            self.recent_diagnostics.pop_front();
        }
        self.recent_diagnostics.push_back(sample);
    }
}
#[derive(Debug, Clone)]
pub struct AgentState {
    pub thread_id: String,
    pub parent_thread_id: Option<String>,
    pub agent_path: Option<String>,
    pub agent_role: Option<String>,
    pub agent_nickname: Option<String>,
    pub cli_version: String,
    pub model: Option<String>,
    pub reasoning_effort: Option<String>,
    pub schema_catalogued: bool,
    pub schema_family: Option<String>,
    pub coverage: CoverageLevel,
    pub own_history_start_ordinal: Option<u64>,
    pub latest_turn: TurnState,
    pub last_activity_at: Option<DateTime<Utc>>,
    pub next_call_sequence: u64,
    pub in_flight_calls: HashMap<String, InFlightCall>,
    pub interactions: VecDeque<AgentInteraction>,
    pub next_interaction_sequence: u64,
    pub last_reasoning_summary: Option<String>,
    pub last_message: Option<String>,
    pub final_message: Option<String>,
    pub result_status_claim: Option<String>,
    pub last_communication: Option<String>,
    pub last_ordinal: Option<u64>,
    agent_list_calls: HashMap<String, AgentListScope>,
    agent_list_snapshots: VecDeque<AgentListSnapshot>,
    spawned_agent_hints: VecDeque<SpawnedAgentHint>,
}
impl AgentState {
    pub fn new(thread_id: String, cli_version: String) -> Self {
        Self {
            thread_id,
            parent_thread_id: None,
            agent_path: None,
            agent_role: None,
            agent_nickname: None,
            cli_version,
            model: None,
            reasoning_effort: None,
            schema_catalogued: false,
            schema_family: None,
            coverage: CoverageLevel::Ingestable,
            own_history_start_ordinal: None,
            latest_turn: TurnState::default(),
            last_activity_at: None,
            next_call_sequence: 0,
            in_flight_calls: HashMap::new(),
            interactions: VecDeque::new(),
            next_interaction_sequence: 0,
            last_reasoning_summary: None,
            last_message: None,
            final_message: None,
            result_status_claim: None,
            last_communication: None,
            last_ordinal: None,
            agent_list_calls: HashMap::new(),
            agent_list_snapshots: VecDeque::new(),
            spawned_agent_hints: VecDeque::new(),
        }
    }
    pub fn current_activity(&self) -> Option<&str> {
        self.newest_in_flight_call()
            .map(|call| call.summary.as_str())
            .or(self.last_reasoning_summary.as_deref())
            .or(self.last_message.as_deref())
    }

    pub fn is_waiting_on_agent(&self) -> bool {
        self.latest_turn.status == TurnStatus::Running
            && self
                .newest_in_flight_call()
                .is_some_and(|call| call.tool_name == "wait_agent")
    }

    fn newest_in_flight_call(&self) -> Option<&InFlightCall> {
        self.in_flight_calls
            .values()
            .max_by_key(|call| call.sequence)
    }

    pub fn active_call_evidence(&self) -> Option<(Option<DateTime<Utc>>, Option<u64>)> {
        self.newest_in_flight_call()
            .map(|call| (call.started_at, call.ordinal))
    }

    fn push_interaction(
        &mut self,
        kind: InteractionKind,
        summary: impl AsRef<str>,
        timestamp: Option<DateTime<Utc>>,
        ordinal: Option<u64>,
        tool_state: Option<ToolInteractionState>,
        finished_at: Option<DateTime<Utc>>,
    ) -> u64 {
        let sequence = self.next_interaction_sequence;
        self.next_interaction_sequence += 1;
        if self.interactions.len() == INTERACTION_LIMIT {
            self.interactions.pop_front();
        }
        self.interactions.push_back(AgentInteraction {
            sequence,
            kind,
            summary: sanitise(summary.as_ref()),
            timestamp,
            ordinal,
            tool_state,
            finished_at,
        });
        sequence
    }

    fn record_interaction(
        &mut self,
        kind: InteractionKind,
        summary: impl AsRef<str>,
        timestamp: Option<DateTime<Utc>>,
        ordinal: Option<u64>,
    ) -> u64 {
        let summary = sanitise(summary.as_ref());
        if let Some(previous) = self.interactions.back_mut().filter(|previous| {
            previous.kind == kind
                && previous.kind != InteractionKind::Tool
                && previous.summary == summary
        }) {
            previous.timestamp = timestamp.or(previous.timestamp);
            previous.ordinal = ordinal.or(previous.ordinal);
            return previous.sequence;
        }
        self.push_interaction(kind, summary, timestamp, ordinal, None, None)
    }

    fn start_tool_interaction(
        &mut self,
        summary: &str,
        timestamp: Option<DateTime<Utc>>,
        ordinal: Option<u64>,
    ) -> u64 {
        self.push_interaction(
            InteractionKind::Tool,
            summary,
            timestamp,
            ordinal,
            Some(ToolInteractionState::Open),
            None,
        )
    }

    fn finish_tool_interaction(
        &mut self,
        call: InFlightCall,
        finished_at: Option<DateTime<Utc>>,
        state: ToolInteractionState,
    ) {
        if let Some(interaction) = self
            .interactions
            .iter_mut()
            .find(|interaction| interaction.sequence == call.interaction_sequence)
        {
            interaction.tool_state = Some(state);
            interaction.finished_at = finished_at;
        }
    }

    fn close_in_flight_calls(&mut self, finished_at: Option<DateTime<Utc>>) {
        let mut calls = self
            .in_flight_calls
            .drain()
            .map(|(_, call)| call)
            .collect::<Vec<_>>();
        calls.sort_by_key(|call| call.sequence);
        for call in calls {
            self.finish_tool_interaction(
                call,
                finished_at,
                ToolInteractionState::EndedWithoutReturn,
            );
        }
        self.agent_list_calls.clear();
    }

    fn record_agent_list_snapshot(
        &mut self,
        scope: AgentListScope,
        observed_at: DateTime<Utc>,
        agent_paths: Vec<String>,
    ) {
        if self.agent_list_snapshots.len() == AGENT_LIST_SNAPSHOT_LIMIT {
            self.agent_list_snapshots.pop_front();
        }
        self.agent_list_snapshots.push_back(AgentListSnapshot {
            observed_at,
            scope,
            agent_paths,
        });
    }

    fn record_spawned_agent_hint(&mut self, hint: SpawnedAgentHint) {
        if self.spawned_agent_hints.len() == SPAWN_HINT_LIMIT {
            self.spawned_agent_hints.pop_front();
        }
        self.spawned_agent_hints.push_back(hint);
    }
}

#[derive(Debug, Default)]
pub struct SessionState {
    pub session_id: String,
    pub cwd: Option<PathBuf>,
    pub started_at: Option<DateTime<Utc>>,
    pub agents: HashMap<String, AgentState>,
    pub data_health: DataHealth,
}

impl SessionState {
    pub fn stale_evidence(&self, agent: &AgentState) -> Option<StaleEvidence> {
        if agent.parent_thread_id.is_none() || agent.latest_turn.status != TurnStatus::Running {
            return None;
        }
        let last_activity = agent.last_activity_at?;

        if let Some(agent_path) = agent
            .agent_path
            .as_deref()
            .and_then(canonical_agent_path_or_none)
        {
            let excluded_by_later_snapshot = self
                .agents
                .values()
                .flat_map(|observer| observer.agent_list_snapshots.iter())
                .any(|snapshot| {
                    snapshot.observed_at > last_activity
                        && snapshot.scope.covers(&agent_path)
                        && !snapshot
                            .agent_paths
                            .iter()
                            .any(|observed| observed == &agent_path)
                });
            if excluded_by_later_snapshot {
                return Some(StaleEvidence::LaterAgentListSnapshot);
            }
        }

        self.agents
            .values()
            .filter_map(|candidate| candidate.last_activity_at)
            .max()
            .filter(|latest_activity| {
                latest_activity
                    .signed_duration_since(last_activity)
                    .num_seconds()
                    >= STALE_AFTER_SESSION_PROGRESS_SECONDS
            })
            .map(|_| StaleEvidence::LaterSessionActivity)
    }

    pub fn take_spawned_agent_hints(&mut self) -> Vec<SpawnedAgentHint> {
        let mut hints = self
            .agents
            .values_mut()
            .flat_map(|agent| agent.spawned_agent_hints.drain(..))
            .collect::<Vec<_>>();
        hints.sort_by(|left, right| {
            left.observed_at
                .cmp(&right.observed_at)
                .then_with(|| left.agent_path.cmp(&right.agent_path))
        });
        hints
    }
}

pub fn parse_time(v: Option<&Value>) -> Option<DateTime<Utc>> {
    let v = v?;
    if let Some(s) = v.as_str() {
        return DateTime::parse_from_rfc3339(s)
            .ok()
            .map(|v| v.with_timezone(&Utc));
    }
    v.as_i64().and_then(|s| DateTime::from_timestamp(s, 0))
}
pub fn sanitise(input: &str) -> String {
    let mut out = String::new();
    let mut escape = false;
    let mut truncated = false;
    let mut characters = input.chars().peekable();
    while let Some(ch) = characters.next() {
        if escape {
            if ch == '[' {
                continue;
            }
            if ('@'..='~').contains(&ch) {
                escape = false;
            }
            continue;
        }
        if ch == '\u{1b}' {
            escape = true;
            continue;
        }
        let emitted = match ch {
            '\r' => {
                if characters.peek() == Some(&'\n') {
                    characters.next();
                }
                '↩'
            }
            '\n' => '↩',
            _ if ch.is_control() => '�',
            _ => ch,
        };
        if out.len() + emitted.len_utf8() > TEXT_LIMIT {
            truncated = true;
            break;
        }
        out.push(emitted);
    }
    if truncated {
        while out.len() + '…'.len_utf8() > TEXT_LIMIT {
            out.pop();
        }
        out.push('…');
    }
    out
}
fn plain_text(content: &Value) -> Option<String> {
    let parts = content.as_array()?;
    let text = parts
        .iter()
        .filter_map(|p| p.get("text").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join(" ");
    (!text.is_empty()).then_some(text)
}

fn canonical_agent_path_or_none(path: &str) -> Option<String> {
    let sanitised = sanitise(path);
    if sanitised != path || !(path == "/root" || path.starts_with("/root/")) {
        return None;
    }
    Some(sanitised)
}

fn parse_spawned_agent_path_or_none(payload: &Value) -> Option<String> {
    let output = payload.get("output").and_then(Value::as_str)?;
    let output: Value = serde_json::from_str(output).ok()?;
    canonical_agent_path_or_none(output.get("task_name")?.as_str()?)
}

fn parse_agent_list_scope_or_none(payload: &Value) -> Option<AgentListScope> {
    if payload.get("name").and_then(Value::as_str) != Some("list_agents") {
        return None;
    }
    let arguments = payload.get("arguments").and_then(Value::as_str)?;
    let arguments: Value = serde_json::from_str(arguments).ok()?;
    let arguments = arguments.as_object()?;
    if arguments.keys().any(|key| key != "path_prefix") {
        return None;
    }
    match arguments.get("path_prefix") {
        None => Some(AgentListScope::All),
        Some(Value::String(prefix)) => {
            canonical_agent_path_or_none(prefix).map(AgentListScope::PathPrefix)
        }
        Some(_) => None,
    }
}

fn parse_agent_list_paths_or_none(payload: &Value) -> Option<Vec<String>> {
    let output = payload.get("output").and_then(Value::as_str)?;
    let output: Value = serde_json::from_str(output).ok()?;
    let output = output.as_object()?;
    if output.len() != 1 {
        return None;
    }
    let agents = output.get("agents")?.as_array()?;
    if agents.len() > AGENT_LIST_MEMBER_LIMIT {
        return None;
    }
    let mut paths = Vec::with_capacity(agents.len());
    for listed_agent in agents {
        let listed_agent = listed_agent.as_object()?;
        let path = listed_agent.get("agent_name")?.as_str()?;
        let status = listed_agent.get("agent_status")?.as_str()?;
        if status.is_empty() {
            return None;
        }
        paths.push(canonical_agent_path_or_none(path)?);
    }
    paths.sort_unstable();
    paths.dedup();
    Some(paths)
}
fn reasoning_summary(payload: &Value) -> Option<String> {
    payload
        .get("summary")
        .and_then(plain_text)
        .or_else(|| {
            payload
                .get("text")
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .map(|text| sanitise(&text.replace("**", "")))
}

fn receipt_claim(text: &str) -> Option<String> {
    text.lines()
        .find_map(|line| line.strip_prefix("status="))
        .map(sanitise)
}
fn summary_for_call(payload: &Value) -> String {
    let name = payload
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or("tool");
    let summary = if name == "exec" {
        payload
            .get("input")
            .and_then(Value::as_str)
            .and_then(summary_for_exec)
            .unwrap_or_else(|| "exec".into())
    } else if let Some(detail) = direct_tool_detail(name, payload) {
        format!("{} — {detail}", humanise_tool_name(name))
    } else {
        name.to_owned()
    };
    sanitise(&summary)
}

fn summary_for_exec(input: &str) -> Option<String> {
    const INPUT_SCAN_LIMIT: usize = 64 * 1024;
    let mut end = input.len().min(INPUT_SCAN_LIMIT);
    while !input.is_char_boundary(end) {
        end -= 1;
    }
    let input = &input[..end];
    let calls = nested_tool_names(input);
    if calls.is_empty() {
        return None;
    }
    if calls.len() == 1 {
        let name = &calls[0];
        let mut summary = humanise_tool_name(name);
        if let Some(key) = tool_detail_key(name) {
            if let Some(detail) = js_string_property(input, key) {
                summary.push_str(" — ");
                summary.push_str(&detail);
            }
        }
        return Some(summary);
    }

    let mut grouped = Vec::<(String, usize)>::new();
    for name in calls {
        let label = humanise_tool_name(&name);
        if let Some((_, count)) = grouped.iter_mut().find(|(known, _)| *known == label) {
            *count += 1;
        } else {
            grouped.push((label, 1));
        }
    }
    Some(
        grouped
            .into_iter()
            .map(|(label, count)| {
                if count == 1 {
                    label
                } else {
                    format!("{label} · {count} calls")
                }
            })
            .collect::<Vec<_>>()
            .join("; "),
    )
}

fn nested_tool_names(input: &str) -> Vec<String> {
    const CALL_LIMIT: usize = 32;
    let bytes = input.as_bytes();
    let mut names = Vec::new();
    let mut index = 0;
    let mut quote = None;

    while index < bytes.len() && names.len() < CALL_LIMIT {
        if let Some(terminator) = quote {
            if bytes[index] == b'\\' {
                index = (index + 2).min(bytes.len());
                continue;
            }
            if bytes[index] == terminator {
                quote = None;
            }
            index += 1;
            continue;
        }

        match bytes[index] {
            b'\'' | b'"' | b'`' => {
                quote = Some(bytes[index]);
                index += 1;
                continue;
            }
            b'/' if bytes.get(index + 1) == Some(&b'/') => {
                index += 2;
                while index < bytes.len() && bytes[index] != b'\n' {
                    index += 1;
                }
                continue;
            }
            b'/' if bytes.get(index + 1) == Some(&b'*') => {
                index += 2;
                while index + 1 < bytes.len() && !(bytes[index] == b'*' && bytes[index + 1] == b'/')
                {
                    index += 1;
                }
                index = (index + 2).min(bytes.len());
                continue;
            }
            _ => {}
        }

        if bytes[index..].starts_with(b"tools.") {
            let start = index + "tools.".len();
            let mut finish = start;
            while finish < bytes.len()
                && (bytes[finish].is_ascii_alphanumeric() || bytes[finish] == b'_')
            {
                finish += 1;
            }
            if finish > start {
                names.push(String::from_utf8_lossy(&bytes[start..finish]).into_owned());
                index = finish;
                continue;
            }
        }
        index += 1;
    }
    names
}

fn humanise_tool_name(name: &str) -> String {
    if name == "exec_command" {
        return "Command".into();
    }
    if let Some(rest) = name.strip_prefix("mcp__") {
        if let Some((server, tool)) = rest.split_once("__") {
            let tool = tool
                .strip_prefix(server)
                .and_then(|suffix| suffix.strip_prefix('_'))
                .unwrap_or(tool);
            return format!("{} {}", humanise_identifier(server), tool.replace('_', " "));
        }
    }
    humanise_identifier(name)
}

fn humanise_identifier(value: &str) -> String {
    let value = value.replace("__", " ").replace('_', " ");
    let mut characters = value.chars();
    match characters.next() {
        Some(first) => first.to_uppercase().collect::<String>() + characters.as_str(),
        None => "Tool".into(),
    }
}

fn tool_detail_key(name: &str) -> Option<&'static str> {
    match name {
        "exec_command" => Some("cmd"),
        "send_message" | "followup_task" => Some("target"),
        "spawn_agent" => Some("task_name"),
        "list_agents" => Some("path_prefix"),
        "view_image" => Some("path"),
        "read_mcp_resource" => Some("uri"),
        "request_plugin_install" => Some("plugin_id"),
        _ if name.ends_with("search") => Some("query"),
        _ if name.ends_with("read") => Some("path"),
        _ => None,
    }
}

fn direct_tool_detail(name: &str, payload: &Value) -> Option<String> {
    let key = tool_detail_key(name)?;
    let parsed = payload
        .get("arguments")
        .and_then(Value::as_str)
        .and_then(|arguments| serde_json::from_str::<Value>(arguments).ok());
    let arguments = parsed
        .as_ref()
        .or_else(|| payload.get("arguments"))
        .and_then(Value::as_object)?;
    arguments.get(key).and_then(Value::as_str).map(sanitise)
}

fn js_string_property(source: &str, key: &str) -> Option<String> {
    let bytes = source.as_bytes();
    for (index, _) in source.match_indices(key) {
        if index > 0 && (bytes[index - 1].is_ascii_alphanumeric() || bytes[index - 1] == b'_') {
            continue;
        }
        let mut cursor = index + key.len();
        if bytes
            .get(cursor)
            .is_some_and(|byte| matches!(byte, b'\'' | b'"'))
        {
            cursor += 1;
        }
        while bytes.get(cursor).is_some_and(u8::is_ascii_whitespace) {
            cursor += 1;
        }
        if bytes.get(cursor) != Some(&b':') {
            continue;
        }
        cursor += 1;
        while bytes.get(cursor).is_some_and(u8::is_ascii_whitespace) {
            cursor += 1;
        }
        let quote = *bytes.get(cursor)?;
        if !matches!(quote, b'\'' | b'"' | b'`') {
            continue;
        }
        if quote == b'"' {
            let mut finish = cursor + 1;
            while finish < bytes.len() {
                if bytes[finish] == b'\\' {
                    finish = (finish + 2).min(bytes.len());
                    continue;
                }
                if bytes[finish] == b'"' {
                    return serde_json::from_str::<String>(&source[cursor..=finish])
                        .ok()
                        .map(|value| sanitise(&value));
                }
                finish += 1;
            }
            return None;
        }

        let mut value = String::new();
        let mut escaped = false;
        for character in source[cursor + 1..].chars() {
            if escaped {
                value.push(match character {
                    'n' => '\n',
                    'r' => '\r',
                    't' => ' ',
                    other => other,
                });
                escaped = false;
            } else if character == '\\' {
                escaped = true;
            } else if character == quote as char {
                return Some(sanitise(&value));
            } else {
                value.push(character);
            }
        }
        return None;
    }
    None
}

pub fn has_malformed_call_id(record: &Value) -> bool {
    let payload = &record["payload"];
    let requires_call_id = match record["type"].as_str() {
        Some("response_item") => matches!(
            payload["type"].as_str(),
            Some("custom_tool_call")
                | Some("function_call")
                | Some("custom_tool_call_output")
                | Some("function_call_output")
        ),
        Some("event_msg") => {
            matches!(
                payload["type"].as_str(),
                Some("item_started") | Some("item_completed")
            ) && matches!(
                payload["item"]["type"].as_str(),
                Some("CommandExecution") | Some("McpToolCall") | Some("CollabAgentToolCall")
            )
        }
        _ => false,
    };
    requires_call_id
        && !payload
            .get("call_id")
            .or_else(|| payload.get("item").and_then(|item| item.get("call_id")))
            .or_else(|| payload.get("item").and_then(|item| item.get("id")))
            .and_then(Value::as_str)
            .is_some_and(|id| !id.is_empty())
}
pub fn has_malformed_communication(record: &Value) -> bool {
    let payload = &record["payload"];
    if record["type"].as_str() != Some("response_item")
        || payload["type"].as_str() != Some("agent_message")
    {
        return false;
    }
    !payload
        .get("author")
        .and_then(Value::as_str)
        .is_some_and(|value| !value.is_empty())
        || !payload
            .get("recipient")
            .and_then(Value::as_str)
            .is_some_and(|value| !value.is_empty())
        || !payload.get("content").is_some_and(Value::is_array)
}

fn has_meaningful_communication_content(payload: &Value) -> bool {
    payload["content"].as_array().is_some_and(|content| {
        content.iter().any(|part| {
            part["type"].as_str() == Some("input_text")
                && part["text"]
                    .as_str()
                    .is_some_and(|text| !text.trim().is_empty())
        })
    })
}

fn compact_agent_reference(reference: &str) -> &str {
    if reference == "/root" {
        "root"
    } else {
        reference.strip_prefix("/root/").unwrap_or(reference)
    }
}

pub fn reduce(agent: &mut AgentState, record: &Value) -> bool {
    let ordinal = record.get("ordinal").and_then(Value::as_u64);
    let timestamp = parse_time(record.get("timestamp"));
    agent.last_ordinal = ordinal.or(agent.last_ordinal);
    let payload = &record["payload"];
    match record["type"].as_str() {
        Some("turn_context") => {
            if let Some(model) = payload.get("model").and_then(Value::as_str) {
                agent.model = Some(sanitise(model));
            }
            if let Some(effort) = payload.get("effort").and_then(Value::as_str) {
                agent.reasoning_effort = Some(sanitise(effort));
            }
            true
        }
        Some("event_msg") => match payload["type"].as_str() {
            Some("task_started") => {
                agent.latest_turn = TurnState {
                    turn_id: payload
                        .get("turn_id")
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                    status: TurnStatus::Running,
                    started_at: parse_time(payload.get("started_at")).or(timestamp),
                    completed_at: None,
                };
                agent.close_in_flight_calls(timestamp);
                agent.next_call_sequence = 0;
                agent.last_reasoning_summary = None;
                agent.last_message = None;
                agent.final_message = None;
                agent.result_status_claim = None;
                agent.last_communication = None;
                agent.last_activity_at = timestamp;
                agent.record_interaction(
                    InteractionKind::Lifecycle,
                    "turn started",
                    agent.latest_turn.started_at,
                    ordinal,
                );
                true
            }
            Some("task_complete") => {
                agent.latest_turn.status = TurnStatus::Completed;
                agent.latest_turn.completed_at =
                    parse_time(payload.get("completed_at")).or(timestamp);
                agent.close_in_flight_calls(agent.latest_turn.completed_at);
                if let Some(raw) = payload.get("last_agent_message").and_then(Value::as_str) {
                    agent.result_status_claim = receipt_claim(raw);
                    agent.final_message = Some(sanitise(raw));
                } else {
                    agent.final_message = agent.last_message.clone();
                }
                if let Some(final_message) = agent.final_message.clone() {
                    agent.record_interaction(
                        InteractionKind::Message,
                        final_message,
                        agent.latest_turn.completed_at,
                        ordinal,
                    );
                }
                agent.record_interaction(
                    InteractionKind::Lifecycle,
                    "turn completed",
                    agent.latest_turn.completed_at,
                    ordinal,
                );
                agent.last_activity_at = timestamp;
                true
            }
            Some("turn_aborted") => {
                agent.latest_turn.status = TurnStatus::Interrupted;
                agent.close_in_flight_calls(timestamp);
                agent.last_activity_at = timestamp;
                agent.record_interaction(
                    InteractionKind::Lifecycle,
                    "turn interrupted",
                    timestamp,
                    ordinal,
                );
                true
            }
            Some("error") => {
                agent.latest_turn.status = TurnStatus::Errored;
                agent.close_in_flight_calls(timestamp);
                agent.last_activity_at = timestamp;
                agent.record_interaction(
                    InteractionKind::Lifecycle,
                    "turn errored",
                    timestamp,
                    ordinal,
                );
                true
            }
            Some("item_completed") | Some("item_started") => {
                let item = &payload["item"];
                match item["type"].as_str() {
                    Some("Reasoning") => {
                        let summary = item
                            .get("summary_text")
                            .and_then(Value::as_array)
                            .and_then(|a| {
                                a.iter().filter_map(Value::as_str).find(|s| !s.is_empty())
                            })
                            .map(|s| sanitise(&s.replace("**", "")));
                        if let Some(summary) = summary {
                            agent.record_interaction(
                                InteractionKind::Reasoning,
                                &summary,
                                timestamp,
                                ordinal,
                            );
                            agent.last_reasoning_summary = Some(summary);
                            agent.last_activity_at = timestamp;
                        }
                    }
                    Some("AgentMessage") => {
                        if let Some(text) = plain_text(&item["content"]) {
                            let text = sanitise(&text);
                            agent.result_status_claim = receipt_claim(&text);
                            agent.record_interaction(
                                InteractionKind::Message,
                                &text,
                                timestamp,
                                ordinal,
                            );
                            agent.last_message = Some(text);
                            agent.last_activity_at = timestamp;
                        }
                    }
                    Some("CommandExecution")
                    | Some("McpToolCall")
                    | Some("CollabAgentToolCall") => {
                        if let Some(id) = item
                            .get("call_id")
                            .or_else(|| item.get("id"))
                            .and_then(Value::as_str)
                        {
                            if payload["type"] == "item_completed" {
                                if let Some(call) = agent.in_flight_calls.remove(id) {
                                    agent.finish_tool_interaction(
                                        call,
                                        timestamp,
                                        ToolInteractionState::Returned,
                                    );
                                }
                            }
                        }
                        agent.last_activity_at = timestamp;
                    }
                    Some("SubAgentActivity") | Some("FileChange") => {
                        agent.last_activity_at = timestamp
                    }
                    Some(_) | None => return false,
                }
                true
            }
            Some("agent_message") => {
                if let Some(raw) = payload
                    .get("message")
                    .or_else(|| payload.get("text"))
                    .and_then(Value::as_str)
                {
                    let message = sanitise(raw);
                    agent.result_status_claim = receipt_claim(&message);
                    agent.record_interaction(
                        InteractionKind::Message,
                        &message,
                        timestamp,
                        ordinal,
                    );
                    agent.last_message = Some(message);
                    agent.last_activity_at = timestamp;
                }
                true
            }
            Some("agent_reasoning") => {
                if let Some(summary) = reasoning_summary(payload) {
                    agent.record_interaction(
                        InteractionKind::Reasoning,
                        &summary,
                        timestamp,
                        ordinal,
                    );
                    agent.last_reasoning_summary = Some(summary);
                    agent.last_activity_at = timestamp;
                }
                true
            }
            Some("token_count") => true,
            Some(_) | None => false,
        },
        Some("response_item") => match payload["type"].as_str() {
            Some("custom_tool_call") | Some("function_call") => {
                let call_id = payload["call_id"]
                    .as_str()
                    .expect("validated call_id")
                    .to_owned();
                let sequence = agent.next_call_sequence;
                agent.next_call_sequence += 1;
                let tool_name = payload
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("tool")
                    .to_owned();
                agent.agent_list_calls.remove(&call_id);
                if let Some(scope) = parse_agent_list_scope_or_none(payload) {
                    agent.agent_list_calls.insert(call_id.clone(), scope);
                }
                let summary = summary_for_call(payload);
                let interaction_sequence =
                    agent.start_tool_interaction(&summary, timestamp, ordinal);
                agent.in_flight_calls.insert(
                    call_id,
                    InFlightCall {
                        summary,
                        tool_name,
                        started_at: timestamp,
                        ordinal,
                        sequence,
                        interaction_sequence,
                    },
                );
                agent.last_activity_at = timestamp;
                true
            }
            Some("custom_tool_call_output") | Some("function_call_output") => {
                let id = payload["call_id"].as_str().expect("validated call_id");
                let agent_list_scope = agent.agent_list_calls.remove(id);
                if let Some(call) = agent.in_flight_calls.remove(id) {
                    if call.tool_name == "list_agents" {
                        if let (Some(scope), Some(observed_at), Some(agent_paths)) = (
                            agent_list_scope,
                            timestamp,
                            parse_agent_list_paths_or_none(payload),
                        ) {
                            agent.record_agent_list_snapshot(scope, observed_at, agent_paths);
                        }
                    }
                    if call.tool_name == "spawn_agent" {
                        if let (Some(observed_at), Some(agent_path)) =
                            (timestamp, parse_spawned_agent_path_or_none(payload))
                        {
                            agent.record_spawned_agent_hint(SpawnedAgentHint {
                                agent_path,
                                observed_at,
                            });
                        }
                    }
                    agent.finish_tool_interaction(call, timestamp, ToolInteractionState::Returned);
                }
                agent.last_activity_at = timestamp;
                true
            }
            Some("reasoning") => {
                if let Some(summary) = reasoning_summary(payload) {
                    agent.record_interaction(
                        InteractionKind::Reasoning,
                        &summary,
                        timestamp,
                        ordinal,
                    );
                    agent.last_reasoning_summary = Some(summary);
                    agent.last_activity_at = timestamp;
                }
                true
            }
            Some("message") => {
                if payload["role"].as_str() == Some("assistant") {
                    if let Some(text) = plain_text(&payload["content"]) {
                        let text = sanitise(&text);
                        agent.result_status_claim = receipt_claim(&text);
                        agent.record_interaction(
                            InteractionKind::Message,
                            &text,
                            timestamp,
                            ordinal,
                        );
                        agent.last_message = Some(text);
                        agent.last_activity_at = timestamp;
                    }
                }
                true
            }
            Some("agent_message") => {
                if !has_malformed_communication(record)
                    && has_meaningful_communication_content(payload)
                {
                    let author = payload["author"].as_str().expect("validated author");
                    let recipient = payload["recipient"].as_str().expect("validated recipient");
                    let communication = sanitise(&format!(
                        "message {} → {}",
                        sanitise(compact_agent_reference(author)),
                        sanitise(compact_agent_reference(recipient))
                    ));
                    agent.record_interaction(
                        InteractionKind::Communication,
                        &communication,
                        timestamp,
                        ordinal,
                    );
                    agent.last_communication = Some(communication);
                    agent.last_activity_at = timestamp;
                }
                true
            }
            Some(_) | None => false,
        },
        Some("session_meta") => true,
        Some(_) | None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn r(s: &str) -> Value {
        serde_json::from_str(s).unwrap()
    }
    fn agent() -> AgentState {
        AgentState::new("t".into(), "0.149.0".into())
    }

    fn record_agent_list_snapshot(
        agent: &mut AgentState,
        id: &str,
        arguments: &str,
        output: &str,
        call_at: i64,
        output_at: i64,
    ) {
        assert!(reduce(
            agent,
            &serde_json::json!({
                "timestamp": call_at,
                "type": "response_item",
                "payload": {
                    "type": "function_call",
                    "call_id": id,
                    "name": "list_agents",
                    "arguments": arguments
                }
            }),
        ));
        assert!(reduce(
            agent,
            &serde_json::json!({
                "timestamp": output_at,
                "type": "response_item",
                "payload": {
                    "type": "function_call_output",
                    "call_id": id,
                    "output": output
                }
            }),
        ));
    }
    #[test]
    fn lifecycle_reactivation_and_final_preference() {
        let mut a = agent();
        reduce(
            &mut a,
            &r(
                r#"{"timestamp":"2024-01-01T00:00:00Z","type":"event_msg","payload":{"type":"task_started","turn_id":"1"}}"#,
            ),
        );
        reduce(
            &mut a,
            &r(
                r#"{"timestamp":"2024-01-01T00:00:01Z","type":"response_item","payload":{"type":"message","role":"assistant","content":[{"text":"old"}]}}"#,
            ),
        );
        reduce(
            &mut a,
            &r(
                r#"{"timestamp":"2024-01-01T00:00:02Z","type":"event_msg","payload":{"type":"task_complete","last_agent_message":"status=BLOCKED"}} "#,
            ),
        );
        assert_eq!(a.final_message.as_deref(), Some("status=BLOCKED"));
        assert_eq!(a.result_status_claim.as_deref(), Some("BLOCKED"));
        reduce(
            &mut a,
            &r(
                r#"{"timestamp":"2024-01-01T00:00:03Z","type":"event_msg","payload":{"type":"task_started","turn_id":"2"}}"#,
            ),
        );
        assert_eq!(a.latest_turn.status, TurnStatus::Running);
        assert!(a.final_message.is_none());
        assert_eq!(
            a.interactions
                .iter()
                .map(|interaction| interaction.summary.as_str())
                .collect::<Vec<_>>(),
            [
                "turn started",
                "old",
                "status=BLOCKED",
                "turn completed",
                "turn started"
            ]
        );
    }
    #[test]
    fn calls_overlap_deterministically() {
        let mut a = agent();
        for (id, name) in [("a", "one"), ("b", "two")] {
            reduce(
                &mut a,
                &r(&format!(
                    r#"{{"timestamp":"2024-01-01T00:00:00Z","type":"response_item","payload":{{"type":"custom_tool_call","call_id":"{id}","name":"{name}"}}}}"#
                )),
            );
        }
        assert_eq!(a.current_activity(), Some("two"));
        reduce(
            &mut a,
            &r(
                r#"{"timestamp":"2024-01-01T00:00:01Z","type":"response_item","payload":{"type":"custom_tool_call_output","call_id":"a"}}"#,
            ),
        );
        assert_eq!(a.current_activity(), Some("two"));
        assert_eq!(a.interactions.len(), 2);
        assert_eq!(a.interactions[0].summary, "one");
        assert_eq!(
            a.interactions[0].tool_state,
            Some(ToolInteractionState::Returned)
        );
        assert_eq!(a.interactions[1].summary, "two");
        assert_eq!(
            a.interactions[1].tool_state,
            Some(ToolInteractionState::Open)
        );

        reduce(
            &mut a,
            &r(
                r#"{"timestamp":"2024-01-01T00:00:02Z","type":"event_msg","payload":{"type":"task_complete"}}"#,
            ),
        );
        assert_eq!(
            a.interactions[1].tool_state,
            Some(ToolInteractionState::EndedWithoutReturn)
        );
        assert_eq!(
            a.interactions[1].finished_at,
            DateTime::parse_from_rfc3339("2024-01-01T00:00:02Z")
                .ok()
                .map(|time| time.with_timezone(&Utc))
        );
    }

    #[test]
    fn list_agents_snapshots_are_complete_later_bounded_and_private() {
        let mut observer = agent();
        observer.agent_path = Some("/root".into());
        record_agent_list_snapshot(
            &mut observer,
            "full",
            "{}",
            r#"{"agents":[{"agent_name":"/root","agent_status":"secret-status"}]}"#,
            10,
            11,
        );
        assert_eq!(observer.agent_list_snapshots.len(), 1);
        assert!(!format!("{observer:?}").contains("secret-status"));

        let mut target = AgentState::new("old".into(), "0.149.0".into());
        target.parent_thread_id = Some("root".into());
        target.agent_path = Some("/root/old".into());
        target.latest_turn.status = TurnStatus::Running;
        target.last_activity_at = DateTime::from_timestamp(10, 0);

        let mut session = SessionState::default();
        session.agents.insert("root".into(), observer);
        session.agents.insert("old".into(), target);
        assert_eq!(
            session.stale_evidence(&session.agents["old"]),
            Some(StaleEvidence::LaterAgentListSnapshot)
        );

        session.agents.get_mut("old").unwrap().last_activity_at = DateTime::from_timestamp(12, 0);
        assert_eq!(session.stale_evidence(&session.agents["old"]), None);

        let mut scoped = agent();
        record_agent_list_snapshot(
            &mut scoped,
            "scoped",
            r#"{"path_prefix":"/root/old"}"#,
            r#"{"agents":[]}"#,
            20,
            21,
        );
        let mut target = session.agents.remove("old").unwrap();
        target.last_activity_at = DateTime::from_timestamp(20, 0);
        let mut scoped_session = SessionState::default();
        scoped_session.agents.insert("observer".into(), scoped);
        scoped_session.agents.insert("old".into(), target);
        assert_eq!(
            scoped_session.stale_evidence(&scoped_session.agents["old"]),
            Some(StaleEvidence::LaterAgentListSnapshot)
        );

        let mut unrelated = agent();
        record_agent_list_snapshot(
            &mut unrelated,
            "unrelated",
            r#"{"path_prefix":"/root/other"}"#,
            r#"{"agents":[]}"#,
            20,
            21,
        );
        let mut unrelated_session = SessionState::default();
        unrelated_session
            .agents
            .insert("observer".into(), unrelated);
        unrelated_session
            .agents
            .insert("old".into(), scoped_session.agents.remove("old").unwrap());
        assert_eq!(
            unrelated_session.stale_evidence(&unrelated_session.agents["old"]),
            None
        );

        let exact_prefix = AgentListScope::PathPrefix("/root/foo".into());
        assert!(exact_prefix.covers("/root/foo"));
        assert!(exact_prefix.covers("/root/foo/child"));
        assert!(!exact_prefix.covers("/root/foobar"));

        let mut malformed = agent();
        record_agent_list_snapshot(&mut malformed, "bad", "{}", "not json", 30, 31);
        assert!(malformed.agent_list_snapshots.is_empty());
        record_agent_list_snapshot(
            &mut malformed,
            "partial",
            "{}",
            r#"{"agents":[{"agent_status":"running"}]}"#,
            32,
            33,
        );
        assert!(malformed.agent_list_snapshots.is_empty());

        let oversized_agents = (0..=AGENT_LIST_MEMBER_LIMIT)
            .map(|_| {
                serde_json::json!({
                    "agent_name": "/root/member",
                    "agent_status": "running"
                })
            })
            .collect::<Vec<_>>();
        let oversized_output = serde_json::json!({"agents": oversized_agents}).to_string();
        record_agent_list_snapshot(&mut malformed, "oversized", "{}", &oversized_output, 34, 35);
        assert!(malformed.agent_list_snapshots.is_empty());

        for index in 0..=AGENT_LIST_SNAPSHOT_LIMIT {
            record_agent_list_snapshot(
                &mut malformed,
                &format!("bounded-{index}"),
                "{}",
                r#"{"agents":[{"agent_name":"/root","agent_status":"running"}]}"#,
                100 + index as i64 * 2,
                101 + index as i64 * 2,
            );
        }
        assert_eq!(
            malformed.agent_list_snapshots.len(),
            AGENT_LIST_SNAPSHOT_LIMIT
        );
        assert_eq!(
            malformed.agent_list_snapshots.front().unwrap().observed_at,
            DateTime::from_timestamp(103, 0).unwrap()
        );
    }
    #[test]
    fn reasoning_sanitising_and_bookkeeping() {
        let mut a = agent();
        assert!(reduce(
            &mut a,
            &r(r#"{"type":"turn_context","payload":{"model":"gpt-5.6-sol","effort":"high"}}"#,),
        ));
        assert_eq!(a.model.as_deref(), Some("gpt-5.6-sol"));
        assert_eq!(a.reasoning_effort.as_deref(), Some("high"));

        let record = serde_json::json!({
            "timestamp":"2024-01-01T00:00:00Z",
            "type":"event_msg",
            "payload":{"type":"item_completed","item":{
                "type":"Reasoning","summary_text":["**safe\u{1b}[31m text**"]
            }}
        });
        reduce(&mut a, &record);
        assert_eq!(a.last_reasoning_summary.as_deref(), Some("safe text"));

        assert!(reduce(
            &mut a,
            &r(
                r#"{"timestamp":"2024-01-01T00:00:01Z","type":"event_msg","payload":{"type":"agent_reasoning","text":"**current summary**"}}"#,
            ),
        ));
        assert_eq!(a.last_reasoning_summary.as_deref(), Some("current summary"));

        assert!(reduce(
            &mut a,
            &r(
                r#"{"timestamp":"2024-01-01T00:00:02Z","type":"response_item","payload":{"type":"reasoning","summary":[{"type":"summary_text","text":"response summary"}]}}"#,
            ),
        ));
        assert_eq!(
            a.last_reasoning_summary.as_deref(),
            Some("response summary")
        );

        let time = a.last_activity_at;
        reduce(
            &mut a,
            &r(
                r#"{"timestamp":"2024-01-02T00:00:00Z","type":"event_msg","payload":{"type":"token_count"}}"#,
            ),
        );
        assert_eq!(a.last_activity_at, time);
    }

    #[test]
    fn interaction_history_is_bounded_and_deduplicates_adjacent_messages() {
        let mut a = agent();
        for ordinal in 0..INTERACTION_LIMIT + 4 {
            assert!(reduce(
                &mut a,
                &serde_json::json!({
                    "ordinal": ordinal,
                    "type": "event_msg",
                    "payload": {"type": "agent_message", "message": format!("message {ordinal}")}
                }),
            ));
        }
        assert_eq!(a.interactions.len(), INTERACTION_LIMIT);
        assert_eq!(a.interactions.front().unwrap().summary, "message 4");
        assert_eq!(
            a.interactions.back().unwrap().summary,
            format!("message {}", INTERACTION_LIMIT + 3)
        );

        let len = a.interactions.len();
        assert!(reduce(
            &mut a,
            &serde_json::json!({
                "ordinal": INTERACTION_LIMIT + 4,
                "type": "event_msg",
                "payload": {
                    "type": "agent_message",
                    "message": format!("message {}", INTERACTION_LIMIT + 3)
                }
            }),
        ));
        assert_eq!(a.interactions.len(), len);
        assert_eq!(
            a.interactions.back().unwrap().ordinal,
            Some((INTERACTION_LIMIT + 4) as u64)
        );
    }
    #[test]
    fn encrypted_message_is_an_accepted_envelope() {
        let mut a = agent();
        assert!(reduce(
            &mut a,
            &r(
                r#"{"type":"response_item","payload":{"type":"message","role":"developer","content":[{"type":"encrypted_content","encrypted_content":"redacted"}]}}"#
            )
        ));
    }
    #[test]
    fn sanitise_is_utf8_bounded_and_diagnostics_are_bounded() {
        let value = sanitise(&format!(
            "metadata\u{1b}[31m\n\tsession\u{7}{}",
            "é".repeat(300)
        ));
        assert!(value.len() <= TEXT_LIMIT);
        assert!(value.is_char_boundary(value.len()));
        assert!(!value.contains('\u{1b}'));
        assert!(!value.contains('\n'));
        assert!(!value.contains('\t'));
        assert!(!value.chars().any(char::is_control));
        assert!(value.starts_with("metadata↩�session�"));
        assert_eq!(sanitise("a\nb\r\nc\rd"), "a↩b↩c↩d");
        assert!(value.ends_with('…'));
        assert_eq!(sanitise("short"), "short");
        let mut health = DataHealth::default();
        for offset in 0..25 {
            health.diagnostic(DiagnosticSample {
                rollout_path: PathBuf::from(if offset == 24 {
                    format!("hostile\n\u{1b}[31m{}", "x".repeat(TEXT_LIMIT))
                } else {
                    "safe".into()
                }),
                byte_offset: offset,
                cli_version: (offset == 24)
                    .then(|| format!("version\n\u{1b}[31m{}", "x".repeat(TEXT_LIMIT))),
                kind: "test".into(),
                ordinal: None,
                detail: None,
            });
        }
        assert_eq!(health.recent_diagnostics.len(), DIAGNOSTIC_LIMIT);
        assert_eq!(health.recent_diagnostics.front().unwrap().byte_offset, 5);
        let hostile = health.recent_diagnostics.back().unwrap();
        for display in [
            hostile.rollout_path.to_string_lossy().as_ref(),
            hostile.cli_version.as_deref().unwrap(),
        ] {
            assert!(display.len() <= TEXT_LIMIT);
            assert!(!display.chars().any(char::is_control));
            assert!(!display.contains('\u{1b}'));
        }
    }
    #[test]
    fn spawn_outputs_create_bounded_discovery_hints() {
        let mut state = agent();
        for index in 0..=SPAWN_HINT_LIMIT {
            let call_id = format!("spawn-{index}");
            assert!(reduce(
                &mut state,
                &serde_json::json!({
                    "timestamp": index as i64,
                    "type": "response_item",
                    "payload": {
                        "type": "function_call",
                        "name": "spawn_agent",
                        "call_id": call_id,
                        "arguments": "{}"
                    }
                }),
            ));
            assert!(reduce(
                &mut state,
                &serde_json::json!({
                    "timestamp": index as i64 + 1,
                    "type": "response_item",
                    "payload": {
                        "type": "function_call_output",
                        "call_id": call_id,
                        "output": serde_json::json!({
                            "task_name": format!("/root/child-{index}"),
                            "message": "not retained"
                        }).to_string()
                    }
                }),
            ));
        }

        let hints = SessionState {
            agents: [(state.thread_id.clone(), state)].into(),
            ..SessionState::default()
        }
        .take_spawned_agent_hints();
        assert_eq!(hints.len(), SPAWN_HINT_LIMIT);
        assert_eq!(hints.first().unwrap().agent_path, "/root/child-1");
        assert_eq!(
            hints.last().unwrap().agent_path,
            format!("/root/child-{SPAWN_HINT_LIMIT}")
        );
        assert!(hints
            .iter()
            .all(|hint| !hint.agent_path.contains("retained")));
    }

    #[test]
    fn malformed_call_ids_and_bounded_tool_summaries() {
        let missing = r(
            r#"{"type":"response_item","payload":{"type":"custom_tool_call","name":"exec","input":"secret-token"}}"#,
        );
        let invalid = r(
            r#"{"type":"response_item","payload":{"type":"custom_tool_call_output","call_id":7}}"#,
        );
        assert!(has_malformed_call_id(&missing));
        assert!(has_malformed_call_id(&invalid));

        let valid = serde_json::json!({
            "type": "response_item",
            "payload": {
                "type": "custom_tool_call",
                "call_id": "c",
                "name": "exec",
                "input": r#"const [one, two, three] = await Promise.all([
                    tools.mcp__tilth__tilth_search({query:"AgentInteraction"}),
                    tools.mcp__tilth__tilth_search({query:"reduce"}),
                    tools.mcp__tilth__tilth_read({path:"src/model.rs"})
                ]);"#
            }
        });
        assert!(!has_malformed_call_id(&valid));
        let mut state = agent();
        assert!(reduce(&mut state, &valid));
        assert_eq!(
            state.in_flight_calls["c"].summary,
            "Tilth search · 2 calls; Tilth read"
        );
        assert_eq!(
            state.interactions.back().unwrap().summary,
            "Tilth search · 2 calls; Tilth read"
        );

        let detailed = serde_json::json!({
            "type": "response_item",
            "payload": {
                "type": "custom_tool_call",
                "call_id": "d",
                "name": "exec",
                "input": r#"const result = await tools.mcp__tilth__tilth_search({
                    query:"agentop|list_agents|stale"
                });"#
            }
        });
        assert!(reduce(&mut state, &detailed));
        assert_eq!(
            state.interactions.back().unwrap().summary,
            "Tilth search — agentop|list_agents|stale"
        );

        let command = serde_json::json!({
            "name": "exec",
            "input": r#"const result = await tools.exec_command({
                cmd:"cargo test --all-targets\ncargo test"
            });"#
        });
        assert_eq!(
            summary_for_call(&command),
            "Command — cargo test --all-targets↩cargo test"
        );

        let direct = serde_json::json!({
            "name": "send_message",
            "arguments": r#"{"target":"/root/reviewer","message":"private body"}"#
        });
        assert_eq!(summary_for_call(&direct), "Send message — /root/reviewer");
        assert!(!summary_for_call(&direct).contains("private body"));

        let long_query = format!(
            r#"const result = await tools.mcp__tilth__tilth_search({{query:"{}"}});"#,
            "x".repeat(TEXT_LIMIT * 2)
        );
        let bounded = summary_for_call(&serde_json::json!({
            "name": "exec",
            "input": long_query
        }));
        assert!(bounded.len() <= TEXT_LIMIT);
        assert!(bounded.ends_with('…'));
    }
    #[test]
    fn exact_wait_agent_tracks_only_newest_running_call() {
        let mut state = agent();
        reduce(
            &mut state,
            &r(r#"{"type":"event_msg","payload":{"type":"task_started","turn_id":"t"}}"#),
        );
        reduce(
            &mut state,
            &r(r#"{"type":"event_msg","payload":{"type":"agent_message","message":"wait_agent"}}"#),
        );
        assert!(!state.is_waiting_on_agent());

        reduce(
            &mut state,
            &r(
                r#"{"type":"response_item","payload":{"type":"custom_tool_call","call_id":"w","name":"wait_agent"}}"#,
            ),
        );
        assert!(state.is_waiting_on_agent());
        assert_eq!(state.current_activity(), Some("wait_agent"));

        reduce(
            &mut state,
            &r(
                r#"{"type":"response_item","payload":{"type":"custom_tool_call","call_id":"n","name":"exec"}}"#,
            ),
        );
        assert!(!state.is_waiting_on_agent());
        reduce(
            &mut state,
            &r(
                r#"{"type":"response_item","payload":{"type":"custom_tool_call_output","call_id":"n"}}"#,
            ),
        );
        assert!(state.is_waiting_on_agent());
        reduce(
            &mut state,
            &r(
                r#"{"type":"response_item","payload":{"type":"custom_tool_call_output","call_id":"w"}}"#,
            ),
        );
        assert!(!state.is_waiting_on_agent());
    }

    #[test]
    fn alpha_4_1_representative_sequence() {
        let mut state = AgentState::new("child".into(), "0.149.0-alpha.4.1".into());
        for record in [
            r#"{"timestamp":"2026-08-24T20:20:51Z","type":"event_msg","payload":{"type":"task_started","turn_id":"t"}}"#,
            r#"{"timestamp":"2026-08-24T20:20:52Z","type":"event_msg","payload":{"type":"agent_message","message":"older output"}}"#,
            r#"{"timestamp":"2026-08-24T20:20:53Z","type":"response_item","payload":{"type":"custom_tool_call","call_id":"c","name":"exec","input":"private"}}"#,
            r#"{"timestamp":"2026-08-24T20:20:54Z","type":"response_item","payload":{"type":"custom_tool_call_output","call_id":"c","output":"private"}}"#,
            r#"{"timestamp":"2026-08-24T20:20:55Z","type":"response_item","payload":{"type":"agent_message","author":"/root","recipient":"/root/parent/child","content":[{"type":"input_text","text":"incoming private"},{"type":"encrypted_content","encrypted_content":"ciphertext"}]}}"#,
            r#"{"timestamp":"2026-08-24T20:20:56Z","type":"event_msg","payload":{"type":"task_complete","last_agent_message":"status=CANDIDATE"}}"#,
        ] {
            assert!(reduce(&mut state, &r(record)));
        }
        assert_eq!(state.latest_turn.status, TurnStatus::Completed);
        assert!(state.in_flight_calls.is_empty());
        assert_eq!(state.last_message.as_deref(), Some("older output"));
        assert_eq!(
            state.last_communication.as_deref(),
            Some("message root → parent/child")
        );
        assert_eq!(state.final_message.as_deref(), Some("status=CANDIDATE"));
        assert_eq!(state.result_status_claim.as_deref(), Some("CANDIDATE"));
        let meaningful_activity = state.last_activity_at;
        let communication = state.last_communication.clone();
        for malformed in [
            r#"{"type":"response_item","payload":{"type":"agent_message","recipient":"child","content":[]}}"#,
            r#"{"type":"response_item","payload":{"type":"agent_message","author":"","recipient":"child","content":[]}}"#,
            r#"{"type":"response_item","payload":{"type":"agent_message","author":"parent","recipient":"","content":[]}}"#,
        ] {
            assert!(has_malformed_communication(&r(malformed)));
        }
        let encrypted_only = r(
            r#"{"timestamp":"2026-08-24T20:21:00Z","type":"response_item","payload":{"type":"agent_message","author":"parent","recipient":"child","content":[{"type":"encrypted_content","encrypted_content":"new-ciphertext"}]}}"#,
        );
        assert!(!has_malformed_communication(&encrypted_only));
        assert!(reduce(&mut state, &encrypted_only));
        assert_eq!(state.last_communication, communication);
        assert_eq!(state.last_activity_at, meaningful_activity);
        let debug = format!("{state:?}");
        assert!(!debug.contains("incoming private"));
        assert!(!debug.contains("ciphertext"));
    }
}
