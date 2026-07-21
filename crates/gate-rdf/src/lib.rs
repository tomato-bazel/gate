//! Project a bzlmod registry to RDF.
//!
//! This crate emits **facts only**. It contains no policy: no thresholds, no
//! allowlists, no judgement about whether a fact is good. Every gate lives in a
//! `.rq` file under `//gates` and is evaluated by `sparql_query_test` — a
//! zero-row gate, where a non-empty result set fails the build and the returned
//! rows *are* the counterexample.
//!
//! Keeping the split strict is what makes policy versionable as data: adding an
//! invariant is a new query file, not a Rust change.
//!
//! ## Why facts, not derived conclusions
//!
//! It is tempting to emit `gate:violatesD3 true` from Rust, because the Rust
//! already knows how. Don't. Two reasons:
//!
//!   * the *definition* of a violation is the thing most likely to be wrong
//!     early (D3's first formulation was wrong, and flagged 16 false groups);
//!     keeping it in SPARQL means correcting it is a one-file diff, reviewable
//!     without reading Rust;
//!   * a certificate has to record *why*, and a derived boolean throws that
//!     away.
//!
//! So Rust emits `gate:choseRepo` / `gate:extIsForeign` / `gate:namespacePrefix`
//! and the query decides. The one thing Rust does compute is MVS, because that
//! is a definition Bazel owns rather than one we are free to choose.
//!
//! ## What is deliberately not here
//!
//! Transitive closure. SPARQL property paths (`gate:dependsOn+`) do it in one
//! query, and the capability transfer function is pure union along edges, so
//! `cap(f) = ⋃ { own(g) : f gate:loads* g }` needs no fixpoint either. Nothing
//! should pre-materialize a closure that a property path expresses directly.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use bzlmod::graph::{resolve_with, ModuleGraph, ModuleKey};
use bzlmod::registry::Registry;
use bzlmod::Version;

pub const GATE_NS: &str = "https://tbzl.dev/ns/gate#";
pub const MODULE_NS: &str = "https://tbzl.dev/m/";
pub const SELECTION_NS: &str = "https://tbzl.dev/sel/";
pub const EXT_NS: &str = "https://tbzl.dev/ext/";
pub const OVERRIDE_NS: &str = "https://tbzl.dev/ovr/";
pub const MODULE_ENTRY_NS: &str = "https://tbzl.dev/mod/";

/// Turtle prefix header shared by every emitted document and by the queries.
pub const PREFIXES: &str = concat!(
    "@prefix gate: <https://tbzl.dev/ns/gate#> .\n",
    "@prefix xsd:  <http://www.w3.org/2001/XMLSchema#> .\n",
);

/// Escape a string literal for Turtle.
fn lit(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// IRI for a module version. Full IRIs rather than prefixed names because a
/// version like `0.0.0-20260422-20ac242a` is not a safe Turtle local name, and
/// silently mangling it would make the graph disagree with the registry path —
/// which *is* the literal version string.
fn module_iri(name: &str, version: &Version) -> String {
    format!("<{MODULE_NS}{name}/{}>", version.as_str())
}

/// The prefixes that count as "namespaced by this module" for D3.
///
/// Emitted as facts so the *policy* — what counts as adequately namespaced —
/// stays in the query. `rules_jena` accepts both `rules_jena_*` and `jena_*`,
/// which is why `rules_jena` naming its Maven repo `jena_maven` is correct and
/// must not be flagged.
fn namespace_prefixes(module: &str) -> Vec<String> {
    let mut v = vec![module.to_string()];
    if let Some(short) = module.strip_prefix("rules_") {
        if !short.is_empty() {
            v.push(short.to_string());
        }
    }
    v
}

/// Project one resolved graph plus its registry metadata into Turtle triples.
///
/// `is_latest` marks the version a consumer resolving today would get; gates
/// that ask "what does the registry look like now" filter on it, while the
/// historical corpus stays queryable.
fn emit_module(
    out: &mut String,
    graph: &ModuleGraph,
    reg: &dyn Registry,
    is_latest: bool,
    referenced: &mut BTreeMap<(String, String), bool>,
) -> anyhow::Result<()> {
    let root = graph.root();
    let subject = module_iri(&root.name, &root.version);
    let Some(file) = graph.get(root).and_then(|r| r.file()) else {
        return Ok(());
    };

    writeln!(out, "\n{subject} a gate:ModuleVersion ;")?;
    if root.name.starts_with("rules_") {
        writeln!(out, "    a gate:RulesModule ;")?;
    }
    writeln!(out, "    gate:name {} ;", lit(&root.name))?;
    writeln!(out, "    gate:version {} ;", lit(root.version.as_str()))?;
    writeln!(out, "    gate:isLatest {is_latest} ;")?;

    for p in namespace_prefixes(&root.name) {
        writeln!(out, "    gate:namespacePrefix {} ;", lit(&p))?;
    }

    if let Some(decl) = &file.module {
        // Captured for the record only. compatibility_level is a NO-OP as of
        // Bazel 8.6.0/9.1.0 — never gate on it.
        if let Some(cl) = decl.compatibility_level {
            writeln!(out, "    gate:compatibilityLevel {cl} ;")?;
        }
    }

    if let Some(meta) = reg.metadata(&root.name)? {
        writeln!(out, "    gate:yanked {} ;", meta.is_yanked(&root.version))?;
        if !meta.homepage.is_empty() {
            writeln!(out, "    gate:homepage {} ;", lit(&meta.homepage))?;
        }
    }
    if let Some(src) = reg.source(&root.name, &root.version)? {
        writeln!(out, "    gate:integrity {} ;", lit(&src.integrity))?;
        writeln!(out, "    gate:sourceUrl {} ;", lit(&src.url))?;
        if !src.strip_prefix.is_empty() {
            writeln!(out, "    gate:stripPrefix {} ;", lit(&src.strip_prefix))?;
        }
    }

    // --- dependency edges ---------------------------------------------------
    //
    // Only the edge is emitted here. The dep's name and version live on the
    // target IRI (see `emit_identity`), so a query joins
    // `?m gate:dependsOn ?d . ?d gate:name ?n ; gate:version ?v` and gets one
    // row per dep. Emitting a sibling `gate:declaresDep "name"` alongside would
    // cross-product with every other repeated property on the module — the
    // classic RDF modelling trap, and it silently inflates gate output.
    for dep in &file.deps {
        let pred = if dep.dev_dependency {
            "gate:devDependsOn"
        } else {
            "gate:dependsOn"
        };
        writeln!(out, "    {pred} {} ;", module_iri(&dep.name, &dep.version))?;
        let key = (dep.name.clone(), dep.version.to_string());
        // Externality is "not published by THIS registry" — deliberately NOT
        // "unresolvable". Before the vendored snapshot the two coincided, so
        // reading it off the graph worked. It no longer does: with the snapshot
        // present every third-party module resolves, `Resolution::External`
        // becomes empty, and every C-family gate silently matches nothing.
        let is_external = reg.version_dirs(&dep.name).map(|v| v.is_empty()).unwrap_or(true);
        referenced.entry(key).or_insert(is_external);
    }

    // --- toolchain registrations -------------------------------------------
    // Split by whether the registration reaches consumers. That is the D2
    // distinction, and it is a property of the directive, not a judgement.
    for reg_t in &file.toolchains {
        let pred = if reg_t.dev_dependency {
            "gate:registersToolchainDev"
        } else {
            "gate:registersToolchain"
        };
        for label in &reg_t.labels {
            writeln!(out, "    {pred} {} ;", lit(label))?;
        }
    }

    // --- overrides ----------------------------------------------------------
    // Reified as nodes for the same reason: a module with two overrides
    // (rules_schema_org has exactly that) would otherwise yield 2×2 rows from
    // a query pairing directive with target.
    let mut override_iris = Vec::new();
    for (i, o) in file.overrides.iter().enumerate() {
        let iri = format!(
            "<{OVERRIDE_NS}{}/{}/{i}>",
            root.name,
            root.version.as_str()
        );
        override_iris.push((iri.clone(), o));
        writeln!(out, "    gate:hasOverride {iri} ;")?;
    }

    // --- extension usages ---------------------------------------------------
    let mut ext_iris = Vec::new();
    for (i, ext) in file.extensions.iter().enumerate() {
        let iri = format!("<{EXT_NS}{}/{}/{i}>", root.name, root.version.as_str());
        ext_iris.push((iri.clone(), ext));
        writeln!(out, "    gate:usesExtension {iri} ;")?;
    }

    writeln!(out, "    gate:projected true .")?;

    for (iri, o) in override_iris {
        writeln!(out, "\n{iri} a gate:Override ;")?;
        writeln!(out, "    gate:overrideOf {subject} ;")?;
        writeln!(out, "    gate:overrideDirective {} ;", lit(o.directive()))?;
        writeln!(out, "    gate:overrideTarget {} .", lit(o.module_name()))?;
    }

    for (iri, ext) in ext_iris {
        writeln!(out, "\n{iri} a gate:ExtensionUsage ;")?;
        writeln!(out, "    gate:extBzlFile {} ;", lit(&ext.bzl_file))?;
        writeln!(out, "    gate:extName {} ;", lit(&ext.name))?;
        writeln!(out, "    gate:extDev {} ;", ext.dev_dependency)?;
        // Shared-vs-own is what decides whether a chosen repo name can collide.
        writeln!(
            out,
            "    gate:extIsForeign {} ;",
            ext.is_foreign_to(&root.name)
        )?;
        for r in &ext.repos {
            writeln!(out, "    gate:introducesRepo {} ;", lit(&r.apparent))?;
        }
        // Names the module CHOSE via a tag, as opposed to singletons the
        // extension emits for everyone (`rust_toolchains`). Only the former can
        // collide, which is the entire D3 distinction.
        for r in ext.module_chosen_repos() {
            writeln!(out, "    gate:choseRepo {} ;", lit(r))?;
        }
        writeln!(out, "    gate:ofModule {} .", module_iri(&root.name, &root.version))?;
    }

    // --- MVS results --------------------------------------------------------
    // Emitted as reified Selection nodes so a query can compare selections
    // across roots — which is how C1 detects an atom pinned at two versions
    // across the registry.
    for (name, version) in graph.selections() {
        if *name == root.name {
            continue;
        }
        // Same rule as above: published-by-us, not resolvable.
        let external = reg.version_dirs(name).map(|v| v.is_empty()).unwrap_or(true);
        let iri = format!(
            "<{SELECTION_NS}{}/{}/{name}>",
            root.name,
            root.version.as_str()
        );
        writeln!(out, "\n{iri} a gate:Selection ;")?;
        writeln!(out, "    gate:selectionRoot {subject} ;")?;
        writeln!(out, "    gate:selectionAtom {} ;", lit(name))?;
        writeln!(out, "    gate:selectionVersion {} ;", lit(version.as_str()))?;
        writeln!(out, "    gate:selectionExternal {external} .")?;
    }

    // --- requirements that lost selection -----------------------------------
    // A module asked for X and MVS gave it Y > X. Bazel used to be able to
    // reject this via compatibility_level; since 8.6/9.1 it cannot, so the
    // registry is the only place it can be caught.
    for (atom, (selected, losers)) in graph.downgraded_requirements() {
        for loser in losers {
            let iri = format!(
                "<{SELECTION_NS}{}/{}/{atom}/upgrade/{}>",
                root.name,
                root.version.as_str(),
                loser.as_str()
            );
            writeln!(out, "\n{iri} a gate:SilentUpgrade ;")?;
            writeln!(out, "    gate:upgradeRoot {subject} ;")?;
            writeln!(out, "    gate:upgradeAtom {} ;", lit(atom))?;
            writeln!(out, "    gate:upgradeFrom {} ;", lit(loser.as_str()))?;
            writeln!(out, "    gate:upgradeTo {} .", lit(selected.as_str()))?;
        }
    }

    Ok(())
}

/// Project a registry: every module's latest non-yanked version, resolved and
/// emitted.
///
/// Two registries, deliberately:
///
///   * `publisher` — the modules we CERTIFY. Only these get projected.
///   * `resolver`  — how dependencies are RESOLVED. Normally the publisher
///     chained with the vendored upstream snapshot, so a third-party module's
///     own dependencies are visible.
///
/// Passing the chained registry for both would project all 192 vendored
/// upstream modules as though this registry published them, which is exactly
/// backwards: we certify what we ship, and resolve through what we consume.
///
/// Returns the Turtle document and any modules that could not be resolved —
/// findings in their own right (`rules_cc_host` has a `metadata.json` but no
/// version directories, its only version having been yanked), so they are
/// reported rather than swallowed.
pub fn project_registry(
    publisher: &dyn Registry,
    resolver: &dyn Registry,
) -> anyhow::Result<(String, Vec<String>)> {
    let mut out = String::new();
    out.push_str("# Generated by `tbzl-gate project`. Do not edit.\n");
    out.push_str("#\n");
    out.push_str("# Facts only — no policy. Every gate is a .rq under //gates.\n");
    out.push_str(PREFIXES);

    // bazel_tools, when the resolver has it: an implicit dependency of every
    // module graph that ships inside the Bazel binary rather than a registry.
    let implicit: Vec<ModuleKey> = match resolver.latest("bazel_tools") {
        Ok(Some(v)) => vec![ModuleKey::new("bazel_tools", v)],
        _ => Vec::new(),
    };

    let mut unresolved = Vec::new();
    // Every (name, version) any module points at, external or not. Identity
    // triples are emitted once at the end so a dep edge can be joined to its
    // name/version without the projector repeating them per referrer.
    let mut referenced: BTreeMap<(String, String), bool> = BTreeMap::new();
    let mut names = publisher.modules()?;
    names.sort();

    for name in names {
        let latest = match publisher.latest(&name) {
            Ok(Some(v)) => v,
            Ok(None) => {
                unresolved.push(format!("{name}: no version directories"));
                continue;
            }
            Err(e) => {
                unresolved.push(format!("{name}: {e}"));
                continue;
            }
        };
        let key = ModuleKey::new(name.clone(), latest);
        match resolve_with(resolver, key.clone(), &implicit) {
            Ok(g) => emit_module(&mut out, &g, publisher, true, &mut referenced)?,
            Err(e) => unresolved.push(format!("{key}: {e}")),
        }
    }

    // Registry-level facts: what metadata.json DECLARES versus what exists on
    // disk. Emitted per module rather than per version, because the drift S6
    // looks for is a disagreement between the two lists — a version listed with
    // no directory, a directory absent from the list, or a module with a
    // metadata.json and no versions at all (`rules_cc_host`, whose only version
    // was yanked).
    //
    // Each list is a repeated single-valued property, so a query can filter one
    // against the other with NOT EXISTS without producing a cross product.
    writeln!(out, "\n# --- registry entries (metadata vs directories) ---")?;
    for name in publisher.modules()? {
        let iri = format!("<{MODULE_ENTRY_NS}{name}>");
        writeln!(out, "{iri} a gate:Module ;")?;
        writeln!(out, "    gate:name {} ;", lit(&name))?;
        if let Some(meta) = publisher.metadata(&name)? {
            for v in &meta.versions {
                writeln!(out, "    gate:declaresVersion {} ;", lit(v))?;
            }
            for v in meta.yanked_versions.keys() {
                writeln!(out, "    gate:yankedVersion {} ;", lit(v))?;
            }
        }
        for v in publisher.version_dirs(&name)? {
            writeln!(out, "    gate:hasVersionDir {} ;", lit(v.as_str()))?;
        }
        writeln!(out, "    gate:isRegistryEntry true .")?;
    }

    // Identity for every referenced module IRI, including third-party ones the
    // registry does not publish. Without these, `?m gate:dependsOn ?d .
    // ?d gate:name ?n` silently drops every external dep — which would make the
    // C-family gates quietly blind to exactly the atoms they exist to count.
    writeln!(out, "\n# --- referenced module identities ---")?;
    for ((name, version), is_external) in &referenced {
        let v = Version::parse(version).unwrap_or_else(|_| Version::empty());
        writeln!(out, "{} gate:name {} ;", module_iri(name, &v), lit(name))?;
        writeln!(out, "    gate:version {} ;", lit(version))?;
        writeln!(out, "    gate:isExternal {is_external} .")?;
    }

    Ok((out, unresolved))
}
