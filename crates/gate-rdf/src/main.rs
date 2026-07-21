//! `project` — emit the registry's facts as Turtle.
//!
//!     project <registry-root> [out.ttl] [--allow-unresolved]
//!
//! Writes to stdout when no output path is given. Unresolvable modules go to
//! stderr and set a non-zero exit by default, because a module the projector
//! cannot read is a hole in every gate that follows — it must not pass
//! silently.
//!
//! `--allow-unresolved` downgrades that to a warning, for the reporting path.
//! The registry genuinely contains an unresolvable module today
//! (`rules_cc_host` has a metadata.json but no version directories, because its
//! only version was yanked), and a periodic report should surface that rather
//! than fail forever. The proper home for it is an S6 gate on metadata/directory
//! drift — this flag is the interim, not the answer.

use bzlmod::registry::FsRegistry;

fn main() -> anyhow::Result<()> {
    let args = std::env::args().skip(1);
    let mut positional: Vec<String> = Vec::new();
    let mut allow_unresolved = false;
    for a in args {
        if a == "--allow-unresolved" {
            allow_unresolved = true;
        } else {
            positional.push(a);
        }
    }
    let mut positional = positional.into_iter();
    let root = positional.next().ok_or_else(|| {
        anyhow::anyhow!("usage: project <registry-root> [out.ttl] [--allow-unresolved]")
    })?;
    let out_path = positional.next();

    let reg = FsRegistry::open(&root)?;
    let (ttl, unresolved) = gate_rdf::project_registry(&reg)?;

    match &out_path {
        Some(p) => std::fs::write(p, &ttl)?,
        None => print!("{ttl}"),
    }

    if !unresolved.is_empty() {
        for u in &unresolved {
            eprintln!("unresolved: {u}");
        }
        if !allow_unresolved {
            anyhow::bail!(
                "{} module(s) could not be projected (pass --allow-unresolved to \
                 downgrade to a warning)",
                unresolved.len()
            );
        }
        eprintln!(
            "warning: {} module(s) could not be projected; the report is incomplete",
            unresolved.len()
        );
    }
    Ok(())
}
