//! Stable rendezvous for stores whose paths exceed Unix socket address limits.
use agent_runtime::{Result, fail};
use std::{
    os::unix::{
        fs::{DirBuilderExt, MetadataExt},
        net::SocketAddr,
    },
    path::{Path, PathBuf},
};

fn canonical_store(store: &Path) -> Result<PathBuf> {
    let absolute = std::path::absolute(store)?;
    let mut existing = absolute.as_path();
    let mut suffix = Vec::new();
    loop {
        match existing.canonicalize() {
            Ok(mut path) => {
                for component in suffix.iter().rev() {
                    path.push(component);
                }
                return Ok(path);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                suffix.push(
                    existing
                        .file_name()
                        .ok_or_else(|| std::io::Error::other("invalid store path"))?,
                );
                existing = existing
                    .parent()
                    .ok_or_else(|| std::io::Error::other("invalid store path"))?;
            }
            Err(error) => return Err(error.into()),
        }
    }
}

pub fn default_socket(store: &Path) -> Result<PathBuf> {
    let store = canonical_store(store)?;
    let mut adjacent = store.clone().into_os_string();
    adjacent.push(".sock");
    let adjacent = PathBuf::from(adjacent);
    if SocketAddr::from_pathname(&adjacent).is_ok() {
        return Ok(adjacent);
    }

    // Versioned FNV-1a-128 gives deterministic path names across processes and
    // Rust releases. This is a rendezvous identifier, not an authentication hash.
    let mut hash = 0x6c62272e07bb014262b821756295c58du128;
    for byte in store.as_os_str().as_encoded_bytes() {
        hash ^= u128::from(*byte);
        hash = hash.wrapping_mul(0x0000000001000000000000000000013b);
    }
    // Do not use TMPDIR: snapshot runners may set it to another long path.
    let uid = unsafe { libc::geteuid() };
    let directory = PathBuf::from(format!("/tmp/agent-{uid}"));
    match std::fs::DirBuilder::new().mode(0o700).create(&directory) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error.into()),
    }
    let metadata = std::fs::symlink_metadata(&directory)?;
    if !metadata.is_dir() || metadata.uid() != uid || metadata.mode() & 0o077 != 0 {
        return fail("unsafe_socket_directory");
    }
    Ok(directory.join(format!("v1-{hash:032x}.sock")))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn long_paths_and_aliases_have_a_stable_short_rendezvous() {
        let root = std::env::temp_dir().join(format!("agent-path-test-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let long = root.join("nested".repeat(25)).join("state.sqlite");
        let before = default_socket(&long).unwrap();
        assert!(SocketAddr::from_pathname(&before).is_ok());
        std::fs::create_dir_all(long.parent().unwrap()).unwrap();
        std::fs::write(&long, b"").unwrap();
        assert_eq!(before, default_socket(&long).unwrap());
        let alias = root.join("alias");
        std::os::unix::fs::symlink(long.parent().unwrap(), &alias).unwrap();
        assert_eq!(before, default_socket(&alias.join("state.sqlite")).unwrap());
        assert_ne!(
            before,
            default_socket(&long.with_file_name("other.sqlite")).unwrap()
        );
        let short = root.join("short.sqlite");
        assert_eq!(
            default_socket(&short).unwrap(),
            root.canonicalize().unwrap().join("short.sqlite.sock")
        );
        std::fs::remove_dir_all(root).unwrap();
    }
}
