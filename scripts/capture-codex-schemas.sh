#!/usr/bin/env bash
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
readonly REPO_ROOT
SCHEMAS_ROOT="${REPO_ROOT}/schemas"
readonly SCHEMAS_ROOT
CODEX_COMMAND="${CODEX_BIN:-codex}"
readonly CODEX_COMMAND
codex_identity="${CODEX_COMMAND##*/}"
if [[ ! "${codex_identity}" =~ ^[A-Za-z0-9._+-]+$ ]]; then
    printf 'CODEX_BIN basename is not safe for the manifest: %s\n' "${codex_identity}" >&2
    exit 1
fi
readonly codex_identity

stage_root=""
publish_lock=""

cleanup() {
    if [[ -n "${stage_root}" && -d "${stage_root}" ]]; then
        rm -r -- "${stage_root}"
    fi
    if [[ -n "${publish_lock}" && -d "${publish_lock}" ]]; then
        rmdir -- "${publish_lock}"
    fi
}
trap cleanup EXIT

mkdir -p -- "${SCHEMAS_ROOT}/.staging" "${SCHEMAS_ROOT}/codex"
stage_root="$(mktemp -d "${SCHEMAS_ROOT}/.staging/capture.XXXXXXXX")"
readonly stage_root

before_version_file="${stage_root}/version.before"
after_version_file="${stage_root}/version.after"
"${CODEX_COMMAND}" --version >"${before_version_file}"

cli_version="$(
    awk '
        NR == 1 && $1 == "codex-cli" && NF == 2 { version = $2; next }
        { invalid = 1 }
        END {
            if (!invalid && NR == 1) print version
            else exit 1
        }
    ' "${before_version_file}"
)"
if [[ ! "${cli_version}" =~ ^[0-9]+\.[0-9]+\.[0-9]+([-.][0-9A-Za-z.-]+)?$ ]]; then
    printf 'Unsupported codex version output: %s\n' "$(tr '\n' ' ' <"${before_version_file}")" >&2
    exit 1
fi
readonly cli_version

archive="${stage_root}/archive"
mkdir -p --     "${archive}/app-server/stable"     "${archive}/app-server/experimental"     "${archive}/internal"

"${CODEX_COMMAND}" app-server generate-json-schema     --out "${archive}/app-server/stable"
"${CODEX_COMMAND}" app-server generate-json-schema     --experimental     --out "${archive}/app-server/experimental"
"${CODEX_COMMAND}" app-server generate-internal-json-schema     --out "${archive}/internal"

"${CODEX_COMMAND}" --version >"${after_version_file}"
if ! cmp -s -- "${before_version_file}" "${after_version_file}"; then
    printf 'Codex version changed during schema generation\n' >&2
    exit 1
fi

for surface in app-server/stable app-server/experimental internal; do
    if ! find "${archive}/${surface}" -type f -name '*.json' -print -quit |
        grep -q .; then
        printf 'Codex generator produced no JSON schemas for %s\n' "${surface}" >&2
        exit 1
    fi
done

mapfile -d '' generated_files < <(
    find "${archive}" -type f -print0 | sort -z
)
for generated_file in "${generated_files[@]}"; do
    if [[ "${generated_file}" != *.json ]]; then
        printf 'Unexpected non-JSON generator output: %s\n' "${generated_file#"${archive}/"}" >&2
        exit 1
    fi
done
schema_files=("${generated_files[@]}")
if [[ ! -f "${archive}/internal/RolloutLine.json" ]]; then
    printf 'Internal schema bundle does not contain RolloutLine.json\n' >&2
    exit 1
fi

for schema_file in "${schema_files[@]}"; do
    if ! jq -e '
        type == "object"
        and (."$schema" | type == "string")
        and (
            has("type")
            or has("oneOf")
            or has("anyOf")
            or has("allOf")
            or has("$ref")
        )
    ' "${schema_file}" >/dev/null; then
        printf 'Invalid JSON Schema: %s\n' "${schema_file#"${archive}/"}" >&2
        exit 1
    fi
done

files_json="$(
    for schema_file in "${schema_files[@]}"; do
        relative_path="${schema_file#"${archive}/"}"
        sha256="$(sha256sum -- "${schema_file}" | awk '{print $1}')"
        jq -n --arg path "${relative_path}" --arg sha256 "${sha256}"             '{path: $path, sha256: $sha256}'
    done | jq -s '.'
)"
raw_version_json="$(jq -Rs '.' <"${before_version_file}")"
rollout_line_canonical_sha256="$(
    jq -cS . "${archive}/internal/RolloutLine.json" | sha256sum | awk '{print $1}'
)"
jq -n \
    --argjson raw_version "${raw_version_json}" \
    --arg cli_version "${cli_version}" \
    --arg command "${codex_identity}" \
    --arg rollout_line_canonical_sha256 "${rollout_line_canonical_sha256}" \
    --argjson files "${files_json}" \
    '{
        raw_version: $raw_version,
        cli_version: $cli_version,
        invocations: [
            {command: $command, arguments: ["app-server", "generate-json-schema", "--out", "app-server/stable"]},
            {command: $command, arguments: ["app-server", "generate-json-schema", "--experimental", "--out", "app-server/experimental"]},
            {command: $command, arguments: ["app-server", "generate-internal-json-schema", "--out", "internal"]}
        ],
        rollout_line_canonical_sha256: $rollout_line_canonical_sha256,
        files: $files
    }' >"${archive}/manifest.json"

target="${SCHEMAS_ROOT}/codex/${cli_version}"
publish_lock="${SCHEMAS_ROOT}/codex/.${cli_version}.publish-lock"
if ! mkdir -- "${publish_lock}"; then
    printf 'Another capture is publishing Codex %s\n' "${cli_version}" >&2
    exit 1
fi

if [[ -e "${target}" ]]; then
    if diff -qr -- "${archive}" "${target}" >/dev/null; then
        printf 'Verified identical Codex schema archive: %s\n' "${target}"
        exit 0
    fi
    printf 'Existing Codex schema archive differs: %s\n' "${target}" >&2
    exit 1
fi

mv -T -- "${archive}" "${target}"
printf 'Published Codex schema archive: %s\n' "${target}"
