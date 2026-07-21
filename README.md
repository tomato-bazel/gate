# gate

Registry admission gating for tomato-bazel: prove invariants about a module
from its graph before it is allowed into the registry.

## Status: v0.0.1 — M1 in progress

`bzlmod` (MODULE.bazel evaluation + module graph) is landing. No gates ship yet.

## Why

`bazel-registry` publishes 74 modules / 314 versions and **nothing validates an
entry**. `rels release` computes the SRI itself and it is never re-verified;
there is no PR-triggered workflow in the registry at all. The cost shows up as
*reactive yanking* — `rules_cc_host 0.1.0` ("cannot work as a `bazel_dep`"),
`rules_lang 0.4.2` ("breaks consumer package loading"). Both are failures a
graph invariant catches at publish time.

Meanwhile the `MODULE.bazel` files in `rules_k8s` and `rules_lean` already carry
hand-written proofs of exactly these invariants, in prose. This repo makes them
machine-checked.

## Design

Rust does **extraction only**. Every gate is a **Bazel target**.

```
  MODULE.bazel + *.bzl + source.json + tarballs
                   │
       [Rust]  tbzl-gate project     → facts.ttl
                   │
               rdf_dataset(...)
                   │
      ┌────────────┴────────────┐
  sparql_query_test(*.rq)   rdf_validate_test(shapes)
  one target per invariant   the convention tier
      └────────────┬────────────┘
                   │
       [Rust]  tbzl-gate certify     → certificate.binpb (signed)
```

`rules_rdf` already provides the primitive: `sparql_query_test` is a **zero-row
gate** — run a `.rq` and fail the build if the result set is non-empty. So a
gate is one query file plus one target, the violating rows *are* the
counterexample, and adding a gate needs no Rust rebuild.

Transitive closure comes from SPARQL property paths (`gate:depends_on+`,
`gate:loads*`) rather than an iterated fixpoint — the capability transfer
function is pure union along load edges, so `cap(f) = ⋃ { own(g) : f loads* g }`
is a single query.

## Crates

| Crate | Purpose |
|---|---|
| `bzlmod` | `MODULE.bazel` evaluation, the module graph, MVS, registry client |

`bzlmod` is also roma's unstarted **B1** (`roma/docs/bzlmod-strategy.md`: own
parse + MVS + registry fetch). Building it here means the registry gets gating
now and roma gets bzlmod resolution from the same implementation — and it
retires the three divergent regex parsers of `MODULE.bazel` currently in
`bazel-registry/tools/graph/graph.py`, `modgraph-operator`, and `tbzl`.

## Reconnaissance

`cargo run -p bzlmod --example survey` reports what the S/C/D/V gates *would*
find across the registry today. It reports; it does not judge. Current output
over the latest version of each of the 74 modules:

| Gate | Finding |
|---|---|
| S4 | 5 `local_path_override` in published modules |
| C1 | 10 of 30 external atoms pinned at more than one version |
| D2 | 32 of 74 modules register toolchains that propagate to every consumer |
| D3 | 34 repo names chosen on a *shared* extension without a module namespace |
| V1 | 0 violations — because no module has reached 1.0.0 yet |

D3 is the one worth reading closely, because the naive form of it is wrong.
Apparent repo names are module-scoped, so `@crates` in two unrelated modules is
not automatically a conflict. The hazard is narrower: a name the module *chose*
via a tag (`oci.pull(name = ...)`) on an extension defined in *another* module,
since extension instances — and the repos they generate — are shared across the
graph. That is the bug `rules_k8s` hit on its first consumer, and the fix it
shipped (prefix with the module name) is what this checks. Singletons the
extension emits for everyone, like `rust_toolchains`, are correctly ignored.

## Install

`.bazelrc`:

```
common --registry=https://registry.fastverk.com/
common --registry=https://bcr.bazel.build/
```

`MODULE.bazel`:

```python
bazel_dep(name = "gate", version = "0.0.1")
```
