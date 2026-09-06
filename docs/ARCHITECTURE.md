# Agentop architecture

Agentop is a single-process Rust terminal application that projects Codex rollout JSONL into a bounded, live, read-only view. The design favours direct evidence from each rollout over inferred orchestration state.

## Data flow

```text
Codex rollout JSONL
        ↓
discovery and session grouping
        ↓
bounded incremental readers
        ↓
tolerant envelope parsing
        ↓
per-agent deterministic reduction
        ↓
session browser, agent tree, details, and interactions
```

The main modules are:

- `src/main.rs`: command-line parsing, path resolution, and application entry points.
- `src/rollout.rs`: discovery, grouping, selection, admission, incremental reads, and work budgets.
- `src/model.rs`: projected session/agent state and deterministic record reduction.
- `src/summary.rs`: cumulative run accounting, timing coverage and concurrency projection.
- `src/ui.rs`: terminal lifecycle, event loop, navigation, rendering, and sanitisation.
- `src/schema.rs`: embedded and user schema-catalogue loading.
- `src/schema_sync.rs`: the explicit networked `build-schema` maintenance command.

## Trust and mutation boundary

The sessions directory is untrusted, potentially sensitive, append-oriented input. Normal TUI operation must never write, truncate, rename, move, delete, or acquire a write lock on any rollout. It performs no network access.

All rollout-derived strings are bounded and stripped of unsafe terminal controls before retention or rendering. Diagnostics retain a bounded classification, file path, byte offset, exact producer version when known, ordinal when known, and a short sanitised detail. Raw records, tool inputs, and tool outputs are not retained for presentation.

The separate `build-schema` command may use the network and may write only to its selected schema catalogue. It never modifies rollout files.

## Discovery and grouping

The sessions directory is selected from `--sessions-dir`, then `$CODEX_HOME/sessions`, then `$HOME/.codex/sessions`.

Discovery recursively considers `rollout-*.jsonl` and reads only enough metadata to identify a rollout. A complete, valid `session_meta` is required before admission. Rollouts are grouped by exact root `session_id`; an agent is keyed by its own thread ID. Parent thread IDs and agent paths establish tree topology.

The global browser orders groups by the greatest observed rollout-file modification time, falling back to the greatest session metadata timestamp. This is file/metadata recency, not proof that a session is active.

After a session opens, bounded rescans recover newly created child files. A correlated `spawn_agent` result can prioritise the likely date directory, but it is only a discovery hint. The child is admitted only when its own metadata confirms the displayed session ID.

## Incremental input

Each file cursor records the next unread byte offset plus any already-read incomplete final line. Readers:

- stream appended bytes instead of loading an entire file as one string;
- parse only newline-terminated records;
- preserve incomplete EOF data for the next poll;
- compact the input buffer once per chunk;
- cap record size, bytes, and work per poll;
- count complete malformed, oversized, and unknown input separately; and
- rebuild the selected session if a file is unexpectedly truncated.

Opening a session creates minimal root state before complete historical reduction. Initial catch-up uses bounded larger batches and approximately 100 ms work windows, checking terminal input between polls and redrawing between windows. Smaller steady-state budgets preserve responsive input, live tails, discovery, and pending-child admission. Ordinary live polling settles to roughly one second.

## Projection boundary

The parser reads a small stable envelope—timestamp, ordinal, type, and payload—and inspects only the fields Agentop understands. It does not generate a Rust model for every Codex schema or validate every line against JSON Schema in the render loop. Unknown records, events, fields, and variants are compatibility observations rather than automatic failures.

For a child with `subagent_history_start_ordinal`, its identifying header may establish identity before the boundary. All other records below the boundary, including inherited `session_meta`, are excluded from semantic reduction. Before the stream crosses that boundary, an ordinal-less record cannot be proven child-owned and is diagnosed rather than projected.

Required metadata failures are classified with available file, byte-offset, producer-version, and ordinal context. Optional evolving fields remain optional.

## Reducer semantics

An agent's own rollout is primary lifecycle evidence. Parent-side subagent activity is supplementary and can bridge discovery delay, but cannot overwrite contradictory child evidence.

A new `task_started` replaces latest-turn state and clears turn-scoped calls, messages, reasoning, final text, and result claims. `task_complete.last_agent_message` is the preferred final result; the current turn's latest message is the fallback.

Tool calls are correlated with outputs by `call_id`. Each call also receives reducer-local arrival sequence, providing a deterministic total order when ordinals are absent or timestamps tie. Completing one overlapping call does not clear newer unfinished work. `WAITING ON AGENT ↓` is rendered only while the latest turn is running and its newest unfinished exact tool call is `wait_agent`.

Meaningful lifecycle, message, reasoning, communication, and tool activity advances `last_activity_at`. Bookkeeping events such as `token_count` and `context_compacted` do not.

### Stale evidence

`STALE` is a display inference, not a stored lifecycle transition or completion claim. It applies only after initial history is structurally caught up, only to non-root agents still recorded as running, and uses either:

1. a later, complete, covering `list_agents` snapshot that excludes the agent; or
2. when no qualifying snapshot exists, at least two hours of later meaningful activity elsewhere in the session.

Malformed, incomplete, oversized, unknown, or non-covering snapshots provide no negative evidence. Stale agents remain visible when completed agents are hidden.

### Context usage

A valid `token_count` observation supplies current request input tokens and a non-zero model context window. Agentop stores the latest observation and previous input count for a delta. Cached input, output, reasoning output, and cumulative usage remain separately labelled accounting values.

A `context_compacted` event clears the superseded occupancy observation and adds an interaction entry. It does not reveal how many tokens were removed. Context pressure is shown only for active, non-stale agents.

### Run accounting

Per-agent summary measurements accumulate after the same own-history boundary as semantic reduction, independently of the bounded interaction deque. Turn starts and terminal events delimit observed running time; between monotonic timestamps, open-call state accounts for the union of tool time and the separate, potentially overlapping agent-wait and execution-wait subsets. Missing or reversed timestamps create coverage gaps. Rendering may extrapolate an open, non-stale turn to now only after catch-up; it does not mutate stored accounting. Time outside calls is unattributed, never labelled measured thinking time.

Call counts and paired latencies include only outer calls recognised by the reducer. Yielding closes that call's measured latency, not the background operation. Calls closed without a return and unmatched outputs remain separate diagnostics. Tool aggregates retain at most 128 distinct bounded names plus an overflow bucket. Up to 4,096 timestamp-only turn intervals per agent support a sweep of half-open intervals for peak concurrency; exceeding the cap suppresses concurrency rather than reporting a partial peak. Accumulated durations and counts continue beyond that cap. Average concurrency is summed turn time divided by the whole elapsed span, including gaps between turns. Up to 1,024 distinct bounded spawned-agent paths per agent support missing-child diagnostics independently of the transient discovery-hint queue; dropped hints are counted.

Token accounting retains the latest reported cumulative snapshot separately from current context occupancy. Optional fields remain absent rather than zero. Repeated snapshots are not summed, decreases are diagnosed rather than guessed to be resets, and compactions preserve totals and high-water context pressure. A child-owned snapshot may still contain a producer-inherited baseline; aggregate reported counters explicitly do not claim unique-run cost. All metrics cover discovered rollouts only and report partial loading, missing lifecycle evidence and data-health observations.

## Presentation and retained interactions

The UI flattens the projected parent/child graph into deterministic parent-first rows. Selection is keyed by thread ID so it survives row insertion and reordering. Hiding completed agents promotes any still-visible descendants by one visual level.

The main view and interaction view divide available height dynamically: short lists use their natural height up to a cap, leaving more room for bounded detail content. The interaction history is chronological and bounded. Tool entries retain the tool name, sanitised action summary, observed outcome, and elapsed duration. An execution wrapper that reports `Script running with cell ID ...` is shown as yielded rather than returned. A single `write_stdin` call with empty `chars` is a terminal poll; when an earlier command result established the same terminal session ID, Agentop carries that bounded command summary through the poll and any later `wait` interaction. Both correlation stores are turn-scoped and bounded. Nested Codex execution wrappers may otherwise be summarised into grouped tool names, but their source is never executed.

Terminal state is owned by an RAII guard so errors restore raw mode, cursor state, and the alternate screen.

## Schema catalogue and compatibility

Agentop uses only Codex's self-contained internal `RolloutLine.json`. It does not collect the stable or experimental app-server schema surfaces.

`schemas/codex/rollout-line/versions.json` maps each exact producer version to:

- a canonical SHA-256 schema family;
- official repository and `rust-v*` tag provenance;
- immutable tag object and peeled commit identities; and
- the compressed export source hash.

Canonical JSON objects are key-sorted, array order is retained, and one trailing newline is hashed. Equal schemas share one file under `by-hash/<sha256>/RolloutLine.json`. Every schema reference must remain local to that file.

The checked-in catalogue is embedded as a read-only seed. `agentop build-schema` discovers missing official release tags at or after `0.149.0-alpha.1`, verifies immutable provenance, bounds compressed and decompressed input, extracts only the internal rollout schema, and publishes schema families before atomically replacing the complete mapping under an advisory lock. Existing version conflicts and attempts to override an embedded version with another hash are errors.

Schema status and runtime evidence are independent:

- **Catalogued** establishes exact structural provenance.
- **Ingestable** establishes safe discovery, grouping, and tailing.
- **Semantically covered** establishes topology, lifecycle, and activity behaviour through representative fixtures.
- **Live verified** establishes exercise against a running producer of that exact version.

There is no nearest-version fallback. Missing schema coverage does not stop tolerant envelope ingestion, and a catalogued schema does not by itself prove reducer semantics.

## Deliberate non-goals

Agentop currently does not provide a persistent index or database, daemon, remote control, cross-machine monitoring, trace-bundle or `trace-reduce` backend, live app-server integration, encrypted-payload recovery, LLM activity summarisation, generic plugins, or a configurable theme/layout framework. New capabilities should follow observed user needs and preserve the immutable-input boundary.
