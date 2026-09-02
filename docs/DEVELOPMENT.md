# Agentop development

This guide covers the operational development workflow. Design choices, reducer semantics and POC acceptance criteria remain in the [POC implementation plan](POC_IMPLEMENTATION_PLAN.md).

## Supported environment

Use the repository Dev Container on Linux/amd64. The current amd64 boundary comes from the Tilth installation in `.devcontainer/Dockerfile`; compatibility with ARM hosts is not claimed.

A host needs:

- Docker or another runtime supported by VS Code Dev Containers;
- VS Code with the Dev Containers extension; and
- `$HOME/.codex/sessions` available for bind mounting.

The initial Dev Container has been rebuilt without cache successfully. Future changes to the Dockerfile or container lifecycle still require their own fresh-build check.

## Container lifecycle

`.devcontainer/devcontainer.json` creates the host sessions directory if needed and copies a host-level Codex `AGENTS.md` into the ignored `.devcontainer/local/` directory. The post-create script then installs that file into the container's persistent Codex home when it is non-empty.

The Compose configuration uses persistent volumes for:

- `/home/vscode/.codex`, including container-side Codex authentication and configuration;
- Cargo registry and Git caches; and
- shell command history.

The host sessions directory is mounted separately at `/home/vscode/.codex/sessions`. It is intentionally read-write so Codex inside the container can append new rollouts. Agentop itself remains a strictly read-only consumer.

The post-create script creates `$HOME/.codex/config.toml` only when it does not already exist. Rebuilding the image therefore does not replace configuration already stored in the persistent volume. Inspect or deliberately reset that volume when testing a changed initial configuration; resetting it also removes container-side Codex credentials and other persistent state.

The container uses Docker as its outer isolation boundary and configures Codex without an inner sandbox. It also has write access to the host sessions bind mount. Treat shell commands accordingly.

`shutdownAction` is `none`, so closing VS Code does not necessarily stop the container. Stop it through the host's container tooling when it should no longer run.

## First-run smoke check

Run these commands inside a freshly built container:

```bash
uname -m
rustc --version
cargo --version
rustfmt --version
cargo clippy --version
node --version
npm --version
codex --version
jq --version
shellcheck --version
```

`uname -m` must report `x86_64` in the currently supported environment. Then verify the required Codex generators and session visibility without writing a probe into the sessions directory:

```bash
codex app-server generate-json-schema --help >/dev/null
codex app-server generate-internal-json-schema --help >/dev/null
test -d "$HOME/.codex/sessions"
test -r "$HOME/.codex/sessions"
findmnt -T "$HOME/.codex/sessions"
```

A read-write mount is expected because it is shared with Codex. Agentop's non-mutating behaviour is an application invariant, not a mount-enforced guarantee.

The Docker CLI is not installed inside the development container. Build and lifecycle checks such as **Rebuild Container Without Cache** run through the host's Dev Containers tooling.

## Codex updates and schema capture

The image installs `@openai/codex@latest`. The [official Codex CLI documentation](https://developers.openai.com/codex/cli/#getting-started) lists npm as a supported installation and update method.

Docker can reuse the layer containing `npm install --global @openai/codex@latest`. An ordinary cached rebuild may therefore retain an older Codex binary. Refresh Codex deliberately with a no-cache rebuild, or with a future project-owned update command that couples the update to schema capture. Do not assume the `latest` tag alone proves freshness.

The official [Codex App Server documentation](https://developers.openai.com/codex/app-server/#message-schema) states that generated schema bundles are specific to the producing CLI version. The current CLI also exposes an internal generator used to obtain `RolloutLine.json`; that internal command is observed CLI behaviour rather than a public stability guarantee.

For each version being catalogued, the capture workflow must stage:

```bash
codex --version
codex app-server generate-json-schema --out ./schemas/staging/app-server/stable
codex app-server generate-json-schema --experimental --out ./schemas/staging/app-server/experimental
codex app-server generate-internal-json-schema --out ./schemas/staging/internal
```

These commands describe the inputs to the planned capture workflow; do not manually publish their staging output as an archive. The capture implementation must provide the version checks, fresh staging directory, schema validation, hashes, manifest and atomic publication required by the POC plan.

Never run the current binary and label its output as a historical version. Acquire and execute the exact historical binary when available. Failure to acquire a historical schema does not prevent tolerant ingestion of that version.

Generated schemas establish permitted shapes, not observed sequencing or semantic correctness. Keep the schema catalogue separate from compatibility levels:

- **Catalogued:** an exact generated schema set and manifest are archived.
- **Ingestable:** discovery, grouping and tailing work without crashes, with unknown data reported.
- **Semantically covered:** representative fixtures prove topology, lifecycle and activity reduction.
- **Live verified:** a running process of that exact version has been observed successfully.

## Build and verification

After the crate is scaffolded, use:

```bash
cargo build
cargo run -- --sessions-dir "$HOME/.codex/sessions"
```

Complete implementation changes before running the relevant final checks:

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

Add a system package only when a selected dependency has a demonstrated native requirement. The dependencies currently proposed in the POC plan do not require additional Debian development packages.

Python is not part of the planned application or tooling. The container does not currently install `uv`; if Python becomes necessary, add `uv` and its documented workflow in the same change rather than introducing direct `pip` or unmanaged Python commands.

## Fixtures and rollout privacy

Real rollout files may contain confidential prompts, tool arguments, command output, paths and identifiers. They are investigation inputs, not repository fixtures.

For each fixture:

1. copy only the minimum records needed to reproduce one invariant;
2. replace user content, secrets and unrelated identifiers;
3. retain structural fields only when the test depends on them;
4. keep the exact producer-version and schema provenance outside the JSON payload; and
5. review the staged diff for accidental rollout content before committing.

Tests should use temporary directories and read-only fixture files. They must not modify the mounted host sessions tree.

## Documentation responsibilities

Keep each document focused:

- `README.md` is the contributor entry point and safety summary.
- `AGENTS.md` governs implementation behaviour in this repository.
- `docs/DEVELOPMENT.md` contains repeatable operational workflows.
- `docs/POC_IMPLEMENTATION_PLAN.md` remains the design and acceptance baseline for the POC.
- `schemas/README.md` should be added with the first archived schema set to document its concrete on-disk format.

Update these documents when a change adds a prerequisite, changes the container lifecycle, alters the schema-capture process or promotes a compatibility claim.
