# Agentop development

The supported environment is the repository Dev Container on Linux/amd64. It supplies Rust, Cargo, rustfmt, Clippy, rust-analyzer, Codex CLI, Node.js, jq, Git, and the contributor tools needed by this POC. No extra native packages are required.

The host sessions directory is mounted at `/home/vscode/.codex/sessions`. Although the mount is read-write for Codex itself, Agentop is strictly a read-only consumer. Use temporary directories and sanitised minimal fixtures in tests; never test by modifying mounted sessions.

## Build, run, and verify

Inside the Dev Container:

```bash
cargo build
cargo run -- --sessions-dir "$HOME/.codex/sessions" --color=auto
cargo run -- --sessions-dir "$HOME/.codex/sessions" --session <SESSION_ID_OR_UNIQUE_PREFIX> --color=none
```

An interactive TTY is required. `--color=auto` is the default and enables the semantic palette; `--color=none` removes all explicit foreground and background colours while retaining labels, the selection marker, and selection attributes for accessibility. Omitting `--session` opens the global session browser; an exact ID or unique prefix bypasses it directly. Groups are ordered by the greatest rollout-file modification time across each group, falling back to the greatest `session_meta` timestamp when file times are unavailable. This is observed file/metadata update recency, not inferred lifecycle or proof that a session is running. The project label is the root rollout's recorded `git.repository_url` repository name, falling back to its recorded `cwd`; picker details show bounded recorded repository and cwd information. Browser discovery reads metadata only. Up/Down or j/k navigate, Enter opens a session, r refreshes the global list, Esc returns from a tree or exits the picker, and q exits. Complete changes before running the final gate:

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

For a live smoke test, confirm the picker navigates, refreshes, opens a tree, and returns with Esc while the sessions tree remains byte-for-byte unchanged. Session opening should render the root promptly, show `loading history…` while existing history is reduced, and then clear that indicator truthfully. Each initial catch-up poll uses a separate bounded 8 MiB/65,536-record allowance while preserving the ordinary 256 KiB/2,048-work reserve for discovery, live tails, and pending children. The event loop groups successive bounded polls into an approximately 100 ms loading work window, checks for terminal input between polls, then redraws; a slow individual poll remains singly bounded, and saturated windows continue without an artificial sleep. JSONL chunks are consumed with one buffer compaction rather than shifting the remaining buffer after every record. Ordinary live polling settles to about one second without busy-spinning. Exceptional file truncation may synchronously rebuild the selected group. Also confirm tree navigation remains responsive while files grow, new child rollouts appear, the catching-up indicator is visible during updates, `WAITING ON AGENT ↓` appears only for a running agent whose newest unfinished exact tool call is `wait_agent`, resize and tiny-terminal views remain usable, and q/Esc restores the cursor, alternate screen, and normal terminal input. Exercise an error after terminal activation with a temporary fixture; the RAII guard must restore the terminal.

## Codex updates and schema catalogue

Agentop consumes only the self-contained internal `RolloutLine.json` schema. It does not archive the unused stable or experimental app-server schema surfaces. Official versions map to canonical RolloutLine hashes, and identical schemas are stored once under `schemas/codex/rollout-line/by-hash/`.

The checked-in `versions.json` is compiled into Agentop as its read-only seed. An installed binary can extend that seed into its user catalogue without a shell toolchain:

```bash
agentop build-schema
```

The command enumerates official `rust-v*` GitHub tag refs at or after `0.149.0-alpha.1`, subtracts exact versions already mapped, resolves immutable tag and commit provenance, downloads each missing stable precomputed export at that commit, and extracts only `internal_json_schema["RolloutLine.json"]`. It validates self-contained JSON Schema references, canonicalises and hashes the result, deduplicates equal families, and publishes families before the mapping under an advisory lock. Existing version conflicts and moved tag objects are errors; a failed run leaves the previous mapping intact.

The default writable catalogue is `$XDG_DATA_HOME/agentop/schemas/codex/rollout-line`, falling back to `$HOME/.local/share/agentop/schemas/codex/rollout-line`. `AGENTOP_SCHEMA_DIR` or `--catalogue-dir` supplies an exact override. Set `GH_TOKEN` or `GITHUB_TOKEN` only through the environment when authenticated GitHub API access is needed. Tokens are sent to the API host and not to the raw-content host.

Maintainers refresh the checked-in seed with:

```bash
cargo run -- build-schema --catalogue-dir schemas/codex/rollout-line
```

Review all resulting provenance and schema-family changes before committing. Catalogue status remains separate from ingestable, semantically covered, and live-verified compatibility evidence. Normal TUI operation performs no network access and never updates the catalogue.

## Fixture and privacy rules

Real rollouts are investigation inputs, not fixtures. For every fixture:

1. Copy only the minimum records needed for one invariant.
2. Replace user content, secrets, and unrelated identifiers.
3. Retain only structural fields required by the test.
4. Record exact producer/schema provenance outside the JSON payload.
5. Review the diff for accidental rollout content.

Agentop deliberately has no database, daemon, networking, remote control, trace backend, decryption, generic plugin system, or exhaustive version-specific model. See the [POC implementation plan](POC_IMPLEMENTATION_PLAN.md) for reducer semantics and acceptance criteria.
