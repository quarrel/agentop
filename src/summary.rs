//! Run accounting is independent of the bounded interaction history.
use crate::model::{
    sanitise, AgentState, InFlightCall, SessionState, ToolInteractionState, TurnStatus,
};
use chrono::{DateTime, Utc};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

const TURN_INTERVAL_LIMIT: usize = 4096;
const TOOL_KIND_LIMIT: usize = 128;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum CallKind {
    #[default]
    Tool,
    AgentWait,
    ExecWait,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Times {
    pub agent_ms: u64,
    pub tool_ms: u64,
    pub agent_wait_ms: u64,
    pub exec_wait_ms: u64,
}
impl Times {
    pub fn add(&mut self, other: Self) {
        self.agent_ms += other.agent_ms;
        self.tool_ms += other.tool_ms;
        self.agent_wait_ms += other.agent_wait_ms;
        self.exec_wait_ms += other.exec_wait_ms;
    }
    pub fn outside_tools_ms(self) -> u64 {
        self.agent_ms - self.tool_ms
    }
}

#[derive(Debug, Clone, Default)]
pub struct ToolTotals {
    pub calls: u64,
    pub paired: u64,
    pub latency_ms: u64,
    pub longest_ms: u64,
    pub yielded: u64,
    pub incomplete: u64,
}
impl ToolTotals {
    fn add(&mut self, other: &Self) {
        self.calls += other.calls;
        self.paired += other.paired;
        self.latency_ms += other.latency_ms;
        self.longest_ms = self.longest_ms.max(other.longest_ms);
        self.yielded += other.yielded;
        self.incomplete += other.incomplete;
    }
}

// Each field is optional: an absent counter is not a measured zero.
#[derive(Debug, Clone, Default)]
pub struct Tokens {
    pub input: Option<u64>,
    pub cached: Option<u64>,
    pub output: Option<u64>,
    pub reasoning: Option<u64>,
    pub total: Option<u64>,
}
impl Tokens {
    fn from_value(value: &Value) -> Self {
        Self {
            input: value.get("input_tokens").and_then(Value::as_u64),
            cached: value.get("cached_input_tokens").and_then(Value::as_u64),
            output: value.get("output_tokens").and_then(Value::as_u64),
            reasoning: value.get("reasoning_output_tokens").and_then(Value::as_u64),
            total: value.get("total_tokens").and_then(Value::as_u64),
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct RunMetrics {
    pub times: Times,
    pub tokens: Tokens,
    pub tools: BTreeMap<String, ToolTotals>,
    pub turns: u64,
    pub compactions: u64,
    pub peak_context_percent: Option<u16>,
    pub timing_gaps: u64,
    pub incomplete_turns: u64,
    pub unmatched_outputs: u64,
    pub token_decreases: u64,
    pub first_start: Option<DateTime<Utc>>,
    pub last_end: Option<DateTime<Utc>>,
    last_at: Option<DateTime<Utc>>,
    current_start: Option<DateTime<Utc>>,
    intervals: Vec<(DateTime<Utc>, DateTime<Utc>)>,
    pub dropped_intervals: u64,
    pub spawned_paths: BTreeSet<String>,
    pub dropped_spawn_hints: u64,
}

fn duration_ms(start: DateTime<Utc>, end: DateTime<Utc>) -> Option<u64> {
    u64::try_from(end.signed_duration_since(start).num_milliseconds()).ok()
}

impl RunMetrics {
    pub fn advance<'a>(
        &mut self,
        timestamp: Option<DateTime<Utc>>,
        running: bool,
        calls: impl Iterator<Item = &'a InFlightCall>,
    ) {
        let Some(at) = timestamp else {
            if running {
                self.timing_gaps += 1;
                self.last_at = None;
            }
            return;
        };
        if let Some(previous) = self.last_at {
            let Some(ms) = duration_ms(previous, at) else {
                self.timing_gaps += 1;
                return;
            };
            if running {
                self.times.agent_ms += ms;
                let mut tool = false;
                let mut agent_wait = false;
                let mut exec_wait = false;
                for call in calls {
                    tool = true;
                    agent_wait |= call.timing_kind == CallKind::AgentWait;
                    exec_wait |= call.timing_kind == CallKind::ExecWait;
                }
                if tool {
                    self.times.tool_ms += ms;
                }
                if agent_wait {
                    self.times.agent_wait_ms += ms;
                }
                if exec_wait {
                    self.times.exec_wait_ms += ms;
                }
            }
        }
        self.last_at = Some(at);
    }

    pub fn start_turn(&mut self, at: Option<DateTime<Utc>>, replacing: bool) {
        if replacing {
            self.incomplete_turns += 1;
            self.end_turn(at);
        }
        self.turns += 1;
        self.current_start = at;
        self.last_at = at;
        if let Some(at) = at {
            self.first_start = Some(self.first_start.map_or(at, |first| first.min(at)));
        } else {
            self.timing_gaps += 1;
        }
    }

    pub fn end_turn(&mut self, at: Option<DateTime<Utc>>) {
        match (self.current_start.take(), at) {
            (Some(start), Some(end)) if end >= start => {
                self.last_end = Some(self.last_end.map_or(end, |last| last.max(end)));
                if self.intervals.len() < TURN_INTERVAL_LIMIT {
                    self.intervals.push((start, end));
                } else {
                    self.dropped_intervals += 1;
                }
            }
            _ => self.timing_gaps += 1,
        }
    }

    fn tool(&mut self, name: &str) -> &mut ToolTotals {
        let name = sanitise(name);
        let key = if self.tools.contains_key(&name) || self.tools.len() < TOOL_KIND_LIMIT {
            name
        } else {
            "(other tools)".into()
        };
        self.tools.entry(key).or_default()
    }

    pub fn record_spawn(&mut self, path: &str) {
        if self.spawned_paths.len() < 1024 || self.spawned_paths.contains(path) {
            self.spawned_paths.insert(sanitise(path));
        } else {
            self.dropped_spawn_hints += 1;
        }
    }

    pub fn start_call(&mut self, name: &str) {
        self.tool(name).calls += 1;
    }

    pub fn finish_call(
        &mut self,
        call: &InFlightCall,
        end: Option<DateTime<Utc>>,
        state: ToolInteractionState,
    ) {
        let ms = call
            .started_at
            .zip(end)
            .and_then(|(start, end)| duration_ms(start, end));
        let tool = self.tool(&call.tool_name);
        if state == ToolInteractionState::EndedWithoutReturn {
            tool.incomplete += 1;
        } else if let Some(ms) = ms {
            tool.paired += 1;
            tool.latency_ms += ms;
            tool.longest_ms = tool.longest_ms.max(ms);
            tool.yielded += u64::from(state == ToolInteractionState::Yielded);
        } else {
            tool.incomplete += 1;
            self.timing_gaps += 1;
        }
    }

    pub fn observe_tokens(&mut self, payload: &Value) {
        if let Some(total) = payload
            .get("info")
            .and_then(|info| info.get("total_token_usage"))
            .filter(|total| total.is_object())
        {
            let tokens = Tokens::from_value(total);
            if self
                .tokens
                .total
                .zip(tokens.total)
                .is_some_and(|(old, new)| new < old)
            {
                self.token_decreases += 1;
            }
            self.tokens = tokens;
        }
    }
}

pub struct AgentSummary<'a> {
    pub agent: &'a AgentState,
    pub times: Times,
    pub stale: bool,
}

pub struct RunSummary<'a> {
    pub agents: Vec<AgentSummary<'a>>,
    pub elapsed_ms: Option<u64>,
    pub times: Times,
    pub peak_concurrency: Option<usize>,
    pub average_concurrency: Option<f64>,
    pub tools: BTreeMap<String, ToolTotals>,
    pub live: bool,
    pub timing_complete: bool,
}

impl<'a> RunSummary<'a> {
    pub fn new(state: &'a SessionState, now: DateTime<Utc>, loading: bool) -> Self {
        let mut agents = Vec::new();
        let mut times = Times::default();
        let mut tools: BTreeMap<String, ToolTotals> = BTreeMap::new();
        let mut events = Vec::new();
        let mut start: Option<DateTime<Utc>> = None;
        let mut end: Option<DateTime<Utc>> = None;
        let mut live = false;
        let mut complete_timing = !loading
            && state.data_health.malformed_records == 0
            && state.data_health.oversized_records == 0;
        for agent in state.agents.values() {
            let stale = !loading && state.stale_evidence(agent).is_some();
            let running = agent.latest_turn.status == TurnStatus::Running;
            // Project on a copy; rendering never changes reduced evidence.
            let mut metrics = agent.metrics.clone();
            if running && !stale && !loading {
                metrics.advance(Some(now), true, agent.in_flight_calls.values());
                live = true;
            }
            let horizon = if running {
                metrics.last_at
            } else {
                metrics.last_end
            };
            if let Some(at) = metrics.first_start {
                start = Some(start.map_or(at, |old| old.min(at)));
            } else {
                complete_timing = false;
            }
            if let Some(at) = horizon {
                end = Some(end.map_or(at, |old| old.max(at)));
            }
            if running {
                if let (Some(start), Some(end)) = (metrics.current_start, horizon) {
                    metrics.intervals.push((start, end));
                } else {
                    complete_timing = false;
                }
            }
            complete_timing &= !stale
                && metrics.timing_gaps == 0
                && metrics.dropped_intervals == 0
                && metrics.incomplete_turns == 0;
            for (start, end) in &metrics.intervals {
                if end > start {
                    events.push((*start, 1i64));
                    events.push((*end, -1i64));
                }
            }
            times.add(metrics.times);
            for (name, totals) in &metrics.tools {
                tools.entry(name.clone()).or_default().add(totals);
            }
            agents.push(AgentSummary {
                agent,
                times: metrics.times,
                stale,
            });
        }
        agents.sort_by(|a, b| {
            a.agent
                .agent_path
                .cmp(&b.agent.agent_path)
                .then_with(|| a.agent.thread_id.cmp(&b.agent.thread_id))
        });
        let elapsed_ms = start
            .zip(end)
            .and_then(|(start, end)| duration_ms(start, end));
        let (peak_concurrency, average_concurrency) = if complete_timing && elapsed_ms.is_some() {
            // Ends sort before starts: touching half-open intervals do not overlap.
            events.sort();
            let mut active = 0i64;
            let mut peak = 0;
            for (_, delta) in events {
                active += delta;
                peak = peak.max(active);
            }
            (
                Some(peak as usize),
                elapsed_ms
                    .filter(|ms| *ms > 0)
                    .map(|ms| times.agent_ms as f64 / ms as f64),
            )
        } else {
            (None, None)
        };
        Self {
            agents,
            elapsed_ms,
            times,
            peak_concurrency,
            average_concurrency,
            tools,
            live,
            timing_complete: complete_timing,
        }
    }
}

pub fn role(agent: &AgentState) -> &str {
    agent
        .agent_role
        .as_deref()
        .unwrap_or(if agent.parent_thread_id.is_none() {
            "orchestrator"
        } else {
            "unspecified"
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::reduce;
    use serde_json::json;

    fn time(seconds: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(seconds, 0).unwrap()
    }
    fn record(agent: &mut AgentState, seconds: i64, kind: &str, payload: Value) {
        reduce(
            agent,
            &json!({"timestamp": time(seconds).to_rfc3339(), "type": kind, "payload": payload}),
        );
    }
    fn event(agent: &mut AgentState, seconds: i64, kind: &str) {
        record(agent, seconds, "event_msg", json!({"type":kind}));
    }
    fn agent(id: &str) -> AgentState {
        AgentState::new(id.into(), "0.152.1".into())
    }
    fn call(agent: &mut AgentState, seconds: i64, id: &str, name: &str) {
        record(
            agent,
            seconds,
            "response_item",
            json!({"type":"function_call","name":name,"call_id":id,"arguments":"{}"}),
        );
    }
    fn returned(agent: &mut AgentState, seconds: i64, id: &str) {
        record(
            agent,
            seconds,
            "response_item",
            json!({"type":"function_call_output","call_id":id,"output":"done"}),
        );
    }

    #[test]
    fn overlapping_calls_use_union_and_keep_wait_subsets_and_call_latencies() {
        let mut a = agent("root");
        event(&mut a, 0, "task_started");
        call(&mut a, 2, "a", "wait_agent");
        call(&mut a, 4, "b", "exec");
        returned(&mut a, 7, "a");
        call(&mut a, 8, "c", "wait");
        returned(&mut a, 9, "b");
        returned(&mut a, 10, "c");
        event(&mut a, 12, "task_complete");
        assert_eq!(
            a.metrics.times,
            Times {
                agent_ms: 12000,
                tool_ms: 8000,
                agent_wait_ms: 5000,
                exec_wait_ms: 2000
            }
        );
        assert_eq!(a.metrics.times.outside_tools_ms(), 4000);
        assert_eq!(
            a.metrics.tools.values().map(|t| t.latency_ms).sum::<u64>(),
            12000
        );
        assert_eq!(a.metrics.tools["exec"].paired, 1);
    }

    #[test]
    fn whole_run_survives_turn_resumption_and_interaction_eviction() {
        let mut a = agent("root");
        event(&mut a, 0, "task_started");
        call(&mut a, 1, "old", "exec");
        for i in 0..300 {
            record(
                &mut a,
                2,
                "event_msg",
                json!({"type":"agent_message","message":format!("Synthetic message {i}")}),
            );
        }
        returned(&mut a, 3, "old");
        event(&mut a, 4, "task_complete");
        event(&mut a, 10, "task_started");
        event(&mut a, 12, "turn_aborted");
        assert_eq!(a.metrics.turns, 2);
        assert_eq!(a.metrics.times.agent_ms, 6000);
        assert_eq!(a.metrics.tools["exec"].latency_ms, 2000);
        assert_eq!(a.interactions.len(), 256);
        let mut state = SessionState::default();
        state.agents.insert("root".into(), a);
        let summary = RunSummary::new(&state, time(100), false);
        assert_eq!(summary.elapsed_ms, Some(12000));
        assert_eq!(summary.peak_concurrency, Some(1));
        assert_eq!(summary.average_concurrency, Some(0.5));
    }

    #[test]
    fn concurrent_agents_touching_turns_and_live_projection() {
        let mut root = agent("root");
        event(&mut root, 0, "task_started");
        event(&mut root, 10, "task_complete");
        let mut child = agent("child");
        child.parent_thread_id = Some("root".into());
        event(&mut child, 2, "task_started");
        event(&mut child, 8, "task_complete");
        event(&mut child, 10, "task_started");
        let mut state = SessionState::default();
        state.agents.insert("root".into(), root);
        state.agents.insert("child".into(), child);
        let summary = RunSummary::new(&state, time(12), false);
        assert_eq!(summary.elapsed_ms, Some(12000));
        assert_eq!(summary.times.agent_ms, 18000);
        assert_eq!(summary.peak_concurrency, Some(2));
        assert_eq!(summary.average_concurrency, Some(1.5));
        assert_eq!(state.agents["child"].metrics.times.agent_ms, 6000);
        assert!(summary.live);
        let loading = RunSummary::new(&state, time(12), true);
        assert_eq!(loading.times.agent_ms, 16000);
        assert_eq!(loading.peak_concurrency, None);
    }

    #[test]
    fn missing_reversed_and_stale_timing_never_produce_precise_concurrency() {
        let mut a = agent("root");
        event(&mut a, 0, "task_started");
        call(&mut a, 5, "x", "exec");
        returned(&mut a, 3, "x");
        reduce(
            &mut a,
            &json!({"type":"event_msg","payload":{"type":"agent_message","message":"missing timestamp"}}),
        );
        event(&mut a, 10, "task_complete");
        assert!(a.metrics.timing_gaps >= 2);
        let mut child = agent("child");
        child.parent_thread_id = Some("root".into());
        event(&mut child, 0, "task_started");
        event(&mut a, 8000, "task_started");
        event(&mut a, 8001, "task_complete");
        let mut state = SessionState::default();
        state.agents.insert("root".into(), a);
        state.agents.insert("child".into(), child);
        let summary = RunSummary::new(&state, time(10000), false);
        assert_eq!(summary.peak_concurrency, None);
        let child = summary
            .agents
            .iter()
            .find(|a| a.agent.thread_id == "child")
            .unwrap();
        assert!(child.stale);
        assert_eq!(child.times.agent_ms, 0);
    }

    #[test]
    fn cumulative_tokens_are_snapshots_not_sums_and_survive_compaction() {
        let mut a = agent("root");
        let payload = json!({"type":"token_count","info":{
            "total_token_usage":{"input_tokens":100,"cached_input_tokens":80,"output_tokens":10,"reasoning_output_tokens":5,"total_tokens":110},
            "last_token_usage":{"input_tokens":90},"model_context_window":100}});
        record(&mut a, 1, "event_msg", payload.clone());
        record(&mut a, 2, "event_msg", payload);
        event(&mut a, 3, "context_compacted");
        assert_eq!(a.metrics.tokens.total, Some(110));
        assert_eq!(a.metrics.tokens.cached, Some(80));
        assert_eq!(a.metrics.tokens.reasoning, Some(5));
        assert_eq!(a.metrics.peak_context_percent, Some(90));
        assert_eq!(a.metrics.compactions, 1);
        record(
            &mut a,
            4,
            "event_msg",
            json!({"type":"token_count","info":{"total_token_usage":{"total_tokens":10}}}),
        );
        assert_eq!(a.metrics.tokens.total, Some(10));
        assert_eq!(a.metrics.tokens.input, None);
        assert_eq!(a.metrics.token_decreases, 1);
    }

    #[test]
    fn yielded_calls_incomplete_calls_and_terminal_polls_are_distinct() {
        let mut a = agent("root");
        event(&mut a, 0, "task_started");
        call(&mut a, 1, "yield", "exec");
        record(
            &mut a,
            2,
            "response_item",
            json!({"type":"function_call_output","call_id":"yield","output":"Script running with cell ID 17"}),
        );
        record(
            &mut a,
            3,
            "response_item",
            json!({"type":"function_call","call_id":"poll","name":"write_stdin","arguments":"{\"session_id\":1,\"chars\":\"\"}"}),
        );
        returned(&mut a, 5, "poll");
        record(
            &mut a,
            5,
            "response_item",
            json!({"type":"custom_tool_call","call_id":"wrapped","name":"exec","input":"await tools.write_stdin({session_id: 1, chars: ''});"}),
        );
        returned(&mut a, 6, "wrapped");
        call(&mut a, 6, "unfinished", "exec");
        event(&mut a, 7, "error");
        returned(&mut a, 8, "orphan");
        assert_eq!(a.metrics.times.exec_wait_ms, 3000);
        assert_eq!(a.metrics.tools["exec"].yielded, 1);
        assert_eq!(a.metrics.tools["exec"].incomplete, 1);
        assert_eq!(a.metrics.unmatched_outputs, 1);
        assert_eq!(a.metrics.times.agent_ms, 7000);
    }

    #[test]
    fn interval_and_tool_caps_preserve_totals_and_suppress_partial_peak() {
        let mut a = agent("root");
        for i in 0..TURN_INTERVAL_LIMIT + 1 {
            event(&mut a, i as i64 * 2, "task_started");
            event(&mut a, i as i64 * 2 + 1, "task_complete");
        }
        assert_eq!(a.metrics.intervals.len(), TURN_INTERVAL_LIMIT);
        assert_eq!(a.metrics.dropped_intervals, 1);
        assert_eq!(
            a.metrics.times.agent_ms,
            (TURN_INTERVAL_LIMIT as u64 + 1) * 1000
        );
        for i in 0..200 {
            a.metrics.start_call(&format!("tool{i}"));
        }
        assert_eq!(a.metrics.tools.len(), TOOL_KIND_LIMIT + 1);
        assert_eq!(a.metrics.tools.values().map(|t| t.calls).sum::<u64>(), 200);
        let mut state = SessionState::default();
        state.agents.insert("root".into(), a);
        assert_eq!(
            RunSummary::new(&state, time(10000), false).peak_concurrency,
            None
        );
    }
}
