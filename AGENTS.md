# Agentop

## Product direction

Agentop is a read-only Rust TUI for observing Codex multi-agent rollout files. Keep it small, evidence-led, and useful to people running Codex. Prefer correcting the model or parser over adding generic backends, compatibility frameworks, or speculative abstractions.

[Architecture](docs/ARCHITECTURE.md) defines the durable ingestion, reduction, compatibility, and safety contracts. Update it when implementation evidence justifies a behavioural change; do not silently depart from those contracts.

## Safety and privacy

- Treat every sessions directory as immutable input. Never write, truncate, rename, move, delete, or lock a rollout for writing.
- Do not run tests against mounted host sessions when a temporary fixture can establish the behaviour.
- Never commit complete real rollouts or paste their raw payloads into source, tests, documentation, logs, recordings, or commit messages.
- Minimise and sanitise fixtures. Remove user content, secrets, paths, and unrelated identifiers while retaining only the structure needed by the test.
- Bound and sanitise all rollout-derived strings before terminal display or diagnostic retention.
- Schema maintenance may write only beneath the explicitly selected schema catalogue. Preserve generated schema artefacts unchanged.

## Compatibility discipline

- Identify a rollout by its exact `session_meta.payload.cli_version`; never substitute the nearest semantic version.
- Keep schema catalogue status separate from runtime compatibility evidence.
- Treat unknown records and fields as expected compatibility observations, while surfacing malformed required data contextually.
- Add version-specific normalisation only when an exact schema or representative fixture demonstrates the need.
- Apply `subagent_history_start_ordinal` before semantic reduction. Only the rollout's identifying child header may bypass that boundary.
- Treat an agent's own rollout as primary lifecycle evidence. Parent activity and plaintext result claims remain separately labelled supplementary evidence.

## Rust workflow

Use Cargo and keep dependencies modest. Do not introduce Tokio, databases, daemons, automatic TUI networking, generated per-version Rust models, or native system dependencies without a demonstrated requirement.

Complete the intended change before running the smallest relevant verification set. The normal final gate is:

```bash
cargo fmt --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
```

Let unexpected errors propagate with file, byte-offset, producer-version, and ordinal context where available. Do not turn malformed required data into plausible-looking defaults.

No Python tooling is currently required. If Python becomes a concrete project dependency, add and use `uv`; do not introduce a pip-based workflow.

## Documentation and releases

Keep [README.md](README.md) focused on installation and operation, [Development](docs/DEVELOPMENT.md) focused on contribution and release workflows, [Architecture](docs/ARCHITECTURE.md) focused on durable design contracts, and [the schema README](schemas/README.md) focused on catalogue provenance.

Do not claim compatibility, platform support, release availability, or crates.io installation without matching evidence. README recordings and fixtures must be sanitised before they are committed.

## Development environment

Linux/amd64 is the currently tested platform. The repository Dev Container is the recommended reproducible environment, not a requirement for native development.

Codex is deliberately installed in the Dev Container from `@openai/codex@latest`. After an intentional Codex update, refresh and review the schema catalogue before claiming new compatibility.

Do not add system packages merely for convenience. Document any genuine new host or container prerequisite in both the Dockerfile change and [Development](docs/DEVELOPMENT.md).
