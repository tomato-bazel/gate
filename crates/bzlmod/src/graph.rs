//! The bzlmod module graph and minimal version selection.
//!
//! MVS is "max of the minimums": the selected version of a module is the
//! highest version any module in the graph requires of it. It is small and
//! deterministic — `roma/docs/bzlmod-strategy.md` calls it "not the hard part",
//! and that is right — but it has two wrinkles worth stating, because both
//! change what the gates see:
//!
//!   1. **Selection feeds back into discovery.** Bumping a module to a higher
//!      version means reading *that* version's `MODULE.bazel`, which may
//!      declare dependencies the lower version never had. So discovery and
//!      selection iterate to a fixed point rather than running once. This
//!      terminates because selected versions only ever increase and the
//!      registry is finite.
//!   2. **`dev_dependency` edges are followed only from the root.** That is the
//!      whole reason a dev dep is invisible to consumers, and therefore the
//!      whole reason D1 exists.
//!
//! ## What this can and cannot see
//!
//! A module the registry does not publish resolves to [`Resolution::External`]
//! — `platforms`, `rules_rust`, `protobuf` and friends live in the BCR, not
//! here. Their `MODULE.bazel` is not on disk, so the graph knows they exist and
//! who depends on them, but **not what they in turn depend on**.
//!
//! Consequence for the C family: [`ModuleGraph::frontier`] is exact, but the
//! full transitive TCB is *not computable from a local registry alone*. Any gate
//! phrased over `Reach(Frontier)` needs a BCR mirror to be honest; until then
//! scope it to the declared frontier and say so.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use crate::parse::ModuleFile;
use crate::registry::Registry;
use crate::version::Version;

/// A specific version of a module — the node identity in the graph.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ModuleKey {
    pub name: String,
    pub version: Version,
}

impl ModuleKey {
    pub fn new(name: impl Into<String>, version: Version) -> Self {
        ModuleKey {
            name: name.into(),
            version,
        }
    }
}

impl std::fmt::Display for ModuleKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.version.is_empty() {
            write!(f, "{}", self.name)
        } else {
            write!(f, "{}@{}", self.name, self.version)
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resolution {
    /// Published by this registry; `MODULE.bazel` parsed.
    Internal(Box<ModuleFile>),
    /// Referenced but not published here — a third-party atom. Its own
    /// dependencies are unknown.
    External,
}

impl Resolution {
    pub fn file(&self) -> Option<&ModuleFile> {
        match self {
            Resolution::Internal(f) => Some(f),
            Resolution::External => None,
        }
    }

    pub fn is_external(&self) -> bool {
        matches!(self, Resolution::External)
    }
}

/// A resolved module graph: the post-selection node set, the edges between
/// them, and the selected version per module name.
#[derive(Debug, Clone)]
pub struct ModuleGraph {
    root: ModuleKey,
    nodes: BTreeMap<ModuleKey, Resolution>,
    edges: BTreeSet<(ModuleKey, ModuleKey)>,
    selected: BTreeMap<String, Version>,
    /// Every (name, version) any module asked for, including versions that lost
    /// selection. This is what makes "X asked for 1.0 but got 2.0 silently"
    /// visible — the situation `compatibility_level` used to catch and, since
    /// Bazel 8.6/9.1 made it a no-op, no longer does.
    requirements: BTreeMap<String, BTreeSet<Version>>,
}

impl ModuleGraph {
    pub fn root(&self) -> &ModuleKey {
        &self.root
    }

    pub fn nodes(&self) -> impl Iterator<Item = (&ModuleKey, &Resolution)> {
        self.nodes.iter()
    }

    pub fn edges(&self) -> impl Iterator<Item = (&ModuleKey, &ModuleKey)> {
        self.edges.iter().map(|(a, b)| (a, b))
    }

    pub fn get(&self, key: &ModuleKey) -> Option<&Resolution> {
        self.nodes.get(key)
    }

    /// The MVS-selected version for a module name.
    pub fn selected(&self, name: &str) -> Option<&Version> {
        self.selected.get(name)
    }

    pub fn selections(&self) -> impl Iterator<Item = (&String, &Version)> {
        self.selected.iter()
    }

    /// Modules published by this registry.
    pub fn internal(&self) -> impl Iterator<Item = &ModuleKey> {
        self.nodes
            .iter()
            .filter(|(_, r)| !r.is_external())
            .map(|(k, _)| k)
    }

    /// **The frontier**: third-party modules directly depended on by something
    /// in this registry. Exact — these are declared edges.
    pub fn frontier(&self) -> BTreeSet<&ModuleKey> {
        self.nodes
            .iter()
            .filter(|(_, r)| r.is_external())
            .map(|(k, _)| k)
            .collect()
    }

    /// Names required at more than one version — i.e. some module asked for a
    /// version it did not get. **This is now the only signal that a consumer
    /// was silently upgraded across a potentially breaking change**, since
    /// `compatibility_level` no longer makes it an error.
    pub fn downgraded_requirements(&self) -> BTreeMap<&str, (&Version, Vec<&Version>)> {
        let mut out = BTreeMap::new();
        for (name, wanted) in &self.requirements {
            let Some(sel) = self.selected.get(name) else {
                continue;
            };
            let losers: Vec<&Version> = wanted.iter().filter(|v| *v != sel).collect();
            if !losers.is_empty() {
                out.insert(name.as_str(), (sel, losers));
            }
        }
        out
    }

    /// Breadth-first reachability from a node, following selected edges.
    pub fn reachable_from(&self, start: &ModuleKey) -> BTreeSet<&ModuleKey> {
        let mut seen = BTreeSet::new();
        let mut queue = VecDeque::new();
        queue.push_back(start);
        while let Some(k) = queue.pop_front() {
            let Some((key, _)) = self.nodes.get_key_value(k) else {
                continue;
            };
            if !seen.insert(key) {
                continue;
            }
            for (from, to) in &self.edges {
                if from == key {
                    queue.push_back(to);
                }
            }
        }
        seen.remove(start);
        seen
    }
}

/// Resolve the module graph rooted at `root`.
pub fn resolve(registry: &dyn Registry, root: ModuleKey) -> anyhow::Result<ModuleGraph> {
    // Selected versions, seeded with the root pinned to its own version.
    let mut selected: BTreeMap<String, Version> = BTreeMap::new();
    selected.insert(root.name.clone(), root.version.clone());

    // Cache parsed MODULE.bazel by key so an iterating fixed point does not
    // re-read and re-evaluate the same files.
    let mut cache: BTreeMap<ModuleKey, Resolution> = BTreeMap::new();

    // Versions only increase and the registry is finite, so this terminates.
    // The bound is a bug backstop, not a semantic limit.
    const MAX_ROUNDS: usize = 64;
    let mut requirements: BTreeMap<String, BTreeSet<Version>> = BTreeMap::new();
    let mut nodes: BTreeMap<ModuleKey, Resolution> = BTreeMap::new();
    let mut edges: BTreeSet<(ModuleKey, ModuleKey)> = BTreeSet::new();

    for round in 0..=MAX_ROUNDS {
        anyhow::ensure!(
            round < MAX_ROUNDS,
            "module selection did not converge after {MAX_ROUNDS} rounds (cycle in version bumps?)"
        );

        requirements.clear();
        nodes.clear();
        edges.clear();

        let mut visited: BTreeSet<ModuleKey> = BTreeSet::new();
        let mut queue: VecDeque<ModuleKey> = VecDeque::new();
        queue.push_back(root.clone());

        while let Some(key) = queue.pop_front() {
            if !visited.insert(key.clone()) {
                continue; // already walked; also how graph cycles terminate
            }

            let resolution = match cache.get(&key) {
                Some(r) => r.clone(),
                None => {
                    let r = match registry.module_file(&key.name, &key.version)? {
                        Some(f) => Resolution::Internal(Box::new(f)),
                        None => Resolution::External,
                    };
                    cache.insert(key.clone(), r.clone());
                    r
                }
            };

            let is_root = key == root;
            if let Some(file) = resolution.file() {
                for dep in &file.deps {
                    // dev_dependency edges are honoured ONLY in the root module.
                    // This is exactly why a dev dep is invisible to consumers.
                    if dep.dev_dependency && !is_root {
                        continue;
                    }
                    requirements
                        .entry(dep.name.clone())
                        .or_default()
                        .insert(dep.version.clone());

                    // Walk at the currently-selected version, not the required
                    // one — otherwise a later round would never see the higher
                    // version's own dependencies.
                    let at = selected
                        .get(&dep.name)
                        .cloned()
                        .unwrap_or_else(|| dep.version.clone());
                    let dep_key = ModuleKey::new(dep.name.clone(), at);
                    edges.insert((key.clone(), dep_key.clone()));
                    queue.push_back(dep_key);
                }
            }
            nodes.insert(key, resolution);
        }

        // Selection: max of the minimums.
        let mut next = selected.clone();
        for (name, wanted) in &requirements {
            if *name == root.name {
                continue; // the root's own version is pinned
            }
            let max = wanted.iter().max().cloned().unwrap_or_else(Version::empty);
            let entry = next.entry(name.clone()).or_insert_with(|| max.clone());
            if max > *entry {
                *entry = max;
            }
        }

        if next == selected {
            break;
        }
        selected = next;
    }

    Ok(ModuleGraph {
        root,
        nodes,
        edges,
        selected,
        requirements,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::{Metadata, SourceJson};

    /// In-memory registry for exercising selection without touching disk.
    #[derive(Default)]
    struct MapRegistry {
        files: BTreeMap<(String, String), String>,
    }

    impl MapRegistry {
        fn add(mut self, name: &str, version: &str, text: &str) -> Self {
            self.files
                .insert((name.to_string(), version.to_string()), text.to_string());
            self
        }
    }

    impl Registry for MapRegistry {
        fn modules(&self) -> anyhow::Result<Vec<String>> {
            let mut v: Vec<String> = self.files.keys().map(|(n, _)| n.clone()).collect();
            v.dedup();
            Ok(v)
        }
        fn version_dirs(&self, name: &str) -> anyhow::Result<Vec<Version>> {
            Ok(self
                .files
                .keys()
                .filter(|(n, _)| n == name)
                .filter_map(|(_, v)| Version::parse(v).ok())
                .collect())
        }
        fn metadata(&self, _name: &str) -> anyhow::Result<Option<Metadata>> {
            Ok(None)
        }
        fn module_text(&self, name: &str, version: &Version) -> anyhow::Result<Option<String>> {
            Ok(self
                .files
                .get(&(name.to_string(), version.to_string()))
                .cloned())
        }
        fn source(&self, _n: &str, _v: &Version) -> anyhow::Result<Option<SourceJson>> {
            Ok(None)
        }
    }

    fn v(s: &str) -> Version {
        Version::parse(s).unwrap()
    }

    fn key(n: &str, ver: &str) -> ModuleKey {
        ModuleKey::new(n, v(ver))
    }

    #[test]
    fn selects_the_max_of_the_minimums() {
        // root -> a 1.0.0, b 1.0.0;  b -> a 2.0.0.  MVS must pick a@2.0.0.
        let reg = MapRegistry::default()
            .add(
                "root",
                "1.0.0",
                r#"
module(name = "root", version = "1.0.0")
bazel_dep(name = "a", version = "1.0.0")
bazel_dep(name = "b", version = "1.0.0")
"#,
            )
            .add("a", "1.0.0", r#"module(name = "a", version = "1.0.0")"#)
            .add("a", "2.0.0", r#"module(name = "a", version = "2.0.0")"#)
            .add(
                "b",
                "1.0.0",
                r#"
module(name = "b", version = "1.0.0")
bazel_dep(name = "a", version = "2.0.0")
"#,
            );

        let g = resolve(&reg, key("root", "1.0.0")).unwrap();
        assert_eq!(g.selected("a"), Some(&v("2.0.0")));
        assert!(g.get(&key("a", "2.0.0")).is_some());
        // The losing version is not in the final graph.
        assert!(g.get(&key("a", "1.0.0")).is_none());
    }

    #[test]
    fn a_bumped_version_contributes_its_own_new_dependencies() {
        // The feedback wrinkle: a@2.0.0 depends on c, which a@1.0.0 never did.
        // A single-pass implementation would miss c entirely.
        let reg = MapRegistry::default()
            .add(
                "root",
                "1.0.0",
                r#"
module(name = "root", version = "1.0.0")
bazel_dep(name = "a", version = "1.0.0")
bazel_dep(name = "b", version = "1.0.0")
"#,
            )
            .add("a", "1.0.0", r#"module(name = "a", version = "1.0.0")"#)
            .add(
                "a",
                "2.0.0",
                r#"
module(name = "a", version = "2.0.0")
bazel_dep(name = "c", version = "1.0.0")
"#,
            )
            .add(
                "b",
                "1.0.0",
                r#"
module(name = "b", version = "1.0.0")
bazel_dep(name = "a", version = "2.0.0")
"#,
            )
            .add("c", "1.0.0", r#"module(name = "c", version = "1.0.0")"#);

        let g = resolve(&reg, key("root", "1.0.0")).unwrap();
        assert_eq!(g.selected("c"), Some(&v("1.0.0")));
        assert!(g.get(&key("c", "1.0.0")).is_some(), "c must be discovered");
    }

    #[test]
    fn dev_dependencies_are_followed_only_from_the_root() {
        // This is the mechanism behind D1: `stardoc` is reachable when `dep` is
        // the root, and invisible when `dep` is a dependency.
        let dep_src = r#"
module(name = "dep", version = "1.0.0")
bazel_dep(name = "stardoc", version = "0.7.2", dev_dependency = True)
"#;
        let reg = MapRegistry::default()
            .add(
                "root",
                "1.0.0",
                r#"
module(name = "root", version = "1.0.0")
bazel_dep(name = "dep", version = "1.0.0")
"#,
            )
            .add("dep", "1.0.0", dep_src)
            .add(
                "stardoc",
                "0.7.2",
                r#"module(name = "stardoc", version = "0.7.2")"#,
            );

        // As a dependency: stardoc is NOT in the graph.
        let g = resolve(&reg, key("root", "1.0.0")).unwrap();
        assert!(g.selected("stardoc").is_none());

        // As the root: it is.
        let g = resolve(&reg, key("dep", "1.0.0")).unwrap();
        assert_eq!(g.selected("stardoc"), Some(&v("0.7.2")));
    }

    #[test]
    fn unpublished_modules_resolve_to_external() {
        // platforms/rules_rust live in the BCR, not this registry. They must
        // appear as frontier nodes, not as errors.
        let reg = MapRegistry::default().add(
            "root",
            "1.0.0",
            r#"
module(name = "root", version = "1.0.0")
bazel_dep(name = "platforms", version = "1.0.0")
"#,
        );
        let g = resolve(&reg, key("root", "1.0.0")).unwrap();
        let ext = g.get(&key("platforms", "1.0.0")).unwrap();
        assert!(ext.is_external());
        let frontier: Vec<String> = g.frontier().iter().map(|k| k.to_string()).collect();
        assert_eq!(frontier, vec!["platforms@1.0.0"]);
    }

    #[test]
    fn records_requirements_that_lost_selection() {
        // Since compatibility_level became a no-op, this is the only remaining
        // signal that root asked for a@1.0.0 and silently got a@2.0.0.
        let reg = MapRegistry::default()
            .add(
                "root",
                "1.0.0",
                r#"
module(name = "root", version = "1.0.0")
bazel_dep(name = "a", version = "1.0.0")
bazel_dep(name = "b", version = "1.0.0")
"#,
            )
            .add("a", "1.0.0", r#"module(name = "a", version = "1.0.0")"#)
            .add("a", "2.0.0", r#"module(name = "a", version = "2.0.0")"#)
            .add(
                "b",
                "1.0.0",
                r#"
module(name = "b", version = "1.0.0")
bazel_dep(name = "a", version = "2.0.0")
"#,
            );

        let g = resolve(&reg, key("root", "1.0.0")).unwrap();
        let d = g.downgraded_requirements();
        let (selected, losers) = d.get("a").expect("a was required at two versions");
        assert_eq!(*selected, &v("2.0.0"));
        assert_eq!(losers.len(), 1);
        assert_eq!(losers[0], &v("1.0.0"));
    }

    #[test]
    fn tolerates_a_dependency_cycle() {
        // Module graphs may contain cycles; discovery must terminate.
        let reg = MapRegistry::default()
            .add(
                "root",
                "1.0.0",
                r#"
module(name = "root", version = "1.0.0")
bazel_dep(name = "a", version = "1.0.0")
"#,
            )
            .add(
                "a",
                "1.0.0",
                r#"
module(name = "a", version = "1.0.0")
bazel_dep(name = "b", version = "1.0.0")
"#,
            )
            .add(
                "b",
                "1.0.0",
                r#"
module(name = "b", version = "1.0.0")
bazel_dep(name = "a", version = "1.0.0")
"#,
            );
        let g = resolve(&reg, key("root", "1.0.0")).unwrap();
        assert_eq!(g.selected("a"), Some(&v("1.0.0")));
        assert_eq!(g.selected("b"), Some(&v("1.0.0")));
    }

    #[test]
    fn reachability_follows_selected_edges() {
        let reg = MapRegistry::default()
            .add(
                "root",
                "1.0.0",
                r#"
module(name = "root", version = "1.0.0")
bazel_dep(name = "a", version = "1.0.0")
"#,
            )
            .add(
                "a",
                "1.0.0",
                r#"
module(name = "a", version = "1.0.0")
bazel_dep(name = "b", version = "1.0.0")
"#,
            )
            .add("b", "1.0.0", r#"module(name = "b", version = "1.0.0")"#);
        let g = resolve(&reg, key("root", "1.0.0")).unwrap();
        let reach: Vec<String> = g
            .reachable_from(&key("root", "1.0.0"))
            .iter()
            .map(|k| k.to_string())
            .collect();
        assert_eq!(reach, vec!["a@1.0.0", "b@1.0.0"]);
    }
}
