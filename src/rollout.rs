use crate::model::{
    has_malformed_call_id, has_malformed_communication, reduce, sanitise, CoverageLevel,
    DiagnosticSample, SessionState,
};
use crate::schema::{lookup, SchemaStatus};
use anyhow::{bail, Context, Result};
use chrono::{DateTime, Utc};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

pub const MAX_RECORD_SIZE: usize = 1024 * 1024;
pub const APPEND_BUDGET: usize = 256 * 1024;

#[derive(Debug, Clone)]
pub struct RolloutMetadata {
    pub path: PathBuf,
    pub session_id: String,
    pub thread_id: String,
    pub parent_thread_id: Option<String>,
    pub cwd: Option<PathBuf>,
    pub timestamp: Option<DateTime<Utc>>,
    pub cli_version: String,
    pub agent_path: Option<String>,
    pub agent_role: Option<String>,
    pub agent_nickname: Option<String>,
    pub depth: Option<u64>,
    pub history_start: Option<u64>,
    pub consumed_offset: u64,
}
#[derive(Debug, Default)]
pub struct Discovery {
    pub admitted: Vec<RolloutMetadata>,
    pub pending: Vec<PathBuf>,
    pub health: crate::model::DataHealth,
}
#[derive(Debug, Clone)]
pub struct SessionGroup {
    pub session_id: String,
    pub rollouts: Vec<RolloutMetadata>,
    pub root: usize,
}
#[derive(Debug)]
pub struct RolloutCursor {
    pub path: PathBuf,
    pub byte_offset: u64,
    pub partial_line: Vec<u8>,
    pub last_ordinal: Option<u64>,
    history_start: Option<u64>,
    crossed_history_start: bool,
    discarding_oversized: bool,
    oversized_start: Option<u64>,
}
#[derive(Debug, PartialEq, Eq)]
pub enum TailOutcome {
    Records(usize),
    RebuildRequired,
}
#[derive(Debug)]
pub enum RecordError {
    Malformed(serde_json::Error),
    Oversized,
}

fn metadata(value: &Value, path: PathBuf, consumed_offset: u64) -> Result<RolloutMetadata> {
    let p = &value["payload"];
    let session_id = p["session_id"]
        .as_str()
        .context("required session_id is missing")?
        .to_owned();
    let thread_id = p["id"]
        .as_str()
        .context("required thread id is missing")?
        .to_owned();
    let cli_version = p["cli_version"]
        .as_str()
        .context("required cli_version is missing")?
        .to_owned();
    let spawn = p.pointer("/source/subagent/thread_spawn");
    Ok(RolloutMetadata {
        path,
        session_id,
        thread_id,
        parent_thread_id: p
            .get("parent_thread_id")
            .and_then(Value::as_str)
            .or_else(|| spawn.and_then(|s| s["parent_thread_id"].as_str()))
            .map(str::to_owned),
        cwd: p.get("cwd").and_then(Value::as_str).map(PathBuf::from),
        timestamp: crate::model::parse_time(p.get("timestamp")),
        cli_version,
        agent_path: p
            .get("agent_path")
            .and_then(Value::as_str)
            .or_else(|| spawn.and_then(|s| s["agent_path"].as_str()))
            .map(str::to_owned),
        agent_role: p
            .get("agent_role")
            .and_then(Value::as_str)
            .or_else(|| spawn.and_then(|s| s["agent_role"].as_str()))
            .map(str::to_owned),
        agent_nickname: p
            .get("agent_nickname")
            .and_then(Value::as_str)
            .or_else(|| spawn.and_then(|s| s["agent_nickname"].as_str()))
            .map(str::to_owned),
        depth: spawn
            .and_then(|s| s.pointer("/depth"))
            .and_then(Value::as_u64),
        history_start: p
            .get("subagent_history_start_ordinal")
            .and_then(Value::as_u64),
        consumed_offset,
    })
}
fn discover_one(
    path: &Path,
    health: &mut crate::model::DataHealth,
) -> Result<Option<RolloutMetadata>> {
    let file = File::open(path).with_context(|| format!("open {}", path.display()))?;
    let mut reader = BufReader::new(file);
    let mut consumed = 0_u64;
    for _ in 0..32 {
        let mut bytes = Vec::new();
        let n = reader.read_until(b'\n', &mut bytes)?;
        if n == 0 || !bytes.ends_with(b"\n") {
            return Ok(None);
        }
        consumed += n as u64;
        let start = consumed - n as u64;
        if bytes.len() > MAX_RECORD_SIZE {
            health.oversized_records += 1;
            health.diagnostic(DiagnosticSample {
                rollout_path: path.to_owned(),
                byte_offset: start,
                cli_version: None,
                ordinal: None,
                kind: "oversized_metadata".into(),
                detail: None,
            });
            continue;
        }
        let v = match serde_json::from_slice::<Value>(&bytes) {
            Ok(value) => value,
            Err(error) => {
                health.malformed_records += 1;
                health.diagnostic(DiagnosticSample {
                    rollout_path: path.to_owned(),
                    byte_offset: start,
                    cli_version: None,
                    ordinal: None,
                    kind: "malformed_metadata_record".into(),
                    detail: Some(sanitise(&error.to_string())),
                });
                continue;
            }
        };
        if v["type"].as_str() == Some("session_meta") {
            return metadata(&v, path.to_owned(), consumed).map(Some);
        }
    }
    bail!("session metadata not found within discovery record budget")
}
pub fn discover(sessions_dir: &Path) -> Result<Discovery> {
    if !sessions_dir.is_dir() {
        bail!(
            "sessions path is not a readable directory: {}",
            sessions_dir.display()
        );
    }
    let mut out = Discovery::default();
    for entry in WalkDir::new(sessions_dir).follow_links(false) {
        let entry = entry.with_context(|| format!("traverse {}", sessions_dir.display()))?;
        let p = entry.path();
        if !entry.file_type().is_file()
            || !p
                .file_name()
                .and_then(|s| s.to_str())
                .is_some_and(|s| s.starts_with("rollout-") && s.ends_with(".jsonl"))
        {
            continue;
        }
        match discover_one(p, &mut out.health) {
            Ok(Some(m)) => out.admitted.push(m),
            Ok(None) => out.pending.push(p.to_owned()),
            Err(e) => {
                let kind = if e.to_string().contains("oversized") {
                    out.health.oversized_records += 1;
                    "oversized_metadata"
                } else {
                    out.health.malformed_records += 1;
                    "malformed_metadata"
                };
                out.health.diagnostic(DiagnosticSample {
                    rollout_path: p.to_owned(),
                    byte_offset: 0,
                    cli_version: None,
                    kind: kind.into(),
                    ordinal: None,
                    detail: Some(crate::model::sanitise(&e.to_string())),
                });
            }
        }
    }
    Ok(out)
}
fn latest_group_timestamp(group: &SessionGroup) -> Option<DateTime<Utc>> {
    group
        .rollouts
        .iter()
        .filter_map(|meta| meta.timestamp)
        .max()
}

fn compare_group_recency(a: &SessionGroup, b: &SessionGroup) -> std::cmp::Ordering {
    latest_group_timestamp(a)
        .cmp(&latest_group_timestamp(b))
        .then_with(|| b.session_id.cmp(&a.session_id))
}

pub fn group(items: Vec<RolloutMetadata>) -> Vec<SessionGroup> {
    let mut map: HashMap<String, Vec<RolloutMetadata>> = HashMap::new();
    for item in items {
        map.entry(item.session_id.clone()).or_default().push(item);
    }
    let mut groups = map
        .into_iter()
        .filter_map(|(session_id, mut rollouts)| {
            rollouts.sort_by(|a, b| a.thread_id.cmp(&b.thread_id));
            let root = rollouts
                .iter()
                .position(|m| m.thread_id == session_id)
                .or_else(|| rollouts.iter().position(|m| m.parent_thread_id.is_none()))?;
            Some(SessionGroup {
                session_id,
                rollouts,
                root,
            })
        })
        .collect::<Vec<_>>();
    groups.sort_by(|a, b| compare_group_recency(b, a));
    groups
}
pub fn select<'a>(
    groups: &'a [SessionGroup],
    requested: Option<&str>,
    cwd: &Path,
) -> Result<&'a SessionGroup> {
    if let Some(id) = requested {
        if let Some(exact) = groups.iter().find(|group| group.session_id == id) {
            return Ok(exact);
        }
        let matches = groups
            .iter()
            .filter(|group| group.session_id.starts_with(id))
            .collect::<Vec<_>>();
        return match matches.as_slice() {
            [group] => Ok(*group),
            [] => bail!("session {id} not found"),
            _ => bail!(
                "session prefix {id} is ambiguous ({} matches)",
                matches.len()
            ),
        };
    }
    groups
        .iter()
        .filter(|group| group.rollouts[group.root].cwd.as_deref() == Some(cwd))
        .max_by(|a, b| compare_group_recency(a, b))
        .or_else(|| groups.iter().max_by(|a, b| compare_group_recency(a, b)))
        .context("no sessions found")
}
impl RolloutCursor {
    pub fn new(meta: &RolloutMetadata) -> Self {
        Self {
            path: meta.path.clone(),
            byte_offset: meta.consumed_offset,
            partial_line: Vec::new(),
            last_ordinal: None,
            history_start: meta.history_start,
            crossed_history_start: meta.history_start.is_none(),
            discarding_oversized: false,
            oversized_start: None,
        }
    }
    fn from_path_start(path: PathBuf) -> Self {
        Self {
            path,
            byte_offset: 0,
            partial_line: Vec::new(),
            last_ordinal: None,
            history_start: None,
            crossed_history_start: true,
            discarding_oversized: false,
            oversized_start: None,
        }
    }
    #[cfg(test)]
    pub fn from_start(path: PathBuf) -> Self {
        Self::from_path_start(path)
    }
    pub fn tail(
        &mut self,
        budget: usize,
        on_record: impl FnMut(u64, Result<Value, RecordError>),
    ) -> Result<TailOutcome> {
        self.tail_bounded(budget, usize::MAX, on_record)
    }
    fn tail_bounded(
        &mut self,
        budget: usize,
        max_records: usize,
        mut on_record: impl FnMut(u64, Result<Value, RecordError>),
    ) -> Result<TailOutcome> {
        let len = std::fs::metadata(&self.path)?.len();
        if len < self.byte_offset {
            return Ok(TailOutcome::RebuildRequired);
        }
        let mut file = OpenOptions::new().read(true).open(&self.path)?;
        file.seek(SeekFrom::Start(self.byte_offset))?;
        let mut take = file.take(budget as u64);
        let mut chunk = Vec::new();
        take.read_to_end(&mut chunk)?;
        self.byte_offset += chunk.len() as u64;
        self.partial_line.extend_from_slice(&chunk);
        let mut count = 0;
        while count < max_records {
            let Some(pos) = self.partial_line.iter().position(|b| *b == b'\n') else {
                break;
            };
            let line = self.partial_line.drain(..=pos).collect::<Vec<_>>();
            let offset = self.byte_offset - self.partial_line.len() as u64 - line.len() as u64;
            if self.discarding_oversized {
                self.discarding_oversized = false;
                let start = self.oversized_start.take().unwrap_or(offset);
                on_record(start, Err(RecordError::Oversized));
                count += 1;
                continue;
            }
            if line.len() > MAX_RECORD_SIZE {
                on_record(offset, Err(RecordError::Oversized));
                count += 1;
                continue;
            }
            let parsed = serde_json::from_slice::<Value>(&line).map_err(RecordError::Malformed);
            if let Ok(value) = &parsed {
                self.last_ordinal = value
                    .get("ordinal")
                    .and_then(Value::as_u64)
                    .or(self.last_ordinal);
            }
            on_record(offset, parsed);
            count += 1;
        }
        if self.partial_line.len() > MAX_RECORD_SIZE {
            self.oversized_start = Some(self.byte_offset - self.partial_line.len() as u64);
            self.partial_line.clear();
            self.discarding_oversized = true;
        }
        Ok(TailOutcome::Records(count))
    }
}
fn ingest_value(
    agent: &mut crate::model::AgentState,
    health: &mut crate::model::DataHealth,
    path: &Path,
    offset: u64,
    version: &str,
    value: &Value,
) {
    let ordinal = value.get("ordinal").and_then(Value::as_u64);
    if value.get("type").and_then(Value::as_str).is_none()
        || !value.get("payload").is_some_and(Value::is_object)
    {
        health.malformed_records += 1;
        health.diagnostic(DiagnosticSample {
            rollout_path: path.to_owned(),
            byte_offset: offset,
            cli_version: Some(version.to_owned()),
            ordinal,
            kind: "malformed_envelope".into(),
            detail: None,
        });
        return;
    }
    if has_malformed_communication(value) {
        health.malformed_records += 1;
        health.diagnostic(DiagnosticSample {
            rollout_path: path.to_owned(),
            byte_offset: offset,
            cli_version: Some(version.to_owned()),
            ordinal,
            kind: "malformed_communication".into(),
            detail: Some("agent_message".into()),
        });
        return;
    }
    if has_malformed_call_id(value) {
        health.malformed_records += 1;
        health.diagnostic(DiagnosticSample {
            rollout_path: path.to_owned(),
            byte_offset: offset,
            cli_version: Some(version.to_owned()),
            ordinal,
            kind: "malformed_call_id".into(),
            detail: value["payload"]["type"].as_str().map(sanitise),
        });
        return;
    }
    if reduce(agent, value) {
        return;
    }
    let (kind, detail) = if value["type"].as_str() == Some("event_msg") {
        health.unknown_events += 1;
        ("unknown_event", value["payload"]["type"].as_str())
    } else {
        health.unknown_records += 1;
        ("unknown_record", value["type"].as_str())
    };
    health.diagnostic(DiagnosticSample {
        rollout_path: path.to_owned(),
        byte_offset: offset,
        cli_version: Some(version.to_owned()),
        ordinal,
        kind: kind.into(),
        detail: detail.map(sanitise),
    });
}

fn agent_from_meta(meta: &RolloutMetadata, repo_root: &Path) -> Result<crate::model::AgentState> {
    let mut agent = crate::model::AgentState::new(meta.thread_id.clone(), meta.cli_version.clone());
    agent.parent_thread_id = meta.parent_thread_id.clone();
    agent.agent_path = meta.agent_path.as_deref().map(sanitise);
    agent.agent_role = meta.agent_role.as_deref().map(sanitise);
    agent.agent_nickname = meta.agent_nickname.as_deref().map(sanitise);
    agent.own_history_start_ordinal = meta.history_start;
    match lookup(repo_root, &meta.cli_version)? {
        SchemaStatus::Catalogued {
            rollout_line_canonical_sha256,
            ..
        } => {
            agent.schema_catalogued = true;
            agent.schema_family = Some(rollout_line_canonical_sha256);
        }
        SchemaStatus::Missing => {}
    }
    agent.coverage = if meta.cli_version == "0.152.1" {
        CoverageLevel::LiveVerified
    } else if meta.cli_version == "0.149.0-alpha.4.1" {
        CoverageLevel::SemanticallyCovered
    } else if meta.cli_version.is_empty() {
        CoverageLevel::Unknown
    } else {
        CoverageLevel::Ingestable
    };
    Ok(agent)
}

pub fn load_group(
    group: &SessionGroup,
    repo_root: &Path,
) -> Result<(SessionState, Vec<RolloutCursor>)> {
    let root = &group.rollouts[group.root];
    let mut state = SessionState {
        session_id: group.session_id.clone(),
        cwd: root.cwd.clone(),
        started_at: root.timestamp,
        ..Default::default()
    };
    let mut cursors = Vec::new();
    for meta in &group.rollouts {
        let mut agent = agent_from_meta(meta, repo_root)?;
        let boundary = meta.history_start;
        let mut crossed = boundary.is_none();
        let mut cursor = RolloutCursor::new(meta);
        let file_len = std::fs::metadata(&meta.path)?.len();
        while cursor.byte_offset < file_len {
            cursor.tail(APPEND_BUDGET, |offset, parsed| match parsed {
                Ok(v) => {
                    let ord = v.get("ordinal").and_then(Value::as_u64);
                    if !crossed {
                        if ord.is_some_and(|o| o >= boundary.unwrap()) {
                            crossed = true;
                        } else {
                            if ord.is_none() {
                                state.data_health.diagnostic(DiagnosticSample {
                                    rollout_path: meta.path.clone(),
                                    byte_offset: offset,
                                    cli_version: Some(meta.cli_version.clone()),
                                    kind: "ordinal_less_pre_boundary".into(),
                                    ordinal: None,
                                    detail: None,
                                });
                            }
                            return;
                        }
                    }
                    ingest_value(
                        &mut agent,
                        &mut state.data_health,
                        &meta.path,
                        offset,
                        &meta.cli_version,
                        &v,
                    );
                }
                Err(error) => {
                    let (kind, detail) = match error {
                        RecordError::Oversized => {
                            state.data_health.oversized_records += 1;
                            ("oversized_record", None)
                        }
                        RecordError::Malformed(error) => {
                            state.data_health.malformed_records += 1;
                            (
                                "malformed_record",
                                Some(crate::model::sanitise(&error.to_string())),
                            )
                        }
                    };
                    state.data_health.diagnostic(DiagnosticSample {
                        rollout_path: meta.path.clone(),
                        byte_offset: offset,
                        cli_version: Some(meta.cli_version.clone()),
                        kind: kind.into(),
                        ordinal: None,
                        detail,
                    });
                }
            })?;
        }
        cursor.crossed_history_start = crossed;
        state.agents.insert(meta.thread_id.clone(), agent);
        cursors.push(cursor);
    }
    Ok((state, cursors))
}
pub fn tree_lines(group: &SessionGroup, state: &SessionState) -> Vec<String> {
    fn visit(
        id: &str,
        depth: usize,
        children: &HashMap<String, Vec<String>>,
        state: &SessionState,
        out: &mut Vec<String>,
        seen: &mut HashSet<String>,
    ) {
        if !seen.insert(id.to_owned()) {
            return;
        }
        let Some(a) = state.agents.get(id) else {
            return;
        };
        let label =
            a.agent_path
                .as_deref()
                .unwrap_or(if depth == 0 { "/root" } else { "(unnamed)" });
        let call_evidence = a
            .active_call_evidence()
            .map(|(time, ordinal)| format!(" call_at={time:?} call_ordinal={ordinal:?}"))
            .unwrap_or_default();
        out.push(format!(
            "{}{} [{}] {:?} version={} schema={} schema_family={:?} coverage={:?} turn={:?} started={:?}{} activity={} communication={:?} final_message={:?} result_claim={:?}",
            "  ".repeat(depth),
            sanitise(label),
            sanitise(id).chars().take(8).collect::<String>(),
            a.latest_turn.status,
            sanitise(&a.cli_version),
            a.schema_catalogued,
            a.schema_family.as_deref().map(sanitise),
            a.coverage,
            a.latest_turn.turn_id.as_deref().map(sanitise),
            a.latest_turn.started_at,
            call_evidence,
            sanitise(a.current_activity().unwrap_or("")),
            a.last_communication.as_deref().map(sanitise),
            a.final_message.as_deref().map(sanitise),
            a.result_status_claim.as_deref().map(sanitise)
        ));
        if let Some(kids) = children.get(id) {
            for kid in kids {
                visit(kid, depth + 1, children, state, out, seen)
            }
        }
    }
    let mut children: HashMap<String, Vec<String>> = HashMap::new();
    for a in state.agents.values() {
        if let Some(p) = &a.parent_thread_id {
            children
                .entry(p.clone())
                .or_default()
                .push(a.thread_id.clone())
        }
    }
    for kids in children.values_mut() {
        kids.sort()
    }
    let root = &group.rollouts[group.root].thread_id;
    let mut out = Vec::new();
    visit(root, 0, &children, state, &mut out, &mut HashSet::new());
    out
}

#[derive(Debug, PartialEq, Eq)]
pub enum PollOutcome {
    Updated { records: usize, admitted: usize },
    Rebuilt,
}
pub struct SelectedReader {
    pub group: SessionGroup,
    pub state: SessionState,
    cursors: Vec<RolloutCursor>,
    pending: Vec<PathBuf>,
    pending_cursors: HashMap<PathBuf, RolloutCursor>,
    known_paths: HashSet<PathBuf>,
    sessions_root: PathBuf,
    discovery_scan: walkdir::IntoIter,
    repo_root: PathBuf,
    cursor_next: usize,
    pending_next: usize,
}
impl SelectedReader {
    pub fn new(
        group: SessionGroup,
        pending: Vec<PathBuf>,
        sessions_root: PathBuf,
        repo_root: PathBuf,
    ) -> Result<Self> {
        let (state, cursors) = load_group(&group, &repo_root)?;
        let pending_cursors = pending
            .iter()
            .cloned()
            .map(|path| (path.clone(), RolloutCursor::from_path_start(path)))
            .collect();
        let known_paths = group
            .rollouts
            .iter()
            .map(|meta| meta.path.clone())
            .chain(pending.iter().cloned())
            .collect();
        let discovery_scan = WalkDir::new(&sessions_root).follow_links(false).into_iter();
        Ok(Self {
            group,
            state,
            cursors,
            pending,
            pending_cursors,
            known_paths,
            sessions_root,
            discovery_scan,
            repo_root,
            cursor_next: 0,
            pending_next: 0,
        })
    }
    pub fn poll(&mut self) -> Result<PollOutcome> {
        const DIRECTORY_ENTRY_BUDGET: usize = 1024;
        const POLL_WORK_BUDGET: usize = 2048;
        const SLICE: usize = 32 * 1024;
        let mut remaining_bytes = APPEND_BUDGET;
        let mut remaining_work = POLL_WORK_BUDGET;

        for _ in 0..DIRECTORY_ENTRY_BUDGET {
            if remaining_bytes == 0 || remaining_work == 0 {
                break;
            }
            remaining_bytes -= 1;
            remaining_work -= 1;
            let Some(entry) = self.discovery_scan.next() else {
                self.discovery_scan = WalkDir::new(&self.sessions_root)
                    .follow_links(false)
                    .into_iter();
                break;
            };
            let entry =
                entry.with_context(|| format!("traverse {}", self.sessions_root.display()))?;
            let path = entry.path();
            if entry.file_type().is_file()
                && path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with("rollout-") && name.ends_with(".jsonl"))
                && self.known_paths.insert(path.to_owned())
            {
                let path = path.to_owned();
                self.pending_cursors
                    .insert(path.clone(), RolloutCursor::from_path_start(path.clone()));
                self.pending.push(path);
            }
        }

        let mut records = 0;
        let cursor_count = self.cursors.len();
        let cursor_allowance = remaining_bytes / 2;
        let cursor_work_allowance = remaining_work / 2;
        let mut cursor_spent = 0;
        let mut cursor_work_spent = 0;
        for _ in 0..cursor_count {
            if cursor_spent >= cursor_allowance
                || cursor_work_spent >= cursor_work_allowance
                || remaining_bytes == 0
                || remaining_work == 0
            {
                break;
            }
            let index = self.cursor_next % self.cursors.len();
            self.cursor_next = (index + 1) % self.cursors.len();
            let budget = SLICE
                .min(cursor_allowance - cursor_spent)
                .min(remaining_bytes);
            let cursor = &mut self.cursors[index];
            let before = cursor.byte_offset;
            let meta = self
                .group
                .rollouts
                .iter()
                .find(|m| m.path == cursor.path)
                .context("cursor metadata missing")?;
            let agent = self
                .state
                .agents
                .get_mut(&meta.thread_id)
                .context("cursor agent missing")?;
            let health = &mut self.state.data_health;
            let path = cursor.path.clone();
            let version = agent.cli_version.clone();
            let boundary = cursor.history_start;
            let mut crossed = cursor.crossed_history_start;
            let outcome = cursor.tail_bounded(
                budget,
                remaining_work.min(cursor_work_allowance - cursor_work_spent),
                |offset, record| {
                    records += 1;
                    match record {
                        Ok(value) => {
                            let ordinal = value.get("ordinal").and_then(Value::as_u64);
                            if !crossed {
                                if ordinal.is_some_and(|value| {
                                    value >= boundary.expect("boundary exists")
                                }) {
                                    crossed = true;
                                } else {
                                    if ordinal.is_none() {
                                        health.diagnostic(DiagnosticSample {
                                            rollout_path: path.clone(),
                                            byte_offset: offset,
                                            cli_version: Some(version.clone()),
                                            ordinal: None,
                                            kind: "pre_boundary_without_ordinal".into(),
                                            detail: None,
                                        });
                                    }
                                    return;
                                }
                            }
                            ingest_value(agent, health, &path, offset, &version, &value);
                        }
                        Err(error) => {
                            let (kind, detail) = match error {
                                RecordError::Oversized => {
                                    health.oversized_records += 1;
                                    ("oversized_record", None)
                                }
                                RecordError::Malformed(error) => {
                                    health.malformed_records += 1;
                                    (
                                        "malformed_record",
                                        Some(crate::model::sanitise(&error.to_string())),
                                    )
                                }
                            };
                            health.diagnostic(DiagnosticSample {
                                rollout_path: path.clone(),
                                byte_offset: offset,
                                cli_version: Some(version.clone()),
                                kind: kind.into(),
                                ordinal: None,
                                detail,
                            });
                        }
                    }
                },
            )?;
            cursor.crossed_history_start = crossed;
            let spent = (cursor.byte_offset - before) as usize;
            let charged_bytes = spent.max(1).min(remaining_bytes);
            cursor_spent += charged_bytes;
            remaining_bytes -= charged_bytes;
            let processed = match outcome {
                TailOutcome::Records(count) => count,
                TailOutcome::RebuildRequired => {
                    let (state, cursors) = load_group(&self.group, &self.repo_root)?;
                    self.state = state;
                    self.cursors = cursors;
                    self.cursor_next = 0;
                    return Ok(PollOutcome::Rebuilt);
                }
            };
            let charged_work = processed.max(1).min(remaining_work);
            cursor_work_spent += charged_work;
            remaining_work -= charged_work;
        }

        let mut admitted = 0;
        let pending_count = self.pending.len();
        for _ in 0..pending_count {
            if remaining_bytes == 0 || remaining_work == 0 || self.pending.is_empty() {
                break;
            }
            let index = self.pending_next % self.pending.len();
            let path = self.pending[index].clone();
            let prefix_len = self.pending_cursors[&path].partial_line.len().min(64);
            if prefix_len > 0 {
                let mut prefix = vec![0; prefix_len];
                let mut file =
                    File::open(&path).with_context(|| format!("open {}", path.display()))?;
                let partial_start = self.pending_cursors[&path].byte_offset
                    - self.pending_cursors[&path].partial_line.len() as u64;
                file.seek(SeekFrom::Start(partial_start))?;
                let read = file.read(&mut prefix)?;
                remaining_bytes -= read.min(remaining_bytes);
                if read != prefix_len
                    || prefix != self.pending_cursors[&path].partial_line[..prefix_len]
                {
                    self.pending_cursors
                        .insert(path.clone(), RolloutCursor::from_path_start(path.clone()));
                }
            }
            let cursor = self
                .pending_cursors
                .get_mut(&path)
                .context("pending cursor missing")?;
            let before = cursor.byte_offset;
            let mut candidate = None;
            let outcome = cursor.tail_bounded(
                SLICE.min(remaining_bytes),
                1.min(remaining_work),
                |offset, record| match record {
                    Ok(value) if value["type"].as_str() == Some("session_meta") => {
                        candidate = Some((value, offset));
                    }
                    Ok(_) => {}
                    Err(RecordError::Oversized) => {
                        self.state.data_health.oversized_records += 1;
                        self.state.data_health.diagnostic(DiagnosticSample {
                            rollout_path: path.clone(),
                            byte_offset: offset,
                            cli_version: None,
                            ordinal: None,
                            kind: "oversized_metadata".into(),
                            detail: None,
                        });
                    }
                    Err(RecordError::Malformed(error)) => {
                        self.state.data_health.malformed_records += 1;
                        self.state.data_health.diagnostic(DiagnosticSample {
                            rollout_path: path.clone(),
                            byte_offset: offset,
                            cli_version: None,
                            ordinal: None,
                            kind: "malformed_metadata_record".into(),
                            detail: Some(sanitise(&error.to_string())),
                        });
                    }
                },
            )?;
            let spent = (cursor.byte_offset - before) as usize;
            remaining_bytes -= spent.max(1).min(remaining_bytes);
            let processed = match outcome {
                TailOutcome::Records(count) => count,
                TailOutcome::RebuildRequired => {
                    self.pending_cursors
                        .insert(path.clone(), RolloutCursor::from_path_start(path.clone()));
                    remaining_work -= 1.min(remaining_work);
                    self.pending_next = (index + 1) % self.pending.len();
                    continue;
                }
            };
            remaining_work -= processed.max(1).min(remaining_work);
            if let Some((value, offset)) = candidate {
                let consumed = cursor.byte_offset - cursor.partial_line.len() as u64;
                self.pending.remove(index);
                self.pending_cursors.remove(&path);
                if !self.pending.is_empty() {
                    self.pending_next = index % self.pending.len();
                } else {
                    self.pending_next = 0;
                }
                let meta = match metadata(&value, path.clone(), consumed) {
                    Ok(meta) => meta,
                    Err(error) => {
                        self.state.data_health.malformed_records += 1;
                        self.state.data_health.diagnostic(DiagnosticSample {
                            rollout_path: path,
                            byte_offset: offset,
                            cli_version: None,
                            ordinal: None,
                            kind: "malformed_session_metadata".into(),
                            detail: Some(sanitise(&error.to_string())),
                        });
                        continue;
                    }
                };
                if meta.session_id != self.group.session_id
                    || self
                        .group
                        .rollouts
                        .iter()
                        .any(|known| known.thread_id == meta.thread_id)
                {
                    continue;
                }
                let agent = agent_from_meta(&meta, &self.repo_root)?;
                self.state.agents.insert(meta.thread_id.clone(), agent);
                self.cursors.push(RolloutCursor::new(&meta));
                self.group.rollouts.push(meta);
                admitted += 1;
            } else {
                self.pending_next = (index + 1) % self.pending.len();
            }
        }
        Ok(PollOutcome::Updated { records, admitted })
    }
}

#[cfg(test)]
fn append_unknown_record(path: &Path, fill: usize) {
    use std::io::Write as _;
    let mut file = std::fs::OpenOptions::new().append(true).open(path).unwrap();
    let record = format!(
        "{{\"type\":\"mystery\",\"payload\":{{\"value\":\"{}\"}}}}\n",
        "x".repeat(fill)
    );
    file.write_all(record.as_bytes()).unwrap();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    fn meta(
        path: PathBuf,
        sid: &str,
        id: &str,
        parent: Option<&str>,
        offset: u64,
    ) -> RolloutMetadata {
        RolloutMetadata {
            path,
            session_id: sid.into(),
            thread_id: id.into(),
            parent_thread_id: parent.map(str::to_owned),
            cwd: None,
            timestamp: None,
            cli_version: "0.149.0".into(),
            agent_path: None,
            agent_role: None,
            agent_nickname: None,
            depth: None,
            history_start: None,
            consumed_offset: offset,
        }
    }
    #[test]
    fn discovery_grouping_and_pending() {
        let t = tempfile::tempdir().unwrap();
        let p = t.path().join("rollout-a.jsonl");
        std::fs::write(&p,r#"{"type":"session_meta","payload":{"session_id":"s","id":"s","cli_version":"0.149.0"}}"#).unwrap();
        assert_eq!(discover(t.path()).unwrap().pending.len(), 1);
        std::fs::OpenOptions::new()
            .append(true)
            .open(&p)
            .unwrap()
            .write_all(b"\n")
            .unwrap();
        let d = discover(t.path()).unwrap();
        assert_eq!(d.admitted.len(), 1);
        let c = t.path().join("rollout-b.jsonl");
        std::fs::write(&c, "").unwrap();
        let groups = group(vec![
            meta(p, "s", "s", None, 1),
            meta(c, "s", "c", Some("s"), 1),
        ]);
        assert_eq!(groups[0].rollouts.len(), 2);
    }
    #[test]
    fn partial_tail_and_truncation() {
        let t = tempfile::tempdir().unwrap();
        let p = t.path().join("r");
        std::fs::write(&p, b"{\"type\":\"x\"").unwrap();
        let mut c = RolloutCursor::from_start(p.clone());
        let mut got = 0;
        c.tail(100, |_, _| got += 1).unwrap();
        assert_eq!(got, 0);
        std::fs::OpenOptions::new()
            .append(true)
            .open(&p)
            .unwrap()
            .write_all(b"}\n")
            .unwrap();
        c.tail(100, |_, v| {
            v.unwrap();
            got += 1
        })
        .unwrap();
        assert_eq!(got, 1);
        std::fs::write(&p, b"").unwrap();
        assert_eq!(
            c.tail(100, |_, _| {}).unwrap(),
            TailOutcome::RebuildRequired
        );
    }
    #[test]
    fn malformed_unknown_boundary_and_readonly() {
        let t = tempfile::tempdir().unwrap();
        let p = t.path().join("rollout-x.jsonl");
        let data=concat!(
            "{\"ordinal\":0,\"type\":\"session_meta\",\"payload\":{\"session_id\":\"s\",\"id\":\"s\",\"cli_version\":\"0.149.0\",\"subagent_history_start_ordinal\":3}}\n",
            "{\"ordinal\":1,\"type\":\"event_msg\",\"payload\":{\"type\":\"task_started\"}}\n",
            "{bad}\n",
            "{\"ordinal\":3,\"type\":\"event_msg\",\"payload\":{\"type\":\"task_started\"}}\n",
            "{\"ordinal\":4,\"type\":\"mystery\",\"payload\":{}}\n");
        std::fs::write(&p, data).unwrap();
        let m = discover_one(&p, &mut crate::model::DataHealth::default())
            .unwrap()
            .unwrap();
        let g = group(vec![m]);
        let (s, _) = load_group(&g[0], t.path()).unwrap();
        let a = s.agents.get("s").unwrap();
        assert_eq!(a.latest_turn.status, crate::model::TurnStatus::Running);
        assert_eq!(s.data_health.malformed_records, 1);
        assert_eq!(s.data_health.unknown_records, 1);
        let mut perms = std::fs::metadata(&p).unwrap().permissions();
        perms.set_readonly(true);
        std::fs::set_permissions(&p, perms).unwrap();
        assert_eq!(discover(t.path()).unwrap().admitted.len(), 1);
    }

    #[test]
    fn fragmented_oversized_record_is_reported_once_at_start() {
        let t = tempfile::tempdir().unwrap();
        let path = t.path().join("oversized");
        std::fs::write(&path, vec![b'x'; MAX_RECORD_SIZE + 1]).unwrap();
        let mut cursor = RolloutCursor::from_start(path.clone());
        let mut events = Vec::new();
        cursor
            .tail(MAX_RECORD_SIZE + 1, |offset, value| {
                events.push((offset, value))
            })
            .unwrap();
        assert!(events.is_empty());
        std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(b"\n")
            .unwrap();
        cursor
            .tail(1, |offset, value| events.push((offset, value)))
            .unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].0, 0);
        assert!(matches!(events[0].1, Err(RecordError::Oversized)));
    }

    #[test]
    fn pending_admission_once_and_truncation_rebuild() {
        let t = tempfile::tempdir().unwrap();
        let root = t.path().join("rollout-root.jsonl");
        let child = t.path().join("rollout-child.jsonl");
        std::fs::write(&root, "{\"type\":\"session_meta\",\"payload\":{\"session_id\":\"s\",\"id\":\"s\",\"cli_version\":\"0.149.0\"}}\n").unwrap();
        std::fs::write(&child, "{\"type\":\"session_meta\"").unwrap();
        let discovery = discover(t.path()).unwrap();
        assert_eq!(discovery.pending, vec![child.clone()]);
        let group = group(discovery.admitted).pop().unwrap();
        let mut reader = SelectedReader::new(
            group,
            discovery.pending,
            t.path().to_owned(),
            t.path().to_owned(),
        )
        .unwrap();
        std::fs::write(&child, concat!(
            "{bad}\n",
            "{\"type\":\"session_meta\",\"payload\":{\"session_id\":\"s\",\"id\":\"c\",\"parent_thread_id\":\"s\",\"cli_version\":\"0.149.1\",\"source\":{\"subagent\":{\"thread_spawn\":{\"agent_path\":\"/root/c\",\"agent_role\":\"worker\",\"depth\":1}}}}}\n",
            "{\"type\":\"event_msg\",\"payload\":{\"type\":\"task_started\"}}\n"
        )).unwrap();
        assert_eq!(
            reader.poll().unwrap(),
            PollOutcome::Updated {
                records: 0,
                admitted: 0
            }
        );
        assert_eq!(
            reader.poll().unwrap(),
            PollOutcome::Updated {
                records: 0,
                admitted: 1
            }
        );
        assert_eq!(
            reader.poll().unwrap(),
            PollOutcome::Updated {
                records: 1,
                admitted: 0
            }
        );
        assert_eq!(reader.state.data_health.malformed_records, 1);
        let diagnostic = reader.state.data_health.recent_diagnostics.back().unwrap();
        assert_eq!(diagnostic.rollout_path, child);
        assert_eq!(diagnostic.byte_offset, 0);
        let child_state = reader.state.agents.get("c").unwrap();
        assert_eq!(child_state.agent_path.as_deref(), Some("/root/c"));
        assert_eq!(child_state.coverage, CoverageLevel::Ingestable);
        assert_eq!(
            child_state.latest_turn.status,
            crate::model::TurnStatus::Running
        );
        std::fs::write(&root, "").unwrap();
        assert_eq!(reader.poll().unwrap(), PollOutcome::Rebuilt);
    }

    #[test]
    fn incremental_tail_preserves_inclusive_history_boundary() {
        let t = tempfile::tempdir().unwrap();
        let path = t.path().join("rollout-boundary.jsonl");
        std::fs::write(&path, "{\"type\":\"session_meta\",\"payload\":{\"session_id\":\"s\",\"id\":\"s\",\"cli_version\":\"0.149.0\",\"subagent_history_start_ordinal\":5}}\n").unwrap();
        let discovery = discover(t.path()).unwrap();
        let group = group(discovery.admitted).pop().unwrap();
        let mut reader =
            SelectedReader::new(group, Vec::new(), t.path().to_owned(), t.path().to_owned())
                .unwrap();
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        file.write_all(
            concat!(
                "{\"ordinal\":2,\"type\":\"event_msg\",\"payload\":{\"type\":\"task_started\"}}\n",
                "{\"type\":\"event_msg\",\"payload\":{\"type\":\"task_started\"}}\n"
            )
            .as_bytes(),
        )
        .unwrap();
        reader.poll().unwrap();
        assert_eq!(
            reader.state.agents["s"].latest_turn.status,
            crate::model::TurnStatus::Pending
        );
        file.write_all(
            b"{\"ordinal\":5,\"type\":\"event_msg\",\"payload\":{\"type\":\"task_started\"}}\n",
        )
        .unwrap();
        reader.poll().unwrap();
        assert_eq!(
            reader.state.agents["s"].latest_turn.status,
            crate::model::TurnStatus::Running
        );
    }
    #[test]
    fn selector_prefix_and_post_start_discovery_with_diagnostics() {
        let t = tempfile::tempdir().unwrap();
        let root = t.path().join("rollout-root.jsonl");
        std::fs::write(&root, "{\"type\":\"session_meta\",\"payload\":{\"session_id\":\"abcdef\",\"id\":\"abcdef\",\"cli_version\":\"0.149.0-alpha.4.1\"}}\n").unwrap();
        let groups = group(discover(t.path()).unwrap().admitted);
        assert_eq!(
            select(&groups, Some("abc"), t.path()).unwrap().session_id,
            "abcdef"
        );

        let second = SessionGroup {
            session_id: "abcxyz".into(),
            rollouts: vec![meta(root.clone(), "abcxyz", "abcxyz", None, 0)],
            root: 0,
        };
        let exact = SessionGroup {
            session_id: "abc".into(),
            rollouts: vec![meta(root.clone(), "abc", "abc", None, 0)],
            root: 0,
        };
        let mut ambiguous = groups.clone();
        ambiguous.push(second);
        ambiguous.push(exact);
        assert_eq!(
            select(&ambiguous, Some("abc"), t.path())
                .unwrap()
                .session_id,
            "abc"
        );
        assert!(select(&ambiguous, Some("ab"), t.path())
            .unwrap_err()
            .to_string()
            .contains("ambiguous"));

        let mut reader = SelectedReader::new(
            groups[0].clone(),
            Vec::new(),
            t.path().to_owned(),
            t.path().to_owned(),
        )
        .unwrap();
        for index in 0..=1024 {
            std::fs::write(t.path().join(format!("decoy-{index:04}.tmp")), "").unwrap();
        }
        std::fs::write(t.path().join("unrelated.jsonl"), "{}\n").unwrap();
        let next_date = t.path().join("2026/09/03");
        std::fs::create_dir_all(&next_date).unwrap();
        let child = next_date.join("rollout-new.jsonl");
        std::fs::write(&child, concat!(
            "{\"type\":\"session_meta\",\"payload\":{\"session_id\":\"abcdef\",\"id\":\"child\",\"parent_thread_id\":\"abcdef\",\"cli_version\":\"0.149.0-alpha.4.1\"}}\n",
            "{\"ordinal\":1,\"type\":\"mystery\",\"payload\":{}}\n",
            "{\"ordinal\":2,\"type\":\"event_msg\",\"payload\":{\"type\":\"future_event\"}}\n",
            "{\"ordinal\":3,\"type\":\"response_item\",\"payload\":{\"type\":\"custom_tool_call\",\"name\":\"exec\",\"input\":\"private\"}}\n",
            "{\"ordinal\":4,\"type\":\"response_item\",\"payload\":{\"type\":\"agent_message\",\"recipient\":\"child\",\"content\":[]}}\n"
        )).unwrap();
        let mut admitted = 0;
        for _ in 0..4 {
            if let PollOutcome::Updated {
                admitted: count, ..
            } = reader.poll().unwrap()
            {
                admitted += count;
            }
            if admitted == 1 {
                break;
            }
        }
        assert_eq!(admitted, 1);
        reader.poll().unwrap();
        let child_state = &reader.state.agents["child"];
        assert_eq!(child_state.coverage, CoverageLevel::SemanticallyCovered);
        assert!(child_state.in_flight_calls.is_empty());
        assert_eq!(reader.state.data_health.unknown_records, 1);
        assert_eq!(reader.state.data_health.unknown_events, 1);
        assert_eq!(reader.state.data_health.malformed_records, 2);
        let samples = &reader.state.data_health.recent_diagnostics;
        assert!(samples.iter().any(|s| s.kind == "unknown_record"
            && s.detail.as_deref() == Some("mystery")
            && s.ordinal == Some(1)
            && s.cli_version.as_deref() == Some("0.149.0-alpha.4.1")
            && s.rollout_path == child));
        assert!(samples.iter().any(|s| s.kind == "unknown_event"
            && s.detail.as_deref() == Some("future_event")
            && s.byte_offset > 0));
        assert!(samples.iter().any(|s| s.kind == "malformed_call_id"));
        assert!(samples.iter().any(|s| {
            s.kind == "malformed_communication"
                && s.detail.as_deref() == Some("agent_message")
                && s.ordinal == Some(4)
                && s.rollout_path == child
                && s.byte_offset > 0
        }));
        assert_eq!(
            reader.poll().unwrap(),
            PollOutcome::Updated {
                records: 0,
                admitted: 0
            }
        );
    }
    #[test]
    fn poll_budget_is_shared_and_cursor_progress_is_fair() {
        let temp = tempfile::tempdir().unwrap();
        for index in 0..6 {
            let id = if index == 0 {
                "s".to_owned()
            } else {
                format!("c{index}")
            };
            let parent = if index == 0 {
                String::new()
            } else {
                ",\"parent_thread_id\":\"s\"".to_owned()
            };
            let path = temp.path().join(format!("rollout-{index}.jsonl"));
            std::fs::write(
                path,
                format!(
                    "{{\"type\":\"session_meta\",\"payload\":{{\"session_id\":\"s\",\"id\":\"{id}\",\"cli_version\":\"0.149.1\"{parent}}}}}\n"
                ),
            )
            .unwrap();
        }
        let discovery = discover(temp.path()).unwrap();
        let selected = group(discovery.admitted).pop().unwrap();
        let mut reader = SelectedReader::new(
            selected,
            discovery.pending,
            temp.path().to_owned(),
            temp.path().to_owned(),
        )
        .unwrap();
        let initial = reader
            .cursors
            .iter()
            .map(|cursor| cursor.byte_offset)
            .collect::<Vec<_>>();
        for cursor in &reader.cursors {
            append_unknown_record(&cursor.path, 40 * 1024);
        }

        reader.poll().unwrap();
        let first = reader
            .cursors
            .iter()
            .zip(&initial)
            .map(|(cursor, start)| cursor.byte_offset - start)
            .collect::<Vec<_>>();
        assert!(first.iter().sum::<u64>() <= APPEND_BUDGET as u64);
        assert!(first.iter().filter(|bytes| **bytes > 0).count() < first.len());

        reader.poll().unwrap();
        assert!(reader
            .cursors
            .iter()
            .zip(initial)
            .all(|(cursor, start)| cursor.byte_offset > start));
        let mut pending_paths = Vec::new();
        for index in 0..8 {
            let path = temp.path().join(format!("candidate-{index}.jsonl"));
            std::fs::write(
                &path,
                format!(
                    "{{\"type\":\"session_meta\",\"payload\":{{\"session_id\":\"s\",\"id\":\"p{index}\",\"parent_thread_id\":\"s\",\"cli_version\":\"0.149.1\",\"padding\":\"{}\"}}}}\n",
                    "x".repeat(30 * 1024)
                ),
            )
            .unwrap();
            reader.known_paths.insert(path.clone());
            reader
                .pending_cursors
                .insert(path.clone(), RolloutCursor::from_path_start(path.clone()));
            reader.pending.push(path.clone());
            pending_paths.push(path);
        }
        let first_pending = match reader.poll().unwrap() {
            PollOutcome::Updated { admitted, .. } => admitted,
            PollOutcome::Rebuilt => panic!("unexpected rebuild"),
        };
        assert!(first_pending > 0 && first_pending < pending_paths.len());
        for _ in 0..8 {
            reader.poll().unwrap();
        }
        assert!(pending_paths.iter().all(|path| {
            let id = path
                .file_stem()
                .unwrap()
                .to_string_lossy()
                .replace("candidate-", "p");
            reader.state.agents.contains_key(&id)
        }));
    }
    #[test]
    fn record_work_is_globally_bounded_and_long_pending_header_progresses() {
        let temp = tempfile::tempdir().unwrap();
        let mut paths = Vec::new();
        for (id, parent) in [("s", ""), ("c", ",\"parent_thread_id\":\"s\"")] {
            let path = temp.path().join(format!("rollout-{id}.jsonl"));
            std::fs::write(
                &path,
                format!(
                    "{{\"type\":\"session_meta\",\"payload\":{{\"session_id\":\"s\",\"id\":\"{id}\",\"cli_version\":\"0.149.1\"{parent}}}}}\n"
                ),
            )
            .unwrap();
            paths.push(path);
        }
        let discovery = discover(temp.path()).unwrap();
        let selected = group(discovery.admitted).pop().unwrap();
        let mut reader = SelectedReader::new(
            selected,
            discovery.pending,
            temp.path().to_owned(),
            temp.path().to_owned(),
        )
        .unwrap();
        for path in &paths {
            let mut file = std::fs::OpenOptions::new().append(true).open(path).unwrap();
            for ordinal in 0..3000 {
                writeln!(
                    file,
                    "{{\"ordinal\":{ordinal},\"type\":\"mystery\",\"payload\":{{}}}}"
                )
                .unwrap();
            }
        }
        let fair_pending = temp.path().join("rollout-fair-pending.jsonl");
        std::fs::write(
            &fair_pending,
            "{\"type\":\"session_meta\",\"payload\":{\"session_id\":\"s\",\"id\":\"fair\",\"parent_thread_id\":\"s\",\"cli_version\":\"0.149.1\"}}\n",
        )
        .unwrap();
        reader.known_paths.insert(fair_pending.clone());
        reader.pending_cursors.insert(
            fair_pending.clone(),
            RolloutCursor::from_path_start(fair_pending.clone()),
        );
        reader.pending.push(fair_pending);
        let (first_records, first_admitted) = match reader.poll().unwrap() {
            PollOutcome::Updated { records, admitted } => (records, admitted),
            PollOutcome::Rebuilt => panic!("unexpected rebuild"),
        };
        assert!(first_records <= 2048);
        assert!(first_records < 6000);
        assert_eq!(first_admitted, 1);
        assert!(paths.iter().any(|path| {
            reader
                .cursors
                .iter()
                .find(|cursor| &cursor.path == path)
                .unwrap()
                .byte_offset
                < std::fs::metadata(path).unwrap().len()
        }));
        for _ in 0..20 {
            reader.poll().unwrap();
        }
        for path in &paths {
            let cursor = reader
                .cursors
                .iter()
                .find(|cursor| &cursor.path == path)
                .unwrap();
            assert_eq!(cursor.byte_offset, std::fs::metadata(path).unwrap().len());
        }

        let pending = temp.path().join("rollout-long-header.jsonl");
        std::fs::write(
            &pending,
            format!(
                "{{\"type\":\"session_meta\",\"payload\":{{\"session_id\":\"s\",\"id\":\"long\",\"parent_thread_id\":\"s\",\"cli_version\":\"0.149.1\",\"padding\":\"{}\"}}}}\n",
                "x".repeat(70 * 1024)
            ),
        )
        .unwrap();
        reader.known_paths.insert(pending.clone());
        reader.pending_cursors.insert(
            pending.clone(),
            RolloutCursor::from_path_start(pending.clone()),
        );
        reader.pending.push(pending);
        let mut admitted = 0;
        for _ in 0..4 {
            if let PollOutcome::Updated {
                admitted: count, ..
            } = reader.poll().unwrap()
            {
                admitted += count;
            }
        }
        assert_eq!(admitted, 1);
        assert!(reader.state.agents.contains_key("long"));
    }

    #[test]
    fn pending_rebuild_resets_cursor_after_same_prefix_truncation() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("rollout-root.jsonl");
        std::fs::write(
            &root,
            "{\"type\":\"session_meta\",\"payload\":{\"session_id\":\"s\",\"id\":\"s\",\"cli_version\":\"0.149.1\"}}\n",
        )
        .unwrap();
        let discovery = discover(temp.path()).unwrap();
        let selected = group(discovery.admitted).pop().unwrap();
        let mut reader = SelectedReader::new(
            selected,
            Vec::new(),
            temp.path().to_owned(),
            temp.path().to_owned(),
        )
        .unwrap();

        let pending = temp.path().join("rollout-replaced.jsonl");
        let prefix = "{\"type\":\"session_meta\",\"payload\":{";
        std::fs::write(&pending, format!("{prefix}{}", "x".repeat(40 * 1024))).unwrap();
        reader.known_paths.insert(pending.clone());
        reader.pending_cursors.insert(
            pending.clone(),
            RolloutCursor::from_path_start(pending.clone()),
        );
        reader.pending.push(pending.clone());
        reader.poll().unwrap();
        let prior_offset = reader.pending_cursors[&pending].byte_offset;
        assert!(prior_offset > 0);

        std::fs::write(&pending, prefix).unwrap();
        reader.poll().unwrap();
        assert_eq!(
            reader.pending_cursors[&pending].byte_offset,
            std::fs::metadata(&pending).unwrap().len()
        );
        assert!(reader.pending_cursors[&pending].byte_offset < prior_offset);

        std::fs::write(
            &pending,
            "{\"type\":\"session_meta\",\"payload\":{\"session_id\":\"s\",\"id\":\"replacement\",\"parent_thread_id\":\"s\",\"cli_version\":\"0.149.1\"}}\n",
        )
        .unwrap();
        let mut admitted = 0;
        for _ in 0..3 {
            if let PollOutcome::Updated {
                admitted: count, ..
            } = reader.poll().unwrap()
            {
                admitted += count;
            }
        }
        assert_eq!(admitted, 1);
        assert!(reader.state.agents.contains_key("replacement"));
    }

    #[test]
    fn whole_session_recency_ranks_groups_and_explicit_selection_wins() {
        fn timed(
            session: &str,
            thread: &str,
            parent: Option<&str>,
            seconds: u32,
            cwd: Option<&Path>,
        ) -> RolloutMetadata {
            let mut value = meta(PathBuf::from(thread), session, thread, parent, 0);
            value.timestamp = Some(format!("2026-01-01T00:00:{seconds:02}Z").parse().unwrap());
            value.cwd = cwd.map(Path::to_owned);
            value
        }

        let cwd = Path::new("/selected");
        let groups = group(vec![
            timed("a", "a", None, 10, Some(cwd)),
            timed("a", "a-child", Some("a"), 30, None),
            timed("b", "b", None, 20, Some(cwd)),
            timed("b", "b-child", Some("b"), 15, Some(cwd)),
            timed("c", "c", None, 40, Some(Path::new("/elsewhere"))),
            timed("d", "d", None, 5, Some(Path::new("/elsewhere"))),
            timed("d", "d-child", Some("d"), 50, Some(cwd)),
        ]);
        assert_eq!(
            groups
                .iter()
                .map(|group| group.session_id.as_str())
                .collect::<Vec<_>>(),
            vec!["d", "c", "a", "b"]
        );
        assert_eq!(select(&groups, None, cwd).unwrap().session_id, "a");
        assert_eq!(
            select(&groups, None, Path::new("/absent"))
                .unwrap()
                .session_id,
            "d"
        );
        assert_eq!(select(&groups, Some("b"), cwd).unwrap().session_id, "b");
    }

    #[test]
    fn malformed_pending_metadata_is_retired_and_does_not_block_valid_child() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("rollout-root.jsonl");
        std::fs::write(
            &root,
            "{\"type\":\"session_meta\",\"payload\":{\"session_id\":\"s\",\"id\":\"s\",\"cli_version\":\"0.149.1\"}}\n",
        )
        .unwrap();
        let discovery = discover(temp.path()).unwrap();
        let selected = group(discovery.admitted).pop().unwrap();
        let mut reader = SelectedReader::new(
            selected,
            Vec::new(),
            temp.path().to_owned(),
            temp.path().to_owned(),
        )
        .unwrap();

        let malformed = temp.path().join("rollout-malformed.jsonl");
        std::fs::write(
            &malformed,
            "{\"type\":\"session_meta\",\"payload\":{\"id\":\"unrelated\",\"cli_version\":\"0.149.1\"}}\n",
        )
        .unwrap();
        for _ in 0..3 {
            reader.poll().unwrap();
        }
        assert!(!reader.pending.contains(&malformed));
        assert!(!reader.pending_cursors.contains_key(&malformed));
        assert_eq!(
            reader
                .state
                .data_health
                .recent_diagnostics
                .iter()
                .filter(|sample| sample.kind == "malformed_session_metadata")
                .count(),
            1
        );

        let valid = temp.path().join("rollout-valid.jsonl");
        std::fs::write(
            &valid,
            "{\"type\":\"session_meta\",\"payload\":{\"session_id\":\"s\",\"id\":\"valid\",\"parent_thread_id\":\"s\",\"cli_version\":\"0.149.1\"}}\n",
        )
        .unwrap();
        let mut admitted = 0;
        for _ in 0..4 {
            if let PollOutcome::Updated {
                admitted: count, ..
            } = reader.poll().unwrap()
            {
                admitted += count;
            }
        }
        assert_eq!(admitted, 1);
        assert!(reader.state.agents.contains_key("valid"));
    }
}
