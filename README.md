# Agentop

[![CI](https://github.com/quarrel/agentop/actions/workflows/ci.yml/badge.svg)](https://github.com/quarrel/agentop/actions/workflows/ci.yml)

Agentop is a read-only terminal UI for observing Codex multi-agent sessions. It reconstructs agent trees, lifecycle state, recent activity, tool use, interaction history, and context pressure from the rollout JSONL files Codex already writes.

It works with Codex CLI sessions and sessions created through the Codex IDE extension when their rollout files are available in the same sessions directory. Agentop does not control Codex or require changes to the sessions it observes.

## Installation

### Prebuilt Linux binary

Tagged releases publish an x86-64 Linux archive and SHA-256 checksum on the [GitHub Releases](https://github.com/quarrel/agentop/releases) page.

### Install from source

A stable Rust toolchain is required:

```bash
cargo install --git https://github.com/quarrel/agentop.git --locked
```

From a local checkout:

```bash
cargo install --path . --locked
```

The crates.io package named [`agentop`](https://crates.io/crates/agentop) is a different project. Do not use `cargo install agentop` for this repository. If this project is published to crates.io later, it will need a distinct package name while retaining the `agentop` executable name.

Agentop is developed and tested on Linux/amd64. Other platforms may work but are not currently covered by CI.

## Quick start

Run Agentop from any directory:

```bash
agentop
```

The sessions directory is resolved in this order:

1. `--sessions-dir <path>`
2. `$CODEX_HOME/sessions`
3. `$HOME/.codex/sessions`

With no `--session`, Agentop opens a browser of the most recently updated sessions. The project label comes from the root rollout's recorded repository name, falling back to its recorded working directory. Select a session with Enter, or open one directly with an exact session ID or unique prefix:

```bash
agentop --session 01abc
agentop --sessions-dir /path/to/sessions --session 01abc
```

Use `--color=none` to disable explicit terminal colours. `--color=auto` is the default and enables colour in the required interactive terminal.

```text
Usage: agentop [OPTIONS] [COMMAND]

Commands:
  build-schema    Fetch and catalogue schemas for new official Codex releases

Options:
  --sessions-dir <PATH>
  --session <ID_OR_PREFIX>
  --color <auto|none>
  -V, --version
```

## Controls

| View | Key | Action |
| --- | --- | --- |
| Session browser | ↑/↓ or j/k | Select a session |
| Session browser | Enter | Open the selected session |
| Session browser | r | Refresh the session list |
| Agent tree | ↑/↓ or j/k | Select an agent |
| Agent tree | Enter | Open the selected agent's interaction history |
| Agent tree | h | Hide or show completed agents |
| Agent tree | r | Rescan for rollout updates and new agents |
| Interaction history | ↑/↓ or j/k | Move through retained interactions |
| Any nested view | Esc | Return one level |
| Any view | q | Quit |

Opening a session renders its root promptly and adds agents progressively while historical records are reduced. Live rollouts and newly created child rollouts continue to appear without reopening the session.

## Status and context

Agentop reports the latest observed turn lifecycle:

| Status | Meaning |
| --- | --- |
| `PENDING` | No running or terminal turn has been observed yet |
| `RUNNING` | The latest observed turn is active |
| `WAITING ON AGENT ↓` | The newest unfinished call is exactly `wait_agent`; the relevant child normally follows below |
| `COMPLETED` | The latest observed turn completed |
| `INTERRUPTED` / `ERRORED` | Codex recorded a terminal interruption or error |
| `STALE` | The rollout still says running, but later session evidence indicates it is no longer active; completion is unknown |

`STALE` is deliberately conservative. Agentop prefers a later complete `list_agents` snapshot that excludes the agent and otherwise requires at least two hours of later meaningful session activity. It never converts stale evidence into a completion claim.

For active agents, `CTX n%` is the latest observed request input count divided by the reported model context window. It is not a live counter. Yellow begins at 70% and red at 85%. Completed and stale agents omit context pressure because it is no longer actionable. Cumulative token usage is labelled separately and is never treated as current context occupancy.

The interaction view retains bounded, sanitised lifecycle, message, reasoning, communication, context-management, and paired tool-call summaries. It does not retain raw tool inputs or outputs.

To make subagent assignments readable in the interaction view, ask subagents to announce their task before starting work. Add the following top-level setting to `.codex/config.toml` in a specific repository (Codex must trust the project), or to `~/.codex/config.toml` for all Codex instances using that user configuration. If `developer_instructions` already exists, merge this section into its existing string; project settings take precedence over user settings. Start a new Codex session after changing the configuration. The announcement appears as a message such as `Task: Print hello.`; it does not decrypt the incoming communication. See [Codex configuration](https://developers.openai.com/codex/config-basic/) for configuration scope and precedence.

```toml
developer_instructions = """
# Subagent task announcement

When acting as a subagent and you receive a NEW_TASK message from a parent:

1. Before doing any work, send a commentary message consisting of:
   `Task: ` followed by the NEW_TASK payload verbatim.
2. The task is only the NEW_TASK payload. Do not include inherited conversation
   context, developer instructions, routing metadata, the sender, or your agent name.
"""
```

## Compatibility and schema catalogue

Codex rollout formats change frequently, and one sessions directory can contain files written by several producer versions. Agentop therefore identifies every rollout by its exact `session_meta.payload.cli_version`; it does not infer compatibility from a version range or substitute a nearby version.

The embedded catalogue maps exact Codex versions to canonical `RolloutLine.json` schema hashes. Structurally identical releases share one schema family. Catalogue membership proves schema provenance only. Runtime claims are separate:

- **Catalogued:** the exact version maps to a verified schema family.
- **Ingestable:** Agentop can discover, group, and tail it without crashing.
- **Semantically covered:** representative fixtures establish the reducer behaviour.
- **Live verified:** that exact version has been exercised against a running producer.

Agentop targets tolerant ingestion of the Codex 0.149 family and later, but semantic coverage is claimed only where there is fixture or live evidence. Unknown fields and variants are expected; malformed required data remains visible as a bounded diagnostic.

Update the user catalogue for official Codex releases newer than the binary:

```bash
agentop build-schema
```

This explicit command is the only Agentop operation that uses the network or writes catalogue data. It reads official immutable tag provenance from the [codex-cli GitHub repo](https://github.com/openai/codex), downloads the stable precomputed export, extracts only `RolloutLine.json`, and stores unique schemas by canonical hash. See [the schema catalogue documentation](schemas/README.md).

## Privacy and scope

Normal TUI operation treats the sessions directory as immutable input: it never writes, truncates, renames, moves, deletes, or locks rollout files for writing. It is also offline.

Rollouts can contain sensitive prompts, messages, reasoning, commands, paths, and identifiers. Agentop sanitises and bounds terminal text, but anyone who can run it already has access to the underlying files. Do not publish real rollouts, raw diagnostics, or unsanitised recordings.

Agentop deliberately has no database, daemon, remote control, cross-machine monitoring, trace backend, app-server integration, payload decryption, or plugin system.

## Development

See [Development](docs/DEVELOPMENT.md) for build, test, schema, recording, and release workflows, and [Architecture](docs/ARCHITECTURE.md) for the ingestion and reducer contracts.

## Licence

Agentop is licensed under the GNU General Public License, version 2 or any later version. See [LICENSE.md](LICENSE.md).
