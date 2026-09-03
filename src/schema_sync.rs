use crate::schema::{
    self, ensure_tag_matches, merge_catalogues, Catalogue, Provenance, Source, VersionEntry,
    CODEX_REPOSITORY, STABLE_EXPORT_PATH,
};
use anyhow::{bail, Context, Result};
use fs2::FileExt;
use reqwest::blocking::Client;
use semver::Version;
use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::Path;
use std::time::Duration;

const API_ROOT: &str = "https://api.github.com/repos/openai/codex";
const RAW_ROOT: &str = "https://raw.githubusercontent.com/openai/codex";
const MINIMUM_VERSION: &str = "0.149.0-alpha.1";
const MAX_PACK_BYTES: u64 = 16 * 1024 * 1024;
const MAX_EXPORT_BYTES: u64 = 128 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportedVersion {
    pub version: String,
    pub rollout_line_sha256: String,
    pub new_family: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncReport {
    pub official_tags: usize,
    pub imported: Vec<ImportedVersion>,
}

#[derive(Debug, Clone, Deserialize)]
struct ApiObject {
    sha: String,
    #[serde(rename = "type")]
    kind: String,
    url: String,
}

#[derive(Debug, Clone, Deserialize)]
struct ApiRef {
    #[serde(rename = "ref")]
    reference: String,
    object: ApiObject,
}

#[derive(Debug, Deserialize)]
struct ApiTag {
    tag: String,
    object: ApiObject,
}

#[derive(Debug, Deserialize)]
struct ReleaseExport {
    internal_json_schema: BTreeMap<String, String>,
}

#[derive(Debug, Clone)]
struct RemoteTag {
    version: Version,
    version_text: String,
    tag: String,
    tag_object: String,
    object_kind: String,
}

#[derive(Debug, Clone)]
struct FetchedSchema {
    version: String,
    entry: VersionEntry,
    canonical_schema: Vec<u8>,
}

trait Remote {
    fn list_tags(&self) -> Result<Vec<RemoteTag>>;
    fn fetch_schema(&self, tag: &RemoteTag) -> Result<FetchedSchema>;
}

struct GitHubRemote {
    client: Client,
    token: Option<String>,
}

impl GitHubRemote {
    fn new() -> Result<Self> {
        let client = Client::builder()
            .user_agent(concat!("agentop/", env!("CARGO_PKG_VERSION")))
            .connect_timeout(Duration::from_secs(15))
            .timeout(Duration::from_secs(60))
            .redirect(reqwest::redirect::Policy::limited(3))
            .build()
            .context("build GitHub HTTP client")?;
        let token = std::env::var("GH_TOKEN")
            .or_else(|_| std::env::var("GITHUB_TOKEN"))
            .ok()
            .filter(|value| !value.is_empty());
        Ok(Self { client, token })
    }

    fn get(&self, url: &str) -> reqwest::blocking::RequestBuilder {
        let request = self
            .client
            .get(url)
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28");
        if let Some(token) = &self.token {
            request.bearer_auth(token)
        } else {
            request
        }
    }

    fn get_json<T: DeserializeOwned>(&self, url: &str) -> Result<T> {
        self.get(url)
            .send()
            .with_context(|| format!("request {url}"))?
            .error_for_status()
            .with_context(|| format!("GitHub rejected {url}"))?
            .json()
            .with_context(|| format!("decode response from {url}"))
    }

    fn resolve_commit(&self, tag: &RemoteTag) -> Result<String> {
        let mut object_sha = tag.tag_object.clone();
        let mut object_kind = tag.object_kind.clone();
        for depth in 0..4 {
            match object_kind.as_str() {
                "commit" => return Ok(object_sha),
                "tag" => {
                    let url = format!("{API_ROOT}/git/tags/{object_sha}");
                    let annotated: ApiTag = self.get_json(&url)?;
                    if depth == 0 {
                        anyhow::ensure!(
                            annotated.tag == tag.tag,
                            "annotated tag name mismatch for {}",
                            tag.tag
                        );
                    }
                    object_sha = annotated.object.sha;
                    object_kind = annotated.object.kind;
                }
                other => bail!("unsupported Git object type {other:?} for {}", tag.tag),
            }
        }
        bail!("tag indirection is too deep for {}", tag.tag)
    }

    fn download(&self, url: &str, limit: u64) -> Result<Vec<u8>> {
        let response = self
            .client
            .get(url)
            .send()
            .with_context(|| format!("download {url}"))?
            .error_for_status()
            .with_context(|| format!("download rejected for {url}"))?;
        read_bounded(response, limit).with_context(|| format!("read {url}"))
    }
}

impl Remote for GitHubRemote {
    fn list_tags(&self) -> Result<Vec<RemoteTag>> {
        let url = format!("{API_ROOT}/git/matching-refs/tags/rust-v");
        let refs: Vec<ApiRef> = self.get_json(&url)?;
        let minimum = Version::parse(MINIMUM_VERSION).expect("minimum version is valid");
        let mut tags = Vec::new();
        for reference in refs {
            let Some(version_text) = reference.reference.strip_prefix("refs/tags/rust-v") else {
                continue;
            };
            let Ok(version) = Version::parse(version_text) else {
                continue;
            };
            if version < minimum {
                continue;
            }
            anyhow::ensure!(
                !reference.object.url.is_empty(),
                "GitHub returned an empty object URL for rust-v{version_text}"
            );
            tags.push(RemoteTag {
                version,
                version_text: version_text.into(),
                tag: format!("rust-v{version_text}"),
                tag_object: reference.object.sha,
                object_kind: reference.object.kind,
            });
        }
        tags.sort_by(|left, right| left.version.cmp(&right.version));
        for pair in tags.windows(2) {
            anyhow::ensure!(
                pair[0].version != pair[1].version,
                "GitHub returned duplicate tag version {}",
                pair[0].version
            );
        }
        Ok(tags)
    }

    fn fetch_schema(&self, tag: &RemoteTag) -> Result<FetchedSchema> {
        let commit = self.resolve_commit(tag)?;
        let url = format!("{RAW_ROOT}/{commit}/{STABLE_EXPORT_PATH}");
        let pack = self.download(&url, MAX_PACK_BYTES)?;
        let source_sha256 = schema::sha256_hex(&pack);
        let schema_value = extract_rollout_schema(&pack)
            .with_context(|| format!("extract RolloutLine schema for {}", tag.tag))?;
        let canonical_schema = schema::canonical_schema_bytes(&schema_value)?;
        let rollout_line_sha256 = schema::sha256_hex(&canonical_schema);
        Ok(FetchedSchema {
            version: tag.version_text.clone(),
            entry: VersionEntry {
                rollout_line_sha256,
                provenance: Provenance {
                    kind: "official-release-export".into(),
                    repository: CODEX_REPOSITORY.into(),
                    tag: tag.tag.clone(),
                    tag_object: tag.tag_object.clone(),
                    commit,
                    source: Source {
                        path: STABLE_EXPORT_PATH.into(),
                        sha256: source_sha256,
                    },
                },
            },
            canonical_schema,
        })
    }
}

fn read_bounded(mut reader: impl Read, limit: u64) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    reader
        .by_ref()
        .take(limit + 1)
        .read_to_end(&mut bytes)
        .context("read bounded data")?;
    anyhow::ensure!(
        bytes.len() as u64 <= limit,
        "data exceeds {limit} byte limit"
    );
    Ok(bytes)
}

fn extract_rollout_schema(pack: &[u8]) -> Result<Value> {
    let decoder = zstd::stream::read::Decoder::new(pack).context("open Zstandard export")?;
    let export_bytes = read_bounded(decoder, MAX_EXPORT_BYTES)?;
    let export: ReleaseExport =
        serde_json::from_slice(&export_bytes).context("parse stable schema export")?;
    let schema = export
        .internal_json_schema
        .get("RolloutLine.json")
        .context("stable export is missing internal RolloutLine.json")?;
    serde_json::from_str(schema).context("parse embedded RolloutLine.json")
}

struct CatalogueLock {
    file: File,
}

impl CatalogueLock {
    fn acquire(catalogue_dir: &Path) -> Result<Self> {
        fs::create_dir_all(catalogue_dir)
            .with_context(|| format!("create catalogue directory {}", catalogue_dir.display()))?;
        let path = catalogue_dir.join(".build-schema.lock");
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&path)
            .with_context(|| format!("open schema catalogue lock {}", path.display()))?;
        file.try_lock_exclusive().with_context(|| {
            format!(
                "acquire schema catalogue lock {} (another update may be running)",
                path.display()
            )
        })?;
        Ok(Self { file })
    }
}

impl Drop for CatalogueLock {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.file);
    }
}

fn publish(
    catalogue_dir: &Path,
    catalogue: &Catalogue,
    fetched: &[FetchedSchema],
) -> Result<BTreeSet<String>> {
    let staging = tempfile::Builder::new()
        .prefix(".build-schema.")
        .tempdir_in(catalogue_dir)
        .with_context(|| format!("create staging directory in {}", catalogue_dir.display()))?;
    let mut unique = BTreeMap::<String, &[u8]>::new();
    for fetched in fetched {
        match unique.get(&fetched.entry.rollout_line_sha256) {
            Some(existing) => anyhow::ensure!(
                *existing == fetched.canonical_schema.as_slice(),
                "one RolloutLine hash resolved to different content"
            ),
            None => {
                unique.insert(
                    fetched.entry.rollout_line_sha256.clone(),
                    &fetched.canonical_schema,
                );
            }
        }
    }

    let mut new_families = BTreeSet::new();
    for (hash, bytes) in &unique {
        let destination = schema::schema_path(catalogue_dir, hash);
        if destination.exists() {
            schema::verify_schema_file(&destination, hash)?;
            anyhow::ensure!(
                fs::read(&destination)? == *bytes,
                "existing RolloutLine schema differs at {}",
                destination.display()
            );
            continue;
        }
        let staged = staging.path().join("by-hash").join(hash);
        fs::create_dir_all(&staged)?;
        let staged_file = staged.join("RolloutLine.json");
        fs::write(&staged_file, bytes)?;
        schema::verify_schema_file(&staged_file, hash)?;
        new_families.insert(hash.clone());
    }

    let catalogue_bytes = schema::serialise_catalogue(catalogue)?;
    let mut staged_catalogue = tempfile::NamedTempFile::new_in(catalogue_dir)
        .with_context(|| format!("stage catalogue in {}", catalogue_dir.display()))?;
    staged_catalogue.write_all(&catalogue_bytes)?;
    staged_catalogue.as_file().sync_all()?;

    fs::create_dir_all(catalogue_dir.join("by-hash"))?;
    for hash in &new_families {
        let staged = staging.path().join("by-hash").join(hash);
        let destination = catalogue_dir.join("by-hash").join(hash);
        fs::rename(&staged, &destination).with_context(|| {
            format!(
                "publish RolloutLine family {} to {}",
                hash,
                destination.display()
            )
        })?;
    }
    staged_catalogue
        .persist(catalogue_dir.join("versions.json"))
        .map_err(|error| error.error)
        .context("publish schema version mapping")?;
    Ok(new_families)
}

fn sync_with_remote(catalogue_dir: &Path, remote: &impl Remote) -> Result<SyncReport> {
    let _lock = CatalogueLock::acquire(catalogue_dir)?;
    let external_path = catalogue_dir.join("versions.json");
    let mut external = schema::load_catalogue(&external_path)?;
    let built_in = schema::built_in_snapshot();
    let merged = merge_catalogues(&built_in, &external)?;
    let tags = remote.list_tags()?;

    let mut missing = Vec::new();
    for tag in &tags {
        if let Some(entry) = merged.versions.get(&tag.version_text) {
            ensure_tag_matches(&tag.version_text, entry, &tag.tag_object)?;
        } else {
            missing.push(tag.clone());
        }
    }

    let mut fetched = Vec::new();
    for tag in &missing {
        let item = remote.fetch_schema(tag)?;
        anyhow::ensure!(
            item.version == tag.version_text,
            "remote returned the wrong version for {}",
            tag.tag
        );
        fetched.push(item);
    }

    for item in &fetched {
        anyhow::ensure!(
            external
                .versions
                .insert(item.version.clone(), item.entry.clone())
                .is_none(),
            "schema version {} was inserted twice",
            item.version
        );
    }
    let new_families = if fetched.is_empty() {
        BTreeSet::new()
    } else {
        publish(catalogue_dir, &external, &fetched)?
    };

    let mut reported_families = BTreeSet::new();
    let imported = fetched
        .into_iter()
        .map(|item| {
            let hash = item.entry.rollout_line_sha256;
            let new_family = new_families.contains(&hash) && reported_families.insert(hash.clone());
            ImportedVersion {
                version: item.version,
                rollout_line_sha256: hash,
                new_family,
            }
        })
        .collect();

    Ok(SyncReport {
        official_tags: tags.len(),
        imported,
    })
}

pub fn build_schema(catalogue_dir: &Path) -> Result<SyncReport> {
    let remote = GitHubRemote::new()?;
    sync_with_remote(catalogue_dir, &remote)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Clone)]
    struct FakeRemote {
        tags: Vec<RemoteTag>,
        fetched: BTreeMap<String, FetchedSchema>,
        fail_version: Option<String>,
    }

    impl Remote for FakeRemote {
        fn list_tags(&self) -> Result<Vec<RemoteTag>> {
            Ok(self.tags.clone())
        }

        fn fetch_schema(&self, tag: &RemoteTag) -> Result<FetchedSchema> {
            if self.fail_version.as_deref() == Some(&tag.version_text) {
                bail!("injected fetch failure");
            }
            self.fetched
                .get(&tag.version_text)
                .cloned()
                .context("missing fake schema")
        }
    }

    fn tag(version: &str, marker: char) -> RemoteTag {
        RemoteTag {
            version: Version::parse(version).unwrap(),
            version_text: version.into(),
            tag: format!("rust-v{version}"),
            tag_object: marker.to_string().repeat(40),
            object_kind: "tag".into(),
        }
    }

    fn fetched(version: &str, tag_object: char, schema_value: Value) -> FetchedSchema {
        let canonical_schema = schema::canonical_schema_bytes(&schema_value).unwrap();
        FetchedSchema {
            version: version.into(),
            entry: VersionEntry {
                rollout_line_sha256: schema::sha256_hex(&canonical_schema),
                provenance: Provenance {
                    kind: "official-release-export".into(),
                    repository: CODEX_REPOSITORY.into(),
                    tag: format!("rust-v{version}"),
                    tag_object: tag_object.to_string().repeat(40),
                    commit: "c".repeat(40),
                    source: Source {
                        path: STABLE_EXPORT_PATH.into(),
                        sha256: "d".repeat(64),
                    },
                },
            },
            canonical_schema,
        }
    }

    #[test]
    fn github_api_models_tolerate_additive_fields() {
        let refs: Vec<ApiRef> = serde_json::from_value(serde_json::json!([{
            "ref": "refs/tags/rust-v0.153.0",
            "node_id": "ignored",
            "url": "https://api.github.test/ref",
            "object": {
                "sha": "a".repeat(40),
                "type": "tag",
                "url": "https://api.github.test/tag",
                "extra": true
            }
        }]))
        .unwrap();
        assert_eq!(refs[0].reference, "refs/tags/rust-v0.153.0");
        assert_eq!(refs[0].object.kind, "tag");
    }

    #[test]
    fn extract_release_selects_rollout_schema_and_ignores_other_surfaces() {
        let rollout =
            r#"{"$schema":"https://json-schema.org/draft/2020-12/schema","type":"object"}"#;
        let export = serde_json::json!({
            "json_schema": {"Ignored.json": "{}"},
            "internal_json_schema": {
                "FutureInternal.json": "{}",
                "RolloutLine.json": rollout
            }
        });
        let pack =
            zstd::stream::encode_all(serde_json::to_vec(&export).unwrap().as_slice(), 1).unwrap();
        let extracted = extract_rollout_schema(&pack).unwrap();
        assert_eq!(
            extracted.get("type").and_then(Value::as_str),
            Some("object")
        );
    }

    #[test]
    fn sync_deduplicates_families_and_is_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let schema_value = serde_json::json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "type": "object"
        });
        let first = fetched("9.8.0", '8', schema_value.clone());
        let second = fetched("9.9.0", '9', schema_value);
        let remote = FakeRemote {
            tags: vec![tag("9.8.0", '8'), tag("9.9.0", '9')],
            fetched: BTreeMap::from([
                ("9.8.0".into(), first.clone()),
                ("9.9.0".into(), second.clone()),
            ]),
            fail_version: None,
        };

        let report = sync_with_remote(tmp.path(), &remote).unwrap();
        assert_eq!(report.imported.len(), 2);
        assert_eq!(
            report
                .imported
                .iter()
                .filter(|item| item.new_family)
                .count(),
            1
        );
        assert!(schema::schema_path(tmp.path(), &first.entry.rollout_line_sha256).is_file());

        let report = sync_with_remote(tmp.path(), &remote).unwrap();
        assert!(report.imported.is_empty());
    }

    #[test]
    fn failed_fetch_does_not_publish_mapping() {
        let tmp = tempfile::tempdir().unwrap();
        let remote = FakeRemote {
            tags: vec![tag("9.9.0", '9')],
            fetched: BTreeMap::new(),
            fail_version: Some("9.9.0".into()),
        };
        assert!(sync_with_remote(tmp.path(), &remote).is_err());
        assert!(!tmp.path().join("versions.json").exists());
    }

    #[test]
    fn canonical_hash_matches_catalogued_identity() {
        let value: Value = serde_json::from_str(include_str!(
            "../schemas/codex/rollout-line/by-hash/301197629ee7040be4f8361b503977761eee7c56a5a06788df16d3d8e8a0e5d4/RolloutLine.json"
        ))
        .unwrap();
        let hash = schema::sha256_hex(&schema::canonical_schema_bytes(&value).unwrap());
        assert_eq!(
            hash,
            "301197629ee7040be4f8361b503977761eee7c56a5a06788df16d3d8e8a0e5d4"
        );
    }
}
