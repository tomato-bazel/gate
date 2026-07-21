//! Evaluating `MODULE.bazel`.
//!
//! `MODULE.bazel` is a *restricted* Starlark dialect: no `def`, no `load()`, no
//! filesystem or network access. Every global it can call is a module directive
//! with a fixed signature. That is what makes evaluating it safe — and
//! evaluating is what gets multi-line calls, comments, `+` concatenation,
//! conditionals, and comprehensions right, none of which the three incumbent
//! regex parsers handle.
//!
//! What is captured here is driven by the gates that consume it:
//!
//! | Directive                     | Gate that needs it |
//! |-------------------------------|--------------------|
//! | `bazel_dep(dev_dependency=)`  | D1 dev-dep soundness, C1/C2 convergence |
//! | `register_toolchains(...)`    | D2 toolchain-registration leakage |
//! | `use_repo(...)`               | D3 repo-name collision |
//! | `*_override(...)`             | S4 no overrides in a published module |
//! | `module(compatibility_level=)`| V1 semver signalling |
//! | `include(...)`                | recorded so an unfollowed include is a visible blind spot, not a silent gap |
//!
//! Unknown kwargs on a known directive are tolerated (`#[starlark(kwargs)]`),
//! following roma's `attr.*` precedent: Bazel adds parameters over time and a
//! registry gate must not fail to parse a file it merely does not fully model.
//! Unknown *directives*, by contrast, are a hard error — silently ignoring one
//! would mean the gate reasons about an incomplete graph.

use std::cell::RefCell;
use std::fmt;

use allocative::Allocative;
use starlark::any::ProvidesStaticType;
use starlark::environment::{Globals, GlobalsBuilder};
use starlark::eval::{Arguments, Evaluator};
use starlark::starlark_module;
use starlark::starlark_simple_value;
use starlark::syntax::{AstModule, Dialect};
use starlark::values::none::NoneType;
use starlark::values::{starlark_value, Heap, NoSerialize, StarlarkValue, Value, ValueLike};

use crate::version::Version;

// ---------------------------------------------------------------------------
// The captured model
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ModuleFile {
    pub module: Option<ModuleDecl>,
    pub deps: Vec<BazelDep>,
    pub extensions: Vec<ExtensionUsage>,
    pub toolchains: Vec<Registration>,
    pub execution_platforms: Vec<Registration>,
    pub overrides: Vec<Override>,
    pub repo_rules: Vec<RepoRuleUsage>,
    pub includes: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModuleDecl {
    pub name: String,
    pub version: Version,
    /// Captured for the record, but **do not gate on it**: `compatibility_level`
    /// is a no-op as of Bazel 8.6.0 / 9.1.0 and Bazel 9.2 warns it will be
    /// removed. Upstream dropped it because raising the level produced version
    /// conflicts end users could not resolve.
    ///
    /// 57 of 74 registry modules never declared it, and that is now the correct
    /// state. Requiring it would ask 56 repos to add a field Bazel is deleting.
    /// The invariant it used to provide — never co-select two incompatible
    /// versions — is now enforced by C1 (one live version per atom) instead.
    pub compatibility_level: Option<i64>,
    pub bazel_compatibility: Vec<String>,
    pub repo_name: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BazelDep {
    pub name: String,
    pub version: Version,
    pub max_compatibility_level: Option<i64>,
    /// Apparent repo name, when the dep is renamed for this module.
    pub repo_name: Option<String>,
    pub dev_dependency: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtensionUsage {
    /// The defining `.bzl`, e.g. `@rules_rust//rust:extensions.bzl` or a
    /// module-local `//tla:extensions.bzl`.
    pub bzl_file: String,
    pub name: String,
    pub dev_dependency: bool,
    pub isolate: bool,
    /// Tag-class calls on the proxy: `rust.toolchain(...)`, `maven.install(...)`.
    pub tags: Vec<Tag>,
    /// Repos pulled into scope by `use_repo`. The apparent names here are what
    /// D3 checks for collisions.
    pub repos: Vec<UseRepo>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tag {
    pub name: String,
    pub attrs: Vec<(String, TagValue)>,
}

/// A tag attribute, flattened to what the gates actually query. Structured
/// values beyond a list of strings collapse to `Opaque` with their `repr` —
/// enough to diff and display, without modelling all of Starlark.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TagValue {
    Bool(bool),
    Int(i64),
    Str(String),
    List(Vec<String>),
    Opaque(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UseRepo {
    /// The name visible inside this module.
    pub apparent: String,
    /// The name the extension generated. Equal to `apparent` unless renamed
    /// via `use_repo(ext, apparent = "actual")`.
    pub actual: String,
}

impl ExtensionUsage {
    /// Is this extension defined in a module *other than* `module`?
    ///
    /// `@rules_oci//oci:extensions.bzl` is foreign; a bare
    /// `//validate:extensions.bzl` is this module's own; and
    /// `@rules_bun//bun:extensions.bzl` read from inside `rules_bun` is a
    /// self-reference by absolute label, not a foreign extension. The
    /// distinction is the whole of D3: extension instances are keyed by the
    /// defining `.bzl`, so only a genuinely foreign extension can be shared
    /// with — and collide against — another module in the graph.
    pub fn is_foreign_to(&self, module: &str) -> bool {
        let Some(rest) = self.bzl_file.strip_prefix('@') else {
            return false; // `//pkg:ext.bzl` — this module's own
        };
        match rest.split_once("//") {
            // `@//pkg:ext.bzl` is the root module, i.e. this one.
            Some(("", _)) => false,
            Some((owner, _)) => owner != module,
            None => false,
        }
    }

    /// Repo names this module *chose*, as opposed to singletons the extension
    /// emits for everyone.
    ///
    /// `oci.pull(name = "distroless_static")` chooses a name and is therefore a
    /// collision risk. `rust.toolchain(edition = "2021")` followed by
    /// `use_repo(rust, "rust_toolchains")` does not — `rust_toolchains` is
    /// emitted by the extension regardless of who asks, so every module
    /// naming it is referring to the same intended repo.
    ///
    /// The test is whether some tag on this extension carries `name = <repo>`.
    pub fn module_chosen_repos(&self) -> impl Iterator<Item = &str> {
        self.tags
            .iter()
            .filter_map(|t| {
                t.attrs.iter().find_map(|(k, v)| match (k.as_str(), v) {
                    ("name", TagValue::Str(s)) => Some(s.as_str()),
                    _ => None,
                })
            })
            // Only names actually brought into scope matter; a tag whose repo is
            // never `use_repo`d is internal to the extension.
            .filter(|n| self.repos.iter().any(|r| r.actual == *n))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Registration {
    pub labels: Vec<String>,
    pub dev_dependency: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoRuleUsage {
    pub bzl_file: String,
    pub rule_name: String,
    pub dev_dependency: bool,
}

/// The `*_override` family. Every variant is an S4 violation when it appears in
/// a *published* module — overrides are honoured only in the root module, so a
/// dependency carrying one is at best dead weight and at worst a supply-chain
/// hole that silently does nothing for consumers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Override {
    SingleVersion {
        module_name: String,
        version: Version,
        registry: Option<String>,
    },
    MultipleVersion {
        module_name: String,
        versions: Vec<Version>,
        registry: Option<String>,
    },
    Archive {
        module_name: String,
        urls: Vec<String>,
        integrity: Option<String>,
        strip_prefix: Option<String>,
    },
    Git {
        module_name: String,
        remote: String,
        commit: Option<String>,
    },
    LocalPath {
        module_name: String,
        path: String,
    },
}

impl Override {
    pub fn module_name(&self) -> &str {
        match self {
            Override::SingleVersion { module_name, .. }
            | Override::MultipleVersion { module_name, .. }
            | Override::Archive { module_name, .. }
            | Override::Git { module_name, .. }
            | Override::LocalPath { module_name, .. } => module_name,
        }
    }

    /// The directive name as written, for diagnostics.
    pub fn directive(&self) -> &'static str {
        match self {
            Override::SingleVersion { .. } => "single_version_override",
            Override::MultipleVersion { .. } => "multiple_version_override",
            Override::Archive { .. } => "archive_override",
            Override::Git { .. } => "git_override",
            Override::LocalPath { .. } => "local_path_override",
        }
    }
}

impl ModuleFile {
    /// Non-dev deps only — the graph a consumer actually sees. C1/C2 and the
    /// TCB computation run over this; D1 uses the difference between this and
    /// [`ModuleFile::dev_deps`].
    pub fn runtime_deps(&self) -> impl Iterator<Item = &BazelDep> {
        self.deps.iter().filter(|d| !d.dev_dependency)
    }

    pub fn dev_deps(&self) -> impl Iterator<Item = &BazelDep> {
        self.deps.iter().filter(|d| d.dev_dependency)
    }

    /// Toolchain labels that propagate to every transitive consumer — the D2
    /// leakage set. `dev_dependency = True` registrations stop at this module.
    pub fn leaking_toolchains(&self) -> impl Iterator<Item = &str> {
        self.toolchains
            .iter()
            .filter(|r| !r.dev_dependency)
            .flat_map(|r| r.labels.iter().map(String::as_str))
    }

    /// Every apparent repo name this module introduces into its own scope.
    pub fn apparent_repo_names(&self) -> impl Iterator<Item = &str> {
        self.extensions
            .iter()
            .flat_map(|e| e.repos.iter().map(|r| r.apparent.as_str()))
    }

    /// D3 violations: repo names chosen by this module on a *foreign* extension
    /// that are not namespaced by the module's own name.
    ///
    /// Encodes the fix `rules_k8s` arrived at by hand after its 0.0.1 broke the
    /// first consumer — `oci.pull(name = "distroless_static")` became
    /// `oci.pull(name = "rules_k8s_distroless_static")`. The Bazel error it
    /// produces is:
    ///
    /// ```text
    /// A repo named distroless_static_linux_amd64 is already generated by this
    /// module extension
    /// ```
    ///
    /// Note the check is deliberately *local* — it needs no dependency closure.
    /// Namespacing by module name makes a collision impossible with any
    /// consumer, present or future, which is strictly stronger than checking
    /// today's graph.
    pub fn unnamespaced_extension_repos(&self) -> Vec<(&str, &str)> {
        let Some(module) = self.module.as_ref().map(|m| m.name.as_str()) else {
            return Vec::new();
        };
        self.extensions
            .iter()
            .filter(|e| e.is_foreign_to(module))
            .flat_map(|e| {
                e.module_chosen_repos()
                    .filter(move |r| !is_namespaced_by(r, module))
                    .map(move |r| (e.bzl_file.as_str(), r))
            })
            .collect()
    }
}

/// Is `repo` namespaced by `module`? Accepts the module name as a prefix, and
/// tolerates the common habit of dropping a leading `rules_`
/// (`rules_lora` → `lora_backend`).
fn is_namespaced_by(repo: &str, module: &str) -> bool {
    if repo == module || repo.starts_with(&format!("{module}_")) {
        return true;
    }
    match module.strip_prefix("rules_") {
        Some(short) if !short.is_empty() => {
            repo == short || repo.starts_with(&format!("{short}_"))
        }
        _ => false,
    }
}

// ---------------------------------------------------------------------------
// Evaluation sink
// ---------------------------------------------------------------------------

#[derive(ProvidesStaticType, Default)]
struct Sink {
    file: RefCell<ModuleFile>,
}

fn sink<'a, 'v>(eval: &'a Evaluator<'v, '_, '_>) -> anyhow::Result<&'a Sink> {
    eval.extra
        .and_then(|e| e.downcast_ref::<Sink>())
        .ok_or_else(|| anyhow::anyhow!("no Sink on the evaluator (internal error)"))
}

// ---------------------------------------------------------------------------
// The extension proxy
// ---------------------------------------------------------------------------

/// What `use_extension(...)` returns. Any attribute access on it yields a
/// [`TagCallable`], because tag classes are declared by the extension's own
/// `.bzl` — which is out of scope here — so the set of valid names is not
/// knowable from `MODULE.bazel` alone. Accepting all of them and recording the
/// call is both correct and what the gates need.
#[derive(Debug, ProvidesStaticType, NoSerialize, Allocative)]
struct ExtensionProxy {
    index: usize,
}
starlark_simple_value!(ExtensionProxy);

impl fmt::Display for ExtensionProxy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "<module_extension_proxy #{}>", self.index)
    }
}

#[starlark_value(type = "module_extension_proxy")]
impl<'v> StarlarkValue<'v> for ExtensionProxy {
    fn get_attr(&self, attribute: &str, heap: Heap<'v>) -> Option<Value<'v>> {
        Some(heap.alloc(TagCallable {
            index: self.index,
            tag: attribute.to_owned(),
        }))
    }

    fn has_attr(&self, _attribute: &str, _heap: Heap<'v>) -> bool {
        true
    }
}

#[derive(Debug, ProvidesStaticType, NoSerialize, Allocative)]
struct TagCallable {
    index: usize,
    tag: String,
}
starlark_simple_value!(TagCallable);

impl fmt::Display for TagCallable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "<module_extension_tag {}>", self.tag)
    }
}

#[starlark_value(type = "module_extension_tag")]
impl<'v> StarlarkValue<'v> for TagCallable {
    fn invoke(
        &self,
        _me: Value<'v>,
        args: &Arguments<'v, '_>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<Value<'v>> {
        let attrs = args
            .names_map()?
            .iter()
            .map(|(k, v)| (k.as_str().to_owned(), tag_value(*v)))
            .collect();
        let s = sink(eval)?;
        s.file.borrow_mut().extensions[self.index].tags.push(Tag {
            name: self.tag.clone(),
            attrs,
        });
        Ok(Value::new_none())
    }
}

/// A `use_repo_rule(...)` result. Invoking it declares a repo; the gate records
/// the declaration but does not model repo-rule semantics.
#[derive(Debug, ProvidesStaticType, NoSerialize, Allocative)]
struct RepoRuleCallable {
    index: usize,
}
starlark_simple_value!(RepoRuleCallable);

impl fmt::Display for RepoRuleCallable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "<repo_rule #{}>", self.index)
    }
}

#[starlark_value(type = "repo_rule")]
impl<'v> StarlarkValue<'v> for RepoRuleCallable {
    fn invoke(
        &self,
        _me: Value<'v>,
        args: &Arguments<'v, '_>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<Value<'v>> {
        // A repo rule invocation names a repo; surface it as an apparent name so
        // D3 sees it alongside `use_repo` names.
        let named = args.names_map()?;
        if let Some(name) = named
            .iter()
            .find(|(k, _)| k.as_str() == "name")
            .and_then(|(_, v)| v.unpack_str())
        {
            let s = sink(eval)?;
            let mut f = s.file.borrow_mut();
            let bzl = f.repo_rules[self.index].bzl_file.clone();
            let rule = f.repo_rules[self.index].rule_name.clone();
            let dev = f.repo_rules[self.index].dev_dependency;
            f.extensions.push(ExtensionUsage {
                bzl_file: bzl,
                name: rule,
                dev_dependency: dev,
                isolate: false,
                tags: Vec::new(),
                repos: vec![UseRepo {
                    apparent: name.to_owned(),
                    actual: name.to_owned(),
                }],
            });
        }
        Ok(Value::new_none())
    }
}

// ---------------------------------------------------------------------------
// Value coercion
// ---------------------------------------------------------------------------

fn tag_value(v: Value<'_>) -> TagValue {
    if let Some(b) = v.unpack_bool() {
        return TagValue::Bool(b);
    }
    if let Some(s) = v.unpack_str() {
        return TagValue::Str(s.to_owned());
    }
    if let Some(i) = v.unpack_i32() {
        return TagValue::Int(i as i64);
    }
    if let Some(list) = starlark::values::list::ListRef::from_value(v) {
        // Only homogeneous string lists are modelled; anything else is opaque.
        let strs: Option<Vec<String>> = list
            .iter()
            .map(|e| e.unpack_str().map(str::to_owned))
            .collect();
        if let Some(strs) = strs {
            return TagValue::List(strs);
        }
    }
    TagValue::Opaque(v.to_repr())
}

/// Flatten a Starlark value to a list of strings. Accepts a bare string, a
/// list, or a tuple — `*args` arrives as a tuple, and directives like
/// `bazel_compatibility` take a list.
fn string_list(v: Option<Value<'_>>) -> Vec<String> {
    let Some(v) = v else { return Vec::new() };
    if let Some(s) = v.unpack_str() {
        return vec![s.to_owned()];
    }
    if let Some(list) = starlark::values::list::ListRef::from_value(v) {
        return list
            .iter()
            .filter_map(|e| e.unpack_str().map(str::to_owned))
            .collect();
    }
    if let Some(tuple) = starlark::values::tuple::TupleRef::from_value(v) {
        return tuple
            .iter()
            .filter_map(|e| e.unpack_str().map(str::to_owned))
            .collect();
    }
    Vec::new()
}

/// Coerce an optional Starlark argument to a string.
///
/// Takes `Option<Value>` rather than `Option<String>` because real
/// `MODULE.bazel` files pass an EXPLICIT `None` for optional parameters —
/// `aspect_rules_js` writes `bazel_dep(..., repo_name = None)`. A typed
/// `Option<String>` parameter means "may be omitted", and starlark-rust
/// correctly rejects an explicit `None` as a type error. Treating the two
/// alike is what a permissive registry reader has to do.
fn opt_string(v: Option<Value<'_>>) -> Option<String> {
    v.and_then(|v| v.unpack_str())
        .map(str::to_owned)
        .filter(|s| !s.is_empty())
}

/// Same tolerance for integer parameters.
fn opt_i64(v: Option<Value<'_>>) -> Option<i64> {
    v.and_then(|v| v.unpack_i32()).map(i64::from)
}

/// Same tolerance for a required-with-default string.
fn str_or(v: Option<Value<'_>>, default: &str) -> String {
    opt_string(v).unwrap_or_else(|| default.to_owned())
}

fn parse_version(v: Option<Value<'_>>) -> anyhow::Result<Version> {
    match opt_string(v) {
        Some(s) => Ok(Version::parse(&s)?),
        None => Ok(Version::empty()),
    }
}

// ---------------------------------------------------------------------------
// The directive globals
// ---------------------------------------------------------------------------

#[starlark_module]
fn module_globals(globals: &mut GlobalsBuilder) {
    fn module<'v>(
        #[starlark(require = named)] name: Option<Value<'v>>,
        #[starlark(require = named)] version: Option<Value<'v>>,
        #[starlark(require = named)] compatibility_level: Option<Value<'v>>,
        #[starlark(require = named)] bazel_compatibility: Option<Value<'v>>,
        #[starlark(require = named)] repo_name: Option<Value<'v>>,
        #[starlark(kwargs)] _extra: Value<'v>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<NoneType> {
        let decl = ModuleDecl {
            name: str_or(name, ""),
            version: parse_version(version)?,
            compatibility_level: opt_i64(compatibility_level),
            bazel_compatibility: string_list(bazel_compatibility),
            repo_name: opt_string(repo_name),
        };
        sink(eval)?.file.borrow_mut().module = Some(decl);
        Ok(NoneType)
    }

    fn bazel_dep<'v>(
        #[starlark(require = named)] name: Option<Value<'v>>,
        #[starlark(require = named)] version: Option<Value<'v>>,
        #[starlark(require = named)] max_compatibility_level: Option<Value<'v>>,
        #[starlark(require = named)] repo_name: Option<Value<'v>>,
        #[starlark(require = named, default = false)] dev_dependency: bool,
        #[starlark(kwargs)] _extra: Value<'v>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<NoneType> {
        let dep = BazelDep {
            name: str_or(name, ""),
            version: parse_version(version)?,
            max_compatibility_level: opt_i64(max_compatibility_level),
            repo_name: opt_string(repo_name),
            dev_dependency,
        };
        sink(eval)?.file.borrow_mut().deps.push(dep);
        Ok(NoneType)
    }

    fn use_extension<'v>(
        #[starlark(require = pos)] bzl_file: Option<Value<'v>>,
        #[starlark(require = pos)] name: Option<Value<'v>>,
        #[starlark(require = named, default = false)] dev_dependency: bool,
        #[starlark(require = named, default = false)] isolate: bool,
        #[starlark(kwargs)] _extra: Value<'v>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<ExtensionProxy> {
        let s = sink(eval)?;
        let mut f = s.file.borrow_mut();
        let index = f.extensions.len();
        f.extensions.push(ExtensionUsage {
            bzl_file: str_or(bzl_file, ""),
            name: str_or(name, ""),
            dev_dependency,
            isolate,
            tags: Vec::new(),
            repos: Vec::new(),
        });
        Ok(ExtensionProxy { index })
    }

    fn use_repo<'v>(
        #[starlark(require = pos)] proxy: Value<'v>,
        #[starlark(args)] names: Value<'v>,
        #[starlark(kwargs)] renames: Value<'v>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<NoneType> {
        let index = proxy
            .downcast_ref::<ExtensionProxy>()
            .map(|p| p.index)
            .ok_or_else(|| {
                anyhow::anyhow!("use_repo: first argument must be a use_extension(...) proxy")
            })?;

        let mut repos: Vec<UseRepo> = string_list(Some(names))
            .into_iter()
            .map(|n| UseRepo {
                apparent: n.clone(),
                actual: n,
            })
            .collect();
        if let Some(dict) = starlark::values::dict::DictRef::from_value(renames) {
            for (k, v) in dict.iter() {
                if let (Some(apparent), Some(actual)) = (k.unpack_str(), v.unpack_str()) {
                    repos.push(UseRepo {
                        apparent: apparent.to_owned(),
                        actual: actual.to_owned(),
                    });
                }
            }
        }

        sink(eval)?.file.borrow_mut().extensions[index]
            .repos
            .extend(repos);
        Ok(NoneType)
    }

    fn use_repo_rule<'v>(
        #[starlark(require = pos)] bzl_file: Option<Value<'v>>,
        #[starlark(require = pos)] rule_name: Option<Value<'v>>,
        #[starlark(require = named, default = false)] dev_dependency: bool,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<RepoRuleCallable> {
        let s = sink(eval)?;
        let mut f = s.file.borrow_mut();
        let index = f.repo_rules.len();
        f.repo_rules.push(RepoRuleUsage {
            bzl_file: str_or(bzl_file, ""),
            rule_name: str_or(rule_name, ""),
            dev_dependency,
        });
        Ok(RepoRuleCallable { index })
    }

    fn register_toolchains<'v>(
        #[starlark(args)] labels: Value<'v>,
        #[starlark(require = named, default = false)] dev_dependency: bool,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<NoneType> {
        sink(eval)?.file.borrow_mut().toolchains.push(Registration {
            labels: string_list(Some(labels)),
            dev_dependency,
        });
        Ok(NoneType)
    }

    fn register_execution_platforms<'v>(
        #[starlark(args)] labels: Value<'v>,
        #[starlark(require = named, default = false)] dev_dependency: bool,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<NoneType> {
        sink(eval)?
            .file
            .borrow_mut()
            .execution_platforms
            .push(Registration {
                labels: string_list(Some(labels)),
                dev_dependency,
            });
        Ok(NoneType)
    }

    fn single_version_override<'v>(
        #[starlark(require = named)] module_name: Option<Value<'v>>,
        #[starlark(require = named)] version: Option<Value<'v>>,
        #[starlark(require = named)] registry: Option<Value<'v>>,
        #[starlark(kwargs)] _extra: Value<'v>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<NoneType> {
        sink(eval)?
            .file
            .borrow_mut()
            .overrides
            .push(Override::SingleVersion {
                module_name: str_or(module_name, ""),
                version: parse_version(version)?,
                registry: opt_string(registry),
            });
        Ok(NoneType)
    }

    fn multiple_version_override<'v>(
        #[starlark(require = named)] module_name: Option<Value<'v>>,
        #[starlark(require = named)] versions: Option<Value<'v>>,
        #[starlark(require = named)] registry: Option<Value<'v>>,
        #[starlark(kwargs)] _extra: Value<'v>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<NoneType> {
        let versions = string_list(versions)
            .iter()
            .map(|v| Version::parse(v))
            .collect::<Result<Vec<_>, _>>()?;
        sink(eval)?
            .file
            .borrow_mut()
            .overrides
            .push(Override::MultipleVersion {
                module_name: str_or(module_name, ""),
                versions,
                registry: opt_string(registry),
            });
        Ok(NoneType)
    }

    fn archive_override<'v>(
        #[starlark(require = named)] module_name: Option<Value<'v>>,
        #[starlark(require = named)] urls: Option<Value<'v>>,
        #[starlark(require = named)] integrity: Option<Value<'v>>,
        #[starlark(require = named)] strip_prefix: Option<Value<'v>>,
        #[starlark(kwargs)] _extra: Value<'v>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<NoneType> {
        sink(eval)?
            .file
            .borrow_mut()
            .overrides
            .push(Override::Archive {
                module_name: str_or(module_name, ""),
                urls: string_list(urls),
                integrity: opt_string(integrity),
                strip_prefix: opt_string(strip_prefix),
            });
        Ok(NoneType)
    }

    fn git_override<'v>(
        #[starlark(require = named)] module_name: Option<Value<'v>>,
        #[starlark(require = named)] remote: Option<Value<'v>>,
        #[starlark(require = named)] commit: Option<Value<'v>>,
        #[starlark(kwargs)] _extra: Value<'v>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<NoneType> {
        sink(eval)?.file.borrow_mut().overrides.push(Override::Git {
            module_name: str_or(module_name, ""),
            remote: str_or(remote, ""),
            commit: opt_string(commit),
        });
        Ok(NoneType)
    }

    fn local_path_override<'v>(
        #[starlark(require = named)] module_name: Option<Value<'v>>,
        #[starlark(require = named)] path: Option<Value<'v>>,
        #[starlark(kwargs)] _extra: Value<'v>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<NoneType> {
        sink(eval)?
            .file
            .borrow_mut()
            .overrides
            .push(Override::LocalPath {
                module_name: str_or(module_name, ""),
                path: str_or(path, ""),
            });
        Ok(NoneType)
    }

    fn include<'v>(
        #[starlark(require = pos)] label: Option<Value<'v>>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<NoneType> {
        sink(eval)?.file.borrow_mut().includes.push(str_or(label, ""));
        Ok(NoneType)
    }
}

fn globals() -> Globals {
    use starlark::environment::LibraryExtension::*;
    let mut b = GlobalsBuilder::standard();
    // `print` and `json` show up in real MODULE.bazel files; `struct` does not,
    // but costs nothing and keeps the dialect closer to Bazel's.
    for ext in [StructType, Map, Filter, Print, Json] {
        ext.add(&mut b);
    }
    module_globals(&mut b);
    b.build()
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

/// The MODULE.bazel dialect.
///
/// Note this is *narrower* than `Dialect::Standard`, which is the Starlark
/// language spec and therefore permits `def` and `load`. Bazel rejects both in
/// `MODULE.bazel`, and so must we: it is what bounds evaluation to something
/// safe to run on untrusted registry content. A file that needs `def` is not a
/// MODULE.bazel, and accepting it would mean executing arbitrary logic during
/// admission.
fn module_dialect() -> Dialect {
    Dialect {
        enable_def: false,
        enable_lambda: false,
        enable_load: false,
        enable_load_reexport: false,
        enable_keyword_only_arguments: false,
        enable_positional_only_arguments: false,
        enable_types: starlark::syntax::DialectTypes::Disable,
        enable_f_strings: false,
        enable_top_level_stmt: true,
        ..Dialect::Standard
    }
}

/// Evaluate a `MODULE.bazel`'s source text.
///
/// `origin` is used only in diagnostics — pass the path the text came from.
pub fn parse(origin: &str, text: &str) -> anyhow::Result<ModuleFile> {
    let ast = AstModule::parse(origin, text.to_owned(), &module_dialect())
        .map_err(|e| anyhow::anyhow!("{origin}: {e}"))?;

    let globals = globals();
    let sink = Sink::default();
    starlark::environment::Module::with_temp_heap(|module| -> anyhow::Result<()> {
        let mut eval = Evaluator::new(&module);
        eval.extra = Some(&sink);
        eval.eval_module(ast, &globals)
            .map_err(|e| anyhow::anyhow!("{origin}: {e}"))?;
        Ok(())
    })?;
    Ok(sink.file.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(text: &str) -> ModuleFile {
        parse("test/MODULE.bazel", text).expect("parse")
    }

    #[test]
    fn captures_module_and_deps() {
        let f = p(r#"
module(
    name = "rules_tla",
    version = "0.1.0",
    compatibility_level = 1,
)
bazel_dep(name = "platforms", version = "1.0.0")
bazel_dep(name = "bazel_skylib", version = "1.8.2")
"#);
        let m = f.module.as_ref().unwrap();
        assert_eq!(m.name, "rules_tla");
        assert_eq!(m.version.to_string(), "0.1.0");
        assert_eq!(m.compatibility_level, Some(1));
        assert_eq!(f.deps.len(), 2);
        assert_eq!(f.deps[0].name, "platforms");
        assert!(!f.deps[0].dev_dependency);
    }

    #[test]
    fn compatibility_level_absent_is_none() {
        // The V1 premise: most modules never declare it.
        let f = p(r#"module(name = "rules_k8s", version = "0.0.3")"#);
        assert_eq!(f.module.unwrap().compatibility_level, None);
    }

    #[test]
    fn separates_dev_dependencies() {
        // This is the D1 input. The regex parsers drop `dev_dependency`
        // entirely, which makes the dev-dep soundness gate unbuildable on them.
        let f = p(r#"
bazel_dep(name = "bazel_skylib", version = "1.8.2")
bazel_dep(name = "stardoc", version = "0.7.2", dev_dependency = True)
"#);
        assert_eq!(f.runtime_deps().count(), 1);
        assert_eq!(f.dev_deps().count(), 1);
        assert_eq!(f.dev_deps().next().unwrap().name, "stardoc");
    }

    #[test]
    fn handles_multiline_and_commented_calls() {
        // The precise shape the incumbent single-line regexes miss.
        let f = p(r#"
bazel_dep(
    # a comment in the middle of the call
    name = "aspect_bazel_lib",
    version = "2.22.5",
)
"#);
        assert_eq!(f.deps.len(), 1);
        assert_eq!(f.deps[0].name, "aspect_bazel_lib");
        assert_eq!(f.deps[0].version.to_string(), "2.22.5");
    }

    #[test]
    fn evaluates_rather_than_scrapes() {
        // Concatenation and a comprehension: only evaluation gets these right.
        let f = p(r#"
_PREFIX = "rules_"
bazel_dep(name = _PREFIX + "cc", version = "0.0.17")
[register_toolchains("//tc:" + n) for n in ["a", "b"]]
"#);
        assert_eq!(f.deps[0].name, "rules_cc");
        let labels: Vec<&str> = f.leaking_toolchains().collect();
        assert_eq!(labels, vec!["//tc:a", "//tc:b"]);
    }

    #[test]
    fn toolchain_registration_dev_flag_is_the_d2_input() {
        let f = p(r#"
register_toolchains("@leaks//:all")
register_toolchains("@contained//:all", dev_dependency = True)
"#);
        // Only the non-dev registration propagates to consumers.
        let leaking: Vec<&str> = f.leaking_toolchains().collect();
        assert_eq!(leaking, vec!["@leaks//:all"]);
    }

    #[test]
    fn captures_extension_tags_and_repos() {
        // Mirrors rules_lean's real lake extension usage.
        let f = p(r#"
lake = use_extension("//lean:lake.bzl", "lake", dev_dependency = True)
lake.workspace(
    name = "lake_deps_smoke",
    allow_source_build = True,
)
use_repo(lake, "lake_deps_smoke")
"#);
        assert_eq!(f.extensions.len(), 1);
        let e = &f.extensions[0];
        assert_eq!(e.bzl_file, "//lean:lake.bzl");
        assert_eq!(e.name, "lake");
        assert!(e.dev_dependency);
        assert_eq!(e.tags.len(), 1);
        assert_eq!(e.tags[0].name, "workspace");
        assert!(e.tags[0]
            .attrs
            .contains(&("allow_source_build".to_owned(), TagValue::Bool(true))));
        assert_eq!(f.apparent_repo_names().collect::<Vec<_>>(), vec!["lake_deps_smoke"]);
    }

    #[test]
    fn use_repo_records_renames() {
        let f = p(r#"
ext = use_extension("//x:e.bzl", "ext")
use_repo(ext, "plain", local = "upstream")
"#);
        let repos = &f.extensions[0].repos;
        assert!(repos.contains(&UseRepo {
            apparent: "plain".into(),
            actual: "plain".into()
        }));
        assert!(repos.contains(&UseRepo {
            apparent: "local".into(),
            actual: "upstream".into()
        }));
    }

    #[test]
    fn d3_flags_a_module_chosen_name_on_a_foreign_extension() {
        // rules_k8s 0.0.1, verbatim shape — the version that broke its first
        // consumer with "a repo named distroless_static_linux_amd64 is already
        // generated by this module extension".
        let f = p(r#"
module(name = "rules_k8s", version = "0.0.1")
oci = use_extension("@rules_oci//oci:extensions.bzl", "oci")
oci.pull(
    name = "distroless_static",
    image = "gcr.io/distroless/static",
)
use_repo(oci, "distroless_static")
"#);
        assert_eq!(
            f.unnamespaced_extension_repos(),
            vec![("@rules_oci//oci:extensions.bzl", "distroless_static")]
        );
    }

    #[test]
    fn d3_accepts_the_namespaced_fix() {
        // rules_k8s 0.0.3 — the shipped fix.
        let f = p(r#"
module(name = "rules_k8s", version = "0.0.3")
oci = use_extension("@rules_oci//oci:extensions.bzl", "oci")
oci.pull(name = "rules_k8s_distroless_static", image = "gcr.io/distroless/static")
use_repo(oci, "rules_k8s_distroless_static")
"#);
        assert!(f.unnamespaced_extension_repos().is_empty());
    }

    #[test]
    fn d3_ignores_extension_singletons() {
        // `rust_toolchains` is emitted by the rules_rust extension for everyone;
        // no tag names it, so no module is choosing it and there is nothing to
        // collide. Flagging this would condemn the documented rules_rust idiom
        // used by 20+ modules in the registry.
        let f = p(r#"
module(name = "fvkit", version = "0.0.2")
rust = use_extension("@rules_rust//rust:extensions.bzl", "rust")
rust.toolchain(edition = "2021")
use_repo(rust, "rust_toolchains")
"#);
        assert!(f.unnamespaced_extension_repos().is_empty());
    }

    #[test]
    fn d3_ignores_a_self_reference_by_absolute_label() {
        // rules_bun refers to its own extension as `@rules_bun//bun:...`.
        // That is the same extension identity as `//bun:...`, so it cannot
        // collide with another module.
        let f = p(r#"
module(name = "rules_bun", version = "0.4.0")
bun = use_extension("@rules_bun//bun:extensions.bzl", "bun")
bun.install(name = "install_npm")
use_repo(bun, "install_npm")
"#);
        assert!(f.unnamespaced_extension_repos().is_empty());
    }

    #[test]
    fn d3_ignores_a_modules_own_extension() {
        // A local extension cannot be shared, so its repo names cannot collide
        // with another module's — `//validate:extensions.bzl` in rules_k8s is a
        // different extension identity from any other module's local one.
        let f = p(r#"
module(name = "rules_k8s", version = "0.0.3")
kubeconform = use_extension("//validate:extensions.bzl", "kubeconform")
kubeconform.fetch(name = "kubeconform")
use_repo(kubeconform, "kubeconform")
"#);
        assert!(f.unnamespaced_extension_repos().is_empty());
    }

    #[test]
    fn d3_accepts_dropping_a_rules_prefix() {
        // rules_jena names its maven install `jena_maven`, not `rules_jena_maven`.
        let f = p(r#"
module(name = "rules_jena", version = "0.3.2")
maven = use_extension("@rules_jvm_external//:extensions.bzl", "maven")
maven.install(name = "jena_maven", artifacts = ["org.apache.jena:jena-arq:5.2.0"])
use_repo(maven, "jena_maven")
"#);
        assert!(f.unnamespaced_extension_repos().is_empty());
    }

    #[test]
    fn captures_every_override_variant() {
        // S4: none of these belong in a published module.
        let f = p(r#"
single_version_override(module_name = "a", version = "1.0.0")
archive_override(module_name = "b", urls = ["https://e/b.tar.gz"])
git_override(module_name = "c", remote = "https://e/c.git", commit = "deadbeef")
local_path_override(module_name = "d", path = "../d")
multiple_version_override(module_name = "e", versions = ["1.0.0", "2.0.0"])
"#);
        let names: Vec<&str> = f.overrides.iter().map(|o| o.directive()).collect();
        assert_eq!(
            names,
            vec![
                "single_version_override",
                "archive_override",
                "git_override",
                "local_path_override",
                "multiple_version_override",
            ]
        );
        assert_eq!(f.overrides[3].module_name(), "d");
    }

    #[test]
    fn tolerates_unmodelled_kwargs_on_known_directives() {
        // Bazel adds params over time; a gate must not fail to parse over one.
        let f = p(r#"bazel_dep(name = "x", version = "1.0.0", some_future_kwarg = 3)"#);
        assert_eq!(f.deps[0].name, "x");
    }

    #[test]
    fn rejects_an_unknown_directive() {
        // The opposite policy: an unmodelled *directive* would mean reasoning
        // about an incomplete graph, so it must be loud.
        let err = parse("t", r#"totally_new_directive(name = "x")"#).unwrap_err();
        assert!(
            err.to_string().contains("totally_new_directive"),
            "unhelpful error: {err}"
        );
    }

    #[test]
    fn rejects_load_and_def() {
        // MODULE.bazel is a restricted dialect; both are errors in Bazel too.
        assert!(parse("t", r#"load("//:x.bzl", "y")"#).is_err());
        assert!(parse("t", "def f():\n    pass\n").is_err());
    }
}
