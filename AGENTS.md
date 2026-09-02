# Agentop

## Purpose and authority

Agentop is a private Rust TUI proof of concept for observing Codex multi-agent rollout files. The [POC implementation plan](docs/POC_IMPLEMENTATION_PLAN.md) is the current design baseline until implementation evidence justifies a documented change. Do not silently depart from its scope, compatibility levels or acceptance criteria.

Keep the POC small and evidence-led. Prefer correcting the model or parser over adding compatibility frameworks, generic backends or speculative abstractions.

## Safety and privacy

- Treat the sessions directory as immutable input. Never write, truncate, rename, move, delete or lock a rollout for writing.
- Do not run tests against the mounted host sessions directory when a temporary fixture can establish the behaviour.
- Never commit complete real rollouts or paste their raw payloads into source, tests, documentation, logs or commit messages.
- Minimise and sanitise fixtures. Remove user content, secrets and unrelated identifiers while retaining only the structure needed by the test.
- Bound and sanitise all rollout-derived strings before terminal display or diagnostic retention.
- Schema capture may write only beneath the repository's schema staging/archive path. Preserve generated schema artefacts unchanged.

## Compatibility discipline

- Identify a rollout by its exact `session_meta.payload.cli_version`; never substitute the nearest semantic version.
- Keep schema catalogue status separate from runtime compatibility evidence.
- Treat unknown records and fields as expected compatibility observations, while surfacing malformed required data contextually.
- Add version-specific normalisation only when an exact schema or representative fixture demonstrates the need.
- Apply `subagent_history_start_ordinal` before semantic reduction. Only the rollout's identifying child header may bypass that boundary.
- Treat an agent's own rollout as primary lifecycle evidence. Parent activity and plaintext receipt claims remain separately labelled supplementary evidence.

## Rust workflow

Use Cargo for the application and keep dependencies modest. Do not introduce Tokio, databases, daemons, networking, generated per-version Rust models or native system dependencies without a demonstrated requirement.

After the Rust crate exists, complete the intended change before running the smallest relevant verification set. The normal final gate is:

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

Let unexpected errors propagate with file, byte-offset, producer-version and ordinal context where available. Do not turn malformed required data into plausible-looking defaults.

No Python tooling is currently required. If Python becomes a concrete project dependency, add and use `uv` in the Dev Container as part of that change; do not introduce a pip-based workflow.

## Development environment

The supported development environment is the repository Dev Container on Linux/amd64. Codex is deliberately installed from `@openai/codex@latest`; after an intentional Codex update, capture and catalogue schemas before claiming new compatibility.

Do not add system packages merely for convenience. Document any genuine new host or container prerequisite in both the Dockerfile change and [docs/DEVELOPMENT.md](docs/DEVELOPMENT.md).
