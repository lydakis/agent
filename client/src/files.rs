//! Files clients write whole.
use std::{
    io::Write,
    path::{Path, PathBuf},
};

/// Write a file whole beside its place, then rename it there, with its mode
/// set from creation, so the new name never has any other.
pub fn replace_mode(path: &Path, text: &[u8], mode: u32) -> Result<(), String> {
    use std::os::unix::fs::OpenOptionsExt;
    let dir = path.parent().ok_or("no folder")?;
    std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let temporary = dir.join(format!(
        ".{}.{}",
        path.file_name().unwrap_or_default().to_string_lossy(),
        std::process::id()
    ));
    let written = (|| {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(mode)
            .open(&temporary)?;
        // An old temporary of this pid may carry another mode.
        std::fs::set_permissions(
            &temporary,
            std::os::unix::fs::PermissionsExt::from_mode(mode),
        )?;
        file.write_all(text)?;
        file.sync_all()?;
        std::fs::rename(&temporary, path)?;
        // The new name is durable only once its folder is.
        std::fs::File::open(dir)?.sync_all()
    })();
    if written.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    written.map_err(|e| format!("{}: {e}", path.display()))
}

/// `path` with what exists of it resolved to its real path and the rest
/// appended; `None` when the rest climbs out with `..`, which can't be
/// known without making it. A symlink whose target is missing is followed
/// to that target, as a write through it would be. The daemon keeps a
/// sandbox's paths so, and a client that compares them does the same.
pub fn real(path: &Path) -> Option<PathBuf> {
    real_within(path, 40)
}

fn real_within(path: &Path, links: u32) -> Option<PathBuf> {
    let mut existing = path.to_owned();
    let mut rest = Vec::new();
    loop {
        let found = std::fs::canonicalize(&existing).ok().or_else(|| {
            // canonicalize fails on a dangling link as on a missing name.
            let link = std::fs::read_link(&existing).ok()?;
            let mut target = existing.parent()?.join(link);
            for part in rest.iter().rev() {
                target.push(part);
            }
            rest.clear();
            real_within(&target, links.checked_sub(1)?)
        });
        if let Some(mut resolved) = found {
            for part in rest.iter().rev() {
                resolved.push(part);
            }
            return Some(resolved);
        }
        if std::fs::symlink_metadata(&existing).is_ok_and(|found| found.file_type().is_symlink()) {
            return None;
        }
        let name = existing.file_name()?.to_owned();
        if !matches!(
            Path::new(&name).components().next(),
            Some(std::path::Component::Normal(_))
        ) {
            return None;
        }
        rest.push(name);
        existing = existing.parent()?.to_owned();
    }
}
