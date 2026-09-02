#!/usr/bin/env bash
set -euo pipefail

if (( $# != 0 )); then
    printf 'Usage: %s (no arguments; imports pinned Codex 0.149.0-alpha.4.1)\n' \
        "${0##*/}" >&2
    exit 64
fi
REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
readonly REPO_ROOT
readonly SCHEMAS_ROOT="${REPO_ROOT}/schemas"
readonly CLI_VERSION="0.149.0-alpha.4.1"
readonly TAG="rust-v0.149.0-alpha.4.1"
readonly TAG_OBJECT="e962398af66be22f74cf6c3f196fd7f46a3e89de"
readonly COMMIT="97e7c55e2b64738ec6fe2311ad77a60b106fefae"
readonly REMOTE="https://github.com/openai/codex.git"
readonly RAW_ROOT="https://raw.githubusercontent.com/openai/codex/${COMMIT}"
readonly STABLE_PATH="codex-rs/app-server-protocol/schema/precomputed/app-server-exports-stable.json.zst"
readonly EXPERIMENTAL_PATH="codex-rs/app-server-protocol/schema/precomputed/app-server-exports-experimental.json.zst"
readonly STABLE_SHA256="5b0cb2524f4719764b11fe4a0321ca300a61daf081bbc391a21e2417254dafb3"
readonly EXPERIMENTAL_SHA256="5185430fffaa0d7c254d74d0d528e24c2814ac6d7f25f9b0679d6ad26f84eadc"
readonly ROLLOUT_CANONICAL_SHA256="0401b0f306ec02c52e82d33a0bdd2b3435befaee9feb5573496e31c441822184"

if [[ "${TAG}" != "rust-v${CLI_VERSION}" ]]; then
    printf 'Release tag and catalogue version mismatch: %s != rust-v%s\n' \
        "${TAG}" "${CLI_VERSION}" >&2
    exit 1
fi
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
stage_root="$(mktemp -d "${SCHEMAS_ROOT}/.staging/import.XXXXXXXX")"
readonly stage_root

remote_refs="$(git -c http.followRedirects=false ls-remote "${REMOTE}"     "refs/tags/${TAG}" "refs/tags/${TAG}^{}")"
actual_tag_object="$(awk -v ref="refs/tags/${TAG}" '$2 == ref { print $1 }' <<<"${remote_refs}")"
actual_commit="$(awk -v ref="refs/tags/${TAG}^{}" '$2 == ref { print $1 }' <<<"${remote_refs}")"
if [[ "${actual_tag_object}" != "${TAG_OBJECT}" || "${actual_commit}" != "${COMMIT}" ]]; then
    printf 'Official tag provenance mismatch for %s\n' "${TAG}" >&2
    exit 1
fi

stable_pack="${stage_root}/stable.json.zst"
experimental_pack="${stage_root}/experimental.json.zst"
curl --fail --silent --show-error --proto '=https' --tlsv1.2 --max-redirs 0     "${RAW_ROOT}/${STABLE_PATH}" --output "${stable_pack}"
curl --fail --silent --show-error --proto '=https' --tlsv1.2 --max-redirs 0     "${RAW_ROOT}/${EXPERIMENTAL_PATH}" --output "${experimental_pack}"
printf '%s  %s\n' "${STABLE_SHA256}" "${stable_pack}" | sha256sum --check --status
printf '%s  %s\n' "${EXPERIMENTAL_SHA256}" "${experimental_pack}" | sha256sum --check --status

stable_export="${stage_root}/stable.json"
experimental_export="${stage_root}/experimental.json"
node -e 'const fs=require("node:fs");const z=require("node:zlib");fs.createReadStream(process.argv[1]).pipe(z.createZstdDecompress()).pipe(process.stdout)'     "${stable_pack}" >"${stable_export}"
node -e 'const fs=require("node:fs");const z=require("node:zlib");fs.createReadStream(process.argv[1]).pipe(z.createZstdDecompress()).pipe(process.stdout)'     "${experimental_pack}" >"${experimental_export}"

for export_file in "${stable_export}" "${experimental_export}"; do
    jq -e '
        type == "object"
        and (.json_schema | type == "object")
        and (.internal_json_schema | type == "object")
        and ([.json_schema[], .internal_json_schema[]] | all(type == "string"))
    ' "${export_file}" >/dev/null
done
jq -e '.internal_json_schema | keys == ["RolloutLine.json"]' "${stable_export}" >/dev/null
jq -e '.internal_json_schema | length == 0' "${experimental_export}" >/dev/null

archive="${stage_root}/archive"
mkdir -p -- "${archive}/app-server/stable" "${archive}/app-server/experimental" "${archive}/internal"

extract_map() {
    local export_file="$1"
    local field="$2"
    local destination="$3"
    local require_nonempty="$4"
    local count=0
    local encoded entry_json filename

    while IFS= read -r encoded; do
        entry_json="$(printf '%s' "${encoded}" | base64 --decode)"
        filename="$(jq -r '.[0]' <<<"${entry_json}")"
        if [[ ! "${filename}" =~ ^([A-Za-z0-9][A-Za-z0-9._-]*/)*[A-Za-z0-9][A-Za-z0-9._-]*[.]json$ ]]; then
            printf 'Unsafe schema filename in release export: %s\n' "${filename}" >&2
            exit 1
        fi
        mkdir -p -- "${destination}/$(dirname "${filename}")"
        jq -rj '.[1]' <<<"${entry_json}" >"${destination}/${filename}"
        ((count += 1))
    done < <(jq -r --arg field "${field}" '.[$field] | to_entries[] | [.key, .value] | @base64' "${export_file}")

    if [[ "${require_nonempty}" == true && "${count}" -eq 0 ]]; then
        printf 'Release export contained no schemas for %s\n' "${field}" >&2
        exit 1
    fi
}

extract_map "${stable_export}" json_schema "${archive}/app-server/stable" true
extract_map "${experimental_export}" json_schema "${archive}/app-server/experimental" true
extract_map "${stable_export}" internal_json_schema "${archive}/internal" true

mapfile -d '' schema_files < <(find "${archive}" -type f -print0 | sort -z)
for schema_file in "${schema_files[@]}"; do
    if [[ "${schema_file}" != *.json ]]; then
        printf 'Unexpected non-JSON extracted output: %s\n' "${schema_file#"${archive}/"}" >&2
        exit 1
    fi
    if ! jq -e '
        type == "object"
        and (."$schema" | type == "string")
        and (has("type") or has("oneOf") or has("anyOf") or has("allOf") or has("$ref"))
    ' "${schema_file}" >/dev/null; then
        printf 'Invalid JSON Schema: %s\n' "${schema_file#"${archive}/"}" >&2
        exit 1
    fi
done

actual_rollout_canonical_sha256="$(
    jq -cS . "${archive}/internal/RolloutLine.json" | sha256sum | awk '{print $1}'
)"
if [[ "${actual_rollout_canonical_sha256}" != "${ROLLOUT_CANONICAL_SHA256}" ]]; then
    printf 'Canonical RolloutLine schema hash mismatch\n' >&2
    exit 1
fi

files_json="$(
    for schema_file in "${schema_files[@]}"; do
        relative_path="${schema_file#"${archive}/"}"
        sha256="$(sha256sum -- "${schema_file}" | awk '{print $1}')"
        jq -n --arg path "${relative_path}" --arg sha256 "${sha256}"             '{path: $path, sha256: $sha256}'
    done | jq -s '.'
)"
sources_json="$(
    jq -n         --arg stable_path "${STABLE_PATH}"         --arg stable_sha256 "${STABLE_SHA256}"         --arg experimental_path "${EXPERIMENTAL_PATH}"         --arg experimental_sha256 "${EXPERIMENTAL_SHA256}"         '[
            {path: $stable_path, sha256: $stable_sha256},
            {path: $experimental_path, sha256: $experimental_sha256}
        ]'
)"
jq -n     --arg cli_version "${CLI_VERSION}"     --arg repository "${REMOTE}"     --arg tag "${TAG}"     --arg tag_object "${TAG_OBJECT}"     --arg commit "${COMMIT}"     --arg rollout_line_canonical_sha256 "${ROLLOUT_CANONICAL_SHA256}"     --argjson sources "${sources_json}"     --argjson files "${files_json}"     '{
        cli_version: $cli_version,
        provenance: {
            kind: "official-release-export",
            repository: $repository,
            tag: $tag,
            tag_object: $tag_object,
            commit: $commit,
            sources: $sources
        },
        rollout_line_canonical_sha256: $rollout_line_canonical_sha256,
        files: $files
    }' >"${archive}/manifest.json"

target="${SCHEMAS_ROOT}/codex/${CLI_VERSION}"
publish_lock="${SCHEMAS_ROOT}/codex/.${CLI_VERSION}.publish-lock"
if ! mkdir -- "${publish_lock}"; then
    printf 'Another import is publishing Codex %s\n' "${CLI_VERSION}" >&2
    exit 1
fi
if [[ -e "${target}" ]]; then
    if diff -qr -- "${archive}" "${target}" >/dev/null; then
        printf 'Verified identical Codex release schema archive: %s\n' "${target}"
        exit 0
    fi
    printf 'Existing Codex release schema archive differs: %s\n' "${target}" >&2
    exit 1
fi

mv -T -- "${archive}" "${target}"
printf 'Published Codex release schema archive: %s\n' "${target}"
