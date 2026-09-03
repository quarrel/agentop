# Agentop POC implementation plan

## Goal

Build a small Rust TUI that gives a useful live view of a Codex multi-agent run by reading the normal rollout JSONL files under `~/.codex/sessions/`.

The POC should answer, at a glance:

- which multi-agent session is being observed;
- what the agent tree looks like;
- which agents are running or completed;
- each agent's role, path, and recent activity;
- how long it has been since an agent did something;
- the most useful recent reasoning/tool/result text available in the rollout;
- enough detail on selection to understand what an agent is doing without opening its raw rollout manually.

This is a quick private POC. Keep the implementation small and easy to change. Multi-version rollout input is already a real requirement, not a hypothetical backend: long-lived Codex processes can continue writing an older shape after the installed CLI advances. Keep compatibility work evidence-led and local to rollout ingestion rather than building a generic backend or plugin architecture.

The rollout files are read-only inputs. They may grow while Agentop is reading them, and new rollout files may appear while the TUI is running.

## Known rollout behaviour to use

Codex is changing rapidly, and a single Agentop process can observe rollouts produced by several Codex versions at once. Each rollout's `session_meta.payload.cli_version` identifies its producer. Do not assume the currently installed CLI produced every active or historical file.

The initial **ingestion target** is the Codex 0.149 family and later, including its prereleases. Semantic coverage is narrower and evidence-based: Codex 0.152.1 is the current reference version for the known three-level hello-world run, and at least one representative 0.149-family fixture must demonstrate the older supported shape. Other versions may initially be merely ingestable until fixtures or live runs prove stronger semantics. As an initial corpus snapshot, the mounted sessions contained 534 rollout files across 29 exact version strings; 392 were from the 0.149 family or later. Treat those counts as investigation evidence, not a permanent product assumption.

Observed 0.152.1 rollouts already provide most of the data needed:

- A root/orchestrator rollout has `session_meta.payload.id == session_meta.payload.session_id` and no parent thread.
- Child rollouts carry the same root `session_id` as the orchestrator.
- Each child has its own `id` and an explicit `parent_thread_id`.
- Child `session_meta` can include:
  - `agent_path`, e.g. `/root/hello_world_owner/hello_world_candidate`;
  - `agent_role`, e.g. `map_implementer`;
  - `agent_nickname`;
  - `source.subagent.thread_spawn.depth`;
  - the same parent/thread/path/role information inside `source.subagent.thread_spawn`;
  - `subagent_history_start_ordinal`, which, when present, marks the first rollout ordinal belonging to the child's own projected history rather than inherited parent context.
- Records have monotonically increasing `ordinal` values in paginated rollouts.
- `task_started` and `task_complete` carry turn timing information.
- `task_complete.last_agent_message` can carry the terminal plaintext result directly and should be preferred over reconstructed prior message state when present.
- `response_item` tool-call records can appear before their matching outputs, joined by `call_id`. This is the dependable observed source of live activity in the reference run.
- `item_completed` exposes useful typed items such as:
  - `Reasoning` with `summary_text`;
  - `AgentMessage` with commentary/final text;
  - `CommandExecution`;
  - `McpToolCall`;
  - `CollabAgentToolCall`;
  - `SubAgentActivity`.
- The generated schema permits `item_started`, but the reference hello-world rollouts emitted none. Treat it as optional rather than the primary live-activity path.
- Root rollouts also see direct-child `SubAgentActivity` items such as `started` and `completed`.
- Parent/child task envelopes reveal sender, recipient, and message type, but v2 task payloads can be `encrypted_content`. Do not try to decrypt them.
- Final agent results are often plaintext and can contain useful compact receipts such as `status=READY_FOR_ACCEPTANCE`, `status=BLOCKED`, validation results, blockers, and candidate identity.

No generated schema is a public stability guarantee. Use the exact internal schema family mapped from each catalogued Codex version to describe permitted shapes, and use small real-rollout fixtures to establish observed sequencing and reducer semantics. The runtime parser must remain tolerant of absent fields, additional fields, and unknown variants.

## Schema catalogue and compatibility

Agentop directly uses only Codex's self-contained internal `RolloutLine.json` schema. The stable and experimental app-server surfaces are outside this POC and are not archived. Generated schemas remain version-specific evidence rather than public stability guarantees.

Store the catalogue by canonical schema identity:

```text
schemas/codex/rollout-line/
  versions.json
  by-hash/
    <canonical-sha256>/
      RolloutLine.json
```

`versions.json` maps each exact `session_meta.payload.cli_version` to a full canonical SHA-256 plus immutable source provenance: official repository, `rust-v*` tag, tag object, peeled commit, stable export path, and compressed source hash. Deterministically canonicalise JSON objects, retain array order, add one trailing newline, and hash those exact bytes. Equal hashes share a single schema file. Require every `$ref` to remain local to that file.

The checked-in mapping is compiled into each Agentop binary as its read-only seed. Later official releases are an explicit maintenance operation, never an automatic TUI side effect:

```bash
agentop build-schema
```

The updater should:

1. enumerate official `rust-v*` tag refs from `0.149.0-alpha.1` onwards and parse exact SemVer versions;
2. compare all remote refs with the merged built-in/user catalogue, so late older tags are not missed;
3. reject a known version whose tag object conflicts with recorded provenance;
4. resolve annotated or lightweight tags to an immutable commit;
5. download only the stable precomputed export at that exact commit;
6. bound compressed and decompressed input, then extract only `internal_json_schema["RolloutLine.json"]`;
7. validate, canonicalise, hash, and deduplicate the schema;
8. stage new families without replacing existing content;
9. publish the complete version mapping last under an advisory lock; and
10. leave the installed catalogue unchanged if any fetch, validation, provenance, or publication step fails.

Use an XDG user-data overlay for releases newer than the binary's seed, with an explicit directory override for tests and maintainers. External entries may add versions or repeat identical built-in mappings, but must never override a built-in version with a different hash. Optional GitHub credentials come from environment variables and are sent only to the API host. Normal TUI startup remains offline and read-only.

Do not generate a separate Rust model from every schema and do not validate every JSONL record against JSON Schema in the live rendering loop. Use schemas for provenance, offline compatibility analysis, fixture selection, and exact-version diagnostics. Keep runtime parsing as a small tolerant normaliser.

Define compatibility claims precisely:

- **Catalogued:** the exact producer version maps through verified provenance to a stored or embedded RolloutLine family hash.
- **Ingestable:** Agentop can discover, group, and tail the rollout without crashing; unknown data is reported.
- **Semantically covered:** fixtures demonstrate correct topology, lifecycle, and activity reduction.
- **Live verified:** Agentop has been exercised against a running process of that exact version.

Cataloguing is orthogonal to runtime compatibility: an exact schema may be archived before Agentop has semantic fixtures for it, and a rollout may be ingestable even when its exact version is absent from the catalogue.

Select schemas only by exact `session_meta.payload.cli_version`. Never validate with the nearest schema. When no exact mapping exists, continue with tolerant envelope parsing, show that schema coverage is missing, and retain enough type/version diagnostics to guide the next compatibility improvement.

## Deliberate POC boundaries

For this first version:

- Read normal rollout JSONL directly.
- Do not require `CODEX_ROLLOUT_TRACE_ROOT`.
- Do not use `codex debug trace-reduce`.
- Do not add a trace backend.
- Do not generate Rust types from any complete generated schema.
- Do not persist Agentop state to a database.
- Do not add a daemon or background service.
- Do not add networking or remote-control support.
- Do not add configuration files unless a real need appears during implementation.
- Do not attempt to recover encrypted inter-agent task/message payloads.
- Do not build a generic plugin/backend architecture.
- Aim to **ingest** the 0.149 family and later safely. Claim semantic coverage only where representative fixtures or live runs prove it; do not build exhaustive pre-0.149 adapters.

A small set of Rust structs plus `serde_json::Value` for the evolving payloads is preferable to reproducing Codex's large internal type graph.

## Suggested dependencies

Keep the dependency set modest:

- `ratatui` — TUI rendering.
- `crossterm` — terminal/event handling.
- `clap` with derive — a couple of useful command-line arguments.
- `serde` + `serde_json` — rollout parsing.
- `walkdir` — initial rollout discovery.
- `anyhow` — simple application errors.
- `time` or `chrono` — parse rollout timestamps and render elapsed/age values.

Avoid `tokio` for the POC unless implementation reveals a concrete need. A normal TUI loop with `crossterm::event::poll()` and periodic file checks is enough.

Do not add a tree-widget dependency initially. The agent hierarchy is small enough to flatten into indented rows for Ratatui.

## Proposed source layout

Keep this compact. A reasonable initial layout is:

```text
src/
  main.rs
  rollout.rs      # discovery + incremental JSONL reader
  schema.rs       # captured-schema lookup and compatibility labels
  model.rs        # small internal state structs / reducer
  ui.rs           # ratatui drawing + key handling
```

If one of these becomes awkwardly large while implementing, split it then. Do not pre-split further.

## Internal model

Use a small model tailored to what the TUI needs.

For example:

```rust
struct SessionState {
    session_id: String,
    cwd: Option<PathBuf>,
    started_at: Option<OffsetDateTime>,
    agents: HashMap<String, AgentState>, // keyed by thread id
    data_health: DataHealth,
}

struct AgentState {
    thread_id: String,
    parent_thread_id: Option<String>,
    agent_path: Option<String>,
    agent_role: Option<String>,
    agent_nickname: Option<String>,
    cli_version: Option<String>,
    schema_catalogued: bool,
    coverage: CoverageLevel,
    own_history_start_ordinal: Option<u64>,

    latest_turn: TurnState,
    last_activity_at: Option<OffsetDateTime>,

    next_call_sequence: u64,
    in_flight_calls: HashMap<String, InFlightCall>,
    last_reasoning_summary: Option<String>,
    last_message: Option<String>,
    final_message: Option<String>,
    result_status_claim: Option<String>,

    last_ordinal: Option<u64>,
}

struct InFlightCall {
    summary: String,
    started_at: Option<OffsetDateTime>,
    ordinal: Option<u64>,
    sequence: u64, // strict reducer-assigned arrival order within the active turn
}

struct TurnState {
    turn_id: Option<String>,
    status: TurnStatus,
    started_at: Option<OffsetDateTime>,
    completed_at: Option<OffsetDateTime>,
}

struct DataHealth {
    unknown_records: u64,
    unknown_events: u64,
    malformed_records: u64,
    oversized_records: u64,
    recent_diagnostics: VecDeque<DiagnosticSample>, // bounded, e.g. last 20
}

struct DiagnosticSample {
    rollout_path: PathBuf,
    byte_offset: u64,
    cli_version: Option<String>,
    kind: String, // bounded classification or unknown-variant name
    ordinal: Option<u64>,
    detail: Option<String>, // bounded and sanitised; never the complete raw payload
}

enum CoverageLevel {
    Unknown,
    Ingestable,
    SemanticallyCovered,
    LiveVerified,
}

enum TurnStatus {
    Pending,
    Running,
    Completed,
    Interrupted,
    Errored,
}
```

Assign `InFlightCall.sequence` when a call record is reduced and increment `next_call_sequence` even when an ordinal is absent or timestamps tie. Select the active call by the highest sequence rather than by `HashMap` iteration order. The ordinal and timestamp remain useful evidence and display data, but they are not required to provide a total order.

A diagnostic sample must remain actionable even when malformed JSON provides neither a producer version nor an ordinal. Its rollout path and byte offset identify the input; `kind` and `detail` stay bounded and sanitised.

The row status is the latest turn's lifecycle, not an irreversible status for the agent thread. A new `task_started` on an existing thread replaces `latest_turn` and clears prior in-flight calls, messages, reasoning, final text, and result claims. Separately retain a bounded chronological interaction history of sanitised lifecycle, message, reasoning, paired tool-invocation, and communication summaries across turns. A tool interaction records only its sanitised name, start time, observed-return or ended-without-return state, and elapsed duration; never retain tool inputs or outputs. Historical entries must remain confined to the explicit interaction view and must not be displayed as though they belonged to the active turn.

Keep status semantics conservative. Wall-clock silence alone must not change lifecycle display. Once initial loading has caught up, derive a yellow `STALE` display state only for a non-root agent whose own latest lifecycle remains `Running` when the same session contains meaningful agent activity at least two hours later than that agent's last activity. This session-relative evidence means the observation stream is stale and completion is unknown; it must not alter the stored lifecycle, claim completion, or make the agent eligible for completed-agent filtering. Disable the inference during loading and catch-up so partial reduction cannot create a transient false label.

Use the agent's own rollout as the primary source for its latest-turn lifecycle. Parent-side `SubAgentActivity` is supplementary evidence and a useful fallback while a child rollout is still pending discovery; it must not overwrite a contradictory child lifecycle. If a plaintext final result contains `status=BLOCKED`, surface it as an explicitly labelled result claim while retaining the lifecycle state separately.

While the lifecycle remains `Running`, render `WAITING ON AGENT ↓` only when the reducer's newest unfinished call has the exact tool name `wait_agent`. A newer unfinished call takes precedence and completion removes it; message or reasoning text alone is never waiting evidence. Keep the independently displayed activity as `wait_agent`.

`last_activity_at` should advance only for meaningful work/lifecycle activity that could reasonably answer “when did this agent last do something?” Do not refresh it for bookkeeping such as `token_count` alone.

## Rollout discovery

### 1. Locate the sessions directory

Use this precedence:

1. an explicit `--sessions-dir`;
2. `$CODEX_HOME/sessions` when `CODEX_HOME` is set; and
3. `~/.codex/sessions` otherwise.

For example:

```text
agentop --sessions-dir /path/to/sessions
```

Resolve the path once at startup, require it to be a readable directory, and display the resolved path in diagnostics. The sessions layout is an observed Codex behaviour rather than a public storage API.

### 2. Scan rollout files

Recursively find `rollout-*.jsonl` files.

For discovery, stream only enough of each file to find its first complete `session_meta` record. Normally this is the first record, but that record can itself be large because it embeds instructions. Do not parse every rollout merely to build the session list, and do not assume “first line” means a small read. Apply a documented fixed maximum record size that is comfortably above observed metadata.

Extract the minimum useful metadata:

```text
session_id
id (thread id)
parent_thread_id
cwd
timestamp
cli_version
agent_path
agent_role
agent_nickname
source.subagent.thread_spawn.depth
subagent_history_start_ordinal
```

### 3. Group a multi-agent run

Across the initial ingestion target, group rollout files by `session_meta.payload.session_id`.

The root is normally the thread where:

```text
id == session_id
```

or, as a fallback, where `parent_thread_id` is absent.

Build the hierarchy by joining each child's `parent_thread_id` to another rollout's `id`.

Use `agent_path` as the preferred display label and as a useful consistency check, not as the only source of topology.

### 4. Browse and select a session

Selection behaviour is:

1. honour an explicit `--session <SESSION_ID>` exact match or unique prefix and bypass the browser;
2. otherwise show all discovered session groups in a global picker, ordered by the group's greatest rollout-file modification time, with the greatest `session_meta` timestamp as a deterministic fallback when modification times are unavailable; and
3. keep the selected session identifier stable across rediscovery when it remains present.

Picker discovery is metadata-only. Display a bounded, sanitised project label from the root rollout's optional `git.repository_url` repository name, falling back to the final root `cwd` component. Details may show the bounded full cwd and repository URL. Enter opens the live tree; Esc returns to the picker, while Esc in the picker and q exit. `r` triggers rediscovery. Do not infer lifecycle health during aggregate discovery, use cwd/path equivalence heuristics, or conflate aggregate archive health with the opened session reader's health.

## Incremental file reading

Each selected rollout needs a tiny tail state:

```rust
struct RolloutCursor {
    path: PathBuf,
    byte_offset: u64,
    partial_line: Vec<u8>,
    last_ordinal: Option<u64>,
}
```

Rules:

1. Open files read-only.
2. Construct the selected reader with only minimal root/session state, snapshot each discovered rollout's current length, and reduce that existing history progressively. Use a larger but bounded initial catch-up allowance while reserving the smaller steady-state byte/work allowance for live tails, discovery, and pending children.
3. Define `byte_offset` as the next unread file position. Bytes held in `partial_line` have already been read and are before that offset.
4. On later ticks, compare file length with the stored offset.
5. If bytes were appended, seek to the offset, append new bytes to `partial_line`, advance the offset by exactly the bytes read, and extract newline-terminated records with a single buffer compaction per chunk rather than shifting the remainder after every record.
6. Never parse an incomplete final record; retain it until more bytes arrive.
7. Treat `ordinal` as per-rollout ordering/deduplication evidence when present. Do not impose a total cross-file order from ordinals.
8. Stream records rather than loading a whole rollout into one string.
9. Enforce a fixed maximum record size. If it is exceeded, discard through the next newline, increment `oversized_records` once, and continue.
10. Classify a complete malformed JSON record separately from an incomplete EOF record. Count and surface the former; retry the latter.
11. Count unknown record and event variants without retaining their potentially large payloads.
12. Do not print ordinary diagnostics while the TUI owns the terminal; expose aggregate counts plus the bounded recent diagnostic samples through `DataHealth` and the detail view.

Rollouts should normally only append. If `file_len < byte_offset`, rebuilding the selected session from its rollouts is simpler and safer than resetting only one cursor while leaving stale reduced state. No elaborate file-rotation machinery is needed for the POC.

## Discover children that appear after startup

New child rollouts can be created while Agentop is running.

Every roughly one second, rescan the sessions directory for new rollout filenames. Track discovery as `pending` until a complete, valid `session_meta` record is available; discovering a filename is not enough to mark it admitted.

For each pending file:

- retry after incomplete EOF rather than reporting malformed input;
- surface a complete malformed or oversized metadata record through `DataHealth`;
- if its `session_id` matches the displayed run, add it to the session;
- create its cursor at the correctly consumed offset;
- parse its remaining current contents exactly once; and
- rebuild the flattened tree rows.

A newly created file whose first line is still being written must eventually be admitted exactly once.

Known rollout files can be checked for appended bytes more frequently, e.g. on a 200–500 ms TUI tick.

Start with polling. Do not add `notify` unless simple polling actually proves unsatisfactory.

## Parsing strategy

Do not define Rust enums for the complete generated Codex schemas or one model per producer version.

Parse each line initially as a lightweight envelope or `serde_json::Value`:

```text
timestamp
ordinal
type
payload
```

Required envelope or metadata fields should be accessed directly and failures classified with file/version/ordinal context at the ingestion boundary. Optional evolving fields can remain optional. Then switch only on the record/item types Agentop currently understands.

Look up an exact archived internal schema from the rollout's `cli_version` for coverage reporting and offline validation. The live reducer should not require that lookup to succeed. Add field aliases or version-specific normalisation only when a captured schema or real fixture demonstrates the need; do not guess compatibility from semver proximity.

For a child rollout with `subagent_history_start_ordinal`, the first identifying `session_meta` header establishes that file's child identity and may bypass the boundary. For every other record, begin semantic reduction inclusively at the first ordinal greater than or equal to the boundary. Records below it—including inherited `session_meta` records—must not create or overwrite agent nodes or affect topology, lifecycle, activity, messages, or tools. While the reader is still below the boundary, an ordinal-less record cannot be classified as child-owned: do not project it, and retain a bounded diagnostic. Once the stream crosses the boundary, later ordinal-less records follow the normal tolerant path. Rollouts without the field retain the normal tolerant path.

### Records worth handling first

#### `session_meta`

The first identifying header creates/populates the rollout's own agent node, tree relationship, producer/schema metadata, and optional `subagent_history_start_ordinal` boundary. An inherited pre-boundary `session_meta` must not create or update another agent node.

#### `event_msg` → `task_started`

Replace the latest-turn state and clear turn-scoped activity:

```text
latest_turn.status = RUNNING
latest_turn.turn_id
latest_turn.started_at
latest_turn.completed_at = none
clear in_flight_calls
reset next_call_sequence
clear last reasoning/message/final/result-claim fields
last_activity_at
```

#### `event_msg` → `task_complete`

Set:

```text
latest_turn.status = COMPLETED
latest_turn.completed_at
final_message = payload.last_agent_message when present, otherwise last_message from this turn
clear in_flight_calls
last_activity_at
```

Run the optional receipt parser against the selected `final_message`, not against stale prior-turn text.

#### `event_msg` → `turn_aborted`

If present, set `latest_turn.status = INTERRUPTED` unless the event contains a clearly terminal error state, and update `last_activity_at`.

#### `event_msg` → `error`

Record a concise error and set `latest_turn.status = ERRORED` where appropriate. Treat it as meaningful activity when it concerns the active turn.

#### `response_item` → tool calls and outputs

Treat `custom_tool_call`, `function_call`, and other observed call records as the primary live-activity path. Record a concise `InFlightCall` keyed by `call_id`, assign the next reducer-local sequence, and retain the record ordinal/timestamp when available. On a matching output or other terminal record, remove that exact in-flight call and update recent activity/result text.

Multiple calls can overlap. Derive the displayed current activity from the remaining call with the highest reducer-assigned sequence rather than clearing all activity when any output arrives. Do not rely on `HashMap` iteration or ordinal/timestamp ties to choose the newest call.

#### `event_msg` → `item_started` / `item_completed`

Look at `payload.item.type` when present.

Handle the following small subset:

- `Reasoning`
- `AgentMessage`
- `CommandExecution`
- `McpToolCall`
- `CollabAgentToolCall`
- `SubAgentActivity`
- optionally `FileChange` if it falls out naturally

Use `item_started` when emitted, but do not depend on it. Use `item_completed` for richer typed summaries and to complete a matching in-flight call when an identifier is available. Deduplicate logical completion when both an item completion and a response output describe the same call.

#### Direct `event_msg` variants

Captured schemas also permit direct events such as `sub_agent_activity`, `exec_command_begin/end`, and MCP begin/end events. Supporting observed equivalents is useful if a few simple match arms cover them, but add them from schema/fixture evidence rather than treating one current path as universal.

#### `response_item` → agent messages and communication envelopes

Use plaintext agent messages for recent/final text and compact result claims. Store sender, recipient, and message type when available. If content includes `encrypted_content`, show only the available envelope, e.g.:

```text
follow-up sent to /root/foo (payload encrypted)
```

Do not treat encrypted content as an error.

## Deterministic activity summaries

The TUI does not need another model to explain the rollout.

Use simple mappings.

Examples:

- `Reasoning.summary_text` → strip simple Markdown emphasis and use the newest non-empty summary.
- `McpToolCall` in progress → `tilth: tilth_read`.
- completed `McpToolCall` → `read via tilth` or simply `tilth_read completed`.
- command start → derive a short display from the first command/program, e.g. `running cargo test`, `running pytest`, `git status`.
- command complete → `cargo test completed` / `command failed (exit N)`.
- `SubAgentActivity started` → `started <agent path>` on the parent.
- collaboration wait → `waiting on <N> agent(s)` if the item exposes receivers.
- plaintext `FINAL_ANSWER` → `completed: <first useful receipt/status line>`.

Keep a bounded, sanitised source excerpt available in the detail pane so summaries do not need to be clever. Strip terminal control sequences and replace unsafe control characters before storing or rendering rollout-derived text.

Do not create a general natural-language classification engine for the POC.

## Optional tiny receipt parser

MAP-style final answers in the current sessions use useful newline-delimited fields such as:

```text
item=...
status=...
candidate=...
validation=...
blocker=...
```

A small helper may extract these exact `key=value` lines from plaintext final messages.

Use it only as presentation sugar. Keep a bounded, sanitised excerpt of the original message and fall back cleanly when a final message is ordinary prose.

Useful display behaviour:

- latest-turn lifecycle `Completed` + receipt `status=BLOCKED` → display `TURN COMPLETED · result claim: BLOCKED`;
- latest-turn lifecycle `Completed` + `status=READY_FOR_ACCEPTANCE` → display `TURN COMPLETED · result claim: READY_FOR_ACCEPTANCE`.

Do not make Agentop depend on MAP-specific receipts for basic operation.

## TUI POC

### Main view

Start with one screen containing a header, tree, and selected-agent detail area.

Example:

```text
agentop · 01a0617f… · /workspaces/agentop · 3 agents

/root                                      RUNNING       3m 19s
└─ hello_world_owner      map_item_owner   COMPLETED       2m
   └─ hello_world_candidate map_implementer COMPLETED      53s

selected: /root/hello_world_owner/hello_world_candidate
role: map_implementer   thread: 01a06182…
last: validating output + Git scope
result: status=CANDIDATE · blocker=none

↑↓ select   Enter details   r rescan   q quit
```

Use a restrained semantic named-colour palette for titles and borders, metadata, roles, lifecycle, schema/compatibility/data health, and warning/error/catching-up states. The public `--color=auto|none` option defaults to `auto`; because rendering requires an interactive TTY, `auto` enables colour. `none` must leave every explicit foreground and background colour reset while preserving labels, a visible selection marker, and non-colour selection attributes. Prefer clarity over decoration.

### Tree rows

Each row should try to fit:

```text
indent + display name | role | status/result hint | elapsed or last-activity age
```

Use `agent_path` segments for labels. If it is absent, fall back to nickname and then abbreviated thread id.

The root can display as `/root` and optionally `ORCHESTRATOR` as its role label.

### Detail area

For the selected agent show, as available:

```text
full agent path
thread id
parent thread id
role / nickname
producer CLI version
schema catalogued/missing
compatibility level
latest-turn lifecycle status
started/completed timestamps or duration
last meaningful activity timestamp
in-flight/recent tool activity
last reasoning summary
last plaintext message
final message / labelled result claim
session data-health counters
recent bounded data-health diagnostics
```

Long text must be bounded, sanitised, and clipped. Scrolling the detail pane is useful if easy, but not required before the tree/live updates work.

### Keys

Keep the first key set small:

```text
q / Esc      quit
↑ / ↓        select row
Enter        toggle a larger detail view, if implemented
r            force session-directory rescan and redraw
```

No command palette or mouse support is needed.

## Event loop

A straightforward single-threaded loop is enough:

```text
enter raw mode + alternate screen through a terminal guard
initial discovery + selected session load

loop:
    poll keyboard events, capped by the next reader deadline
    handle key if present
    process a bounded record/byte budget from known rollouts
    periodically rescan for newly created rollout files
    update derived state and data-health counters
    draw, including a catching-up indicator when work remains

drop terminal guard to restore terminal
```

Use an RAII terminal guard so every ordinary error path restores raw mode, cursor state, and the alternate screen. Add explicit resize and very-small-terminal handling.

Keep filesystem and per-tick work bounded:

- discovery streams only through `session_meta` from unrelated rollouts;
- only the selected session's files are parsed in full/tail mode;
- subsequent updates read appended bytes only;
- initial catch-up has a separate 8 MiB/65,536-record per-poll allowance while preserving a 256 KiB/2,048-work reserve for live tails, discovery, and pending children;
- steady ticks retain their smaller record and byte budget so an append burst cannot starve keyboard handling; and
- stored text, diagnostics, and activity history remain capped independently of rollout size.

This should remain responsive even with old large orchestrator rollouts without needing an indexing service.

Opening a selected group constructs root/minimal state without parsing its complete history. Existing rollouts are reduced in bounded batches with a separate 8 MiB/65,536-record initial allowance; each poll preserves the ordinary 256 KiB/2,048-work reserve for live reading, discovery, and pending children. Successive completed snapshots may therefore be admitted within one poll. Order work deterministically with parents before children when metadata permits and use a bounded deterministic fallback for malformed or orphan metadata. Display `loading history…` exactly while this queue remains. During initial loading, group successive bounded polls into an approximately 100 ms work window, check for terminal input between polls, redraw between windows, and do not add an artificial delay after a saturated window. A slow individual poll remains governed by its byte/work ceilings. Settle ordinary live polling to about one second without busy-spinning. Selection remains keyed by thread ID as rows appear or reorder. An exceptional truncation rebuild may remain synchronous in this POC.

## Implementation sequence

### Step 0 — capture reference schemas and inventory the corpus

Capture the current CLI's stable, experimental, and internal schemas with exact version provenance. Add a small repeatable capture command or script that stages outputs, verifies the version before and after generation, validates expected files, and writes hashes plus invocations to the manifest.

Inventory the exact `cli_version` values present in the local rollout corpus. For the POC, capture 0.152.1 and one representative 0.149-family producer version when its exact historical binary is readily obtainable. Do not block the first executable on broader historical schema acquisition.

Broad acquisition of 0.150/0.151 and other historical schema sets, and a generic cross-version schema-diff summary tool, belong in Step 7 unless they fall out trivially from the capture work.

### Step 1 — Rust executable scaffold

Create the binary crate in the existing repo if it does not already exist.

Add the small dependency set above.

Make this work first:

```text
cargo run -- --sessions-dir ~/.codex/sessions
```

It may initially print discovered sessions rather than launch a TUI.

### Step 2 — session metadata discovery

Implement:

- recursive rollout discovery with pending first-record handling;
- bounded streaming through the first complete `session_meta`;
- minimal metadata extraction including exact producer version and optional child-history boundary;
- exact-schema catalogue lookup;
- grouping by `session_id`;
- root detection;
- explicit `--session` exact/unique-prefix override before the picker;
- a global newest-observed-update-first session picker using group rollout-file modification times with metadata timestamp fallback; and
- visible data-health counts for rejected metadata.

Temporarily print a tree such as:

```text
/root [01a0617f]
└─ /root/hello_world_owner [01a06181]
   └─ /root/hello_world_owner/hello_world_candidate [01a06182] map_implementer
```

Validate this against the existing hello-world session before moving on.

### Step 3 — parse selected session into `SessionState`

Construct the selected reader without consuming all existing history before display. Render the minimal root state first, then reduce the construction-time rollout snapshots over successive bounded polls with a larger initial catch-up allowance while ordinary discovery and live tailing retain a reserved steady-state allowance. Agents should appear deterministically, root first and parent before child when the metadata graph permits, as each rollout begins reduction. Clear the loading indication only when the snapshot is fully reduced; the eventual state, cursor offsets, and data-health accounting must match full-load reducer semantics.

At the end, a debug print should show sensible latest-turn states, in-flight/recent activity, producer/schema coverage, and labelled final result claims for the hello-world root/owner/candidate chain.

Unknown records must be harmless and counted. Validate response call/output pairing without relying on `item_started`, validate overlapping calls completing independently, validate that `task_complete.last_agent_message` wins when present, validate that a second `task_started` reactivates an existing thread cleanly, and validate that inherited pre-boundary child history is not attributed to the child when `subagent_history_start_ordinal` is present.

### Step 4 — incremental tailing

Add `RolloutCursor`, bounded append-only reading, pending-file discovery, and selected-session rebuild on truncation.

Verify with a running Codex session that:

- Agentop can start while the session is active;
- appended events appear without restarting Agentop;
- a newly spawned child rollout is discovered and inserted exactly once, including when its first record was initially partial;
- incomplete final JSONL writes do not produce repeated visible errors;
- malformed/oversized records are surfaced without corrupting the TUI; and
- an append burst leaves navigation responsive while a catching-up indicator is visible.

### Step 5 — Ratatui tree screen

Replace the debug output with the basic TUI.

Implement:

- header;
- flattened indented tree;
- selected-row state;
- detail panel;
- quit and navigation keys.

Use a simple depth-first flatten of the `parent_thread_id` graph with the root first. At every branch, order sibling subtrees by their greatest meaningful activity timestamp—`last_activity_at`, falling back to latest-turn start—newest first. Preserve hierarchy and use path/name then thread ID as deterministic tie-breaks.

### Step 6 — useful activity summaries

Map the small set of typed items/events to compact row/detail text.

Prioritize:

1. reasoning summaries;
2. active/recent command;
3. MCP tool call;
4. agent message/final result;
5. collaboration wait/spawn activity if present.

Prefer the newest meaningful activity. Do not let bookkeeping-only events refresh the displayed activity age.

### Step 7 — compatibility, optional schema analysis, and live smoke tests

First run fixture/corpus checks for representative 0.149-family, intermediate, and current-version rollouts. Report exact schema catalogue status, unknown variants, and the highest demonstrated compatibility level for each tested version.

If useful after the core reader works, acquire additional high-volume historical schema sets whose exact binaries remain obtainable. A small schema-diff summary may list added/removed top-level rollout records, event variants, response-item variants, and app-server methods between two captures. Keep it an analysis aid, not a runtime compatibility engine.

Then run a small 3-level Codex session similar to the hello-world run:

```text
/root
└─ owner
   └─ implementer
```

While it runs, verify Agentop shows:

- the root;
- owner appearing after spawn;
- implementer appearing after the nested spawn;
- correct parent/child hierarchy;
- roles and producer versions when supplied;
- activity changing while each agent works;
- completion and separately labelled result claims after each child finishes;
- exact schema catalogue status or a clear missing-schema label;
- compatibility level without overstating semantic coverage; and
- the final root completion.

Then point Agentop at the existing long multi-day session and confirm that loading and navigating it remains usable while newer-version sessions are also growing. Do not tune performance further unless a concrete problem appears.

## Small tests worth having

Keep tests focused on capture validation, parsing, reduction, reader invariants, and targeted terminal styling where correctness or accessibility requires it.

A handful of unit tests and tiny fixtures should cover:

1. exact schema lookup by `cli_version`, plus explicit missing-schema status;
2. capture manifest raw/parsed version and hash validation, rejection of stale/non-empty staging, and exact archive-key lookup;
3. root `session_meta`;
4. child `session_meta` with `parent_thread_id`, path, role, depth, and optional `subagent_history_start_ordinal`;
5. grouping root + child + grandchild by common `session_id`;
6. `task_started → task_complete → task_started` latest-turn reactivation;
7. `task_complete.last_agent_message` preferred over prior message state;
8. `response_item` call → matching output live activity without `item_started`;
9. overlapping call identifiers completing independently and preserving deterministic newest-call ordering when ordinals are absent and timestamps tie;
10. `item_completed` Reasoning summary extraction;
11. plaintext final receipt retained as a result claim rather than lifecycle truth;
12. encrypted `NEW_TASK` being accepted without attempting to decode it;
13. an inherited pre-boundary `session_meta` not creating or overwriting the parent node, and other inherited records not being attributed to child activity/lifecycle;
14. incremental reader retaining a partial final JSON line and parsing it after completion;
15. an incomplete first `session_meta` being retried and admitted exactly once;
16. truncation/replacement rebuilding state rather than retaining stale reduction;
17. malformed, oversized, and unknown records updating the correct data-health counters and bounded diagnostic samples with path/offset context even when version and ordinal are unavailable;
18. rollout text containing ANSI/control bytes being safely sanitised and bounded;
19. bookkeeping-only events such as `token_count` not updating `last_activity_at`;
20. operation against read-only fixture files;
21. explicit-session picker bypass, global newest-observed-update ordering by greatest group rollout-file modification time with metadata timestamp fallback, long-running old-root/recent-child ranking, and picker selection stability across rediscovery;
22. root-first hierarchical tree ordering by newest subtree activity, including direct siblings and deterministic ties while preserving selection by thread ID;
23. selected-session health excluding archive-wide discovery diagnostics from unrelated rollouts;
24. exact colour-mode CLI values/help, representative semantic colour use, and colour-free picker/tree/tiny rendering that retains non-colour selection cues;
25. exact newest-running in-flight `wait_agent` precedence in tree and detail status, including clearing on completion, a newer non-wait call taking precedence, preserved activity text, and message text alone not being lifecycle evidence;
26. a root-only selected-reader constructor followed by progressive deterministic parent-first admission, same-poll completion of many tiny rollout snapshots, separately bounded bulk initial catch-up, preserved steady-state byte/work limits including charged admission and completion, live-tail and pending-child fairness during saturated initial catch-up, linear JSONL buffer consumption, eventual full-load state/cursor/health equivalence, a truthful loading indicator, stable selection as agents appear and reorder, and deterministic loading/steady deadline timing helpers;
27. exact-version content-addressed schema lookup, embedded/user catalogue conflict rejection, self-contained schema and hash validation, release-export extraction, tag/provenance validation, family deduplication, idempotent synchronisation, and failure-atomic publication; and
28. conservative stale display requiring a non-root `Running` lifecycle and at least two hours of later meaningful activity elsewhere in the fully loaded session, including exact threshold behaviour, loading/catch-up suppression, warning styling and explanatory detail, root/terminal exclusions, and continued visibility under completed-agent filtering.
Do not vendor entire real rollout files. Minimise representative lines into small, sanitised fixtures or inline JSON test data. Keep provenance outside fixture payloads: record which exact producer version and schema informed each fixture.

## POC acceptance check

The POC is good enough when, from the Agentop dev container, the user can run it against the shared Codex sessions and get a live screen that:

- safely discovers, groups, and tails the available 0.149-family-and-later rollouts without assuming they match the installed CLI;
- displays each agent's exact producer version, whether its exact schema is catalogued, and the highest demonstrated compatibility level;
- reports unknown/malformed/oversized input with bounded useful diagnostics rather than silently presenting incomplete state;
- claims semantic support only for versions covered by representative fixtures;
- does not let inherited pre-boundary records—including `session_meta`—create, overwrite, or otherwise affect projected agent state when Codex provides `subagent_history_start_ordinal`; and
- renders the first selected-session screen before complete history reduction, adds agents progressively in deterministic parent-first order while loading, and remains responsive while selected rollouts grow.

For the known 0.152.1 hello-world run, it should reconstruct at least:

```text
/root
└─ /root/hello_world_owner
   └─ /root/hello_world_owner/hello_world_candidate
```

It should show the candidate as `map_implementer`, with useful live activity derived from call/output pairing and completion information from its own rollout. Where `task_complete.last_agent_message` is present, that is the preferred final result source.

At least one representative 0.149-family fixture must demonstrate correct discovery, topology, latest-turn lifecycle, and unknown-variant handling. Other rollouts from the 0.149 family and later may initially be merely ingestable; the UI must not overstate their semantic coverage.

During a new live run Agentop must update when rollout files grow and when a new child rollout file appears, even if that child initially exposes only a partial first record.

It must never write to, truncate, rename, move, lock for writing, or otherwise mutate anything under the sessions directory. Schema capture writes only beneath the repository's schema staging/archive path and is never performed by the live reader.

After these checks pass, stop and assess the POC before adding larger features.

## Things explicitly left for later

Do not implement these as part of this plan:

- trace-bundle support;
- `trace-reduce` integration;
- live app-server integration (capturing and comparing its schemas is in scope);
- remote-control functionality;
- a database/index of historic rollouts;
- cross-machine monitoring;
- an LLM-based activity summarizer;
- sophisticated task decryption/recovery;
- exhaustive version-specific adapters, particularly for pre-0.149 rollouts;
- plugin systems;
- configurable themes/layout frameworks;
- elaborate performance work before the ordinary rollout approach is measured.

Natural next experiments after the POC are likely to be session selection/history, app-server features guided by stable/experimental schema diffs, better status summarisation, and richer drill-down. Decide those from actual use rather than building them now.
