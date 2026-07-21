//! Print the MVS-selected version of every module in a graph, as JSON.
//!
//! Exists to be diffed against Bazel's own answer. Bazel 8/9 lockfiles
//! (`lockFileVersion` 28) no longer embed the module dep graph, but
//! `registryFileHashes` records every registry file fetched — and a
//! `<name>/<version>/source.json` fetch means Bazel *selected* that version.
//! That makes the lockfile usable ground truth without running a build.
//!
//!     cargo run -p bzlmod --example selections -- <registry> <module> [version]

use bzlmod::graph::{resolve, ModuleKey};
use bzlmod::registry::{FsRegistry, Registry};

fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let registry = args
        .next()
        .ok_or_else(|| anyhow::anyhow!("usage: selections <registry> <module> [version]"))?;
    let module = args
        .next()
        .ok_or_else(|| anyhow::anyhow!("usage: selections <registry> <module> [version]"))?;

    let reg = FsRegistry::open(&registry)?;
    let version = match args.next() {
        Some(v) => bzlmod::Version::parse(&v)?,
        None => reg
            .latest(&module)?
            .ok_or_else(|| anyhow::anyhow!("{module}: no versions in {registry}"))?,
    };

    let g = resolve(&reg, ModuleKey::new(module.clone(), version.clone()))?;

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
