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

This is a quick private POC. Keep the implementation small and easy to change. Do not build abstractions for hypothetical future backends or compatibility layers unless they directly simplify the POC.

The rollout files are read-only inputs. They may grow while Agentop is reading them, and new rollout files may appear while the TUI is running.

## Known rollout behaviour to use

The current target is Codex CLI 0.152.1 with multi-agent v2 and paginated history.

Observed 0.152.1 rollouts already provide most of the data needed:

- A root/orchestrator rollout has `session_meta.payload.id == session_meta.payload.session_id` and no parent thread.
- Child rollouts carry the same root `session_id` as the orchestrator.
- Each child has its own `id` and an explicit `parent_thread_id`.
- Child `session_meta` can include:
  - `agent_path`, e.g. `/root/hello_world_owner/hello_world_candidate`;
  - `agent_role`, e.g. `map_implementer`;
  - `agent_nickname`;
  - `source.subagent.thread_spawn.depth`;
  - the same parent/thread/path/role information inside `source.subagent.thread_spawn`.
- Records have monotonically increasing `ordinal` values in paginated rollouts.
- `task_started` and `task_complete` carry turn timing information.
- `item_started` / `item_completed` expose useful typed items such as:
  - `Reasoning` with `summary_text`;
  - `AgentMessage` with commentary/final text;
  - `CommandExecution`;
  - `McpToolCall`;
  - `CollabAgentToolCall`;
  - `SubAgentActivity`.
- Root rollouts also see direct-child `SubAgentActivity` events such as `started` and `completed`.
- Parent/child task envelopes reveal sender, recipient, and message type, but v2 task payloads can be `encrypted_content`. Do not try to decrypt them.
- Final agent results are often plaintext and can contain useful compact receipts such as `status=READY_FOR_ACCEPTANCE`, `status=BLOCKED`, validation results, blockers, and candidate identity.

For the POC, treat this observed 0.152.1 shape as the main contract. Be tolerant of absent fields and unknown record/item variants: ignore what is not needed rather than modelling the entire Codex schema.

## Deliberate POC boundaries

For this first version:

- Read normal rollout JSONL directly.
- Do not require `CODEX_ROLLOUT_TRACE_ROOT`.
- Do not use `codex debug trace-reduce`.
- Do not add a trace backend.
- Do not generate Rust types from the full `RolloutLine.json` schema.
- Do not persist Agentop state to a database.
- Do not add a daemon or background service.
- Do not add networking or remote-control support.
- Do not add configuration files unless a real need appears during implementation.
- Do not attempt to recover encrypted inter-agent task/message payloads.
- Do not build a generic plugin/backend architecture.
- Do not spend time supporting old rollout versions beyond graceful best-effort parsing where the fields happen to match.

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
}

struct AgentState {
    thread_id: String,
    parent_thread_id: Option<String>,
    agent_path: Option<String>,
    agent_role: Option<String>,
    agent_nickname: Option<String>,
    cli_version: Option<String>,

    status: AgentStatus,
    turn_id: Option<String>,
    started_at: Option<OffsetDateTime>,
    completed_at: Option<OffsetDateTime>,
    last_event_at: Option<OffsetDateTime>,

    current_activity: Option<String>,
    last_reasoning_summary: Option<String>,
    last_message: Option<String>,
    final_message: Option<String>,

    last_ordinal: Option<u64>,
}

enum AgentStatus {
    Pending,
    Running,
    Completed,
    Interrupted,
    Errored,
}
```

Keep status semantics conservative. If the rollout only tells us an agent is running and it has been quiet for a long time, display `RUNNING · last activity 2h ago`; do not infer `STALLED` or `BLOCKED` from silence.

If a plaintext final result explicitly contains a compact line such as `status=BLOCKED`, it is useful to surface that as result metadata, but retain the underlying lifecycle state separately.

## Rollout discovery

### 1. Locate the sessions directory

Default to:

```text
~/.codex/sessions
```

Support a simple override for development/testing, for example:

```text
agentop --sessions-dir /path/to/sessions
```

Expand the user's home directory once at startup.

### 2. Scan rollout files

Recursively find `rollout-*.jsonl` files.

For discovery, read only enough of each file to find its first `session_meta` line. Normally this is the first record. Do not parse every rollout merely to build the session list.

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
```

### 3. Group a multi-agent run

For 0.152.1, group rollout files by `session_meta.payload.session_id`.

The root is normally the thread where:

```text
id == session_id
```

or, as a fallback, where `parent_thread_id` is absent.

Build the hierarchy by joining each child's `parent_thread_id` to another rollout's `id`.

Use `agent_path` as the preferred display label and as a useful consistency check, not as the only source of topology.

### 4. Pick the initial session simply

The convenient default for development is:

1. prefer sessions whose root `cwd` matches the current working directory;
2. among those, choose the most recent root timestamp;
3. if none match, choose the most recent root session overall.

Also support:

```text
agentop --session <SESSION_ID>
```

This is enough for the POC. Do not build a session picker before the live tree works; add a basic picker later only if it is trivial.

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
2. On initial load of the selected session, parse the existing content once.
3. Remember the byte offset reached.
4. On later ticks, compare file length with the stored offset.
5. If bytes were appended, seek to the offset and read only the new bytes.
6. Only parse complete newline-terminated JSON records.
7. Keep an incomplete final line in `partial_line` until more bytes arrive.
8. Use `ordinal` as a logical order/deduplication check when present.
9. If one line is malformed or an unknown record type appears, skip/log it and continue rather than killing the TUI.

Rollouts should normally only append. A tiny safety check for `file_len < byte_offset` can reset that cursor to zero; no elaborate file-rotation machinery is needed for the POC.

## Discover children that appear after startup

New child rollouts can be created while Agentop is running.

Every roughly one second, rescan the sessions directory for new rollout filenames. For any unseen file:

- read its `session_meta`;
- if its `session_id` matches the currently displayed run, add it to the session;
- create its cursor;
- parse its current contents;
- rebuild the flattened tree rows.

Known rollout files can be checked for appended bytes more frequently, e.g. on a 200–500 ms TUI tick.

Start with polling. Do not add `notify` unless simple polling actually proves unsatisfactory.

## Parsing strategy

Do not define Rust enums for all ~11k lines of the generated Codex schema.

Parse each line initially as a lightweight envelope or `serde_json::Value`:

```text
timestamp
ordinal
type
payload
```

Then switch only on the record/item types Agentop currently understands.

### Records worth handling first

#### `session_meta`

Creates/populates the agent node and tree relationship.

#### `event_msg` → `task_started`

Set:

```text
status = RUNNING
turn_id
started_at
last_event_at
```

#### `event_msg` → `task_complete`

Set:

```text
status = COMPLETED
completed_at
final_message = last_agent_message
last_event_at
```

#### `event_msg` → `turn_aborted`

If present, show `INTERRUPTED` unless the event contains a clearly terminal error state.

#### `event_msg` → `error`

Record a concise error and set `ERRORED` where appropriate.

#### `event_msg` → `item_started` / `item_completed`

Look at `payload.item.type`.

Handle the following small subset:

- `Reasoning`
- `AgentMessage`
- `CommandExecution`
- `McpToolCall`
- `CollabAgentToolCall`
- `SubAgentActivity`
- optionally `FileChange` if it falls out naturally

For an `item_started`, set a concise current activity when possible.

For an `item_completed`, update recent activity/result text.

#### Legacy/direct `event_msg` variants

The schema also permits direct events such as `sub_agent_activity`, `exec_command_begin/end`, and MCP begin/end events. Supporting the obvious equivalents is useful if a few simple match arms cover them, but do not let this delay the main 0.152.1 `item_*` path.

#### `response_item` → `agent_message`

Useful for parent/child communications and final receipts.

Store sender/recipient/message type if plaintext. If content includes `encrypted_content`, show only the available envelope, e.g.:

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

Keep the raw text available in the detail pane so the summaries do not need to be clever.

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

Use it only as presentation sugar. Store the original message unchanged and fall back cleanly when a final message is ordinary prose.

Useful display behaviour:

- lifecycle `Completed` + receipt `status=BLOCKED` → row can display `COMPLETED · BLOCKED`;
- lifecycle `Completed` + `status=READY_FOR_ACCEPTANCE` → `COMPLETED · READY_FOR_ACCEPTANCE`.

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

Exact styling is unimportant. Prefer clarity over decoration.

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
lifecycle status
started/completed timestamps or duration
last activity timestamp
last reasoning summary
current/last tool activity
last plaintext message
final message / receipt
```

Long text can be clipped initially. Scrolling the detail pane is useful if easy, but not required before the tree/live updates work.

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
initialize terminal
initial discovery + selected session load

loop:
    poll keyboard event with ~250 ms timeout
    handle key if present
    tail known selected-session rollouts
    periodically rescan for newly created rollout files
    update derived state
    draw

restore terminal
```

Keep filesystem work bounded:

- discovery reads only `session_meta` from unrelated rollouts;
- only the selected session's files are parsed in full/tail mode;
- subsequent updates read appended bytes only.

This should remain responsive even with old large orchestrator rollouts without needing an indexing service.

## Implementation sequence

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

- recursive rollout discovery;
- first-line/session-meta reading;
- minimal metadata extraction;
- grouping by `session_id`;
- root detection;
- current-cwd/latest session selection;
- explicit `--session` override.

Temporarily print a tree such as:

```text
/root [01a0617f]
└─ /root/hello_world_owner [01a06181]
   └─ /root/hello_world_owner/hello_world_candidate [01a06182] map_implementer
```

Validate this against the existing hello-world session before moving on.

### Step 3 — parse selected session into `SessionState`

Read all currently existing records for the selected session's rollouts and reduce the supported records into `AgentState`.

At the end, a debug print should show sensible lifecycle states, recent activity, and final result for the hello-world root/owner/candidate chain.

Unknown records must be harmless.

### Step 4 — incremental tailing

Add `RolloutCursor` and append-only reading.

Verify with a running Codex session that:

- Agentop can start while the session is active;
- appended events appear without restarting Agentop;
- a newly spawned child rollout is discovered and inserted into the tree;
- incomplete final JSONL writes do not produce repeated visible errors.

### Step 5 — Ratatui tree screen

Replace the debug output with the basic TUI.

Implement:

- header;
- flattened indented tree;
- selected-row state;
- detail panel;
- quit and navigation keys.

Use a simple depth-first flatten of the `parent_thread_id` graph. Sort siblings by start/session-meta timestamp, falling back to path/name.

### Step 6 — useful activity summaries

Map the small set of typed items/events to compact row/detail text.

Prioritize:

1. reasoning summaries;
2. active/recent command;
3. MCP tool call;
4. agent message/final result;
5. collaboration wait/spawn activity if present.

Prefer the newest meaningful activity.

### Step 7 — live smoke test

Run a small 3-level Codex session similar to the hello-world run:

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
- roles when supplied;
- activity changing while each agent works;
- completion/result text after each child finishes;
- the final root completion.

Then point Agentop at the existing long multi-day session and confirm that loading and navigating it remains usable. Do not tune performance further unless a concrete problem appears.

## Small tests worth having

Keep tests focused on parsing/reduction rather than the terminal UI.

A handful of unit tests or a tiny fixture should cover:

1. root `session_meta`;
2. child `session_meta` with `parent_thread_id`, path, role, and depth;
3. grouping root + child + grandchild by common `session_id`;
4. `task_started` then `task_complete` lifecycle;
5. `item_completed` Reasoning summary extraction;
6. plaintext `FINAL_ANSWER` receipt extraction;
7. encrypted `NEW_TASK` being accepted without attempting to decode it;
8. incremental reader retaining a partial final JSON line and parsing it after completion.

Do not vendor entire real rollout files. Minimize representative lines into a small fixture or inline JSON test data.

## POC acceptance check

The POC is good enough when, from the `agentop` dev container, the user can run it against the shared Codex sessions and get a live screen that correctly reconstructs the current 0.152.1 multi-agent hierarchy.

For the known hello-world run, it should reconstruct at least:

```text
/root
└─ /root/hello_world_owner
   └─ /root/hello_world_owner/hello_world_candidate
```

and show the candidate as `map_implementer`, with useful recent/completion information from its own rollout.

During a new live run it must update when rollout files grow and when a new child rollout file appears.

It must never write to, truncate, rename, move, lock for writing, or otherwise mutate anything under the sessions directory.

After that works, stop and assess the POC before adding larger features.

## Things explicitly left for later

Do not implement these as part of this plan:

- trace-bundle support;
- `trace-reduce` integration;
- app-server integration;
- remote-control functionality;
- a database/index of historic rollouts;
- cross-machine monitoring;
- an LLM-based activity summarizer;
- sophisticated task decryption/recovery;
- broad backwards-compatibility machinery;
- plugin systems;
- configurable themes/layout frameworks;
- elaborate performance work before the ordinary rollout approach is measured.

Natural next experiments after the POC are likely to be session selection/history, better status summarization, and richer drill-down. Decide those from actual use rather than building them now.
