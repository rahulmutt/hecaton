//! Atomic file writes with explicit modes.
//!
//! Consumed by the materialization steps added in Tasks 10-16; until then
//! these functions are only exercised by this module's own tests, so the
//! plain (non-test) build sees them as dead code.
#![allow(dead_code)]

use std::fs;
use std::fs::OpenOptions;
use std::io::{self, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::Path;

pub(crate) fn ensure_dir(path: &Path) -> io::Result<()> {
    fs::create_dir_all(path)
}

/// Writes `contents` to `path` via a sibling temp file and rename, with
/// `mode` (e.g. `0o600`) applied before the rename. The temp file is
/// created with `mode` from the start (via `OpenOptionsExt::mode`), so a
/// secrets file is never briefly readable at the umask default while the
/// mode is fixed up after the fact. `set_permissions` is still applied
/// afterwards: `mode()` only governs permissions at creation time, and an
/// umask can only remove bits, so a restrictive umask (e.g. `0o077`) could
/// otherwise leave the file narrower than the caller asked for.
pub(crate) fn write_atomic(path: &Path, contents: &[u8], mode: u32) -> io::Result<()> {
    let dir = path
        .parent()
        .ok_or_else(|| io::Error::other("path has no parent"))?;
    ensure_dir(dir)?;
    let name = path
        .file_name()
        .ok_or_else(|| io::Error::other("path has no file name"))?
        .to_string_lossy();
    let tmp = dir.join(format!(".{name}.tmp-{}", std::process::id()));
    let mut f = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(mode)
        .open(&tmp)?;
    f.write_all(contents)?;
    f.sync_all()?;
    drop(f);
    fs::set_permissions(&tmp, fs::Permissions::from_mode(mode))?;
    fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_with_mode_and_replaces_existing() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("sub").join("f");
        write_atomic(&p, b"one", 0o600).unwrap();
        assert_eq!(fs::read_to_string(&p).unwrap(), "one");
        assert_eq!(
            fs::metadata(&p).unwrap().permissions().mode() & 0o777,
            0o600
        );
        write_atomic(&p, b"two", 0o644).unwrap();
        assert_eq!(fs::read_to_string(&p).unwrap(), "two");
        assert_eq!(
            fs::metadata(&p).unwrap().permissions().mode() & 0o777,
            0o644
        );
        assert_eq!(
            fs::read_dir(p.parent().unwrap()).unwrap().count(),
            1,
            "no temp file left"
        );
    }
}
