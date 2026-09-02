#!/usr/bin/env bash
set -euo pipefail

uid="$(id -u)"
gid="$(id -g)"

for directory in \
    /commandhistory \
    /usr/local/cargo/git \
    /usr/local/cargo/registry \
    "$HOME/.codex"; do
    sudo install -d -o "$uid" -g "$gid" "$directory"
done

history_file=/commandhistory/.bash_history
touch "$history_file"
history_declaration='export HISTFILE=/commandhistory/.bash_history'
if ! grep -Fqx "$history_declaration" "$HOME/.bashrc"; then
    printf '\n%s\n' "$history_declaration" >>"$HOME/.bashrc"
fi

codex_config="$HOME/.codex/config.toml"
if [[ ! -e "$codex_config" ]]; then
    install -m 0600 /dev/stdin "$codex_config" <<'EOF'
# Docker is the outer isolation boundary for this Dev Container.
approval_policy = "never"
sandbox_mode = "danger-full-access"
cli_auth_credentials_store = "file"

[mcp_servers.context7]
command = "npx"
args = ["-y", "@upstash/context7-mcp"]
env_vars = ["CONTEXT7_API_KEY"]

[mcp_servers.tilth]
command = "tilth"
args = ["--mcp", "--edit"]
EOF
fi

host_agents=/workspaces/agentop/.devcontainer/local/AGENTS.md
if [[ -s "$host_agents" ]]; then
    install -m 0644 "$host_agents" "$HOME/.codex/AGENTS.md"
fi

codex mcp list
