# Codex RolloutLine schema catalogue

Agentop uses only Codex's self-contained internal `RolloutLine.json` schema for exact-version catalogue provenance. It does not consume or retain stable or experimental app-server schema surfaces.

The catalogue is content-addressed:

```text
codex/rollout-line/
  versions.json
  by-hash/
    <canonical-sha256>/
      RolloutLine.json
```

`versions.json` maps each exact `session_meta.payload.cli_version` to a canonical RolloutLine hash and immutable official-source provenance. Equal hashes share one schema file. There is no nearest-version fallback, and catalogue membership does not itself claim semantic compatibility.

Schemas use deterministic sorted-object JSON with a trailing newline. The directory name is the SHA-256 of those exact canonical bytes. Every `$ref` must remain local to the same schema.

## Refreshing official releases

The Rust binary contains the checked-in mapping as a read-only seed. Fetch and catalogue official Codex tags missing from that seed with:

```bash
agentop build-schema
```

For a maintainer refresh of this checked-in catalogue, run from the repository:

```bash
cargo run -- build-schema --catalogue-dir schemas/codex/rollout-line
```

The command enumerates official `rust-v*` tag refs from `0.149.0-alpha.1` onwards, resolves immutable tag and commit identities, downloads the stable precomputed export at the exact commit, and extracts only `internal_json_schema["RolloutLine.json"]`. It validates and hashes every result, reuses existing families, rejects conflicts, and publishes the complete mapping atomically after all new family files exist.

The default end-user overlay is `$XDG_DATA_HOME/agentop/schemas/codex/rollout-line`, falling back to `$HOME/.local/share/agentop/schemas/codex/rollout-line`. `AGENTOP_SCHEMA_DIR` and `--catalogue-dir` provide an exact override. Optional GitHub authentication comes from `GH_TOKEN` or `GITHUB_TOKEN`; never put a token on the command line.

Normal TUI startup is offline. It merges the external overlay with the embedded seed, accepts new exact versions and identical repeats, and rejects attempts to override a built-in version with another hash.
