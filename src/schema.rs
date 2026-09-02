use anyhow::{Context, Result};
use serde_json::Value;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SchemaStatus {
    Catalogued {
        path: PathBuf,
        rollout_line_canonical_sha256: String,
    },
    Missing,
}

fn valid_version_component(cli_version: &str) -> bool {
    let mut components = Path::new(cli_version).components();
    matches!(components.next(), Some(std::path::Component::Normal(_)))
        && components.next().is_none()
        && !cli_version.contains(['/', '\\'])
}

pub fn lookup(root: &Path, cli_version: &str) -> Result<SchemaStatus> {
    if !valid_version_component(cli_version) {
        return Ok(SchemaStatus::Missing);
    }
    let path = root.join("schemas").join("codex").join(cli_version);
    if !path.exists() {
        return Ok(SchemaStatus::Missing);
    }
    let manifest_path = path.join("manifest.json");
    let bytes = std::fs::read(&manifest_path)
        .with_context(|| format!("read schema manifest {}", manifest_path.display()))?;
    let manifest: Value = serde_json::from_slice(&bytes)
        .with_context(|| format!("parse schema manifest {}", manifest_path.display()))?;
    anyhow::ensure!(
        manifest.get("cli_version").and_then(Value::as_str) == Some(cli_version),
        "schema manifest {} has wrong or missing cli_version",
        manifest_path.display()
    );
    let hash = manifest
        .get("rollout_line_canonical_sha256")
        .and_then(Value::as_str)
        .context("schema manifest missing rollout_line_canonical_sha256")?;
    anyhow::ensure!(
        hash.len() == 64 && hash.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "invalid rollout_line_canonical_sha256 in {}",
        manifest_path.display()
    );
    let rollout_line = path.join("internal/RolloutLine.json");
    anyhow::ensure!(
        rollout_line.is_file(),
        "schema catalogue missing {}",
        rollout_line.display()
    );
    Ok(SchemaStatus::Catalogued {
        path,
        rollout_line_canonical_sha256: hash.to_owned(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const HASH: &str = "0401b0f306ec02c52e82d33a0bdd2b3435befaee9feb5573496e31c441822184";

    fn write_manifest(dir: &Path, version: &str, hash: &str) {
        std::fs::write(
            dir.join("manifest.json"),
            format!(
                r#"{{"cli_version":"{version}","rollout_line_canonical_sha256":"{hash}","files":[{{"path":"internal/RolloutLine.json","sha256":"raw-file-hash"}}]}}"#
            ),
        )
        .unwrap();
    }

    #[test]
    fn catalogue_is_exact_atomic_and_traversal_safe() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(
            lookup(tmp.path(), "0.152.1").unwrap(),
            SchemaStatus::Missing
        );
        let exact = tmp.path().join("schemas/codex/0.152.1");
        std::fs::create_dir_all(&exact).unwrap();
        std::fs::write(exact.join("manifest.json"), "{}").unwrap();
        assert!(lookup(tmp.path(), "0.152.1").is_err());

        write_manifest(&exact, "wrong", HASH);
        assert!(lookup(tmp.path(), "0.152.1").is_err());
        write_manifest(&exact, "0.152.1", "bad");
        assert!(lookup(tmp.path(), "0.152.1").is_err());
        write_manifest(&exact, "0.152.1", HASH);
        assert!(lookup(tmp.path(), "0.152.1").is_err());

        std::fs::create_dir_all(exact.join("internal")).unwrap();
        std::fs::write(exact.join("internal/RolloutLine.json"), "{}").unwrap();
        assert_eq!(
            lookup(tmp.path(), "0.152.1").unwrap(),
            SchemaStatus::Catalogued {
                path: exact,
                rollout_line_canonical_sha256: HASH.into(),
            }
        );
        assert_eq!(
            lookup(tmp.path(), "0.152.0").unwrap(),
            SchemaStatus::Missing
        );
        for hostile in ["", ".", "..", "../0.152.1", "/tmp", "a/b", r"a\b"] {
            assert_eq!(lookup(tmp.path(), hostile).unwrap(), SchemaStatus::Missing);
        }
    }
}
