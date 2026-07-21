//! Survey the registry for the findings the S/C/D gates will formalize.
//!
//! Reconnaissance, not a gate: it reports, it does not judge. Its job is to say
//! whether an invariant is worth encoding before we encode it — and to exercise
//! the whole `registry` → `graph` → MVS stack against real data.
//!
//!     bazel run //crates/bzlmod:survey -- /abs/path/to/bazel-registry
//!     cargo run -p bzlmod --example survey

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use bzlmod::graph::{resolve, ModuleKey};
use bzlmod::registry::{FsRegistry, Registry};

fn main() -> anyhow::Result<()> {
    // Under `bazel run` the sibling checkout is unreachable from
    // CARGO_MANIFEST_DIR, so the path is an explicit argument there. The cargo
    // default is a local-iteration convenience only.
    let root = match std::env::args().nth(1) {
        Some(p) => PathBuf::from(p),
        None => Path::new(env!("CARGO_MANIFEST_DIR"))
            .ancestors()
            .nth(3)
            .map(|p| p.join("bazel-registry"))
            .unwrap_or_default(),
    };
    let reg = FsRegistry::open(&root)
        .map_err(|e| anyhow::anyhow!("{e}\nusage: survey <path-to-bazel-registry>"))?;

    // Resolve each module's latest non-yanked version as its own root. That is
    // the graph a consumer depending on it would actually get.
    let mut graphs = BTreeMap::new();
    let mut unresolved = Vec::new();
    for name in reg.modules()? {
        let Some(latest) = reg.latest(&name)? else {
            unresolved.push(format!("{name}: no version directories"));
            continue;
        };
        let key = ModuleKey::new(name.clone(), latest);
        match resolve(&reg, key.clone()) {
            Ok(g) => {
                graphs.insert(name, g);
            }
            Err(e) => unresolved.push(format!("{key}: {e}")),
        }
    }
    println!(
        "== resolved {} module graphs from {} ==\n",
        graphs.len(),
        root.display()
    );
    for u in &unresolved {
        println!("  UNRESOLVED {u}");
    }

    let files: BTreeMap<&String, &bzlmod::ModuleFile> = graphs
        .iter()
        .filter_map(|(n, g)| g.get(g.root()).and_then(|r| r.file()).map(|f| (n, f)))
        .collect();

    // --- S4: overrides in a published module -------------------------------
    println!("S4 — *_override in a published MODULE.bazel:");
    let mut s4 = 0;
    for (name, f) in &files {
        for o in &f.overrides {
            println!("  {name}: {}({})", o.directive(), o.module_name());
            s4 += 1;
        }
    }
    println!("  ({s4} found)");

    // --- D2: non-dev register_toolchains -----------------------------------
    let leaks: Vec<(&&String, usize)> = files
        .iter()
        .map(|(n, f)| (n, f.leaking_toolchains().count()))
        .filter(|(_, c)| *c > 0)
        .collect();
    println!(
        "\nD2 — modules registering toolchains that reach every consumer: {} of {}",
        leaks.len(),
        files.len()
    );

    // --- D3: module-chosen repo names on foreign extensions ----------------
    let mut d3 = 0;
    for (_, f) in &files {
        d3 += f.unnamespaced_extension_repos().len();
    }
    println!("D3 — unnamespaced repo names on a SHARED extension: {d3}");

    // --- C1, via real MVS --------------------------------------------------
    // The frontier is exact (declared edges). The full TCB is not computable
    // from a local registry — external modules' own MODULE.bazel lives in the
    // BCR — so anything phrased over Reach(Frontier) needs a BCR mirror.
    println!("\nC1 — external atoms selected at different versions across the 74 graphs:");
    let mut atom_versions: BTreeMap<String, BTreeMap<String, Vec<&String>>> = BTreeMap::new();
    let mut frontier_names: BTreeSet<String> = BTreeSet::new();
    for (name, g) in &graphs {
        for ext in g.frontier() {
            frontier_names.insert(ext.name.clone());
            atom_versions
                .entry(ext.name.clone())
                .or_default()
                .entry(ext.version.to_string())
                .or_default()
                .push(name);
        }
    }
    let mut multi: Vec<_> = atom_versions.iter().filter(|(_, v)| v.len() > 1).collect();
    multi.sort_by_key(|(_, v)| std::cmp::Reverse(v.len()));
    for (atom, versions) in &multi {
        let detail: Vec<String> = versions
            .iter()
            .map(|(v, users)| format!("{v}×{}", users.len()))
            .collect();
        println!("  {atom}: {}", detail.join("  "));
    }
    println!(
        "  ({} of {} frontier atoms are multi-version)",
        multi.len(),
        frontier_names.len()
    );

    // --- The post-compatibility_level signal -------------------------------
    // A module asked for one version and MVS silently gave it a higher one.
    // Bazel used to be able to reject this via compatibility_level; since 8.6/9.1
    // it cannot, so the registry is the only place it can be caught.
    println!("\nSilent upgrades — a module required X, MVS selected Y > X:");
    let mut upgrades: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for (name, g) in &graphs {
        for (dep, (selected, losers)) in g.downgraded_requirements() {
            for l in losers {
                upgrades
                    .entry(format!("{dep}: {l} → {selected}"))
                    .or_default()
                    .insert((*name).clone());
            }
        }
    }
    for (what, who) in upgrades.iter().take(25) {
        let mut names: Vec<&str> = who.iter().map(|s| s.as_str()).collect();
        names.sort();
        let shown = if names.len() > 4 {
            format!("{} … (+{})", names[..4].join(", "), names.len() - 4)
        } else {
            names.join(", ")
        };
        println!("  {what}   in: {shown}");
    }
    println!("  ({} distinct silent upgrades)", upgrades.len());

    // --- Graph size, for the atomicity score -------------------------------
    println!("\nC5 — widest dependency closures (candidates for decomposition):");
    let mut sizes: Vec<(&String, usize, usize)> = graphs
        .iter()
        .map(|(n, g)| {
            let reach = g.reachable_from(g.root());
            let ext = reach.iter().filter(|k| {
                g.get(k).map(|r| r.is_external()).unwrap_or(false)
            }).count();
            (n, reach.len(), ext)
        })
        .collect();
    sizes.sort_by_key(|(_, total, _)| std::cmp::Reverse(*total));
    for (name, total, ext) in sizes.iter().take(10) {
        println!("  {name}: {total} modules ({ext} third-party)");
    }

    Ok(())
}
