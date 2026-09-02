# Agentop

Agentop is a private Rust terminal UI for observing live Codex multi-agent work. It reconstructs an agent tree and recent activity by reading rollout JSONL files without modifying them.

## Run

Use the repository Dev Container on Linux/amd64, then run:

```bash
cargo run -- --sessions-dir "$HOME/.codex/sessions"
```

Agentop requires an interactive terminal. It selects the newest session rooted in the current working directory, falling back to the newest session overall. Select a specific session with an exact ID or unique prefix:

```bash
cargo run -- --sessions-dir "$HOME/.codex/sessions" --session 01abc
```

Use Up/Down or j/k to select an agent, r to rescan, and q or Esc to quit.

The screen shows exact producer version, exact-schema catalogue status, evidence-based compatibility, latest-turn lifecycle, meaningful activity age, bounded health diagnostics, and any plaintext status as an explicitly untrusted result claim.

## Safety and limitations

Agentop is a read-only observer. It never sends agent messages or writes, truncates, renames, moves, deletes, or locks rollout files for writing. Rollouts may contain sensitive prompts, messages, reasoning, command output, paths, and identifiers; never commit real rollouts or paste raw payloads into diagnostics.

The POC has representative semantic evidence for exact Codex version `0.149.0-alpha.4.1` and bounded live verification for exact version `0.152.1`. Other exact versions may merely be ingestable and/or uncatalogued; compatibility is never inferred from version ranges. Compatibility is chosen from each rollout's exact `session_meta.payload.cli_version`. Catalogue status does not prove semantic coverage, and silence is not interpreted as a stalled or blocked agent.

There is no session picker, mouse support, persistent index, networking, remote control, trace backend, or app-server integration.

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
