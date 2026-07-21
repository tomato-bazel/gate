//! Reading a bzlmod registry.
//!
//! The layout is Bazel's registry protocol, which `bazel-registry` follows
//! exactly:
//!
//! ```text
//! <registry>/bazel_registry.json
//! <registry>/modules/<name>/metadata.json
//! <registry>/modules/<name>/<version>/MODULE.bazel
//! <registry>/modules/<name>/<version>/source.json
//! ```
//!
//! Only the local filesystem is implemented. That is deliberate and sufficient:
//! the gate runs against a *checkout* of the registry (in CI, and in
//! `rels release` before it writes), and every fact it certifies has to come
//! from bytes on disk that a human can review in a PR diff. Fetching over HTTP
//! would make the projection depend on network state and quietly break
//! reproducibility — the certificate is supposed to be a function of the commit.
//!
//! Archive fetching (S1's re-verification) is a separate concern and belongs in
//! `gate-cli`, where it can be cached by content hash.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::version::Version;

/// `modules/<name>/<version>/source.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceJson {
    pub integrity: String,
    #[serde(default)]
    pub strip_prefix: String,
    pub url: String,
    /// Bazel infers the archive type from the URL extension; codeload.github.com
    /// URLs have none, so entries for those set this explicitly. Named `type` on
    /// disk, matching `http_archive`'s attr.
    #[serde(rename = "type", default, skip_serializing_if = "Option::is_none")]
    pub archive_type: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub patches: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub patch_strip: Option<u32>,
}

/// `modules/<name>/metadata.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Metadata {
    #[serde(default)]
    pub homepage: String,
    #[serde(default)]
    pub maintainers: Vec<Maintainer>,
    #[serde(default)]
    pub repository: Vec<String>,
    #[serde(default)]
    pub versions: Vec<String>,
    /// Already used in anger — `rules_cc_host 0.1.0` and `rules_lang 0.4.2` were
    /// both yanked reactively for failures an admission gate would have caught.
    #[serde(default)]
    pub yanked_versions: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Maintainer {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub github: String,
}

impl Metadata {
    pub fn is_yanked(&self, version: &Version) -> bool {
        self.yanked_versions.contains_key(version.as_str())
    }

    /// Declared versions, parsed and sorted. Unparseable entries are dropped
    /// here; `Registry::version_dirs` is what a gate compares against to catch
    /// metadata/directory drift.
    pub fn parsed_versions(&self) -> Vec<Version> {
        let mut v: Vec<Version> = self
            .versions
            .iter()
            .filter_map(|s| Version::parse(s).ok())
            .collect();
        v.sort();
        v
    }
}

/// Read access to a registry.
pub trait Registry {
    /// Module names present in the registry.
    fn modules(&self) -> anyhow::Result<Vec<String>>;

    /// Version directories that actually exist on disk for `name`. Distinct from
    /// `metadata.json`'s `versions` list on purpose — the two disagreeing is an
    /// S6 finding, so a gate needs both.
    fn version_dirs(&self, name: &str) -> anyhow::Result<Vec<Version>>;

    fn metadata(&self, name: &str) -> anyhow::Result<Option<Metadata>>;

    /// Raw `MODULE.bazel` text, or `None` when the module/version is not here.
    fn module_text(&self, name: &str, version: &Version) -> anyhow::Result<Option<String>>;

    fn source(&self, name: &str, version: &Version) -> anyhow::Result<Option<SourceJson>>;

    /// Parsed `MODULE.bazel`, or `None` when absent.
    fn module_file(
        &self,
        name: &str,
        version: &Version,
    ) -> anyhow::Result<Option<crate::parse::ModuleFile>> {
        match self.module_text(name, version)? {
            Some(text) => {
                let origin = format!("{name}@{version}/MODULE.bazel");
                Ok(Some(crate::parse::parse(&origin, &text)?))
            }
            None => Ok(None),
        }
    }

    /// Highest non-yanked version of `name` present on disk.
    fn latest(&self, name: &str) -> anyhow::Result<Option<Version>> {
        let meta = self.metadata(name)?;
        let mut versions = self.version_dirs(name)?;
        if let Some(meta) = &meta {
            versions.retain(|v| !meta.is_yanked(v));
        }
        versions.sort();
        Ok(versions.pop())
    }
}

/// A registry on the local filesystem.
pub struct FsRegistry {
    root: PathBuf,
}

impl FsRegistry {
    /// Open a registry rooted at a directory containing `modules/`.
    pub fn open(root: impl Into<PathBuf>) -> anyhow::Result<Self> {
        let root = root.into();
        anyhow::ensure!(
            root.join("modules").is_dir(),
            "not a registry root (no modules/): {}",
            root.display()
        );
        Ok(FsRegistry { root })
    }

    /// Walk up from `start` looking for a registry root, the way `rels` locates
    /// it — a directory holding both `bazel_registry.json` and `modules/`.
    pub fn discover(start: impl AsRef<Path>) -> anyhow::Result<Self> {
        for dir in start.as_ref().ancestors() {
            if dir.join("bazel_registry.json").is_file() && dir.join("modules").is_dir() {
                return FsRegistry::open(dir);
            }
        }
        anyhow::bail!(
            "no registry root above {} (looked for bazel_registry.json + modules/)",
            start.as_ref().display()
        )
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn modules_dir(&self) -> PathBuf {
        self.root.join("modules")
    }

    fn version_dir(&self, name: &str, version: &Version) -> PathBuf {
        self.modules_dir().join(name).join(version.as_str())
    }
}

fn read_dir_names(dir: &Path) -> anyhow::Result<Vec<String>> {
    if !dir.is_dir() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            out.push(entry.file_name().to_string_lossy().into_owned());
        }
    }
    out.sort();
    Ok(out)
}

impl Registry for FsRegistry {
    fn modules(&self) -> anyhow::Result<Vec<String>> {
        let dir = self.modules_dir();
        let mut out = Vec::new();
        for name in read_dir_names(&dir)? {
            // A module is a directory with metadata.json; anything else under
            // modules/ is not one.
            if dir.join(&name).join("metadata.json").is_file() {
                out.push(name);
            }
        }
        Ok(out)
    }

    fn version_dirs(&self, name: &str) -> anyhow::Result<Vec<Version>> {
        let dir = self.modules_dir().join(name);
        let mut out = Vec::new();
        for v in read_dir_names(&dir)? {
            if !dir.join(&v).join("MODULE.bazel").is_file() {
                continue;
            }
            // An unparseable directory name is a finding, not a panic — surface
            // it by simply not resolving, so the S6 gate reports the drift.
            if let Ok(version) = Version::parse(&v) {
                out.push(version);
            }
        }
        out.sort();
        Ok(out)
    }

    fn metadata(&self, name: &str) -> anyhow::Result<Option<Metadata>> {
        let path = self.modules_dir().join(name).join("metadata.json");
        if !path.is_file() {
            return Ok(None);
        }
        let text = std::fs::read_to_string(&path)?;
        let meta = serde_json::from_str(&text)
            .map_err(|e| anyhow::anyhow!("{}: {e}", path.display()))?;
        Ok(Some(meta))
    }

    fn module_text(&self, name: &str, version: &Version) -> anyhow::Result<Option<String>> {
        let path = self.version_dir(name, version).join("MODULE.bazel");
        if !path.is_file() {
            return Ok(None);
        }
        Ok(Some(std::fs::read_to_string(&path)?))
    }

    fn source(&self, name: &str, version: &Version) -> anyhow::Result<Option<SourceJson>> {
        let path = self.version_dir(name, version).join("source.json");
        if !path.is_file() {
            return Ok(None);
        }
        let text = std::fs::read_to_string(&path)?;
        let src = serde_json::from_str(&text)
            .map_err(|e| anyhow::anyhow!("{}: {e}", path.display()))?;
        Ok(Some(src))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_real_source_json() {
        // rules_lean 0.5.5, verbatim.
        let src: SourceJson = serde_json::from_str(
            r#"{
              "integrity": "sha256-mYAi/q2LeZS3qMZ4Mvm87CFyj8sK5v2fKVQ4al1iz7c=",
              "strip_prefix": "rules_lean-0.5.5",
              "url": "https://github.com/tomato-bazel/rules_lean/archive/refs/tags/v0.5.5.tar.gz"
            }"#,
        )
        .unwrap();
        assert!(src.integrity.starts_with("sha256-"));
        assert_eq!(src.strip_prefix, "rules_lean-0.5.5");
        assert_eq!(src.archive_type, None);
        assert!(src.patches.is_empty());
    }

    #[test]
    fn source_json_round_trips_the_type_field() {
        // codeload URLs need an explicit archive type; it must survive a
        // read/write cycle or `rels release` output would change under us.
        let json = r#"{"integrity":"sha256-x","strip_prefix":"p","url":"https://e/a","type":"tar.gz"}"#;
        let src: SourceJson = serde_json::from_str(json).unwrap();
        assert_eq!(src.archive_type.as_deref(), Some("tar.gz"));
        let out = serde_json::to_string(&src).unwrap();
        assert!(out.contains(r#""type":"tar.gz""#), "got {out}");
    }

    #[test]
    fn metadata_tracks_yanks() {
        let meta: Metadata = serde_json::from_str(
            r#"{
              "homepage": "https://github.com/tomato-bazel/rules_cc_host",
              "maintainers": [{"name": "Matt Marshall", "github": "mattmarshall"}],
              "repository": ["github:tomato-bazel/rules_cc_host"],
              "versions": ["0.1.0", "0.1.1"],
              "yanked_versions": {"0.1.0": "cannot work as a bazel_dep"}
            }"#,
        )
        .unwrap();
        assert!(meta.is_yanked(&Version::parse("0.1.0").unwrap()));
        assert!(!meta.is_yanked(&Version::parse("0.1.1").unwrap()));
        assert_eq!(meta.parsed_versions().len(), 2);
    }

    #[test]
    fn metadata_tolerates_a_minimal_file() {
        let meta: Metadata = serde_json::from_str("{}").unwrap();
        assert!(meta.versions.is_empty());
        assert!(meta.yanked_versions.is_empty());
    }
}
