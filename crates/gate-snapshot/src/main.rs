//! `snapshot` — vendor the third-party `MODULE.bazel` closure.
//!
//! **Read-only.** This fetches from an upstream registry (the BCR by default)
//! and writes to a local directory. It publishes nothing, anywhere. Getting a
//! module *into* the BCR means opening a pull request against
//! `bazelbuild/bazel-central-registry`; this tool has no write path of any kind.
//!
//! ## Why vendor rather than fetch at projection time
//!
//! The gate's projection is deliberately filesystem-only, so that a certificate
//! is a function of the commit rather than of network state on the day it ran.
//! Fetching upstream during projection would quietly break that.
//!
//! So the network lives here, in a tool that runs occasionally and whose output
//! is committed. The projection then reads the snapshot through the same
//! `FsRegistry` as the local registry, chained via `MultiRegistry` — exactly
//! Bazel's own `--registry` semantics.
//!
//! ## The snapshot is the TCB manifest
//!
//! The closure is ~460 module versions and under a megabyte, which is small
//! enough that vendoring stops being a cost and becomes the point: the
//! third-party trusted computing base is enumerable, and growing it shows up as
//! added files in a pull request rather than as a number in a report that
//! somebody has to notice changed.
//!
//!     snapshot --registry <local-registry> --out vendor/bcr
//!     snapshot --registry <local-registry> --out vendor/bcr --check

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use bzlmod::graph::{resolve, ModuleKey};
use bzlmod::registry::{FsRegistry, Registry};
use bzlmod::Version;

const DEFAULT_UPSTREAM: &str = "https://bcr.bazel.build";
/// Politeness bound. The closure is a few hundred small files; there is no
/// reason to hammer a public CDN harder than this.
const PARALLELISM: usize = 12;

struct Args {
    registry: PathBuf,
    out: PathBuf,
    upstream: String,
    check: bool,
    /// Bazel's builtin `bazel_tools` module, normally
    /// `$(bazel info install_base)/embedded_tools/MODULE.bazel`. Auto-detected
    /// when Bazel is on PATH.
    bazel_tools: Option<PathBuf>,
    /// Version to file `bazel_tools` under. It declares none of its own, so the
    /// Bazel release is the honest key: upgrading Bazel changes the content,
    /// and filing it under the release makes that show up as snapshot drift
    /// rather than silently changing resolution.
    bazel_version: Option<String>,
}

fn parse_args() -> anyhow::Result<Args> {
    let mut registry = None;
    let mut out = None;
    let mut upstream = DEFAULT_UPSTREAM.to_string();
    let mut check = false;
    let mut bazel_tools = None;
    let mut bazel_version = None;
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        let mut next = || {
            it.next()
                .ok_or_else(|| anyhow::anyhow!("{a} expects a value"))
        };
        match a.as_str() {
            "--registry" => registry = Some(PathBuf::from(next()?)),
            "--out" => out = Some(PathBuf::from(next()?)),
            "--upstream" => upstream = next()?,
            "--check" => check = true,
            "--bazel-tools" => bazel_tools = Some(PathBuf::from(next()?)),
            "--bazel-version" => bazel_version = Some(next()?),
            other => anyhow::bail!("unknown flag {other}"),
        }
    }
    Ok(Args {
        registry: registry.ok_or_else(|| anyhow::anyhow!("--registry is required"))?,
        out: out.ok_or_else(|| anyhow::anyhow!("--out is required"))?,
        upstream,
        check,
        bazel_tools,
        bazel_version,
    })
}

/// Locate Bazel's builtin `bazel_tools` module and the release it belongs to.
fn detect_bazel_tools() -> Option<(PathBuf, String)> {
    let out = std::process::Command::new("bazel")
        .arg("info")
        .arg("install_base")
        .output()
        .ok()?;
    let base = PathBuf::from(String::from_utf8(out.stdout).ok()?.trim());
    let module = base.join("embedded_tools").join("MODULE.bazel");
    if !module.is_file() {
        return None;
    }
    let ver = std::process::Command::new("bazel")
        .arg("--version")
        .output()
        .ok()?;
    let ver = String::from_utf8(ver.stdout).ok()?;
    let ver = ver.split_whitespace().nth(1)?.to_string();
    Some((module, ver))
}

/// Every third-party `(name, version)` the local registry depends on directly.
///
/// Derived by resolving each published module rather than scraping pins, so a
/// dependency only reachable at a selected version is still counted.
fn seeds(reg: &dyn Registry) -> anyhow::Result<BTreeSet<(String, Version)>> {
    let mut out = BTreeSet::new();
    for name in reg.modules()? {
        let Some(latest) = reg.latest(&name)? else {
            continue;
        };
        let graph = match resolve(reg, ModuleKey::new(name.clone(), latest)) {
            Ok(g) => g,
            // A module the local registry cannot resolve is a finding for the
            // gates, not a reason to abandon the snapshot.
            Err(_) => continue,
        };
        for key in graph.frontier() {
            out.insert((key.name.clone(), key.version.clone()));
        }
    }
    Ok(out)
}

fn fetch(client: &reqwest::blocking::Client, upstream: &str, n: &str, v: &Version) -> Option<String> {
    let url = format!("{upstream}/modules/{n}/{}/MODULE.bazel", v.as_str());
    let resp = client.get(&url).send().ok()?;
    if !resp.status().is_success() {
        return None;
    }
    resp.text().ok()
}

fn main() -> anyhow::Result<()> {
    let args = parse_args()?;
    let local = FsRegistry::open(&args.registry)?;

    let client = reqwest::blocking::Client::builder()
        .user_agent("tbzl-gate-snapshot (+https://github.com/tomato-bazel/gate)")
        .timeout(std::time::Duration::from_secs(30))
        .build()?;

    let mut seeds = seeds(&local)?;
    eprintln!("snapshot: {} frontier seeds", seeds.len());

    // bazel_tools is an implicit dependency of EVERY module graph and ships
    // inside the Bazel binary rather than in a registry. Its requirements are
    // newer than anything this registry declares — protobuf 33.4, rules_java
    // 9.1.0, rules_cc 0.2.17 — so omitting it makes MVS select systematically
    // lower versions than Bazel does.
    let bt = match (&args.bazel_tools, &args.bazel_version) {
        (Some(p), Some(v)) => Some((p.clone(), v.clone())),
        _ => detect_bazel_tools(),
    };
    let mut bazel_tools_entry: Option<(String, String)> = None;
    match &bt {
        Some((path, version)) => {
            let text = std::fs::read_to_string(path)?;
            let file = bzlmod::parse("bazel_tools/MODULE.bazel", &text)?;
            for dep in file.runtime_deps() {
                seeds.insert((dep.name.clone(), dep.version.clone()));
            }
            eprintln!(
                "snapshot: bazel_tools from Bazel {version} contributes {} seeds",
                file.runtime_deps().count()
            );
            bazel_tools_entry = Some((version.clone(), text));
        }
        None => eprintln!(
            "snapshot: WARNING no bazel_tools found — MVS will under-resolve. \
             Pass --bazel-tools and --bazel-version."
        ),
    }

    // BFS over the dependency graph. Parsed with the real MODULE.bazel
    // evaluator, not a regex — a regex sweep over these files picks up
    // `bazel_dep` calls it should not, which is the whole reason `bzlmod`
    // exists.
    let mut found: BTreeMap<(String, Version), String> = BTreeMap::new();
    let mut absent: BTreeSet<(String, Version)> = BTreeSet::new();
    let mut frontier: Vec<(String, Version)> = seeds.into_iter().collect();
    let mut round = 0;

    while !frontier.is_empty() {
        round += 1;
        let results: Mutex<Vec<((String, Version), Option<String>)>> = Mutex::new(Vec::new());
        let chunks: Vec<&[(String, Version)]> = frontier
            .chunks(frontier.len().div_ceil(PARALLELISM).max(1))
            .collect();
        std::thread::scope(|s| {
            for chunk in chunks {
                let client = &client;
                let upstream = args.upstream.as_str();
                let results = &results;
                s.spawn(move || {
                    for (n, v) in chunk {
                        let text = fetch(client, upstream, n, v);
                        results.lock().unwrap().push(((n.clone(), v.clone()), text));
                    }
                });
            }
        });

        let mut next: BTreeSet<(String, Version)> = BTreeSet::new();
        for (key, text) in results.into_inner().unwrap() {
            match text {
                Some(text) => {
                    let origin = format!("{}@{}", key.0, key.1);
                    if let Ok(file) = bzlmod::parse(&origin, &text) {
                        for dep in file.runtime_deps() {
                            let k = (dep.name.clone(), dep.version.clone());
                            if !found.contains_key(&k) && !absent.contains(&k) {
                                next.insert(k);
                            }
                        }
                    }
                    found.insert(key, text);
                }
                None => {
                    // Not upstream. Usually one of OUR modules appearing as a
                    // dependency of another of ours; the local registry
                    // resolves it, so this is expected, not an error.
                    absent.insert(key);
                }
            }
        }
        next.retain(|k| !found.contains_key(k) && !absent.contains(k));
        eprintln!(
            "snapshot: round {round}: {} fetched, {} new",
            found.len(),
            next.len()
        );
        frontier = next.into_iter().collect();
    }

    let bytes: usize = found.values().map(|s| s.len()).sum();
    eprintln!(
        "snapshot: closure = {} module versions ({} KB), {} not upstream",
        found.len(),
        bytes / 1024,
        absent.len()
    );

    if let Some((version, text)) = &bazel_tools_entry {
        found.insert(("bazel_tools".to_string(), Version::parse(version)?), text.clone());
    }

    if args.check {
        return check(&args.out, &found);
    }
    write(&args.out, &found)
}

/// Registry layout, so the snapshot is readable by the same `FsRegistry` as any
/// other registry. `bazel_registry.json` makes it discoverable too.
fn write(out: &Path, found: &BTreeMap<(String, Version), String>) -> anyhow::Result<()> {
    if out.exists() {
        std::fs::remove_dir_all(out)?;
    }
    std::fs::create_dir_all(out.join("modules"))?;
    std::fs::write(out.join("bazel_registry.json"), "{\n  \"mirrors\": []\n}\n")?;
    for ((n, v), text) in found {
        let dir = out.join("modules").join(n).join(v.as_str());
        std::fs::create_dir_all(&dir)?;
        std::fs::write(dir.join("MODULE.bazel"), text)?;
    }
    // metadata.json is what FsRegistry uses to recognise a directory as a
    // module, so every vendored module needs one. Only `versions` is
    // meaningful here — the snapshot is not the authority for homepage or
    // maintainers, and inventing those would be fabrication.
    let mut by_name: BTreeMap<&str, Vec<&Version>> = BTreeMap::new();
    for (n, v) in found.keys() {
        by_name.entry(n.as_str()).or_default().push(v);
    }
    for (name, mut versions) in by_name {
        versions.sort();
        let list: Vec<String> = versions
            .iter()
            .map(|v| format!("    \"{}\"", v.as_str()))
            .collect();
        let json = format!(
            "{{\n  \"homepage\": \"\",\n  \"maintainers\": [],\n  \"repository\": [],\n  \"versions\": [\n{}\n  ],\n  \"yanked_versions\": {{}}\n}}\n",
            list.join(",\n")
        );
        std::fs::write(out.join("modules").join(name).join("metadata.json"), json)?;
    }
    eprintln!("snapshot: wrote {}", out.display());
    Ok(())
}

/// Compare the live closure against what is committed, without writing.
///
/// This is what keeps the snapshot honest in CI: a dependency added upstream,
/// or a frontier pin moved locally, shows up as drift rather than as a stale
/// vendored tree nobody notices.
fn check(out: &Path, found: &BTreeMap<(String, Version), String>) -> anyhow::Result<()> {
    let mut missing = Vec::new();
    let mut changed = Vec::new();
    for ((n, v), text) in found {
        let path = out.join("modules").join(n).join(v.as_str()).join("MODULE.bazel");
        match std::fs::read_to_string(&path) {
            Ok(on_disk) if &on_disk == text => {}
            Ok(_) => changed.push(format!("{n}@{v}")),
            Err(_) => missing.push(format!("{n}@{v}")),
        }
    }
    let mut extra = Vec::new();
    let modules_dir = out.join("modules");
    if modules_dir.is_dir() {
        for e in std::fs::read_dir(&modules_dir)? {
            let dir = e?.path();
            let Some(name) = dir.file_name().map(|s| s.to_string_lossy().into_owned()) else {
                continue;
            };
            for ev in std::fs::read_dir(&dir)? {
                let vdir = ev?.path();
                if !vdir.is_dir() {
                    continue;
                }
                let v = vdir.file_name().unwrap().to_string_lossy().into_owned();
                let Ok(version) = Version::parse(&v) else { continue };
                if !found.contains_key(&(name.clone(), version)) {
                    extra.push(format!("{name}@{v}"));
                }
            }
        }
    }

    for (label, list) in [
        ("missing from the snapshot", &missing),
        ("differs from upstream", &changed),
        ("in the snapshot but no longer reachable", &extra),
    ] {
        if !list.is_empty() {
            eprintln!("{label} ({}):", list.len());
            for x in list.iter().take(20) {
                eprintln!("  {x}");
            }
        }
    }
    anyhow::ensure!(
        missing.is_empty() && changed.is_empty() && extra.is_empty(),
        "snapshot is stale — re-run without --check and commit the result"
    );
    eprintln!("snapshot: up to date");
    Ok(())
}
