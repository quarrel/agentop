# Agentop development

The supported environment is the repository Dev Container on Linux/amd64. It supplies Rust, Cargo, rustfmt, Clippy, rust-analyzer, Codex CLI, Node.js, jq, Git, and the contributor tools needed by this POC. No extra native packages are required.

The host sessions directory is mounted at `/home/vscode/.codex/sessions`. Although the mount is read-write for Codex itself, Agentop is strictly a read-only consumer. Use temporary directories and sanitised minimal fixtures in tests; never test by modifying mounted sessions.

## Build, run, and verify

Inside the Dev Container:

```bash
cargo build
cargo run -- --sessions-dir "$HOME/.codex/sessions"
cargo run -- --sessions-dir "$HOME/.codex/sessions" --session <SESSION_ID_OR_UNIQUE_PREFIX>
```

An interactive TTY is required. Complete changes before running the final gate:

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

For a live smoke test, confirm navigation remains responsive while files grow, new child rollouts appear, the catching-up indicator is visible during updates, resize and tiny-terminal views remain usable, and q/Esc restores the cursor, alternate screen, and normal terminal input. Also exercise an error after terminal activation with a temporary fixture; the RAII guard must restore the terminal.

## Codex updates and schema capture

Generated schema bundles are specific to their producing Codex CLI version. Capture all three surfaces using the repository script:

```bash
scripts/capture-codex-schemas.sh
```

The historical importer is a no-argument, provenance-pinned workflow for exact Codex CLI version `0.149.0-alpha.4.1` only:

```bash
scripts/import-codex-release-schemas.sh
```

These workflows enforce exact version identity, fresh staging, JSON validation, hashes, and collision-safe publication beneath `schemas/`. Never generate with the current binary and label the output as historical. Catalogue status is separate from ingestable, semantically covered, and live-verified compatibility evidence.

## Fixture and privacy rules

Real rollouts are investigation inputs, not fixtures. For every fixture:

1. Copy only the minimum records needed for one invariant.
2. Replace user content, secrets, and unrelated identifiers.
3. Retain only structural fields required by the test.
4. Record exact producer/schema provenance outside the JSON payload.
5. Review the diff for accidental rollout content.

Agentop deliberately has no database, daemon, networking, remote control, trace backend, decryption, generic plugin system, or exhaustive version-specific model. See the [POC implementation plan](POC_IMPLEMENTATION_PLAN.md) for reducer semantics and acceptance criteria.
