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

Within a session tree, use Up/Down or j/k to select an agent and r to rescan. `--color=auto` is the default and enables a restrained semantic palette because an interactive TTY is required. Use `--color=none` to disable every explicit foreground and background colour; labels, the `›` selection marker, and bold/reversed selection attributes remain available as non-colour cues.

The screen shows exact producer version, exact-schema catalogue status, evidence-based compatibility, latest-turn lifecycle, meaningful activity age, bounded health diagnostics, and any plaintext status as an explicitly untrusted result claim. Opening a session shows its root promptly and reduces existing history in bounded batches. Initial catch-up has a separate 8 MiB/65,536-record budget per roughly 50 ms poll, while reserving the original 256 KiB/2,048-work allowance for live tails, discovery, and pending children; ordinary live polling then settles to about one second. A running agent whose newest unfinished call is exactly `wait_agent` is labelled `WAITING ON AGENT ↓`, while its activity remains `wait_agent`.

## Safety and limitations

Agentop is a read-only observer. It never sends agent messages or writes, truncates, renames, moves, deletes, or locks rollout files for writing. Rollouts may contain sensitive prompts, messages, reasoning, command output, paths, and identifiers; never commit real rollouts or paste raw payloads into diagnostics.

The POC has representative semantic evidence for exact Codex version `0.149.0-alpha.4.1` and bounded live verification for exact version `0.152.1`. Other exact versions may merely be ingestable and/or uncatalogued; compatibility is never inferred from version ranges. Compatibility is chosen from each rollout's exact `session_meta.payload.cli_version`. Catalogue status does not prove semantic coverage, and silence is not interpreted as a stalled or blocked agent.

There is no mouse support, persistent index, networking, remote control, trace backend, or app-server integration.

## Development

Build and verify with Cargo:

```bash
cargo build
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

Schema capture and contributor workflows are documented in [docs/DEVELOPMENT.md](docs/DEVELOPMENT.md). The [POC implementation plan](docs/POC_IMPLEMENTATION_PLAN.md) remains the design and acceptance baseline.

## Licence

This repository is proprietary and confidential. See [LICENSE.md](LICENSE.md).
