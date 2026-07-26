//! `gate-report` — project gate results into a `tomato.gate.v1` report.
//!
//!     gate-report --out-binpb <conformance.binpb> \
//!                 --generated-at <rfc3339> \
//!                 --generated-from <registry url> \
//!                 --bom <versions.json> \
//!                 --inventory <inventory.json> \
//!                 --selections <c1_by_module.json> \
//!                 --upgrades <silent_upgrades.json> \
//!                 --declared <declared_atoms.json> \
//!                 --gate <id>=<query.rq>=<results.json> ...
//!
//! Every input is a build artifact: the JSON files come from `sparql_query`
//! targets, so the report is a pure function of the projection plus the
//! queries. There is no hidden state and nothing is fetched — including the
//! timestamp, which is why `--generated-at` is a flag rather than a clock read.

use std::path::PathBuf;

use gate_report::{pb, Bom, Gate, Report, SparqlResults};
use prost::Message as _;

fn read_results(path: &str) -> anyhow::Result<SparqlResults> {
    let text = std::fs::read_to_string(path).map_err(|e| anyhow::anyhow!("read {path}: {e}"))?;
    // An empty file is not an empty result set. A query with no matches still
    // emits a full JSON document with an empty bindings array, so zero bytes
    // means the action produced nothing — a truncated write, a cached empty
    // output, a failed JVM. This guard is the one durable thing to come out of
    // chasing a zero-byte-output episode to a wrong conclusion; without it the
    // symptom is a silently wrong report.
    anyhow::ensure!(
        !text.trim().is_empty(),
        "{path} is empty — a SPARQL result set is never zero bytes, so the \
         producing action emitted nothing"
    );
    serde_json::from_str(&text).map_err(|e| anyhow::anyhow!("parse {path}: {e}"))
}

fn main() -> anyhow::Result<()> {
    let mut out: Option<PathBuf> = None;
    let mut generated_at: Option<String> = None;
    let mut generated_from: Option<String> = None;
    let mut bom_path: Option<String> = None;
    let mut inventory: Option<String> = None;
    let mut selections: Option<String> = None;
    let mut upgrades: Option<String> = None;
    let mut declared: Option<String> = None;
    let mut gate_specs: Vec<String> = Vec::new();

    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        let mut next = || {
            args.next()
                .ok_or_else(|| anyhow::anyhow!("{a} expects a value"))
        };
        match a.as_str() {
            "--out-binpb" => out = Some(PathBuf::from(next()?)),
            "--generated-at" => generated_at = Some(next()?),
            "--generated-from" => generated_from = Some(next()?),
            "--bom" => bom_path = Some(next()?),
            "--inventory" => inventory = Some(next()?),
            "--selections" => selections = Some(next()?),
            "--upgrades" => upgrades = Some(next()?),
            "--declared" => declared = Some(next()?),
            "--gate" => gate_specs.push(next()?),
            other => anyhow::bail!("unknown flag {other}"),
        }
    }

    let out = out.ok_or_else(|| anyhow::anyhow!("--out-binpb is required"))?;
    let bom: Bom = match &bom_path {
        Some(p) => serde_json::from_str(&std::fs::read_to_string(p)?)?,
        None => Bom::default(),
    };

    let mut gates = Vec::new();
    for spec in &gate_specs {
        // <id>=<query.rq>=<results.json>
        let parts: Vec<&str> = spec.splitn(3, '=').collect();
        anyhow::ensure!(
            parts.len() == 3,
            "--gate expects <id>=<query.rq>=<results.json>, got {spec:?}"
        );
        let query_src = std::fs::read_to_string(parts[1])
            .map_err(|e| anyhow::anyhow!("read {}: {e}", parts[1]))?;
        gates.push(Gate::from_query(
            parts[0],
            &query_src,
            read_results(parts[2])?,
        ));
    }

    let empty = || SparqlResults {
        head: gate_report::Head { vars: vec![] },
        results: gate_report::Bindings { bindings: vec![] },
    };

    let report = Report {
        gates,
        inventory: match &inventory {
            Some(p) => read_results(p)?,
            None => empty(),
        },
        selections: match &selections {
            Some(p) => read_results(p)?,
            None => empty(),
        },
        upgrades: match &upgrades {
            Some(p) => read_results(p)?,
            None => empty(),
        },
        declared: match &declared {
            Some(p) => read_results(p)?,
            None => empty(),
        },
        bom,
    };

    // An unset timestamp stays EMPTY rather than defaulting to "now". The docs
    // site treats a missing `generated_at` as unknown and says so; inventing one
    // would make a stale report look fresh, which is the failure this field
    // exists to expose.
    let msg: pb::ConformanceReport = report.to_proto(
        generated_at.as_deref().unwrap_or(""),
        generated_from.as_deref().unwrap_or(""),
    );

    if let Some(parent) = out.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&out, msg.encode_to_vec())?;

    let t = msg.totals.unwrap_or_default();
    eprintln!(
        "gate-report: {} findings across {} invariants ({} clean), \
         {} modules ({} with findings), {} atoms ({} multi-version) -> {}",
        t.findings,
        t.gates,
        t.clean_gates,
        t.modules,
        t.dirty_modules,
        t.atoms,
        t.multi_version_atoms,
        out.display()
    );
    Ok(())
}
