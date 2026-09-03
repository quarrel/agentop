use chrono::{DateTime, Utc};
use serde_json::Value;
use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;

const TEXT_LIMIT: usize = 512;
const DIAGNOSTIC_LIMIT: usize = 20;

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
}
#[derive(Debug, Clone)]
pub struct DiagnosticSample {
    pub rollout_path: PathBuf,
    pub byte_offset: u64,
    pub cli_version: Option<String>,
    pub kind: String,
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
    pub schema_catalogued: bool,
    pub schema_family: Option<String>,
    pub coverage: CoverageLevel,
    pub own_history_start_ordinal: Option<u64>,
    pub latest_turn: TurnState,
    pub last_activity_at: Option<DateTime<Utc>>,
    pub next_call_sequence: u64,
    pub in_flight_calls: HashMap<String, InFlightCall>,
    pub last_reasoning_summary: Option<String>,
    pub last_message: Option<String>,
    pub final_message: Option<String>,
    pub result_status_claim: Option<String>,
    pub last_communication: Option<String>,
    pub last_ordinal: Option<u64>,
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
            schema_catalogued: false,
            schema_family: None,
            coverage: CoverageLevel::Ingestable,
            own_history_start_ordinal: None,
            latest_turn: TurnState::default(),
            last_activity_at: None,
            next_call_sequence: 0,
            in_flight_calls: HashMap::new(),
            last_reasoning_summary: None,
            last_message: None,
            final_message: None,
            result_status_claim: None,
            last_communication: None,
            last_ordinal: None,
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
}

#[derive(Debug, Default)]
pub struct SessionState {
    pub session_id: String,
    pub cwd: Option<PathBuf>,
    pub started_at: Option<DateTime<Utc>>,
    pub agents: HashMap<String, AgentState>,
    pub data_health: DataHealth,
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
    for ch in input.chars() {
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
        let emitted = if ch.is_control() { '�' } else { ch };
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
    sanitise(if name == "exec" { "running exec" } else { name })
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

pub fn reduce(agent: &mut AgentState, record: &Value) -> bool {
    let ordinal = record.get("ordinal").and_then(Value::as_u64);
    let timestamp = parse_time(record.get("timestamp"));
    agent.last_ordinal = ordinal.or(agent.last_ordinal);
    let payload = &record["payload"];
    match record["type"].as_str() {
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
                agent.in_flight_calls.clear();
                agent.next_call_sequence = 0;
                agent.last_reasoning_summary = None;
                agent.last_message = None;
                agent.final_message = None;
                agent.result_status_claim = None;
                agent.last_communication = None;
                agent.last_activity_at = timestamp;
                true
            }
            Some("task_complete") => {
                agent.latest_turn.status = TurnStatus::Completed;
                agent.latest_turn.completed_at =
                    parse_time(payload.get("completed_at")).or(timestamp);
                if let Some(raw) = payload.get("last_agent_message").and_then(Value::as_str) {
                    agent.result_status_claim = receipt_claim(raw);
                    agent.final_message = Some(sanitise(raw));
                } else {
                    agent.final_message = agent.last_message.clone();
                }
                agent.in_flight_calls.clear();
                agent.last_activity_at = timestamp;
                true
            }
            Some("turn_aborted") => {
                agent.latest_turn.status = TurnStatus::Interrupted;
                agent.last_activity_at = timestamp;
                true
            }
            Some("error") => {
                agent.latest_turn.status = TurnStatus::Errored;
                agent.last_activity_at = timestamp;
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
                        if summary.is_some() {
                            agent.last_reasoning_summary = summary;
                            agent.last_activity_at = timestamp;
                        }
                    }
                    Some("AgentMessage") => {
                        if let Some(text) = plain_text(&item["content"]) {
                            agent.result_status_claim = receipt_claim(&text);
                            agent.last_message = Some(sanitise(&text));
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
                                agent.in_flight_calls.remove(id);
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
                    agent.result_status_claim = receipt_claim(raw);
                    agent.last_message = Some(sanitise(raw));
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
                agent.in_flight_calls.insert(
                    call_id,
                    InFlightCall {
                        summary: summary_for_call(payload),
                        tool_name,
                        started_at: timestamp,
                        ordinal,
                        sequence,
                    },
                );
                agent.last_activity_at = timestamp;
                true
            }
            Some("custom_tool_call_output") | Some("function_call_output") => {
                let id = payload["call_id"].as_str().expect("validated call_id");
                agent.in_flight_calls.remove(id);
                agent.last_activity_at = timestamp;
                true
            }
            Some("message") => {
                if payload["role"].as_str() == Some("assistant") {
                    if let Some(text) = plain_text(&payload["content"]) {
                        agent.result_status_claim = receipt_claim(&text);
                        agent.last_message = Some(sanitise(&text));
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
                    agent.last_communication = Some(sanitise(&format!(
                        "message {} → {}",
                        sanitise(author),
                        sanitise(recipient)
                    )));
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
                r#"{"timestamp":"2024-01-01T00:00:00Z","type":"response_item","payload":{"type":"custom_tool_call_output","call_id":"a"}}"#,
            ),
        );
        assert_eq!(a.current_activity(), Some("two"));
    }
    #[test]
    fn reasoning_sanitising_and_bookkeeping() {
        let mut a = agent();
        let record = serde_json::json!({
            "timestamp":"2024-01-01T00:00:00Z",
            "type":"event_msg",
            "payload":{"type":"item_completed","item":{
                "type":"Reasoning","summary_text":["**safe\u{1b}[31m text**"]
            }}
        });
        reduce(&mut a, &record);
        assert_eq!(a.last_reasoning_summary.as_deref(), Some("safe text"));
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
        assert!(value.starts_with("metadata��session�"));
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
    fn malformed_call_ids_and_exec_privacy() {
        let missing = r(
            r#"{"type":"response_item","payload":{"type":"custom_tool_call","name":"exec","input":"secret-token"}}"#,
        );
        let invalid = r(
            r#"{"type":"response_item","payload":{"type":"custom_tool_call_output","call_id":7}}"#,
        );
        assert!(has_malformed_call_id(&missing));
        assert!(has_malformed_call_id(&invalid));
        let valid = r(
            r#"{"type":"response_item","payload":{"type":"custom_tool_call","call_id":"c","name":"exec","input":"secret-token"}}"#,
        );
        assert!(!has_malformed_call_id(&valid));
        let mut state = agent();
        assert!(reduce(&mut state, &valid));
        assert_eq!(state.in_flight_calls["c"].summary, "running exec");
        assert!(!state.in_flight_calls["c"].summary.contains("secret"));
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
            r#"{"timestamp":"2026-08-24T20:20:55Z","type":"response_item","payload":{"type":"agent_message","author":"parent","recipient":"child","content":[{"type":"input_text","text":"incoming private"},{"type":"encrypted_content","encrypted_content":"ciphertext"}]}}"#,
            r#"{"timestamp":"2026-08-24T20:20:56Z","type":"event_msg","payload":{"type":"task_complete","last_agent_message":"status=CANDIDATE"}}"#,
        ] {
            assert!(reduce(&mut state, &r(record)));
        }
        assert_eq!(state.latest_turn.status, TurnStatus::Completed);
        assert!(state.in_flight_calls.is_empty());
        assert_eq!(state.last_message.as_deref(), Some("older output"));
        assert_eq!(
            state.last_communication.as_deref(),
            Some("message parent → child")
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
