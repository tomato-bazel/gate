//! Corpus test: every `MODULE.bazel` in the real registry must parse.
//!
//! This is the M1 acceptance criterion. A registry gate that cannot read the
//! registry is worthless, and the failure mode we care about is silent: a
//! parser that *drops* a directive still "succeeds". So this test asserts both
//! that every file parses AND that the aggregate directive counts are non-zero
//! for each family the gates depend on — if a future refactor stops capturing
//! `dev_dependency` or `register_toolchains`, the corpus totals move and this
//! test fails.
//!
//! Skipped (not failed) when the registry checkout is absent, so the crate
//! stays buildable standalone. Point `BZLMOD_REGISTRY` at a registry root to
//! override the default location.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

fn registry_root() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("BZLMOD_REGISTRY") {
        let p = PathBuf::from(p);
        return p.join("modules").is_dir().then_some(p);
    }
    // The sibling checkout in the tomato-bazel org root.
    let default = Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(3)?
        .join("bazel-registry");
    default.join("modules").is_dir().then_some(default)
}

struct Totals {
    files: usize,
    deps: usize,
    dev_deps: usize,
    extensions: usize,
    tags: usize,
    use_repos: usize,
    toolchains: usize,
    leaking_toolchains: usize,
    overrides: usize,
    compat_level: usize,
}

#[test]
fn every_registry_module_bazel_parses() {
    let Some(root) = registry_root() else {
        eprintln!("skipping: no bazel-registry checkout found");
        return;
    };
    let modules = root.join("modules");

    let mut failures: Vec<(PathBuf, String)> = Vec::new();
    let mut t = Totals {
        files: 0,
        deps: 0,
        dev_deps: 0,
        extensions: 0,
        tags: 0,
        use_repos: 0,
        toolchains: 0,
        leaking_toolchains: 0,
        overrides: 0,
        compat_level: 0,
    };
    // Which modules declare a compatibility_level, for the V1 baseline.
    let mut declares_compat: BTreeMap<String, bool> = BTreeMap::new();

    let mut module_dirs: Vec<PathBuf> = std::fs::read_dir(&modules)
        .expect("read modules/")
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    module_dirs.sort();

    for module_dir in &module_dirs {
        let module_name = module_dir.file_name().unwrap().to_string_lossy().to_string();
        let mut version_dirs: Vec<PathBuf> = std::fs::read_dir(module_dir)
            .expect("read module dir")
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.is_dir())
            .collect();
        version_dirs.sort();

        for version_dir in &version_dirs {
            let path = version_dir.join("MODULE.bazel");
            if !path.is_file() {
                continue;
            }
            let text = std::fs::read_to_string(&path).expect("read MODULE.bazel");
            match bzlmod::parse(&path.to_string_lossy(), &text) {
                Ok(f) => {
                    t.files += 1;
                    t.deps += f.runtime_deps().count();
                    t.dev_deps += f.dev_deps().count();
                    t.extensions += f.extensions.len();
                    t.tags += f.extensions.iter().map(|e| e.tags.len()).sum::<usize>();
                    t.use_repos += f.apparent_repo_names().count();
                    t.toolchains += f.toolchains.len();
                    t.leaking_toolchains += f.leaking_toolchains().count();
                    t.overrides += f.overrides.len();
                    if let Some(m) = &f.module {
                        if m.compatibility_level.is_some() {
                            t.compat_level += 1;
                            declares_compat.insert(module_name.clone(), true);
                        } else {
                            declares_compat.entry(module_name.clone()).or_insert(false);
                        }
                    }
                }
                Err(e) => failures.push((path.clone(), e.to_string())),
            }
        }
    }

    eprintln!(
        "corpus: {} files parsed across {} modules\n  \
         deps={} dev_deps={} extensions={} tags={} use_repo_names={}\n  \
         toolchain_registrations={} (leaking={}) overrides={} entries_with_compatibility_level={}",
        t.files,
        module_dirs.len(),
        t.deps,
        t.dev_deps,
        t.extensions,
        t.tags,
        t.use_repos,
        t.toolchains,
        t.leaking_toolchains,
        t.overrides,
        t.compat_level,
    );
    let with = declares_compat.values().filter(|v| **v).count();
    eprintln!(
        "  modules declaring compatibility_level: {}/{}",
        with,
        declares_compat.len()
    );

    if !failures.is_empty() {
        for (p, e) in failures.iter().take(20) {
            eprintln!("FAIL {}: {}", p.display(), e);
        }
        panic!("{} of {} MODULE.bazel files failed to parse", failures.len(), t.files + failures.len());
    }

    // The registry is large enough that all of these must be non-zero. A zero
    // here means a directive family silently stopped being captured.
    assert!(t.files >= 300, "expected the full corpus, got {}", t.files);
    assert!(t.deps > 0, "no runtime deps captured");
    assert!(t.dev_deps > 0, "no dev deps captured");
    assert!(t.extensions > 0, "no module extensions captured");
    assert!(t.tags > 0, "no extension tags captured");
    assert!(t.use_repos > 0, "no use_repo names captured");
    assert!(t.toolchains > 0, "no toolchain registrations captured");
    assert!(t.compat_level > 0, "no compatibility_level captured");
}
