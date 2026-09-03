# Agentop

Agentop is a private Rust terminal UI for observing live Codex multi-agent work. It reconstructs an agent tree and recent activity by reading rollout JSONL files without modifying them.

## Run

Use the repository Dev Container on Linux/amd64, then run:

```bash
cargo run -- --sessions-dir "$HOME/.codex/sessions" --color=auto
```

With no `--session`, Agentop opens a global session browser ordered by the greatest rollout-file modification time in each group, falling back to the greatest `session_meta` timestamp. This is observed file/metadata update recency, not inferred lifecycle or proof that a session is running. Each row shows a bounded, sanitised project label from the root rollout's recorded `git.repository_url` repository name, falling back to its recorded `cwd`, plus an abbreviated session ID, rollout count, and update age; picker details show bounded recorded repository and cwd information. Use Up/Down or j/k to navigate, Enter to open, r to refresh the global list, Esc to return from a tree or exit the picker, and q to exit. Select a specific session directly, bypassing the picker, with an exact ID or unique prefix:

```bash
cargo run -- --sessions-dir "$HOME/.codex/sessions" --session 01abc
```

Within a session tree, use Up/Down or j/k to select an agent, Enter to open its bounded interaction history, h to hide or show completed agents, and r to rescan. Hidden completed parents do not hide live descendants; those descendants are promoted one visual level. In interaction history, use Up/k for older entries and Down/j for newer entries. Esc returns one level and q exits from anywhere. `--color=auto` is the default and enables a restrained semantic palette because an interactive TTY is required. Use `--color=none` to disable every explicit foreground and background colour; labels, the `›` selection marker, and bold/reversed selection attributes remain available as non-colour cues.

The screen shows each agent's model and reasoning effort before its latest-turn lifecycle, followed by known timestamps, meaningful activity, reasoning summaries, messages, and final results when available. Healthy schema-catalogue and compatibility values stay out of the way; missing schemas, unknown compatibility, and malformed or oversized session records appear in red. Unknown record/event counts and raw parser diagnostics are retained internally but omitted from normal agent details because they are session-wide development evidence rather than actionable per-agent state. Opening a session shows its root promptly and reduces existing history in bounded batches. Each initial catch-up poll has a separate 8 MiB/65,536-record budget while reserving the original 256 KiB/2,048-work allowance for live tails, discovery, and pending children. The event loop runs successive bounded polls for up to roughly 100 ms before redrawing, checking for terminal input between polls; saturated loading windows continue without an artificial inter-window delay. Ordinary live polling then settles to about one second. A running agent whose newest unfinished call is exactly `wait_agent` is labelled `WAITING ON AGENT ↓`, while its activity remains `wait_agent`.

## Update the schema catalogue

Agentop embeds the exact Codex version-to-RolloutLine mappings available when it is built. To discover later official releases without reinstalling Agentop, run:

```bash
agentop build-schema
```

This explicit maintenance command uses GitHub's official `rust-v*` tags, immutable tag/commit provenance, and stable release exports. It downloads only missing versions, stores each unique canonical `RolloutLine.json` once in a content-addressed user catalogue, and never replaces a conflicting mapping. The default catalogue is under `$XDG_DATA_HOME/agentop` (or `$HOME/.local/share/agentop`); `AGENTOP_SCHEMA_DIR` and `--catalogue-dir` override it. `GH_TOKEN` or `GITHUB_TOKEN` may be set for GitHub API authentication.

## Safety and limitations

Agentop is a read-only observer. It never sends agent messages or writes, truncates, renames, moves, deletes, or locks rollout files for writing. Rollouts may contain sensitive prompts, messages, reasoning, command output, paths, and identifiers; never commit real rollouts or paste raw payloads into diagnostics.

The POC has representative semantic evidence for exact Codex version `0.149.0-alpha.4.1` and bounded live verification for exact version `0.152.1`. Other exact versions may merely be ingestable and/or uncatalogued; compatibility is never inferred from version ranges. Compatibility is chosen from each rollout's exact `session_meta.payload.cli_version`. Catalogue status does not prove semantic coverage, and silence is not interpreted as a stalled or blocked agent.

The normal TUI has no networking, mouse support, persistent index, remote control, trace backend, or app-server integration. Only the explicit `build-schema` maintenance command uses the network and writes Agentop's schema catalogue; it never accesses rollout files for writing.

## Development

Build and verify with Cargo:

```bash
cargo build
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

Schema catalogue and contributor workflows are documented in [docs/DEVELOPMENT.md](docs/DEVELOPMENT.md). The [POC implementation plan](docs/POC_IMPLEMENTATION_PLAN.md) remains the design and acceptance baseline.

## Licence

This repository is proprietary and confidential. See [LICENSE.md](LICENSE.md).
