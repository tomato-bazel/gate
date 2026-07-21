//! `project` — emit the registry's facts as Turtle.
//!
//!     project <registry-root> [out.ttl]
//!
//! Writes to stdout when no output path is given. Unresolvable modules go to
//! stderr and set a non-zero exit, because a module the projector cannot read
//! is a hole in every gate that follows — it must not pass silently.

use bzlmod::registry::FsRegistry;

fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let root = args
        .next()
        .ok_or_else(|| anyhow::anyhow!("usage: project <registry-root> [out.ttl]"))?;
    let out_path = args.next();

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
        anyhow::bail!("{} module(s) could not be projected", unresolved.len());
    }
    Ok(())
}
