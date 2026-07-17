//! Installed packages. A local fs + git operation — no daemon round-trip.

use anyhow::{Result, anyhow};

use crate::cli::PkgCmd;

fn packages_root() -> Result<std::path::PathBuf> {
    Ok(dirs::data_dir()
        .ok_or_else(|| anyhow!("could not locate a data directory (XDG) on this system"))?
        .join("agentd")
        .join("packages"))
}

pub(crate) fn run_packages(cmd: PkgCmd) -> Result<()> {
    let root = packages_root()?;
    let index_path = root.join("index.toml");
    let mut index = agentd_packages::PackageIndex::load(&index_path).map_err(|e| anyhow!(e))?;

    match cmd {
        PkgCmd::Ls => {
            for e in index.iter() {
                let dir = root.join(&e.name);
                let upd = agentd_packages::update_check(e, &dir).unwrap_or(false);
                println!(
                    "{}\t{}\t{}{}",
                    e.name,
                    &e.commit[..e.commit.len().min(8)],
                    e.r#ref,
                    if upd { "\t(update available)" } else { "" }
                );
            }
        }
        PkgCmd::Install { url, r#ref } => {
            // Clone to a scratch dir to discover the manifest name.
            let scratch = tempfile::tempdir()?;
            agentd_packages::install(&url, r#ref.as_deref(), scratch.path(), "_probe")?;
            let manifest = agentd_packages::Manifest::load(
                &scratch.path().join("_probe").join("package.toml"),
            )?;
            let name = manifest.name.clone();
            // Real install under the manifest name.
            let entry = agentd_packages::install(&url, r#ref.as_deref(), &root, &name)?;
            index.set(entry);
            index.save(&index_path).map_err(|e| anyhow!(e))?;
            println!("installed {name}");
            if !manifest.permissions.is_empty() {
                println!(
                    "declares permissions (add `[package.{name}] trusted = true` to grants.toml to approve):"
                );
                for p in &manifest.permissions {
                    println!("  {p}");
                }
            }
        }
        PkgCmd::Update { name } => {
            let entry = index
                .get(&name)
                .ok_or_else(|| anyhow!("no package named `{name}` is installed — run `agentctl pkg ls` to see what is"))?
                .clone();
            let dir = root.join(&name);
            let commit = agentd_packages::update(&entry, &dir)?;
            let mut e = entry;
            e.commit = commit;
            index.set(e);
            index.save(&index_path).map_err(|e| anyhow!(e))?;
            println!("updated {name}");
        }
        PkgCmd::Remove { name } => {
            let dir = root.join(&name);
            if dir.exists() {
                std::fs::remove_dir_all(&dir)?;
            }
            index.remove(&name);
            index.save(&index_path).map_err(|e| anyhow!(e))?;
            println!("removed {name}");
        }
    }
    Ok(())
}
