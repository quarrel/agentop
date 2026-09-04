use anyhow::{bail, Context, Result};
use semver::Version;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

pub const CATALOGUE_FORMAT_VERSION: u32 = 1;
pub const CANONICALISATION: &str = "sorted-json-with-trailing-newline-v1";
pub const CODEX_REPOSITORY: &str = "https://github.com/openai/codex";
pub const STABLE_EXPORT_PATH: &str =
    "codex-rs/app-server-protocol/schema/precomputed/app-server-exports-stable.json.zst";

const BUILT_IN_CATALOGUE_JSON: &str = include_str!("../schemas/codex/rollout-line/versions.json");

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SchemaStatus {
    Catalogued {
        rollout_line_canonical_sha256: String,
    },
    Missing,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Catalogue {
    pub format_version: u32,
    pub canonicalisation: String,
    pub versions: BTreeMap<String, VersionEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct VersionEntry {
    pub rollout_line_sha256: String,
    pub provenance: Provenance,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Provenance {
    pub kind: String,
    pub repository: String,
    pub tag: String,
    pub tag_object: String,
    pub commit: String,
    pub source: Source,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Source {
    pub path: String,
    pub sha256: String,
}

impl Catalogue {
    pub fn empty() -> Self {
        Self {
            format_version: CATALOGUE_FORMAT_VERSION,
            canonicalisation: CANONICALISATION.into(),
            versions: BTreeMap::new(),
        }
    }
}

fn valid_hex(value: &str, lengths: &[usize]) -> bool {
    lengths.contains(&value.len())
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

pub fn validate_catalogue(catalogue: &Catalogue, label: &str) -> Result<()> {
    anyhow::ensure!(
        catalogue.format_version == CATALOGUE_FORMAT_VERSION,
        "{label} has unsupported format_version {}",
        catalogue.format_version
    );
    anyhow::ensure!(
        catalogue.canonicalisation == CANONICALISATION,
        "{label} has unsupported canonicalisation {:?}",
        catalogue.canonicalisation
    );
    for (version, entry) in &catalogue.versions {
        Version::parse(version)
            .with_context(|| format!("{label} contains invalid version {version:?}"))?;
        anyhow::ensure!(
            valid_hex(&entry.rollout_line_sha256, &[64]),
            "{label} contains invalid RolloutLine hash for {version}"
        );
        anyhow::ensure!(
            entry.provenance.kind == "official-release-export",
            "{label} contains unsupported provenance kind for {version}"
        );
        anyhow::ensure!(
            entry.provenance.repository == CODEX_REPOSITORY,
            "{label} contains unexpected repository for {version}"
        );
        anyhow::ensure!(
            entry.provenance.tag == format!("rust-v{version}"),
            "{label} contains mismatched tag for {version}"
        );
        anyhow::ensure!(
            valid_hex(&entry.provenance.tag_object, &[40, 64]),
            "{label} contains invalid tag object for {version}"
        );
        anyhow::ensure!(
            valid_hex(&entry.provenance.commit, &[40, 64]),
            "{label} contains invalid commit for {version}"
        );
        anyhow::ensure!(
            entry.provenance.source.path == STABLE_EXPORT_PATH,
            "{label} contains unexpected source path for {version}"
        );
        anyhow::ensure!(
            valid_hex(&entry.provenance.source.sha256, &[64]),
            "{label} contains invalid source hash for {version}"
        );
    }
    Ok(())
}

pub fn parse_catalogue(bytes: &[u8], label: &str) -> Result<Catalogue> {
    let catalogue: Catalogue =
        serde_json::from_slice(bytes).with_context(|| format!("parse {label}"))?;
    validate_catalogue(&catalogue, label)?;
    Ok(catalogue)
}

pub fn serialise_catalogue(catalogue: &Catalogue) -> Result<Vec<u8>> {
    validate_catalogue(catalogue, "schema catalogue")?;
    let mut bytes = serde_json::to_vec_pretty(catalogue).context("serialise schema catalogue")?;
    bytes.push(b'\n');
    Ok(bytes)
}

fn built_in_catalogue() -> &'static Catalogue {
    static CATALOGUE: OnceLock<Catalogue> = OnceLock::new();
    CATALOGUE.get_or_init(|| {
        parse_catalogue(
            BUILT_IN_CATALOGUE_JSON.as_bytes(),
            "built-in schema catalogue",
        )
        .expect("built-in schema catalogue must be valid")
    })
}

pub fn load_catalogue(path: &Path) -> Result<Catalogue> {
    if !path.exists() {
        return Ok(Catalogue::empty());
    }
    let bytes =
        std::fs::read(path).with_context(|| format!("read schema catalogue {}", path.display()))?;
    parse_catalogue(&bytes, &format!("schema catalogue {}", path.display()))
}

pub fn default_catalogue_dir() -> Result<PathBuf> {
    if let Some(path) = std::env::var_os("AGENTOP_SCHEMA_DIR") {
        anyhow::ensure!(!path.is_empty(), "AGENTOP_SCHEMA_DIR is empty");
        return Ok(PathBuf::from(path));
    }
    if let Some(path) = std::env::var_os("XDG_DATA_HOME") {
        let path = PathBuf::from(path);
        anyhow::ensure!(path.is_absolute(), "XDG_DATA_HOME must be absolute");
        return Ok(path.join("agentop/schemas/codex/rollout-line"));
    }
    let home = std::env::var_os("HOME").context("HOME is not set")?;
    Ok(PathBuf::from(home).join(".local/share/agentop/schemas/codex/rollout-line"))
}

pub fn schema_path(catalogue_dir: &Path, hash: &str) -> PathBuf {
    catalogue_dir
        .join("by-hash")
        .join(hash)
        .join("RolloutLine.json")
}

pub fn lookup(catalogue_dir: &Path, cli_version: &str) -> Result<SchemaStatus> {
    if Version::parse(cli_version).is_err() {
        return Ok(SchemaStatus::Missing);
    }
    let built_in = built_in_catalogue().versions.get(cli_version);
    let external_path = catalogue_dir.join("versions.json");
    let external_catalogue = load_catalogue(&external_path)?;
    let external = external_catalogue.versions.get(cli_version);

    if let (Some(built_in), Some(external)) = (built_in, external) {
        anyhow::ensure!(
            built_in == external,
            "external schema mapping conflicts with the built-in mapping for {cli_version}"
        );
    }

    let Some(entry) = external.or(built_in) else {
        return Ok(SchemaStatus::Missing);
    };
    if external.is_some() && built_in.is_none() {
        let path = schema_path(catalogue_dir, &entry.rollout_line_sha256);
        anyhow::ensure!(
            path.is_file(),
            "schema catalogue missing {}",
            path.display()
        );
        verify_schema_file(&path, &entry.rollout_line_sha256)?;
    }
    Ok(SchemaStatus::Catalogued {
        rollout_line_canonical_sha256: entry.rollout_line_sha256.clone(),
    })
}

pub fn canonical_schema_bytes(value: &Value) -> Result<Vec<u8>> {
    validate_rollout_schema(value)?;
    let mut bytes = serde_json::to_vec(value).context("serialise canonical RolloutLine schema")?;
    bytes.push(b'\n');
    Ok(bytes)
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut hash = String::with_capacity(digest.len() * 2);
    for byte in digest {
        write!(&mut hash, "{byte:02x}").expect("writing to a String cannot fail");
    }
    hash
}

pub fn verify_schema_file(path: &Path, expected_hash: &str) -> Result<()> {
    let bytes = std::fs::read(path)
        .with_context(|| format!("read RolloutLine schema {}", path.display()))?;
    anyhow::ensure!(
        sha256_hex(&bytes) == expected_hash,
        "RolloutLine schema hash mismatch at {}",
        path.display()
    );
    let value: Value = serde_json::from_slice(&bytes)
        .with_context(|| format!("parse RolloutLine schema {}", path.display()))?;
    validate_rollout_schema(&value)
        .with_context(|| format!("validate RolloutLine schema {}", path.display()))
}

pub fn validate_rollout_schema(value: &Value) -> Result<()> {
    let object = value
        .as_object()
        .context("RolloutLine schema must be a JSON object")?;
    anyhow::ensure!(
        object.get("$schema").and_then(Value::as_str).is_some(),
        "RolloutLine schema is missing $schema"
    );
    anyhow::ensure!(
        ["type", "oneOf", "anyOf", "allOf", "$ref"]
            .iter()
            .any(|key| object.contains_key(*key)),
        "RolloutLine schema has no root type or composition"
    );

    fn validate_refs(value: &Value) -> Result<()> {
        match value {
            Value::Object(object) => {
                if let Some(reference) = object.get("$ref") {
                    let reference = reference
                        .as_str()
                        .context("RolloutLine $ref must be a string")?;
                    anyhow::ensure!(
                        reference.starts_with("#/"),
                        "RolloutLine schema contains non-local $ref {reference:?}"
                    );
                }
                for value in object.values() {
                    validate_refs(value)?;
                }
            }
            Value::Array(values) => {
                for value in values {
                    validate_refs(value)?;
                }
            }
            _ => {}
        }
        Ok(())
    }

    validate_refs(value)
}

pub fn merge_catalogues(built_in: &Catalogue, external: &Catalogue) -> Result<Catalogue> {
    let mut merged = built_in.clone();
    for (version, entry) in &external.versions {
        if let Some(existing) = merged.versions.get(version) {
            anyhow::ensure!(
                existing == entry,
                "external schema mapping conflicts with the built-in mapping for {version}"
            );
        } else {
            merged.versions.insert(version.clone(), entry.clone());
        }
    }
    Ok(merged)
}

pub fn built_in_snapshot() -> Catalogue {
    built_in_catalogue().clone()
}

pub fn ensure_tag_matches(version: &str, entry: &VersionEntry, tag_object: &str) -> Result<()> {
    if entry.provenance.tag_object != tag_object {
        bail!("official tag object changed for rust-v{version}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(version: &str, hash: &str) -> VersionEntry {
        VersionEntry {
            rollout_line_sha256: hash.into(),
            provenance: Provenance {
                kind: "official-release-export".into(),
                repository: CODEX_REPOSITORY.into(),
                tag: format!("rust-v{version}"),
                tag_object: "1".repeat(40),
                commit: "2".repeat(40),
                source: Source {
                    path: STABLE_EXPORT_PATH.into(),
                    sha256: "3".repeat(64),
                },
            },
        }
    }

    #[test]
    fn catalogue_lookup_is_exact_content_addressed_and_traversal_safe() {
        let tmp = tempfile::tempdir().unwrap();
        let catalogue_dir = tmp.path().join("catalogue");
        std::fs::create_dir_all(&catalogue_dir).unwrap();
        let bytes = b"{\"$schema\":\"x\",\"type\":\"object\"}\n";
        let hash = sha256_hex(bytes);
        let version = "9.9.9";
        let mut catalogue = Catalogue::empty();
        catalogue
            .versions
            .insert(version.into(), entry(version, &hash));
        std::fs::write(
            catalogue_dir.join("versions.json"),
            serialise_catalogue(&catalogue).unwrap(),
        )
        .unwrap();

        assert!(lookup(&catalogue_dir, version).is_err());
        let path = schema_path(&catalogue_dir, &hash);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, bytes).unwrap();
        assert_eq!(
            lookup(&catalogue_dir, version).unwrap(),
            SchemaStatus::Catalogued {
                rollout_line_canonical_sha256: hash,
            }
        );
        assert_eq!(
            lookup(&catalogue_dir, "9.9.8").unwrap(),
            SchemaStatus::Missing
        );
        for hostile in ["", ".", "..", "../9.9.9", "/tmp", "a/b", r"a\b"] {
            assert_eq!(
                lookup(&catalogue_dir, hostile).unwrap(),
                SchemaStatus::Missing
            );
        }
    }

    #[test]
    fn conflicting_overlay_is_rejected() {
        let Some((version, built_in)) = built_in_catalogue().versions.iter().next() else {
            return;
        };
        let tmp = tempfile::tempdir().unwrap();
        let mut external = Catalogue::empty();
        let mut conflicting = built_in.clone();
        conflicting.rollout_line_sha256 = "f".repeat(64);
        external.versions.insert(version.clone(), conflicting);
        std::fs::write(
            tmp.path().join("versions.json"),
            serialise_catalogue(&external).unwrap(),
        )
        .unwrap();
        assert!(lookup(tmp.path(), version).is_err());
    }
    #[test]
    fn embedded_seed_catalogues_known_versions_without_overlay_files() {
        let tmp = tempfile::tempdir().unwrap();
        for (version, expected_hash) in [
            (
                "0.149.0-alpha.4.1",
                "0401b0f306ec02c52e82d33a0bdd2b3435befaee9feb5573496e31c441822184",
            ),
            (
                "0.152.1",
                "301197629ee7040be4f8361b503977761eee7c56a5a06788df16d3d8e8a0e5d4",
            ),
            (
                "0.153.0",
                "edfb1d10b777cdc144f170317a3b8b89943e6be589a4a1664657b7dfecb19305",
            ),
        ] {
            assert_eq!(
                lookup(tmp.path(), version).unwrap(),
                SchemaStatus::Catalogued {
                    rollout_line_canonical_sha256: expected_hash.into(),
                }
            );
        }
    }
    #[test]
    fn sha256_hex_is_lowercase_and_zero_padded() {
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
}
