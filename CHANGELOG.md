# Changelog

All notable changes to gate. The format is loosely
[Keep a Changelog](https://keepachangelog.com/) — version headers mirror the
published bazel-registry entries.

## Unreleased

### Added
- `bzlmod` crate: `MODULE.bazel` evaluation over starlark-rust in a restricted
  dialect (no `def`, no `load`), replacing regex scraping. Captures the
  directives the gates need: `dev_dependency`, `register_toolchains`,
  `use_repo`, every `*_override`, and `compatibility_level`.
- `bzlmod::Version`: bzlmod version ordering per upstream `Version.java`,
  including the digit-vs-alphanumeric prerelease precedence that both of
  `rels`' hand-rolled `version_cmp` functions get wrong.
- Corpus test over all 314 registry entries.
- `--example survey`: reconnaissance over the registry for the S/C/D/V gates.
- `bzlmod::registry`: BCR-layout registry reader (`FsRegistry`), `source.json` /
  `metadata.json` models including `yanked_versions`.
- `bzlmod::graph`: the module graph and **minimal version selection**. Discovery
  and selection iterate to a fixed point, because bumping a module to a higher
  version means reading that version's `MODULE.bazel`, which may declare deps
  the lower version never had. `dev_dependency` edges are followed only from the
  root — the mechanism that makes a dev dep invisible to consumers, and the
  basis of D1.
- `ModuleGraph::downgraded_requirements`: modules that asked for one version and
  silently got a higher one. Since `compatibility_level` became a no-op in Bazel
  8.6/9.1 this is the only remaining signal for that class of break.
- `--example selections`: prints MVS results as JSON for diffing against Bazel's
  own answer. Verified against `rules_k8s`, `rules_lean` and `rules_tla`
  lockfiles: **zero mismatches** on every module the registry can resolve.
- Bazel build: `bazel test //...` alongside `cargo test`, over @gate_crates.
- `gate-rdf`: projects the registry to Turtle. Facts only, no policy.
- `//gates`: the first four invariants as `sparql_query_test` targets — S4
  (no override in a published module), D2 (undeclared toolchain leakage), D3
  (unnamespaced repo on a shared extension), C1 (one live version per atom).
  Each is wired both against a clean fixture (must return zero rows) and a
  known-violation fixture diffed against a committed golden, so a malformed
  query cannot masquerade as a passing gate.
- `gate-report` + `//gates:report`: the conformance report — a scoreboard, the
  findings per invariant, the findings per module, and the third-party atom
  frontier with BOM conformance. Regenerated from the projection on every build.
  Gate rationale is EXTRACTED from each `.rq` file's header comment rather than
  written separately, so documentation cannot drift from the gate it documents.
  Findings are attributed to a module only when the query exposes a `?module`
  variable; registry-level invariants like C1 stay on the report as a whole
  rather than being blamed on an arbitrary module.
- `//proto/tomato/gate/v1:conformance_proto` — the report's wire contract, and
  the new shape of `//gates:report`. It emits a single
  `tomato.gate.v1.ConformanceReport` (`report.binpb`) plus its own
  `FileDescriptorSet` (`conformance-descriptor.binpb`), so a consumer can decode
  it without a copy of the schema. `//gates:report_bundle` is the pair.

  This REPLACES the mdBook source tree the report used to be. Rendering moved to
  [tomato-bazel/docs](https://github.com/tomato-bazel/docs), which publishes
  docs.tbzl.dev/conformance/ — one site, one theme, one search index, and a
  module's findings sitting on the same page as its `bazel_dep` line instead of
  in a parallel book. The report is now data; the editorial framing lives where
  editorial things belong.

  Provenance (`generated_at`, `generated_from`) arrives via `string_flag`, so a
  local `bazel build //gates:report` is byte-reproducible and an unstamped report
  is visibly unstamped rather than quietly claiming to be current.

  The prost toolchain (`//proto:prost_toolchain`) is tonic-free — the default one
  wires `tonic_runtime` into every `rust_prost_library`, and nothing here speaks
  gRPC — and is registered `dev_dependency = True`, because a non-dev
  `register_toolchains` is exactly the D2 leak this repo detects.

## 0.0.1 — scaffold
