use crate::model::{
    has_malformed_call_id, has_malformed_communication, reduce, sanitise, CoverageLevel,
    DiagnosticSample, SessionState,
};
use crate::schema::{lookup, SchemaStatus};
use anyhow::{bail, Context, Result};
use chrono::{DateTime, Utc};
use serde_json::Value;
use std::collections::{HashMap, HashSet, VecDeque};
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::SystemTime;
use walkdir::{IntoIter, WalkDir};

pub const MAX_RECORD_SIZE: usize = 1024 * 1024;
pub const APPEND_BUDGET: usize = 256 * 1024;
const INITIAL_LOAD_BYTE_BUDGET: usize = 8 * 1024 * 1024;
const POLL_WORK_BUDGET: usize = 2048;
const INITIAL_LOAD_WORK_BUDGET: usize = 65_536;
const READ_SLICE: usize = 1024 * 1024;
const DIRECTORY_ENTRY_BUDGET: usize = 1024;

type DiscoveryScan = IntoIter;

fn discovery_scan(root: &Path) -> DiscoveryScan {
    WalkDir::new(root).follow_links(false).into_iter()
}

#[derive(Debug, Clone)]
pub struct RolloutMetadata {
    pub path: PathBuf,
    pub session_id: String,
    pub thread_id: String,
    pub parent_thread_id: Option<String>,
    pub cwd: Option<PathBuf>,
    pub repository_url: Option<String>,
    pub timestamp: Option<DateTime<Utc>>,
    pub modified_at: Option<DateTime<Utc>>,
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
    let modified_at = std::fs::metadata(&path)
        .and_then(|metadata| metadata.modified())
        .ok()
        .and_then(|time| (time != SystemTime::UNIX_EPOCH).then(|| DateTime::<Utc>::from(time)));
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
        repository_url: p
            .pointer("/git/repository_url")
            .and_then(Value::as_str)
            .map(str::to_owned),
        timestamp: crate::model::parse_time(p.get("timestamp")),
        modified_at,
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
    for entry in discovery_scan(sessions_dir) {
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
pub fn session_updated_at(group: &SessionGroup) -> Option<DateTime<Utc>> {
    group
        .rollouts
        .iter()
        .filter_map(|meta| meta.modified_at)
        .max()
        .or_else(|| {
            group
                .rollouts
                .iter()
                .filter_map(|meta| meta.timestamp)
                .max()
        })
}

fn compare_group_recency(a: &SessionGroup, b: &SessionGroup) -> std::cmp::Ordering {
    session_updated_at(a)
        .cmp(&session_updated_at(b))
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
pub fn select<'a>(groups: &'a [SessionGroup], requested: Option<&str>) -> Result<&'a SessionGroup> {
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
    groups.first().context("no sessions found")
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

        let buffer_start = self.byte_offset - self.partial_line.len() as u64;
        let mut consumed = 0;
        let mut count = 0;
        while count < max_records {
            let Some(relative_pos) = self.partial_line[consumed..]
                .iter()
                .position(|byte| *byte == b'\n')
            else {
                break;
            };
            let end = consumed + relative_pos + 1;
            let offset = buffer_start + consumed as u64;
            if self.discarding_oversized {
                self.discarding_oversized = false;
                let start = self.oversized_start.take().unwrap_or(offset);
                on_record(start, Err(RecordError::Oversized));
                count += 1;
                consumed = end;
                continue;
            }
            let line = &self.partial_line[consumed..end];
            if line.len() > MAX_RECORD_SIZE {
                on_record(offset, Err(RecordError::Oversized));
                count += 1;
                consumed = end;
                continue;
            }
            let parsed = serde_json::from_slice::<Value>(line).map_err(RecordError::Malformed);
            if let Ok(value) = &parsed {
                self.last_ordinal = value
                    .get("ordinal")
                    .and_then(Value::as_u64)
                    .or(self.last_ordinal);
            }
            on_record(offset, parsed);
            count += 1;
            consumed = end;
        }
        if consumed > 0 {
            self.partial_line.drain(..consumed);
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

fn agent_from_meta(
    meta: &RolloutMetadata,
    catalogue_dir: &Path,
) -> Result<crate::model::AgentState> {
    let mut agent = crate::model::AgentState::new(meta.thread_id.clone(), meta.cli_version.clone());
    agent.parent_thread_id = meta.parent_thread_id.clone();
    agent.agent_path = meta.agent_path.as_deref().map(sanitise);
    agent.agent_role = meta.agent_role.as_deref().map(sanitise);
    agent.agent_nickname = meta.agent_nickname.as_deref().map(sanitise);
    agent.own_history_start_ordinal = meta.history_start;
    match lookup(catalogue_dir, &meta.cli_version)? {
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
    catalogue_dir: &Path,
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
        let mut agent = agent_from_meta(meta, catalogue_dir)?;
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

#[derive(Debug)]
struct InitialLoad {
    meta: RolloutMetadata,
    cursor: RolloutCursor,
    target_offset: u64,
}

struct HintedDiscovery {
    root: PathBuf,
    scan: DiscoveryScan,
}

pub struct SelectedReader {
    pub group: SessionGroup,
    pub state: SessionState,
    cursors: Vec<RolloutCursor>,
    initial_load: VecDeque<InitialLoad>,
    pending: Vec<PathBuf>,
    pending_cursors: HashMap<PathBuf, RolloutCursor>,
    known_paths: HashSet<PathBuf>,
    sessions_root: PathBuf,
    discovery_scan: DiscoveryScan,
    hinted_discovery: VecDeque<HintedDiscovery>,
    catalogue_dir: PathBuf,
    cursor_next: usize,
    pending_next: usize,
}
impl SelectedReader {
    pub fn new(
        group: SessionGroup,
        pending: Vec<PathBuf>,
        sessions_root: PathBuf,
        catalogue_dir: PathBuf,
    ) -> Result<Self> {
        let root = group.rollouts[group.root].clone();
        let mut state = SessionState {
            session_id: group.session_id.clone(),
            cwd: root.cwd.clone(),
            started_at: root.timestamp,
            ..Default::default()
        };
        state.agents.insert(
            root.thread_id.clone(),
            agent_from_meta(&root, &catalogue_dir)?,
        );

        // Discovery has already bounded and parsed metadata. Sort it once, then make
        // stable parent-aware passes without reopening rollout content here.
        let mut remaining = group.rollouts.to_vec();
        remaining.sort_by(|a, b| {
            a.depth
                .unwrap_or(u64::MAX)
                .cmp(&b.depth.unwrap_or(u64::MAX))
                .then_with(|| a.agent_path.cmp(&b.agent_path))
                .then_with(|| a.thread_id.cmp(&b.thread_id))
                .then_with(|| a.path.cmp(&b.path))
        });
        let root_index = remaining
            .iter()
            .position(|meta| meta.path == root.path)
            .context("selected root metadata missing")?;
        let mut ordered = Vec::with_capacity(remaining.len());
        ordered.push(remaining.remove(root_index));
        let mut placed = HashSet::from([root.thread_id.clone()]);
        while !remaining.is_empty() {
            let before = remaining.len();
            remaining.retain(|meta| {
                let ready = meta
                    .parent_thread_id
                    .as_ref()
                    .is_some_and(|parent| placed.contains(parent));
                if ready {
                    placed.insert(meta.thread_id.clone());
                    ordered.push(meta.clone());
                }
                !ready
            });
            if remaining.len() == before {
                // Deterministic fallback for orphaned or cyclic metadata.
                ordered.append(&mut remaining);
            }
        }
        let initial_load = ordered
            .into_iter()
            .map(|meta| {
                let target_offset = std::fs::metadata(&meta.path)?.len();
                let cursor = RolloutCursor::new(&meta);
                Ok(InitialLoad {
                    meta,
                    cursor,
                    target_offset,
                })
            })
            .collect::<Result<VecDeque<_>>>()?;
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
        let discovery_scan = discovery_scan(&sessions_root);
        Ok(Self {
            group,
            state,
            cursors: Vec::new(),
            initial_load,
            pending,
            pending_cursors,
            known_paths,
            sessions_root,
            discovery_scan,
            hinted_discovery: VecDeque::new(),
            catalogue_dir,
            cursor_next: 0,
            pending_next: 0,
        })
    }

    fn queue_spawn_discovery_hints(&mut self) {
        for hint in self.state.take_spawned_agent_hints() {
            if self
                .state
                .agents
                .values()
                .any(|agent| agent.agent_path.as_deref() == Some(hint.agent_path.as_str()))
            {
                continue;
            }
            let root = self
                .sessions_root
                .join(hint.observed_at.format("%Y/%m/%d").to_string());
            if root.is_dir()
                && !self
                    .hinted_discovery
                    .iter()
                    .any(|queued| queued.root == root)
            {
                self.hinted_discovery.push_back(HintedDiscovery {
                    scan: discovery_scan(&root),
                    root,
                });
            }
        }
    }

    fn queue_discovered_rollout(&mut self, path: &Path) {
        if path
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
    pub fn is_loading(&self) -> bool {
        !self.initial_load.is_empty()
    }
    pub fn poll(&mut self) -> Result<PollOutcome> {
        self.queue_spawn_discovery_hints();
        let loading_at_start = !self.initial_load.is_empty();
        let (mut remaining_bytes, mut remaining_work) = if loading_at_start {
            (
                INITIAL_LOAD_BYTE_BUDGET + APPEND_BUDGET,
                INITIAL_LOAD_WORK_BUDGET + POLL_WORK_BUDGET,
            )
        } else {
            (APPEND_BUDGET, POLL_WORK_BUDGET)
        };
        let mut records = 0;
        let initial_byte_floor = APPEND_BUDGET;
        let initial_work_floor = POLL_WORK_BUDGET;

        while remaining_bytes > initial_byte_floor
            && remaining_work > initial_work_floor
            && !self.initial_load.is_empty()
        {
            let load = self.initial_load.front_mut().expect("initial load exists");
            if !self.state.agents.contains_key(&load.meta.thread_id) {
                let agent = agent_from_meta(&load.meta, &self.catalogue_dir)?;
                self.state.agents.insert(load.meta.thread_id.clone(), agent);
                remaining_work -= 1;
                if remaining_work == initial_work_floor {
                    continue;
                }
            }
            if load.cursor.byte_offset >= load.target_offset {
                let finished = self.initial_load.pop_front().expect("initial load exists");
                self.cursors.push(finished.cursor);
                remaining_work -= 1;
                continue;
            }

            let agent = self
                .state
                .agents
                .get_mut(&load.meta.thread_id)
                .context("initial-load agent missing")?;
            let health = &mut self.state.data_health;
            let path = load.cursor.path.clone();
            let version = agent.cli_version.clone();
            let boundary = load.cursor.history_start;
            let mut crossed = load.cursor.crossed_history_start;
            let before = load.cursor.byte_offset;
            let budget = READ_SLICE
                .min(remaining_bytes - initial_byte_floor)
                .min((load.target_offset - before) as usize);
            let work_budget = remaining_work - initial_work_floor;
            let outcome = load
                .cursor
                .tail_bounded(budget, work_budget, |offset, record| {
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
                })?;
            load.cursor.crossed_history_start = crossed;
            let spent = (load.cursor.byte_offset - before) as usize;
            remaining_bytes -= spent.max(1).min(remaining_bytes);
            let processed = match outcome {
                TailOutcome::Records(count) => count,
                TailOutcome::RebuildRequired => {
                    let (state, cursors) = load_group(&self.group, &self.catalogue_dir)?;
                    self.state = state;
                    self.cursors = cursors;
                    self.initial_load.clear();
                    self.cursor_next = 0;
                    return Ok(PollOutcome::Rebuilt);
                }
            };
            remaining_work -= processed.max(1).min(remaining_work);
        }
        // The bulk catch-up allowance belongs only to initial history. Preserve
        // the ordinary budget boundary for discovery, live tails, and pending files.
        remaining_bytes = remaining_bytes.min(APPEND_BUDGET);
        remaining_work = remaining_work.min(POLL_WORK_BUDGET);
        let directory_entry_budget = if loading_at_start {
            DIRECTORY_ENTRY_BUDGET / 4
        } else {
            DIRECTORY_ENTRY_BUDGET
        };
        let mut discovery_entries = 0;
        while discovery_entries < directory_entry_budget
            && remaining_bytes > 0
            && remaining_work > 0
            && !self.hinted_discovery.is_empty()
        {
            let next = self
                .hinted_discovery
                .front_mut()
                .expect("hinted discovery exists")
                .scan
                .next();
            let Some(entry) = next else {
                self.hinted_discovery.pop_front();
                continue;
            };
            remaining_bytes -= 1;
            remaining_work -= 1;
            discovery_entries += 1;
            let root = &self
                .hinted_discovery
                .front()
                .expect("hinted discovery exists")
                .root;
            let entry = entry.with_context(|| format!("traverse {}", root.display()))?;
            if entry.file_type().is_file() {
                self.queue_discovered_rollout(entry.path());
            }
        }
        for _ in discovery_entries..directory_entry_budget {
            if remaining_bytes == 0 || remaining_work == 0 {
                break;
            }
            remaining_bytes -= 1;
            remaining_work -= 1;
            let Some(entry) = self.discovery_scan.next() else {
                self.discovery_scan = discovery_scan(&self.sessions_root);
                break;
            };
            let entry =
                entry.with_context(|| format!("traverse {}", self.sessions_root.display()))?;
            if entry.file_type().is_file() {
                self.queue_discovered_rollout(entry.path());
            }
        }

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
            let budget = READ_SLICE
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
                    let (state, cursors) = load_group(&self.group, &self.catalogue_dir)?;
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
                READ_SLICE.min(remaining_bytes),
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
                let agent = agent_from_meta(&meta, &self.catalogue_dir)?;
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

    fn finish_initial_load(reader: &mut SelectedReader) {
        while reader.is_loading() {
            reader.poll().unwrap();
        }
    }

    #[test]
    fn many_tiny_initial_rollouts_complete_in_one_poll() {
        let temp = tempfile::tempdir().unwrap();
        let mut rollouts = Vec::new();
        for index in 0..128 {
            let id = format!("agent-{index:03}");
            let path = temp.path().join(format!("rollout-{index:03}.jsonl"));
            std::fs::write(&path, "").unwrap();
            let parent = (index > 0).then(|| format!("agent-{:03}", index - 1));
            let mut metadata = meta(path, "session", &id, parent.as_deref(), 0);
            metadata.depth = Some(index as u64);
            rollouts.push(metadata);
        }
        rollouts.reverse();
        let root = rollouts
            .iter()
            .position(|metadata| metadata.thread_id == "agent-000")
            .unwrap();
        let group = SessionGroup {
            session_id: "session".into(),
            rollouts,
            root,
        };
        let mut reader = SelectedReader::new(
            group,
            Vec::new(),
            temp.path().to_owned(),
            temp.path().to_owned(),
        )
        .unwrap();

        assert_eq!(reader.state.agents.len(), 1);
        assert!(reader.is_loading());
        assert_eq!(
            reader.poll().unwrap(),
            PollOutcome::Updated {
                records: 0,
                admitted: 0
            }
        );
        assert!(!reader.is_loading());
        assert_eq!(reader.state.agents.len(), 128);
        assert_eq!(reader.cursors.len(), 128);
        assert_eq!(
            reader.cursors[0].path,
            temp.path().join("rollout-000.jsonl")
        );
        assert_eq!(
            reader.cursors[127].path,
            temp.path().join("rollout-127.jsonl")
        );
    }

    #[test]
    fn large_initial_history_uses_bulk_but_bounded_budget() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("large.jsonl");
        let record = format!(
            "{{\"type\":\"mystery\",\"payload\":{{\"value\":\"{}\"}}}}\n",
            "x".repeat(1024)
        );
        let mut history = String::new();
        while history.len() <= INITIAL_LOAD_BYTE_BUDGET + READ_SLICE {
            history.push_str(&record);
        }
        std::fs::write(&path, history).unwrap();
        let group = SessionGroup {
            session_id: "session".into(),
            rollouts: vec![meta(path, "session", "session", None, 0)],
            root: 0,
        };
        let mut reader = SelectedReader::new(
            group,
            Vec::new(),
            temp.path().to_owned(),
            temp.path().to_owned(),
        )
        .unwrap();

        let outcome = reader.poll().unwrap();
        let PollOutcome::Updated { records, .. } = outcome else {
            panic!("unexpected rebuild");
        };
        let consumed = reader.initial_load.front().unwrap().cursor.byte_offset;
        assert_eq!(consumed, INITIAL_LOAD_BYTE_BUDGET as u64);
        assert!(consumed > APPEND_BUDGET as u64);
        assert!(records > POLL_WORK_BUDGET);
        assert!(records <= INITIAL_LOAD_WORK_BUDGET);
        assert!(reader.is_loading());
    }
    #[test]
    fn selected_reader_is_root_only_then_parent_first_and_equivalent() {
        let temp = tempfile::tempdir().unwrap();
        let paths = ["root", "child", "grand"].map(|id| temp.path().join(format!("{id}.jsonl")));
        std::fs::write(
            &paths[0],
            concat!(
                "{\"ordinal\":1,\"type\":\"event_msg\",\"payload\":{\"type\":\"task_started\"}}\n",
                "{\"ordinal\":2,\"type\":\"mystery\",\"payload\":{}}\n"
            ),
        )
        .unwrap();
        for path in &paths[1..] {
            std::fs::write(
                path,
                "{\"ordinal\":1,\"type\":\"event_msg\",\"payload\":{\"type\":\"task_started\"}}\n",
            )
            .unwrap();
        }
        let mut root = meta(paths[0].clone(), "s", "root", None, 0);
        root.depth = Some(0);
        let mut child = meta(paths[1].clone(), "s", "child", Some("root"), 0);
        child.depth = Some(1);
        let mut grand = meta(paths[2].clone(), "s", "grand", Some("child"), 0);
        grand.depth = Some(2);
        let group = SessionGroup {
            session_id: "s".into(),
            rollouts: vec![grand, root, child],
            root: 1,
        };
        let (expected, expected_cursors) = load_group(&group, temp.path()).unwrap();
        let mut reader = SelectedReader::new(
            group,
            Vec::new(),
            temp.path().to_owned(),
            temp.path().to_owned(),
        )
        .unwrap();

        assert!(reader.is_loading());
        assert_eq!(reader.state.agents.len(), 1);
        assert_eq!(
            reader.state.agents["root"].latest_turn.status,
            crate::model::TurnStatus::Pending
        );
        assert_eq!(reader.state.data_health.unknown_records, 0);
        assert!(reader.cursors.is_empty());
        assert_eq!(
            reader
                .initial_load
                .iter()
                .map(|load| load.meta.thread_id.as_str())
                .collect::<Vec<_>>(),
            ["root", "child", "grand"]
        );

        let mut appearances = Vec::new();
        let mut previous = 1;
        while reader.is_loading() {
            if let PollOutcome::Updated { records, .. } = reader.poll().unwrap() {
                assert!(records <= INITIAL_LOAD_WORK_BUDGET);
            }
            if reader.state.agents.len() > previous {
                appearances.push(reader.state.agents.len());
                previous = reader.state.agents.len();
            }
        }
        assert_eq!(appearances, [3]);
        assert!(!reader.is_loading());
        for id in ["root", "child", "grand"] {
            assert_eq!(
                reader.state.agents[id].latest_turn.status,
                expected.agents[id].latest_turn.status
            );
            assert_eq!(
                reader.state.agents[id].last_ordinal,
                expected.agents[id].last_ordinal
            );
        }
        assert_eq!(
            reader.state.data_health.unknown_records,
            expected.data_health.unknown_records
        );
        let mut actual_offsets = reader
            .cursors
            .iter()
            .map(|cursor| (&cursor.path, cursor.byte_offset))
            .collect::<Vec<_>>();
        let mut expected_offsets = expected_cursors
            .iter()
            .map(|cursor| (&cursor.path, cursor.byte_offset))
            .collect::<Vec<_>>();
        actual_offsets.sort();
        expected_offsets.sort();
        assert_eq!(actual_offsets, expected_offsets);
    }

    #[test]
    fn initial_backlog_does_not_starve_live_or_pending_in_saturated_directory() {
        let temp = tempfile::tempdir().unwrap();
        let root_path = temp.path().join("root.jsonl");
        let backlog_path = temp.path().join("backlog.jsonl");
        let pending_path = temp.path().join("pending.jsonl");
        std::fs::write(&root_path, "{}\n").unwrap();
        let mut history = String::new();
        let padding = "x".repeat(8 * 1024);
        for ordinal in 0..3000 {
            history.push_str(&format!(
                "{{\"ordinal\":{ordinal},\"type\":\"mystery\",\"payload\":{{\"padding\":\"{padding}\"}}}}\n"
            ));
        }
        std::fs::write(&backlog_path, history).unwrap();
        for index in 0..=1024 {
            std::fs::write(temp.path().join(format!("decoy-{index:04}.tmp")), "").unwrap();
        }
        let group = SessionGroup {
            session_id: "s".into(),
            rollouts: vec![
                meta(root_path.clone(), "s", "root", None, 0),
                meta(backlog_path, "s", "backlog", Some("root"), 0),
            ],
            root: 0,
        };
        let mut reader = SelectedReader::new(
            group,
            Vec::new(),
            temp.path().to_owned(),
            temp.path().to_owned(),
        )
        .unwrap();
        reader.poll().unwrap();
        assert!(reader.is_loading());
        std::fs::write(&pending_path, "{\"type\":\"session_meta\",\"payload\":{\"session_id\":\"s\",\"id\":\"child\",\"parent_thread_id\":\"root\",\"cli_version\":\"0.149.1\"}}\n").unwrap();
        reader.known_paths.insert(pending_path.clone());
        reader.pending.push(pending_path.clone());
        reader.pending_cursors.insert(
            pending_path.clone(),
            RolloutCursor::from_path_start(pending_path),
        );
        let mut root = std::fs::OpenOptions::new()
            .append(true)
            .open(root_path)
            .unwrap();
        writeln!(
            root,
            "{{\"ordinal\":9000,\"type\":\"event_msg\",\"payload\":{{\"type\":\"task_started\"}}}}"
        )
        .unwrap();

        let outcome = reader.poll().unwrap();
        assert!(reader.is_loading());
        assert!(
            matches!(outcome, PollOutcome::Updated { records, admitted: 1 } if records <= 2048)
        );
        assert!(reader.state.agents.contains_key("child"));
        assert_eq!(
            reader.state.agents["root"].latest_turn.status,
            crate::model::TurnStatus::Running
        );
    }

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
            repository_url: None,
            timestamp: None,
            modified_at: None,
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
    fn record_budget_preserves_complete_buffered_lines() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("buffered.jsonl");
        std::fs::write(&path, b"{}\n{}\n{}\n").unwrap();
        let mut cursor = RolloutCursor::from_start(path);
        let mut offsets = Vec::new();

        assert_eq!(
            cursor
                .tail_bounded(9, 1, |offset, value| {
                    value.unwrap();
                    offsets.push(offset);
                })
                .unwrap(),
            TailOutcome::Records(1)
        );
        assert_eq!(cursor.byte_offset, 9);
        assert_eq!(cursor.partial_line, b"{}\n{}\n");
        assert_eq!(
            cursor
                .tail_bounded(0, 2, |offset, value| {
                    value.unwrap();
                    offsets.push(offset);
                })
                .unwrap(),
            TailOutcome::Records(2)
        );
        assert_eq!(offsets, [0, 3, 6]);
        assert!(cursor.partial_line.is_empty());
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
        finish_initial_load(&mut reader);
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
        finish_initial_load(&mut reader);
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
    fn spawn_output_prioritises_live_child_discovery() {
        let temp = tempfile::tempdir().unwrap();
        let date_dir = temp.path().join("2026/09/03");
        std::fs::create_dir_all(&date_dir).unwrap();
        let root = date_dir.join("rollout-root.jsonl");
        std::fs::write(&root, "{\"type\":\"session_meta\",\"payload\":{\"session_id\":\"s\",\"id\":\"s\",\"cli_version\":\"0.149.0-alpha.4.1\"}}\n").unwrap();

        let discovery = discover(temp.path()).unwrap();
        let group = group(discovery.admitted).pop().unwrap();
        let mut reader = SelectedReader::new(
            group,
            discovery.pending,
            temp.path().to_owned(),
            temp.path().to_owned(),
        )
        .unwrap();
        finish_initial_load(&mut reader);

        let noise = temp.path().join("noise");
        std::fs::create_dir_all(&noise).unwrap();
        for index in 0..DIRECTORY_ENTRY_BUDGET * 2 {
            std::fs::write(noise.join(format!("payload-{index}.json")), "{}").unwrap();
        }
        reader.discovery_scan = discovery_scan(&noise);

        let child = date_dir.join("rollout-child.jsonl");
        std::fs::write(&child, "{\"type\":\"session_meta\",\"payload\":{\"session_id\":\"s\",\"id\":\"child\",\"parent_thread_id\":\"s\",\"agent_path\":\"/root/child\",\"cli_version\":\"0.149.0-alpha.4.1\"}}\n").unwrap();
        let mut root_file = std::fs::OpenOptions::new()
            .append(true)
            .open(&root)
            .unwrap();
        writeln!(
            root_file,
            "{}",
            serde_json::json!({
                "timestamp": "2026-09-03T13:41:58Z",
                "type": "response_item",
                "payload": {
                    "type": "function_call",
                    "name": "spawn_agent",
                    "call_id": "spawn",
                    "arguments": "{}"
                }
            })
        )
        .unwrap();
        writeln!(
            root_file,
            "{}",
            serde_json::json!({
                "timestamp": "2026-09-03T13:41:59Z",
                "type": "response_item",
                "payload": {
                    "type": "function_call_output",
                    "call_id": "spawn",
                    "output": "{\"task_name\":\"/root/child\"}"
                }
            })
        )
        .unwrap();

        assert_eq!(
            reader.poll().unwrap(),
            PollOutcome::Updated {
                records: 2,
                admitted: 0
            }
        );
        assert!(!reader.state.agents.contains_key("child"));
        assert_eq!(
            reader.poll().unwrap(),
            PollOutcome::Updated {
                records: 0,
                admitted: 1
            }
        );
        assert!(reader.state.agents.contains_key("child"));
    }
    #[test]
    fn selector_prefix_and_post_start_discovery_with_diagnostics() {
        let t = tempfile::tempdir().unwrap();
        let root = t.path().join("rollout-root.jsonl");
        std::fs::write(&root, "{\"type\":\"session_meta\",\"payload\":{\"session_id\":\"abcdef\",\"id\":\"abcdef\",\"cli_version\":\"0.149.0-alpha.4.1\"}}\n").unwrap();
        let groups = group(discover(t.path()).unwrap().admitted);
        assert_eq!(select(&groups, Some("abc")).unwrap().session_id, "abcdef");

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
        assert_eq!(select(&ambiguous, Some("abc")).unwrap().session_id, "abc");
        assert!(select(&ambiguous, Some("ab"))
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
        finish_initial_load(&mut reader);
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
        finish_initial_load(&mut reader);
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
                    "x".repeat(40 * 1024)
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
        finish_initial_load(&mut reader);
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
        finish_initial_load(&mut reader);

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
            let time = format!("2026-01-01T00:00:{seconds:02}Z").parse().unwrap();
            value.timestamp = Some(time);
            value.modified_at = Some(time);
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
        assert_eq!(select(&groups, None).unwrap().session_id, "d");
        assert_eq!(select(&groups, Some("b")).unwrap().session_id, "b");
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
        finish_initial_load(&mut reader);

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
