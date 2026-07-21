//! `bzlmod` — MODULE.bazel evaluation, the module graph, and minimal version
//! selection.
//!
//! This crate exists because there are currently **three divergent regex
//! parsers of `MODULE.bazel`** across the constellation:
//!
//!   * `bazel-registry/tools/graph/graph.py` — `DEP`/`MOD` regexes
//!   * `modgraph-operator/internal/registry/registry.go` — `bazelDep`
//!   * `tbzl/crates/tbzl/src/module.rs` — hand-rolled `extract_quoted`
//!
//! All three miss multi-line `bazel_dep(...)`, `dev_dependency`, every
//! `*_override`, and `use_extension`/`use_repo`. A registry gate cannot be
//! built on any of them: the invariants it must prove (dev-dep soundness,
//! toolchain-registration leakage, repo-name collision) are precisely the
//! directives the regexes drop.
//!
//! So `MODULE.bazel` is **evaluated**, not scraped. It is a restricted Starlark
//! dialect — no `def`, no `load()`, no arbitrary I/O — which makes evaluation
//! safe and gives correct handling of comments, line continuation, string
//! concatenation, conditionals, and comprehensions for free.
//!
//! Note the asymmetry with the sibling `starmap` crate: arbitrary `.bzl` files
//! are **parsed to an AST and never evaluated**, because they are not a
//! restricted dialect.
//!
//! This is also roma's unstarted "B1" (see `roma/docs/bzlmod-strategy.md`):
//! own MODULE.bazel parse + MVS + registry fetch. The crate is deliberately
//! free of gate-specific policy so it can be lifted there unchanged.

pub mod graph;
pub mod parse;
pub mod registry;
pub mod version;

pub use graph::{resolve, ModuleGraph, ModuleKey, Resolution};
pub use parse::{parse, BazelDep, ModuleDecl, ModuleFile, Override};
pub use registry::{FsRegistry, Metadata, MultiRegistry, Registry, SourceJson};
pub use version::{Version, VersionError};
