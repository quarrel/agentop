# Agentop development

Agentop is developed and tested on Linux/amd64. The repository Dev Container is the quickest reproducible setup, but it is optional.

## Prerequisites

For a native checkout, install:

- a stable Rust toolchain with Cargo, rustfmt, and Clippy;
- Git; and
- a C compiler and linker suitable for Rust dependencies.

The Codex CLI is needed only to create/live-test sessions or intentionally compare locally generated schemas. Normal builds and tests do not require Codex, Node.js, Python, a database, or a mounted sessions directory.

The Dev Container supplies Rust, Cargo, rustfmt, Clippy, rust-analyzer, Codex CLI, Node.js, jq, Git, and the repository's contributor tools. It mounts the host Codex directory for live observation. Although Codex itself needs write access there, Agentop remains a read-only consumer.

## Build, run, and verify

```bash
cargo build --locked
cargo run --locked --
cargo run --locked -- --sessions-dir "$HOME/.codex/sessions" --session <ID_OR_PREFIX>
```

An interactive TTY is required for the TUI. Run the normal gate after completing a change:

```bash
cargo fmt --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
```

For a release-optimised local build:

```bash
RUSTFLAGS="-C target-cpu=native" cargo build --release --locked
```

That native build is for the current machine and must not be used for portable release artefacts.

## Source map

- `src/main.rs`: CLI, sessions-directory resolution, and entry points.
- `src/rollout.rs`: discovery, session grouping, incremental reading, and work budgets.
- `src/model.rs`: projected state and record reduction.
- `src/ui.rs`: terminal management, navigation, rendering, and sanitisation.
- `src/schema.rs`: embedded/user schema catalogue.
- `src/schema_sync.rs`: official-release schema updater.
- `schemas/`: checked-in exact-version-to-schema-family catalogue.

The behavioural contracts live in [Architecture](ARCHITECTURE.md). Keep that document and the README current when user-visible behaviour or compatibility evidence changes.

## Live smoke test

Use a disposable or existing read-only sessions directory. Do not modify mounted host sessions to create test conditions.

Confirm that:

1. the browser is ordered by observed update recency and can refresh, open, and return;
2. opening a long session renders promptly, remains responsive, and clears `loading history…` only after catch-up;
3. existing agents update and a newly spawned child appears without reopening the session;
4. selection, completed-agent hiding, interaction navigation, resizing, and small terminals remain usable;
5. `WAITING ON AGENT ↓`, `STALE`, context pressure, and compatibility warnings appear only when their documented evidence exists;
6. `q`, `Esc`, and an induced error after terminal activation all restore the cursor, raw mode, and alternate screen; and
7. the sessions tree is byte-for-byte unchanged after the run.

Use temporary fixtures for induced errors and edge cases. Exceptional file truncation may rebuild a selected session synchronously.

## Fixtures and privacy

Real rollouts are investigation inputs, not test fixtures. For every committed fixture:

1. copy only the minimum records required for one invariant;
2. replace user content, secrets, paths, and unrelated identifiers;
3. retain only structurally necessary fields;
4. record exact producer/schema provenance outside the JSON payload; and
5. inspect the final diff for accidental rollout content.

Never commit a complete real rollout, raw tool payload, access token, or unsanitised terminal recording. Keep all retained and displayed rollout-derived strings bounded and terminal-safe.

## Schema maintenance

Agentop consumes only the self-contained internal `RolloutLine.json`. Stable and experimental app-server schemas are not used or archived.

The checked-in version mapping is compiled into the binary. End users can extend it without a shell toolchain:

```bash
agentop build-schema
```

The default writable catalogue is `$XDG_DATA_HOME/agentop/schemas/codex/rollout-line`, falling back to `$HOME/.local/share/agentop/schemas/codex/rollout-line`. `AGENTOP_SCHEMA_DIR` and `--catalogue-dir` provide exact overrides. Optional GitHub authentication comes from `GH_TOKEN` or `GITHUB_TOKEN`; never put a token on the command line.

Maintainers refresh the checked-in seed with:

```bash
cargo run --locked -- build-schema --catalogue-dir schemas/codex/rollout-line
```

The command walks official `rust-v*` tag refs from `0.149.0-alpha.1`, resolves immutable tag/commit provenance, downloads the stable precomputed export at the exact commit, and extracts `internal_json_schema["RolloutLine.json"]`. It validates and canonicalises the schema, deduplicates equal hashes, and publishes the mapping only after all new families exist.

Review every provenance, mapping, and schema-family change before committing. Catalogue membership does not establish semantic coverage or live verification. More detail is in [the schema catalogue README](../schemas/README.md).

## Recording the README demo

[VHS by Charmbracelet](https://github.com/charmbracelet/vhs) is a good fit for the animated terminal view: recordings are scripted, repeatable, and can be exported directly as GIF.

Record only a deliberately sanitised session. Put the output at `docs/demo.gif`, then add it immediately below the opening paragraph in the README:

```markdown
![Agentop displaying a live Codex multi-agent session](docs/demo.gif)
```

A minimal tape can look like:

```text
Output docs/demo.gif
Set Width 1400
Set Height 900
Set FontSize 16
Set TypingSpeed 20ms

Type "agentop --sessions-dir \"$AGENTOP_DEMO_SESSIONS\""
Enter
Sleep 3s
Type "j"
Sleep 1s
Enter
Sleep 2s
Escape
Sleep 1s
Type "q"
```

Set `AGENTOP_DEMO_SESSIONS` to a temporary directory containing only sanitised rollout fixtures. Review the rendered frames for usernames, home paths, repository names, session/thread IDs, prompts, messages, commands, and tokens before committing the GIF. Do not record directly from a real sessions archive and assume terminal cropping is sufficient.

## Release process

The Cargo package name is currently unavailable on crates.io, so releases are GitHub binary releases only.

1. Update the package version in `Cargo.toml` and let Cargo update the root package entry in `Cargo.lock`.
2. Run the normal gate, `cargo package --list`, and a clean portable release build.
3. Commit and push the version change.
4. Create an annotated `v<version>` tag whose version exactly matches `Cargo.toml`, then push it.
5. Review the generated Linux/amd64 archive, SHA-256 checksum, and release notes on GitHub.

The release workflow verifies the tag/version match, reruns formatting, Clippy, tests, and packaging, builds `x86_64-unknown-linux-gnu`, and publishes the archive. It never creates or pushes a tag.
