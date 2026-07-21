//! Render gate results as an mdBook source tree for tbzl.dev.
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
//! is live at two versions", not "this module is at fault" — and those are
//! reported on the summary and atoms pages rather than being forced onto an
//! arbitrary module's page. Companion queries (`c1_atom_selection_by_module`)
//! supply the per-module view where one is meaningful.
//!
//! ## Not a gate
//!
//! Nothing here decides anything. It renders. Counts, tables, and status
//! strings are derived from query output; if a number here is wrong, the query
//! is wrong.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

use serde::Deserialize;

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
        // *_override." The gate code is already the page heading, so drop it.
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

    /// The gate family letter (`S`, `C`, `D`, `K`, `V`), from the id prefix.
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
        for row in self.rows_ordered() {
            if let Some(m) = row.get("module") {
                out.entry((*m).to_string()).or_default().push(row);
            }
        }
        out
    }

    pub fn is_module_scoped(&self) -> bool {
        self.results.head.vars.iter().any(|v| v == "module")
    }

    fn rows_ordered(&self) -> Vec<BTreeMap<&str, &str>> {
        self.results.rows()
    }
}

fn md_escape(s: &str) -> String {
    s.replace('|', "\\|")
}

/// Render a result set as a markdown table, preserving the query's own variable
/// order rather than alphabetising — the query author chose that order to make
/// the counterexample readable.
fn table(vars: &[String], rows: &[BTreeMap<&str, &str>]) -> String {
    if rows.is_empty() {
        return "_No findings._\n".to_string();
    }
    let mut out = String::new();
    let _ = writeln!(out, "| {} |", vars.join(" | "));
    let _ = writeln!(
        out,
        "|{}|",
        vars.iter().map(|_| "---").collect::<Vec<_>>().join("|")
    );
    for row in rows {
        let cells: Vec<String> = vars
            .iter()
            .map(|v| md_escape(row.get(v.as_str()).copied().unwrap_or("")))
            .collect();
        let _ = writeln!(out, "| {} |", cells.join(" | "));
    }
    out
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
// Rendering
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

/// A rendered file: repo-relative path and contents.
pub struct Page {
    pub path: String,
    pub body: String,
}

impl Report {
    pub fn render(&self) -> Vec<Page> {
        let mut pages = Vec::new();
        pages.push(self.index());
        pages.push(self.atoms());
        for g in &self.gates {
            pages.push(self.gate_page(g));
        }
        for name in self.module_names() {
            pages.push(self.module_page(&name));
        }
        pages.push(self.summary(&pages));
        pages
    }

    fn module_names(&self) -> Vec<String> {
        self.inventory
            .rows()
            .iter()
            .filter_map(|r| r.get("module").map(|s| s.to_string()))
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

    fn index(&self) -> Page {
        let mut b = String::new();
        b.push_str("# Registry conformance\n\n");
        b.push_str(
            "Every invariant below is a [zero-row SPARQL gate](https://tbzl.dev): the query \
             selects violations, so an empty result set means the invariant holds and any \
             row is a counterexample. Gates run as Bazel targets over a projection of the \
             registry's `MODULE.bazel` graph.\n\n",
        );

        let total: usize = self.gates.iter().map(|g| g.results.len()).sum();
        let clean = self.gates.iter().filter(|g| g.results.is_empty()).count();
        let _ = writeln!(
            b,
            "**{total} findings across {} invariants** ({clean} currently clean).\n",
            self.gates.len()
        );

        b.push_str("| Gate | Invariant | Family | Findings |\n|---|---|---|---|\n");
        for g in &self.gates {
            let _ = writeln!(
                b,
                "| [{}](gates/{}.md) | {} | {} | {} |",
                g.code(),
                g.id,
                md_escape(&g.title),
                g.family(),
                g.results.len()
            );
        }

        // Module scoreboard — worst first, so remediation has an order.
        let mut counts: Vec<(String, usize)> = self
            .module_names()
            .into_iter()
            .map(|m| {
                let n = self.findings_for(&m).iter().map(|(_, r)| r.len()).sum();
                (m, n)
            })
            .collect();
        counts.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));

        let dirty = counts.iter().filter(|(_, n)| *n > 0).count();
        let _ = writeln!(
            b,
            "\n## Modules\n\n{} of {} modules have at least one finding.\n",
            dirty,
            counts.len()
        );
        b.push_str("| Module | Findings |\n|---|---|\n");
        for (m, n) in &counts {
            let mark = if *n == 0 { "clean" } else { &n.to_string() };
            let _ = writeln!(b, "| [{m}](modules/{m}.md) | {mark} |");
        }

        Page {
            path: "index.md".into(),
            body: b,
        }
    }

    fn atoms(&self) -> Page {
        let mut b = String::new();
        b.push_str("# Third-party atoms\n\n");
        b.push_str(
            "The **frontier** is the set of third-party modules the registry depends on \
             directly. Driving it down to a small, single-version set is the convergence \
             goal — and since Bazel 8.6.0 / 9.1.0 made `compatibility_level` a no-op, it is \
             also the *only* compatibility mechanism left: with one live version per atom, \
             an incompatible co-selection cannot occur.\n\n",
        );

        // atom -> version -> modules
        let mut atoms: BTreeMap<&str, BTreeMap<&str, Vec<&str>>> = BTreeMap::new();
        for row in self.selections.rows() {
            let (Some(a), Some(v), Some(m)) =
                (row.get("atom"), row.get("version"), row.get("module"))
            else {
                continue;
            };
            atoms.entry(a).or_default().entry(v).or_default().push(m);
        }

        let multi = atoms.values().filter(|v| v.len() > 1).count();
        let _ = writeln!(
            b,
            "**{} atoms on the frontier, {} of them live at more than one version.**\n",
            atoms.len(),
            multi
        );

        let declared: BTreeSet<&str> = self
            .declared
            .rows()
            .iter()
            .filter_map(|r| r.get("atom").copied())
            .collect();
        let pinned = declared.iter().filter(|a| self.bom.modules.contains_key(**a)).count();
        let _ = writeln!(
            b,
            "Of those, **{} are declared** by a module here and **{} are reached only \
             transitively**. The distinction matters for reading the BOM column: the BOM \
             is what `rels deps` ratchets our repos onto, so it can only govern atoms our \
             repos actually write down. **{}/{} declared atoms carry a pin**; a transitively \
             reached atom showing `—` is not a gap, because there would be nothing for the \
             ratchet to bump.\n",
            declared.len(),
            atoms.len().saturating_sub(declared.len()),
            pinned,
            declared.len(),
        );

        b.push_str("| Atom | Declared | BOM | Live versions | Converged |\n|---|---|---|---|---|\n");
        for (atom, versions) in &atoms {
            let is_declared = declared.contains(*atom);
            let decl = if is_declared { "yes" } else { "transitive" };
            let bom = match self.bom.modules.get(*atom) {
                Some(v) => v.as_str(),
                // Only meaningful for declared atoms; blank for the rest.
                None if is_declared => "**missing**",
                None => "n/a",
            };
            let live: Vec<String> = versions
                .iter()
                .map(|(v, ms)| format!("`{v}` ×{}", ms.len()))
                .collect();
            let ok = if versions.len() == 1 { "yes" } else { "**no**" };
            let _ = writeln!(
                b,
                "| `{atom}` | {decl} | {bom} | {} | {ok} |",
                live.join("<br>")
            );
        }

        if !self.bom.notes.is_empty() {
            b.push_str(
                "\n## BOM notes\n\nWhy an atom is pinned where it is, from \
                 `rules_tomato//bom/versions.json`.\n\n",
            );
            for (k, v) in &self.bom.notes {
                let _ = writeln!(b, "- **`{k}`** — {}", md_escape(v));
            }
        }

        if !self.upgrades.is_empty() {
            b.push_str(
                "\n## Silent upgrades\n\nA module declared one version and MVS gave it a \
                 higher one. Bazel used to be able to reject this via `compatibility_level`; \
                 since 8.6.0 / 9.1.0 it cannot, so nothing catches it but this report.\n\n",
            );
            b.push_str(&table(&self.upgrades.head.vars, &self.upgrades.rows()));
        }

        Page {
            path: "atoms.md".into(),
            body: b,
        }
    }

    fn gate_page(&self, g: &Gate) -> Page {
        let mut b = String::new();
        let _ = writeln!(b, "# {} — {}\n", g.code(), md_escape(&g.title));
        let _ = writeln!(b, "*{}*\n", g.family());

        if !g.rationale.is_empty() {
            b.push_str(&g.rationale);
            b.push_str("\n\n");
        }

        let _ = writeln!(b, "## Findings\n");
        if g.results.is_empty() {
            b.push_str("_No findings — this invariant currently holds._\n");
        } else {
            let _ = writeln!(
                b,
                "{} finding(s). Each row is a counterexample.\n",
                g.results.len()
            );
            b.push_str(&table(&g.results.head.vars, &g.results.rows()));
        }

        let _ = writeln!(
            b,
            "\n---\n\nSource: [`gates/{}.rq`](https://github.com/tomato-bazel/gate/blob/main/gates/{}.rq)\n",
            g.id, g.id
        );

        Page {
            path: format!("gates/{}.md", g.id),
            body: b,
        }
    }

    fn module_page(&self, module: &str) -> Page {
        let mut b = String::new();
        let _ = writeln!(b, "# {module}\n");

        let inv = self
            .inventory
            .rows()
            .into_iter()
            .find(|r| r.get("module") == Some(&module));
        if let Some(inv) = &inv {
            if let Some(v) = inv.get("version") {
                let _ = writeln!(b, "**Latest:** `{v}`  ");
            }
            if let Some(h) = inv.get("homepage") {
                let _ = writeln!(b, "**Homepage:** <{h}>  ");
            }
            if let Some(k) = inv.get("kind") {
                let _ = writeln!(b, "**Kind:** {k}  ");
            }
            if let Some(i) = inv.get("integrity") {
                let _ = writeln!(b, "**Integrity:** `{i}`  ");
            }
            b.push('\n');
        }

        let findings = self.findings_for(module);
        if findings.is_empty() {
            b.push_str("## Conformance\n\nNo findings — this module passes every gate that applies to it.\n");
        } else {
            let n: usize = findings.iter().map(|(_, r)| r.len()).sum();
            let _ = writeln!(b, "## Conformance\n\n{n} finding(s).\n");
            for (g, rows) in &findings {
                let _ = writeln!(
                    b,
                    "### [{} — {}](../gates/{}.md)\n",
                    g.code(),
                    md_escape(&g.title),
                    g.id
                );
                // Drop the module column: it is the page.
                let vars: Vec<String> = g
                    .results
                    .head
                    .vars
                    .iter()
                    .filter(|v| *v != "module")
                    .cloned()
                    .collect();
                b.push_str(&table(&vars, rows));
                b.push('\n');
            }
        }

        // Contested atoms this module participates in — the actionable form of a
        // registry-level C1 finding.
        let mut contested = Vec::new();
        let mut by_atom: BTreeMap<&str, BTreeMap<&str, BTreeSet<&str>>> = BTreeMap::new();
        for row in self.selections.rows() {
            let (Some(a), Some(v), Some(m)) =
                (row.get("atom"), row.get("version"), row.get("module"))
            else {
                continue;
            };
            by_atom.entry(a).or_default().entry(v).or_default().insert(m);
        }
        for (atom, versions) in &by_atom {
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
            let others: Vec<String> = versions
                .iter()
                .filter(|(v, _)| **v != mine)
                .map(|(v, ms)| format!("`{v}` (×{})", ms.len()))
                .collect();
            contested.push(format!(
                "| `{atom}` | `{mine}` | {} |",
                others.join(", ")
            ));
        }
        if !contested.is_empty() {
            b.push_str(
                "\n## Contested atoms\n\nThird-party modules where this module's resolved \
                 version differs from what others resolve. Convergence is what prevents a \
                 silent incompatible upgrade.\n\n",
            );
            b.push_str("| Atom | Resolved here | Elsewhere |\n|---|---|---|\n");
            for row in contested {
                b.push_str(&row);
                b.push('\n');
            }
        }

        Page {
            path: format!("modules/{module}.md"),
            body: b,
        }
    }

    /// mdBook's `SUMMARY.md`.
    ///
    /// Uses mdBook's own structure rather than an ad-hoc nested list: entries
    /// before the first list are PREFIX chapters, and `# Heading` lines are
    /// part dividers. An earlier version linked `index.md` three times as
    /// pseudo-headings, which mdBook renders as three duplicate chapters
    /// pointing at the same page.
    fn summary(&self, pages: &[Page]) -> Page {
        let mut b = String::new();
        b.push_str("# Summary\n\n");
        b.push_str("[Registry conformance](index.md)\n");
        b.push_str("[Third-party atoms](atoms.md)\n");

        b.push_str("\n# Invariants\n\n");
        for g in &self.gates {
            let _ = writeln!(
                b,
                "- [{} — {}](gates/{}.md)",
                g.code(),
                md_escape(&g.title),
                g.id
            );
        }

        b.push_str("\n# Modules\n\n");
        for p in pages {
            if let Some(name) = p
                .path
                .strip_prefix("modules/")
                .and_then(|s| s.strip_suffix(".md"))
            {
                let _ = writeln!(b, "- [{name}](modules/{name}.md)");
            }
        }

        Page {
            path: "SUMMARY.md".into(),
            body: b,
        }
    }
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

        let s4 = Gate::from_query(
            "s4_x",
            "# S4 — x.",
            results(&["module", "version"], vec![]),
        );
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
    fn table_preserves_query_variable_order() {
        // Alphabetising would scramble the counterexample the author laid out.
        let vars = vec!["module".to_string(), "version".to_string(), "target".to_string()];
        let rows = results(
            &["module", "version", "target"],
            vec![vec![("module", "m"), ("version", "1"), ("target", "t")]],
        );
        let t = table(&vars, &rows.rows());
        assert!(t.starts_with("| module | version | target |"), "got {t}");
    }

    #[test]
    fn table_escapes_pipes() {
        let vars = vec!["v".to_string()];
        let rows = results(&["v"], vec![vec![("v", "a|b")]]);
        assert!(table(&vars, &rows.rows()).contains("a\\|b"));
    }

    #[test]
    fn empty_result_renders_as_no_findings() {
        let vars = vec!["v".to_string()];
        assert!(table(&vars, &[]).contains("No findings"));
    }
}
