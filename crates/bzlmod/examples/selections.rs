//! Print the MVS-selected version of every module in a graph, as JSON.
//!
//! Exists to be diffed against Bazel's own answer. Bazel 8/9 lockfiles
//! (`lockFileVersion` 28) no longer embed the module dep graph, but
//! `registryFileHashes` records every registry file fetched — and a
//! `<name>/<version>/source.json` fetch means Bazel *selected* that version.
//! That makes the lockfile usable ground truth without running a build.
//!
//!     cargo run -p bzlmod --example selections -- <registry> <module> [version]

use bzlmod::graph::{resolve_with, ModuleKey};
use bzlmod::registry::{FsRegistry, MultiRegistry, Registry};


/// The `bazel_tools` entry in the vendored snapshot, if present.
///
/// Bazel injects this into every module graph; it is not declared anywhere and
/// ships inside the binary. Resolving without it selects systematically lower
/// versions than Bazel does.
fn implicit_deps(reg: &dyn Registry) -> Vec<ModuleKey> {
    match reg.latest("bazel_tools") {
        Ok(Some(v)) => vec![ModuleKey::new("bazel_tools", v)],
        _ => Vec::new(),
    }
}

fn main() -> anyhow::Result<()> {
    let mut positional = Vec::new();
    let mut vendor: Option<String> = None;
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        if a == "--vendor" {
            vendor = it.next();
        } else {
            positional.push(a);
        }
    }
    let mut args = positional.into_iter();
    let registry = args.next().ok_or_else(|| {
        anyhow::anyhow!("usage: selections <registry> <module> [version] [--vendor <dir>]")
    })?;
    let module = args.next().ok_or_else(|| {
        anyhow::anyhow!("usage: selections <registry> <module> [version] [--vendor <dir>]")
    })?;

    let reg = FsRegistry::open(&registry)?;
    let vendored = match &vendor {
        Some(p) => Some(FsRegistry::open(p)?),
        None => None,
    };
    let version = match args.next() {
        Some(v) => bzlmod::Version::parse(&v)?,
        None => reg
            .latest(&module)?
            .ok_or_else(|| anyhow::anyhow!("{module}: no versions in {registry}"))?,
    };

    let chained;
    let resolver: &dyn Registry = match &vendored {
        Some(v) => {
            chained = MultiRegistry::new(vec![&reg, v]);
            &chained
        }
        None => &reg,
    };
    let implicit = implicit_deps(resolver);
    let g = resolve_with(
        resolver,
        ModuleKey::new(module.clone(), version.clone()),
        &implicit,
    )?;

    let mut out = serde_json::Map::new();
    for (name, v) in g.selections() {
        let external = g
            .get(&ModuleKey::new(name.clone(), v.clone()))
            .map(|r| r.is_external())
            .unwrap_or(false);
        out.insert(
            name.clone(),
            serde_json::json!({ "version": v.to_string(), "external": external }),
        );
    }
    println!(
        "{}",
        serde_json::to_string_pretty(&serde_json::json!({
            "root": format!("{module}@{version}"),
            "selected": out,
        }))?
    );
    Ok(())
}
