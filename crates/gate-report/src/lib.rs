//! Project gate results into the `tomato.gate.v1` conformance report.
//!
//! ## Where the prose comes from
//!
//! The rationale for each invariant is **extracted from the `.rq` file's header
//! comment**, not written separately. Every gate query already carries a
//! thorough explanation — what the invariant is, which post-mortem produced it,
//! which distinctions make it correct — and duplicating that into a docs page
//! would create two things that drift apart. Since the query IS the enforcement
//! mechanism, taking its comment as the documentation makes drift impossible:
//! change the gate, the docs change with it.
//!
//! ## Attribution
//!
//! A gate result is attributed to a module when the query exposes a `?module`
//! variable. Some invariants are genuinely registry-level — C1 says "this atom
//! is live at two versions", not "this module is at fault" — and those are left
//! on the gate and atom records rather than being forced onto an arbitrary
//! module. Companion queries (`c1_atom_selection_by_module`) supply the
//! per-module view where one is meaningful, as `Module.contested`.
//!
//! ## Not a gate
//!
//! Nothing here decides anything. It projects. Counts and flags are derived from
//! query output; if a number here is wrong, the query is wrong. In particular
//! there is no pass/fail: a gate with zero rows holds, and drawing that
//! conclusion is the reader's job.
//!
//! ## Not a renderer, either — any more
//!
//! This used to emit an mdBook source tree: an index, an atoms page, a page per
//! invariant, a page per module, and a `SUMMARY.md`. All of that presentation
//! now lives in `tomato-bazel/docs` (docs.tbzl.dev/conformance/), so the report
//! is one proto message and the editorial framing is authored where editorial
//! things belong. Markdown escaping went with it — no `|` to break out of a
//! table cell, no `<` read as raw HTML.

use std::collections::{BTreeMap, BTreeSet};

use serde::Deserialize;

pub use conformance_proto::tomato::gate::v1 as pb;

// ---------------------------------------------------------------------------
// SPARQL JSON results
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct SparqlResults {
    pub head: Head,
    pub results: Bindings,
}

#[derive(Debug, Deserialize)]
pub struct Head {
    #[serde(default)]
    pub vars: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub struct Bindings {
    #[serde(default)]
    pub bindings: Vec<BTreeMap<String, Binding>>,
}

#[derive(Debug, Deserialize)]
pub struct Binding {
    pub value: String,
}

impl SparqlResults {
    pub fn rows(&self) -> Vec<BTreeMap<&str, &str>> {
        self.results
            .bindings
            .iter()
            .map(|b| {
                b.iter()
                    .map(|(k, v)| (k.as_str(), v.value.as_str()))
                    .collect()
            })
            .collect()
    }

    pub fn len(&self) -> usize {
        self.results.bindings.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Project rows onto an explicit variable list, preserving the query's own
/// variable order rather than alphabetising — the query author chose that order
/// to make the counterexample readable, and a `BTreeMap` row would otherwise
/// silently sort it.
///
/// A variable the row does not bind becomes an empty cell. SPARQL `OPTIONAL`
/// makes that a normal outcome, not an error.
fn rows_to_proto(vars: &[String], rows: &[BTreeMap<&str, &str>]) -> Vec<pb::Row> {
    rows.iter()
        .map(|row| pb::Row {
            cells: vars
                .iter()
                .map(|v| row.get(v.as_str()).copied().unwrap_or("").to_string())
                .collect(),
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Gates
// ---------------------------------------------------------------------------

pub struct Gate {
    pub id: String,
    /// First line of the `.rq` header comment, minus the leading `<ID> — `.
    pub title: String,
    /// The rest of the header comment, as markdown.
    pub rationale: String,
    pub results: SparqlResults,
}

impl Gate {
    /// Split a `.rq` file's leading comment block into a title and a body.
    ///
    /// Header lines are `#`-prefixed; the block ends at the first line that is
    /// not a comment (in practice `PREFIX`). Blank comment lines become
    /// paragraph breaks, and an indented comment line is preserved as such so
    /// the quoted Bazel errors in D2/D3 survive as code blocks.
    pub fn from_query(id: &str, query_src: &str, results: SparqlResults) -> Gate {
        let mut lines = Vec::new();
        for line in query_src.lines() {
            let t = line.trim_start();
            if let Some(rest) = t.strip_prefix('#') {
                lines.push(rest.strip_prefix(' ').unwrap_or(rest).to_string());
            } else if t.is_empty() && lines.is_empty() {
                continue;
            } else {
                break;
            }
        }

        let title_raw = lines.first().cloned().unwrap_or_else(|| id.to_string());
        // Header lines read "S4 — a published module must not carry a
        // *_override." The gate code is rendered separately, so drop it.
        let title = title_raw
            .split_once("— ")
            .map(|(_, rest)| rest)
            .unwrap_or(&title_raw)
            .trim()
            .trim_end_matches('.')
            .to_string();

        let rationale = lines
            .iter()
            .skip(1)
            .map(|s| s.as_str())
            .collect::<Vec<_>>()
            .join("\n")
            .trim()
            .to_string();

        Gate {
            id: id.to_string(),
            title,
            rationale,
            results,
        }
    }

    /// The gate family, from the id's first letter (`S`, `C`, `D`, `K`, `V`).
    pub fn family(&self) -> &str {
        match self.id.chars().next() {
            Some('s') => "Supply chain",
            Some('c') => "Convergence",
            Some('d') => "Dev-dep & toolchains",
            Some('k') => "Capability",
            Some('v') => "Semver",
            _ => "Other",
        }
    }

    /// The gate code, e.g. `S4`, parsed from the id (`s4_no_override_...`).
    pub fn code(&self) -> String {
        let head: String = self
            .id
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric())
            .collect();
        head.to_uppercase()
    }

    /// Rows grouped by the `?module` variable, when the query has one.
    pub fn by_module(&self) -> BTreeMap<String, Vec<BTreeMap<&str, &str>>> {
        let mut out: BTreeMap<String, Vec<BTreeMap<&str, &str>>> = BTreeMap::new();
        for row in self.results.rows() {
            if let Some(m) = row.get("module") {
                out.entry((*m).to_string()).or_default().push(row);
            }
        }
        out
    }

    pub fn is_module_scoped(&self) -> bool {
        self.results.head.vars.iter().any(|v| v == "module")
    }

    fn source_url(&self) -> String {
        format!(
            "https://github.com/tomato-bazel/gate/blob/main/gates/{}.rq",
            self.id
        )
    }

    fn to_proto(&self) -> pb::Gate {
        pb::Gate {
            id: self.id.clone(),
            code: self.code(),
            title: self.title.clone(),
            family: self.family().to_string(),
            rationale_md: self.rationale.clone(),
            module_scoped: self.is_module_scoped(),
            vars: self.results.head.vars.clone(),
            rows: rows_to_proto(&self.results.head.vars, &self.results.rows()),
            source_url: self.source_url(),
        }
    }
}

// ---------------------------------------------------------------------------
// The BOM
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, Default)]
pub struct Bom {
    #[serde(default)]
    pub modules: BTreeMap<String, String>,
    #[serde(default)]
    pub notes: BTreeMap<String, String>,
}

// ---------------------------------------------------------------------------
// Projection
// ---------------------------------------------------------------------------

pub struct Report {
    pub gates: Vec<Gate>,
    pub inventory: SparqlResults,
    pub selections: SparqlResults,
    pub upgrades: SparqlResults,
    /// Atoms our modules DECLARE. Only these can carry a BOM pin — see
    /// `gates/declared_atoms.rq`.
    pub declared: SparqlResults,
    pub bom: Bom,
}

impl Report {
    /// `generated_at` and `generated_from` are passed in rather than read from a
    /// clock or the network, so the report stays a pure function of
    /// (projection x queries) and rebuilds byte-identically.
    pub fn to_proto(&self, generated_at: &str, generated_from: &str) -> pb::ConformanceReport {
        let gates: Vec<pb::Gate> = self.gates.iter().map(Gate::to_proto).collect();
        let modules = self.modules();
        let atoms = self.atoms();

        let findings: usize = self.gates.iter().map(|g| g.results.len()).sum();
        let clean_gates = self.gates.iter().filter(|g| g.results.is_empty()).count();
        let dirty_modules = modules.iter().filter(|m| m.finding_count > 0).count();
        let multi_version = atoms.iter().filter(|a| !a.converged).count();
        let declared_atoms = atoms.iter().filter(|a| a.declared).count();
        let pinned = atoms
            .iter()
            .filter(|a| a.declared && !a.bom_version.is_empty())
            .count();

        pb::ConformanceReport {
            generated_from: generated_from.to_string(),
            generated_at: generated_at.to_string(),
            totals: Some(pb::Totals {
                findings: findings as u32,
                gates: gates.len() as u32,
                clean_gates: clean_gates as u32,
                modules: modules.len() as u32,
                dirty_modules: dirty_modules as u32,
                atoms: atoms.len() as u32,
                multi_version_atoms: multi_version as u32,
                declared_atoms: declared_atoms as u32,
                pinned_declared_atoms: pinned as u32,
            }),
            gates,
            modules,
            atoms,
            bom_notes: self
                .bom
                .notes
                .iter()
                .map(|(atom, note)| pb::BomNote {
                    atom: atom.clone(),
                    note: note.clone(),
                })
                .collect(),
            silent_upgrades: self
                .upgrades
                .rows()
                .iter()
                .map(|r| pb::SilentUpgrade {
                    module: r.get("module").copied().unwrap_or("").to_string(),
                    atom: r.get("atom").copied().unwrap_or("").to_string(),
                    declared: r.get("declared").copied().unwrap_or("").to_string(),
                    got: r.get("got").copied().unwrap_or("").to_string(),
                })
                .collect(),
        }
    }

    /// Distinct module names from the inventory, in the query's order.
    ///
    /// `inventory.rq` has no `DISTINCT` and two `OPTIONAL`s, so a module with
    /// two homepage triples would appear twice; dedup here rather than emit a
    /// duplicate `Module` record.
    fn module_names(&self) -> Vec<String> {
        let mut seen = BTreeSet::new();
        self.inventory
            .rows()
            .iter()
            .filter_map(|r| r.get("module").map(|s| s.to_string()))
            .filter(|m| seen.insert(m.clone()))
            .collect()
    }

    /// Findings for one module, across every module-scoped gate.
    fn findings_for(&self, module: &str) -> Vec<(&Gate, Vec<BTreeMap<&str, &str>>)> {
        self.gates
            .iter()
            .filter_map(|g| {
                let rows: Vec<_> = g
                    .results
                    .rows()
                    .into_iter()
                    .filter(|r| r.get("module") == Some(&module))
                    .collect();
                (!rows.is_empty()).then_some((g, rows))
            })
            .collect()
    }

    /// atom -> version -> the distinct latest-modules resolving it.
    ///
    /// `BTreeSet` for the module set, not `Vec`: `c1_atom_selection_by_module.rq`
    /// has no `DISTINCT`, and a repeated (module, atom, version) triple would
    /// otherwise inflate every count on the atoms table.
    fn selections_by_atom(&self) -> BTreeMap<&str, BTreeMap<&str, BTreeSet<&str>>> {
        let mut by_atom: BTreeMap<&str, BTreeMap<&str, BTreeSet<&str>>> = BTreeMap::new();
        for row in self.selections.rows() {
            let (Some(a), Some(v), Some(m)) =
                (row.get("atom"), row.get("version"), row.get("module"))
            else {
                continue;
            };
            by_atom.entry(a).or_default().entry(v).or_default().insert(m);
        }
        by_atom
    }

    fn atoms(&self) -> Vec<pb::Atom> {
        let declared: BTreeSet<&str> = self
            .declared
            .rows()
            .iter()
            .filter_map(|r| r.get("atom").copied())
            .collect();

        self.selections_by_atom()
            .iter()
            .map(|(atom, versions)| pb::Atom {
                name: (*atom).to_string(),
                declared: declared.contains(*atom),
                // Empty means unpinned. Only meaningful for a declared atom — a
                // transitively reached one has nothing for the ratchet to bump.
                bom_version: self.bom.modules.get(*atom).cloned().unwrap_or_default(),
                live: versions
                    .iter()
                    .map(|(v, ms)| pb::LiveVersion {
                        version: (*v).to_string(),
                        count: ms.len() as u32,
                    })
                    .collect(),
                converged: versions.len() == 1,
            })
            .collect()
    }

    /// Modules in REMEDIATION order: most findings first, ties by name.
    fn modules(&self) -> Vec<pb::Module> {
        let by_atom = self.selections_by_atom();

        let mut out: Vec<pb::Module> = self
            .module_names()
            .into_iter()
            .map(|name| {
                let inv = self
                    .inventory
                    .rows()
                    .into_iter()
                    .find(|r| r.get("module") == Some(&name.as_str()));
                let field = |k: &str| {
                    inv.as_ref()
                        .and_then(|r| r.get(k).copied())
                        .unwrap_or("")
                        .to_string()
                };

                let findings: Vec<pb::Finding> = self
                    .findings_for(&name)
                    .into_iter()
                    .map(|(g, rows)| {
                        // Drop the module column: on a module's page, the
                        // module is the page.
                        let vars: Vec<String> = g
                            .results
                            .head
                            .vars
                            .iter()
                            .filter(|v| *v != "module")
                            .cloned()
                            .collect();
                        pb::Finding {
                            gate_id: g.id.clone(),
                            rows: rows_to_proto(&vars, &rows),
                            vars,
                        }
                    })
                    .collect();

                pb::Module {
                    finding_count: findings.iter().map(|f| f.rows.len() as u32).sum(),
                    version: field("version"),
                    homepage: field("homepage"),
                    integrity: field("integrity"),
                    kind: field("kind"),
                    contested: contested_for(&by_atom, &name),
                    findings,
                    name,
                }
            })
            .collect();

        out.sort_by(|a, b| b.finding_count.cmp(&a.finding_count).then(a.name.cmp(&b.name)));
        out
    }
}

/// Atoms where `module` resolves a different version than someone else does —
/// the actionable form of a registry-level C1 finding.
fn contested_for(
    by_atom: &BTreeMap<&str, BTreeMap<&str, BTreeSet<&str>>>,
    module: &str,
) -> Vec<pb::ContestedAtom> {
    let mut out = Vec::new();
    for (atom, versions) in by_atom {
        if versions.len() < 2 {
            continue;
        }
        let Some(mine) = versions
            .iter()
            .find(|(_, ms)| ms.contains(&module))
            .map(|(v, _)| *v)
        else {
            continue;
        };
        out.push(pb::ContestedAtom {
            atom: (*atom).to_string(),
            resolved_here: mine.to_string(),
            elsewhere: versions
                .iter()
                .filter(|(v, _)| **v != mine)
                .map(|(v, ms)| pb::LiveVersion {
                    version: (*v).to_string(),
                    count: ms.len() as u32,
                })
                .collect(),
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn results(vars: &[&str], rows: Vec<Vec<(&str, &str)>>) -> SparqlResults {
        SparqlResults {
            head: Head {
                vars: vars.iter().map(|s| s.to_string()).collect(),
            },
            results: Bindings {
                bindings: rows
                    .into_iter()
                    .map(|r| {
                        r.into_iter()
                            .map(|(k, v)| {
                                (
                                    k.to_string(),
                                    Binding {
                                        value: v.to_string(),
                                    },
                                )
                            })
                            .collect()
                    })
                    .collect(),
            },
        }
    }

    fn report(gates: Vec<Gate>, inventory: SparqlResults, selections: SparqlResults) -> Report {
        Report {
            gates,
            inventory,
            selections,
            upgrades: results(&[], vec![]),
            declared: results(&["atom"], vec![]),
            bom: Bom::default(),
        }
    }

    #[test]
    fn extracts_title_and_rationale_from_the_query_header() {
        // The docs must come from the gate file, or they drift from the gate.
        let src = "# S4 — a published module must not carry a *_override.\n\
                   #\n\
                   # Overrides are honoured only in the ROOT module.\n\
                   PREFIX gate: <https://tbzl.dev/ns/gate#>\n\
                   SELECT * WHERE {}\n";
        let g = Gate::from_query("s4_no_override", src, results(&[], vec![]));
        assert_eq!(g.title, "a published module must not carry a *_override");
        assert!(g.rationale.contains("honoured only in the ROOT module"));
        // The PREFIX line must not leak into the prose.
        assert!(!g.rationale.contains("PREFIX"));
        // …and it must survive into the wire type, since that is what the docs
        // site renders.
        assert!(g.to_proto().rationale_md.contains("ROOT module"));
    }

    #[test]
    fn derives_code_and_family_from_the_id() {
        let g = Gate::from_query("d3_unnamespaced_extension_repo", "# x", results(&[], vec![]));
        assert_eq!(g.code(), "D3");
        assert_eq!(g.family(), "Dev-dep & toolchains");
    }

    #[test]
    fn registry_level_gates_are_not_module_scoped() {
        // C1 reports that an atom is live twice, not that a module is at fault.
        // Forcing it onto a module page would misattribute blame.
        let c1 = Gate::from_query(
            "c1_atom_multi_version",
            "# C1 — one live version per atom.",
            results(&["atom", "version_a", "version_b"], vec![]),
        );
        assert!(!c1.is_module_scoped());
        assert!(!c1.to_proto().module_scoped);

        let s4 = Gate::from_query("s4_x", "# S4 — x.", results(&["module", "version"], vec![]));
        assert!(s4.is_module_scoped());
    }

    #[test]
    fn groups_findings_by_module() {
        let g = Gate::from_query(
            "s4_x",
            "# S4 — x.",
            results(
                &["module", "target"],
                vec![
                    vec![("module", "a"), ("target", "t1")],
                    vec![("module", "a"), ("target", "t2")],
                    vec![("module", "b"), ("target", "t3")],
                ],
            ),
        );
        let by = g.by_module();
        assert_eq!(by["a"].len(), 2);
        assert_eq!(by["b"].len(), 1);
    }

    #[test]
    fn rows_preserve_query_variable_order() {
        // Alphabetising would scramble the counterexample the author laid out —
        // and a BTreeMap row does exactly that unless the vars drive the order.
        let g = Gate::from_query(
            "s4_x",
            "# S4 — x.",
            results(
                &["module", "version", "target"],
                vec![vec![("module", "m"), ("version", "1"), ("target", "t")]],
            ),
        );
        let p = g.to_proto();
        assert_eq!(p.vars, vec!["module", "version", "target"]);
        assert_eq!(p.rows[0].cells, vec!["m", "1", "t"]);
    }

    #[test]
    fn an_unbound_variable_becomes_an_empty_cell() {
        // OPTIONAL makes this a normal outcome, not an error.
        let g = Gate::from_query(
            "s4_x",
            "# S4 — x.",
            results(&["module", "note"], vec![vec![("module", "m")]]),
        );
        assert_eq!(g.to_proto().rows[0].cells, vec!["m", ""]);
    }

    #[test]
    fn modules_are_ordered_worst_first_then_by_name() {
        // The scoreboard's job is to say what to fix next, so the wire order is
        // the remediation order.
        let g = Gate::from_query(
            "s4_x",
            "# S4 — x.",
            results(
                &["module", "target"],
                vec![
                    vec![("module", "busy"), ("target", "t1")],
                    vec![("module", "busy"), ("target", "t2")],
                    vec![("module", "one"), ("target", "t3")],
                ],
            ),
        );
        let r = report(
            vec![g],
            results(
                &["module"],
                vec![
                    vec![("module", "aaa_clean")],
                    vec![("module", "busy")],
                    vec![("module", "one")],
                    vec![("module", "zzz_clean")],
                ],
            ),
            results(&["module", "atom", "version"], vec![]),
        );
        let p = r.to_proto("2026-07-26T00:00:00Z", "test");
        let names: Vec<&str> = p.modules.iter().map(|m| m.name.as_str()).collect();
        assert_eq!(names, vec!["busy", "one", "aaa_clean", "zzz_clean"]);
        assert_eq!(p.totals.as_ref().unwrap().dirty_modules, 2);
        assert_eq!(p.totals.as_ref().unwrap().findings, 3);
    }

    #[test]
    fn a_findings_row_drops_the_module_column() {
        let g = Gate::from_query(
            "s4_x",
            "# S4 — x.",
            results(
                &["module", "target"],
                vec![vec![("module", "m"), ("target", "t")]],
            ),
        );
        let r = report(
            vec![g],
            results(&["module"], vec![vec![("module", "m")]]),
            results(&["module", "atom", "version"], vec![]),
        );
        let p = r.to_proto("2026-07-26T00:00:00Z", "test");
        let f = &p.modules[0].findings[0];
        assert_eq!(f.vars, vec!["target"]);
        assert_eq!(f.rows[0].cells, vec!["t"]);
    }

    #[test]
    fn a_duplicate_selection_triple_does_not_inflate_the_count() {
        // c1_atom_selection_by_module.rq has no DISTINCT.
        let r = report(
            vec![],
            results(&["module"], vec![vec![("module", "a")]]),
            results(
                &["module", "atom", "version"],
                vec![
                    vec![("module", "a"), ("atom", "x"), ("version", "1")],
                    vec![("module", "a"), ("atom", "x"), ("version", "1")],
                ],
            ),
        );
        let p = r.to_proto("2026-07-26T00:00:00Z", "test");
        assert_eq!(p.atoms[0].live[0].count, 1);
        assert!(p.atoms[0].converged);
    }

    #[test]
    fn a_contested_atom_names_what_others_resolve() {
        let r = report(
            vec![],
            results(&["module"], vec![vec![("module", "mine")]]),
            results(
                &["module", "atom", "version"],
                vec![
                    vec![("module", "mine"), ("atom", "x"), ("version", "1.0")],
                    vec![("module", "other"), ("atom", "x"), ("version", "2.0")],
                    vec![("module", "third"), ("atom", "x"), ("version", "2.0")],
                ],
            ),
        );
        let p = r.to_proto("2026-07-26T00:00:00Z", "test");
        let c = &p.modules[0].contested[0];
        assert_eq!(c.atom, "x");
        assert_eq!(c.resolved_here, "1.0");
        assert_eq!(c.elsewhere.len(), 1);
        assert_eq!(c.elsewhere[0].version, "2.0");
        assert_eq!(c.elsewhere[0].count, 2);
        // …and the atom itself is not converged.
        assert!(!p.atoms[0].converged);
        assert_eq!(p.totals.unwrap().multi_version_atoms, 1);
    }

    #[test]
    fn only_a_declared_atom_can_lack_a_pin() {
        // A transitively reached atom showing no BOM version is not a gap: there
        // would be nothing for the ratchet to bump. So BOM coverage counts
        // declared atoms only.
        let mut bom = Bom::default();
        bom.modules.insert("declared_pinned".into(), "1.0".into());
        bom.modules.insert("transitive_pinned".into(), "9.9".into());
        let r = Report {
            gates: vec![],
            inventory: results(&["module"], vec![]),
            selections: results(
                &["module", "atom", "version"],
                vec![
                    vec![("module", "m"), ("atom", "declared_pinned"), ("version", "1")],
                    vec![("module", "m"), ("atom", "declared_bare"), ("version", "1")],
                    vec![("module", "m"), ("atom", "transitive_pinned"), ("version", "1")],
                ],
            ),
            upgrades: results(&[], vec![]),
            declared: results(
                &["atom"],
                vec![
                    vec![("atom", "declared_pinned")],
                    vec![("atom", "declared_bare")],
                ],
            ),
            bom,
        };
        let t = r.to_proto("2026-07-26T00:00:00Z", "test").totals.unwrap();
        assert_eq!(t.atoms, 3);
        assert_eq!(t.declared_atoms, 2);
        assert_eq!(t.pinned_declared_atoms, 1);
    }
}
