# Agentop

Agentop is a private proof of concept for observing live Codex multi-agent work. It will be a small Rust terminal UI that reconstructs an agent tree and recent activity by reading Codex rollout JSONL files.

Implementation has not started. The [POC implementation plan](docs/POC_IMPLEMENTATION_PLAN.md) is the current design baseline.

## Intended behaviour

Agentop will:

- discover related root and child rollouts under a Codex sessions directory;
- display agent topology, role, latest-turn lifecycle and meaningful recent activity;
- tail active rollout files without loading unrelated histories in full;
- ingest evolving rollout shapes conservatively and report unknown data; and
- treat every rollout as read-only input.

It is an observer, not a Codex controller. The POC will not send messages, accept work, mutate sessions, run a daemon or expose a network service.

## Supported development environment

The primary supported environment is this repository's Dev Container on Linux/amd64. The amd64 boundary is explicit because the image installs an amd64 Tilth binary. Other architectures are not currently supported.

Host prerequisites:

- Docker or another runtime supported by VS Code Dev Containers;
- VS Code with the Dev Containers extension; and
- a readable `$HOME/.codex/sessions` directory on the host.

The container supplies Rust, Cargo, rustfmt, Clippy, rust-analyzer, Codex CLI, Node.js, `jq`, Git and the other contributor tools currently needed. No additional native packages are required for the planned Rust dependencies.

## Open the project

1. Ensure the host sessions directory exists:

   ```bash
   mkdir -p "$HOME/.codex/sessions"
   ```

2. Open the repository in VS Code.
3. Select **Dev Containers: Reopen in Container**. Use **Rebuild Container Without Cache** when deliberately refreshing Codex or the base image.
4. If the Codex CLI is not authenticated in the persistent container volume, run `codex` and follow its sign-in flow.
5. Inside the container, perform the smoke check from [Development](docs/DEVELOPMENT.md#first-run-smoke-check).

The official [Codex CLI documentation](https://developers.openai.com/codex/cli/#getting-started) describes the supported installation and sign-in methods. This image installs the npm distribution during its build.

Once the Rust executable exists, the basic development invocation will be:

```bash
cargo run -- --sessions-dir "$HOME/.codex/sessions"
```

## Session mount and data safety

The host's `$HOME/.codex/sessions` is bind-mounted at `/home/vscode/.codex/sessions`. The mount is read-write because Codex running inside the container may need to create and append rollouts. Agentop must nevertheless open that tree only for reading and must never write, truncate, rename, move or lock its contents for writing.

Rollouts can contain prompts, messages, reasoning summaries, tool arguments, command output, paths and other sensitive material. Therefore:

- never commit complete real rollouts;
- never copy raw rollout payloads into logs, diagnostics, issues or documentation;
- minimise fixtures to the few records needed for a test;
- remove user content, secrets and unrelated identifiers from fixtures; and
- sanitise and bound all rollout-derived text before terminal display.

## Codex compatibility

A single Agentop run may observe rollouts produced by several Codex versions. Compatibility is selected by each rollout's exact `session_meta.payload.cli_version`, not by the currently installed CLI.

Agentop distinguishes four evidence levels: catalogued, ingestable, semantically covered and live verified. An archived schema does not by itself prove semantic support.

The repository will capture version-specific stable, experimental and internal schema surfaces. The [official Codex App Server documentation](https://developers.openai.com/codex/app-server/#message-schema) states that generated schema artefacts match the Codex version that produced them. Internal rollout schemas and real, sanitised fixtures remain separate evidence sources.

See [Development](docs/DEVELOPMENT.md#codex-updates-and-schema-capture) for the update workflow and the [POC implementation plan](docs/POC_IMPLEMENTATION_PLAN.md#schema-capture-and-compatibility) for the compatibility model.

## Development

Contributor workflow, container behaviour, verification commands and fixture rules are documented in [docs/DEVELOPMENT.md](docs/DEVELOPMENT.md).

## Licence

This repository is proprietary and confidential. See [LICENSE.md](LICENSE.md).
