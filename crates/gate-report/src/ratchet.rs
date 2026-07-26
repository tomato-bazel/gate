//! `gate-ratchet` — fail a PR that makes conformance worse.
//!
//!     gate-ratchet --base <base.binpb> --head <head.binpb> [--summary <md>]
//!
//! ## Why a ratchet rather than a gate
//!
//! The gates are not yet green: the live registry carries findings across every
//! invariant, accrued over 300-odd published versions because nothing ever
//! checked one. Requiring zero before enforcing anything would mean enforcing
//! nothing until a remediation campaign finishes — during which more findings
//! accrue, which is exactly how the corpus got here.
//!
//! So this compares the PR against its base and fails only on an INCREASE. A
//! red corpus stops getting redder from the day it lands, each invariant
//! becomes a real `sparql_query_test` the moment its count reaches zero, and no
//! remediation has to happen first.
//!
//! ## What it does not do
//!
//! It does not judge severity, rank gates, or decide what is acceptable. It
//! compares two counts that `gate-report` derived from query output. If a number
//! here is wrong, the query is wrong.

use std::collections::BTreeMap;
use std::path::PathBuf;

use gate_report::pb;
use prost::Message as _;

fn read(path: &PathBuf) -> anyhow::Result<pb::ConformanceReport> {
    let bytes = std::fs::read(path).map_err(|e| anyhow::anyhow!("read {}: {e}", path.display()))?;
    // Same reasoning as gate-report's own input guard: a ConformanceReport
    // always carries a `totals` submessage, so zero bytes means the producing
    // step emitted nothing — a failed Bazel action or a truncated copy — rather
    // than "a registry with no findings".
    anyhow::ensure!(
        !bytes.is_empty(),
        "{} is empty — the projection step produced nothing",
        path.display()
    );
    pb::ConformanceReport::decode(&bytes[..])
        .map_err(|e| anyhow::anyhow!("decode {}: {e}", path.display()))
}

struct Delta {
    code: String,
    title: String,
    base: Option<u32>,
    head: u32,
}

impl Delta {
    /// `None` on the base side means the gate did not exist there — a newly
    /// added invariant. Its findings are pre-existing debt being surfaced for
    /// the first time, NOT something this PR introduced, so it must not fail.
    fn is_regression(&self) -> bool {
        matches!(self.base, Some(b) if self.head > b)
    }
    fn change(&self) -> String {
        match self.base {
            None => format!("new gate · {} finding(s)", self.head),
            Some(b) if self.head > b => format!("+{} (was {b})", self.head - b),
            Some(b) if self.head < b => format!("−{} (was {b})", b - self.head),
            Some(_) => "unchanged".to_string(),
        }
    }
}

fn counts(r: &pb::ConformanceReport) -> BTreeMap<String, u32> {
    r.gates
        .iter()
        .map(|g| (g.id.clone(), g.rows.len() as u32))
        .collect()
}

fn main() -> anyhow::Result<()> {
    let mut base: Option<PathBuf> = None;
    let mut head: Option<PathBuf> = None;
    let mut summary: Option<PathBuf> = None;

    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        let mut next = || args.next().ok_or_else(|| anyhow::anyhow!("{a} expects a value"));
        match a.as_str() {
            "--base" => base = Some(PathBuf::from(next()?)),
            "--head" => head = Some(PathBuf::from(next()?)),
            "--summary" => summary = Some(PathBuf::from(next()?)),
            other => anyhow::bail!("unknown flag {other}"),
        }
    }
    let base = read(&base.ok_or_else(|| anyhow::anyhow!("--base is required"))?)?;
    let head = read(&head.ok_or_else(|| anyhow::anyhow!("--head is required"))?)?;

    let base_counts = counts(&base);
    let deltas: Vec<Delta> = head
        .gates
        .iter()
        .map(|g| Delta {
            code: g.code.clone(),
            title: g.title.clone(),
            base: base_counts.get(&g.id).copied(),
            head: g.rows.len() as u32,
        })
        .collect();

    let regressions: Vec<&Delta> = deltas.iter().filter(|d| d.is_regression()).collect();
    let improvements: Vec<&Delta> = deltas
        .iter()
        .filter(|d| matches!(d.base, Some(b) if d.head < b))
        .collect();

    let mut md = String::new();
    md.push_str("## Registry conformance\n\n");
    md.push_str("| Gate | Invariant | Base | This PR | Change |\n|---|---|--:|--:|---|\n");
    for d in &deltas {
        md.push_str(&format!(
            "| {} | {} | {} | {} | {}{} |\n",
            d.code,
            d.title.replace('|', "\\|"),
            d.base.map(|b| b.to_string()).unwrap_or_else(|| "—".into()),
            d.head,
            if d.is_regression() { "⛔ " } else if matches!(d.base, Some(b) if d.head < b) { "✅ " } else { "" },
            d.change(),
        ));
    }

    // A gate at zero is enforceable outright — worth surfacing, because that is
    // the moment it graduates from this ratchet to a real sparql_query_test.
    let clean: Vec<&Delta> = deltas.iter().filter(|d| d.head == 0).collect();
    if !clean.is_empty() {
        md.push_str(&format!(
            "\n**{} invariant(s) now hold with zero findings** — {} can graduate from this \
             ratchet to a `sparql_query_test` in `//...`.\n",
            clean.len(),
            clean.iter().map(|d| d.code.as_str()).collect::<Vec<_>>().join(", "),
        ));
    }

    if regressions.is_empty() {
        md.push_str("\nNo invariant got worse.\n");
        if !improvements.is_empty() {
            md.push_str(&format!(
                "\n{} improved: {}.\n",
                improvements.len(),
                improvements.iter().map(|d| d.code.as_str()).collect::<Vec<_>>().join(", "),
            ));
        }
    } else {
        md.push_str("\n### This PR makes conformance worse\n\n");
        for d in &regressions {
            md.push_str(&format!("- **{} — {}**: {}\n", d.code, d.title, d.change()));
        }
        md.push_str(
            "\nEach counted row is a counterexample the gate's query selected. See the \
             findings on <https://docs.tbzl.dev/conformance/>, or run `bazel run \
             //gates:report_<gate>` in tomato-bazel/gate to print what it flags.\n",
        );
    }

    if let Some(p) = &summary {
        std::fs::write(p, &md)?;
    }
    print!("{md}");

    if regressions.is_empty() {
        Ok(())
    } else {
        anyhow::bail!(
            "{} invariant(s) regressed: {}",
            regressions.len(),
            regressions.iter().map(|d| d.code.as_str()).collect::<Vec<_>>().join(", ")
        )
    }
}
