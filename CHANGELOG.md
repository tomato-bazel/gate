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

## 0.0.1 — scaffold
