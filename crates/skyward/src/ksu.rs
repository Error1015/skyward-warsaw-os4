//! KernelSU's modules in the data partition, as the modules page lists
//! them and turns them on and off. A module starts with the system unless
//! a file named `disable` stands in its folder, so switching one makes or
//! takes that file away, to hold from the next time the system starts.

use anyhow::{Context, Result};
use skyward_ui::app::KsuModule;
use std::fs;
use std::path::Path;

/// Where KernelSU keeps the modules installed, in the data partition.
const MODULES: &str = "/data/adb/modules";
/// Where an update to one waits, to replace it the next time the system
/// starts.
const UPDATES: &str = "/data/adb/modules_update";

/// The modules as they are now, one to each folder of `MODULES`, by their
/// ids in that order.
pub fn read() -> Result<Vec<KsuModule>, String> {
    read_at().map_err(|e| format!("{e:#}"))
}

fn read_at() -> Result<Vec<KsuModule>> {
    let dir = Path::new(MODULES);
    let mut found = Vec::new();
    for entry in fs::read_dir(dir).with_context(|| format!("read {}", dir.display()))? {
        let path = entry.with_context(|| format!("read {}", dir.display()))?.path();
        if !path.is_dir() {
            continue;
        }
        let id = path.file_name().and_then(|n| n.to_str()).with_context(|| format!("{} names no module", path.display()))?.to_string();
        let prop = path.join("module.prop");
        let text = fs::read_to_string(&prop).with_context(|| format!("read {}", prop.display()))?;
        let prop = |wanted: &str| {
            text.lines().find_map(|line| {
                let (key, value) = line.split_once('=')?;
                (key.trim() == wanted).then(|| value.trim().to_string())
            })
        };
        found.push(KsuModule {
            id: id.clone(),
            name: prop("name").unwrap_or_else(|| id.clone()),
            version: prop("version"),
            author: prop("author"),
            description: prop("description"),
            // The file that stands in its folder keeps it from starting.
            enabled: !path.join("disable").exists(),
            removed: path.join("remove").exists(),
            update: Path::new(UPDATES).join(&id).exists(),
        });
    }
    found.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(found)
}

/// Make or take away the file that keeps the module `id` from starting:
/// `on` starts it, `off` leaves it out.
pub fn set(id: &str, on: bool) -> Result<()> {
    let dir = Path::new(MODULES).join(id);
    anyhow::ensure!(dir.is_dir(), "{} is not a module's folder", dir.display());
    let flag = dir.join("disable");
    match on {
        true => match fs::remove_file(&flag) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e).with_context(|| format!("remove {}", flag.display())),
        },
        false => fs::write(&flag, b"").with_context(|| format!("write {}", flag.display())),
    }
}
