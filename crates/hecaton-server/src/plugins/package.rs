//! Package handling (plugins spec §2, §2.1): digests, deterministic
//! tarballs, safe unpacking, fetch, install.

use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use sha2::{Digest, Sha256};

use super::PluginError;
use super::config::Source;

/// Largest tarball `fetch` accepts (spec §2.1).
const MAX_TARBALL: u64 = 64 << 20;
const FETCH_TIMEOUT: Duration = Duration::from_secs(60);

pub fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

/// The first twelve hex digits: the install directory name. Shorter input
/// is returned whole; a non-hex string is never sliced mid-character.
pub fn digest_prefix(hex: &str) -> &str {
    hex.get(..hex.len().min(12)).unwrap_or(hex)
}

/// `https://` only, 60 s, 64 MiB cap, no TLS features beyond ureq's default.
pub fn fetch(url: &str) -> Result<Vec<u8>, PluginError> {
    let fail = |message: String| PluginError::Fetch {
        url: url.to_string(),
        message,
    };
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(FETCH_TIMEOUT))
        .http_status_as_error(true)
        .build()
        .into();
    let mut resp = agent.get(url).call().map_err(|e| fail(e.to_string()))?;
    let mut buf = Vec::new();
    resp.body_mut()
        .as_reader()
        .take(MAX_TARBALL + 1)
        .read_to_end(&mut buf)
        .map_err(|e| fail(e.to_string()))?;
    if buf.len() as u64 > MAX_TARBALL {
        return Err(fail(format!("larger than {} MiB", MAX_TARBALL >> 20)));
    }
    Ok(buf)
}

fn relative_inside(path: &Path) -> bool {
    !path.as_os_str().is_empty()
        && path
            .components()
            .all(|c| matches!(c, Component::Normal(_) | Component::CurDir))
}

/// Unpacks a `.tar.gz` into `dest` (created; must not exist). Rejects
/// absolute paths, `..`, links and special files; files end up 0444 (0555
/// when executable), directories 0755. On any error `dest` is removed.
pub fn unpack(tarball: &[u8], dest: &Path) -> Result<(), PluginError> {
    let result = unpack_inner(tarball, dest);
    if result.is_err() {
        let _ = std::fs::remove_dir_all(dest);
    }
    result
}

/// Creates `path` and every missing ancestor, each 0755, so an unpacked
/// tree does not depend on the daemon's umask.
fn ensure_dir(path: &Path) -> Result<(), PluginError> {
    if path.is_dir() {
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        ensure_dir(parent)?;
    }
    match std::fs::create_dir(path) {
        Ok(()) => {}
        // A tarball that lists `a` as a file and then `a/b` must not
        // quietly get a directory here.
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists && path.is_dir() => {
            return Ok(());
        }
        Err(e) => return Err(PluginError::io(path, e)),
    }
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
        .map_err(|e| PluginError::io(path, e))
}

fn unpack_inner(tarball: &[u8], dest: &Path) -> Result<(), PluginError> {
    let bad = PluginError::Package;
    ensure_dir(dest)?;
    let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(tarball));
    for entry in archive.entries().map_err(|e| bad(e.to_string()))? {
        let mut entry = entry.map_err(|e| bad(e.to_string()))?;
        // `path()` is the header's name verbatim; tar-rs sanitises only
        // inside its own `unpack`, which this deliberately does not use.
        let rel = entry
            .path()
            .map(|p| p.into_owned())
            .map_err(|e| bad(e.to_string()))?;
        let shown = rel.display().to_string();
        if !relative_inside(&rel) {
            return Err(bad(format!("entry {shown:?}: path escapes the package")));
        }
        let kind = entry.header().entry_type();
        let full = dest.join(&rel);
        match kind {
            tar::EntryType::Directory => {
                ensure_dir(&full)?;
            }
            tar::EntryType::Regular => {
                if let Some(parent) = full.parent() {
                    ensure_dir(parent)?;
                }
                let mut f = std::fs::File::create(&full).map_err(|e| PluginError::io(&full, e))?;
                std::io::copy(&mut entry, &mut f).map_err(|e| PluginError::io(&full, e))?;
                let exec = entry
                    .header()
                    .mode()
                    .map(|m| m & 0o111 != 0)
                    .unwrap_or(false);
                let mode = if exec { 0o555 } else { 0o444 };
                std::fs::set_permissions(&full, std::fs::Permissions::from_mode(mode))
                    .map_err(|e| PluginError::io(&full, e))?;
            }
            _ => {
                return Err(bad(format!(
                    "entry {shown:?}: symlinks, hard links and special files are not allowed"
                )));
            }
        }
    }
    Ok(())
}

fn walk(dir: &Path, rel: &Path, out: &mut Vec<(PathBuf, PathBuf)>) -> std::io::Result<()> {
    let mut entries: Vec<_> = std::fs::read_dir(dir)?.collect::<Result<_, _>>()?;
    entries.sort_by_key(|e| e.file_name());
    for e in entries {
        let name = e.file_name();
        let full = e.path();
        let r = rel.join(&name);
        let meta = std::fs::symlink_metadata(&full)?;
        if meta.is_dir() {
            out.push((full.clone(), r.clone()));
            walk(&full, &r, out)?;
        } else if meta.is_file() {
            out.push((full, r));
        } else {
            return Err(std::io::Error::other(format!(
                "{}: symlinks and special files cannot be packaged",
                full.display()
            )));
        }
    }
    Ok(())
}

/// A deterministic `.tar.gz` of `dir` (sorted entries, mtime 0, uid/gid 0,
/// modes 0755 for directories, 0755/0644 for files by exec bit). Returns
/// the sha256 hex of the written file.
pub fn create(dir: &Path, out: &Path) -> Result<String, PluginError> {
    let mut entries = Vec::new();
    walk(dir, Path::new(""), &mut entries).map_err(|e| PluginError::io(dir, e))?;
    let gz = flate2::GzBuilder::new()
        .mtime(0)
        .write(Vec::new(), flate2::Compression::default());
    let mut b = tar::Builder::new(gz);
    for (full, rel) in entries {
        let meta = std::fs::metadata(&full).map_err(|e| PluginError::io(&full, e))?;
        let mut h = tar::Header::new_gnu();
        h.set_mtime(0);
        h.set_uid(0);
        h.set_gid(0);
        if meta.is_dir() {
            h.set_entry_type(tar::EntryType::Directory);
            h.set_size(0);
            h.set_mode(0o755);
            h.set_cksum();
            b.append_data(&mut h, &rel, std::io::empty())
                .map_err(|e| PluginError::io(&full, e))?;
        } else {
            let exec = meta.permissions().mode() & 0o111 != 0;
            h.set_entry_type(tar::EntryType::Regular);
            h.set_size(meta.len());
            h.set_mode(if exec { 0o755 } else { 0o644 });
            h.set_cksum();
            let f = std::fs::File::open(&full).map_err(|e| PluginError::io(&full, e))?;
            b.append_data(&mut h, &rel, f)
                .map_err(|e| PluginError::io(&full, e))?;
        }
    }
    let gz = b.into_inner().map_err(|e| PluginError::io(out, e))?;
    let bytes = gz.finish().map_err(|e| PluginError::io(out, e))?;
    if let Some(parent) = out.parent() {
        std::fs::create_dir_all(parent).map_err(|e| PluginError::io(parent, e))?;
    }
    std::fs::write(out, &bytes).map_err(|e| PluginError::io(out, e))?;
    Ok(sha256_hex(&bytes))
}

/// Makes the package available and returns `(package_dir, digest)`. A
/// directory is used in place. A tarball or URL is read, digest-checked
/// against `expected` *before* unpacking, and unpacked into
/// `<install_root>/<name>/<digest12>/`, reused if already there.
pub fn install(
    name: &str,
    source: &Source,
    expected: Option<&str>,
    install_root: &Path,
) -> Result<(PathBuf, Option<String>), PluginError> {
    let bytes = match source {
        Source::Directory(d) => {
            let canon = std::fs::canonicalize(d).map_err(|e| PluginError::io(d, e))?;
            return Ok((canon, None));
        }
        Source::Tarball(p) => std::fs::read(p).map_err(|e| PluginError::io(p, e))?,
        Source::Url(u) => fetch(u)?,
    };
    let digest = sha256_hex(&bytes);
    if let Some(exp) = expected
        && exp != digest
    {
        return Err(PluginError::Digest {
            expected: exp.to_string(),
            got: digest,
        });
    }
    let dest = install_root.join(name).join(digest_prefix(&digest));
    if dest.is_dir() {
        return Ok((dest, Some(digest)));
    }
    let tmp = install_root.join(name).join(format!(
        ".tmp-{}-{}",
        digest_prefix(&digest),
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&tmp);
    unpack(&bytes, &tmp)?;
    std::fs::rename(&tmp, &dest).map_err(|e| PluginError::io(&dest, e))?;
    Ok((dest, Some(digest)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;

    fn package_dir() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("hecaton-plugin.yaml"),
            "apiVersion: hecaton/v1\nkind: Plugin\nname: p\nversion: 0.0.1\nprotocol: 1\nstart: serve\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("mise.toml"),
            "[tasks.serve]\nrun = \"./run.sh\"\n",
        )
        .unwrap();
        std::fs::create_dir(dir.path().join("lib")).unwrap();
        std::fs::write(dir.path().join("lib/a.py"), "print(1)\n").unwrap();
        std::fs::write(dir.path().join("run.sh"), "#!/bin/sh\necho hi\n").unwrap();
        std::fs::set_permissions(
            dir.path().join("run.sh"),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        dir
    }

    #[test]
    fn digests_are_hex_sha256() {
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(digest_prefix("ba7816bf8f01cfea414140de"), "ba7816bf8f01");
        assert_eq!(digest_prefix("ba78"), "ba78", "shorter than twelve");
        // eleven ASCII then a two-byte char straddling index 12
        let straddles = "aaaaaaaaaaa\u{fc}x";
        assert_eq!(
            digest_prefix(straddles),
            straddles,
            "never sliced mid-character"
        );
    }

    #[test]
    fn create_is_deterministic_and_unpack_round_trips_with_read_only_files() {
        let src = package_dir();
        let out = tempfile::tempdir().unwrap();
        let t1 = out.path().join("p1.tar.gz");
        let t2 = out.path().join("p2.tar.gz");
        let d1 = create(src.path(), &t1).unwrap();
        let d2 = create(src.path(), &t2).unwrap();
        assert_eq!(d1, d2, "same tree, same digest");
        assert_eq!(d1, sha256_hex(&std::fs::read(&t1).unwrap()));

        let dest = out.path().join("unpacked");
        unpack(&std::fs::read(&t1).unwrap(), &dest).unwrap();
        assert_eq!(
            std::fs::read_to_string(dest.join("lib/a.py")).unwrap(),
            "print(1)\n"
        );
        let mode = |p: &str| {
            std::fs::metadata(dest.join(p))
                .unwrap()
                .permissions()
                .mode()
                & 0o777
        };
        assert_eq!(mode("lib/a.py"), 0o444);
        assert_eq!(mode("run.sh"), 0o555, "exec bit kept, write bits dropped");
        assert_eq!(mode("lib"), 0o755);
    }

    fn tar_with(f: impl FnOnce(&mut tar::Builder<Vec<u8>>)) -> Vec<u8> {
        let mut b = tar::Builder::new(Vec::new());
        f(&mut b);
        let raw = b.into_inner().unwrap();
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        gz.write_all(&raw).unwrap();
        gz.finish().unwrap()
    }

    /// A tarball of one entry whose *stored* name is `name` verbatim:
    /// `append_data` refuses the escaping names an attacker's tar carries,
    /// so the raw header bytes are written by hand.
    fn raw_entry(name: &str, kind: tar::EntryType) -> Vec<u8> {
        let data: &[u8] = if kind == tar::EntryType::Regular {
            b"x"
        } else {
            b""
        };
        tar_with(|b| {
            let mut h = tar::Header::new_gnu();
            h.set_entry_type(kind);
            h.set_size(data.len() as u64);
            h.set_mode(0o644);
            h.set_path("x".repeat(name.len())).unwrap();
            h.as_gnu_mut().unwrap().name[..name.len()].copy_from_slice(name.as_bytes());
            h.set_cksum();
            b.append(&h, data).unwrap();
        })
    }

    #[test]
    fn unpack_rejects_escapes_links_and_odd_entries() {
        let dest = tempfile::tempdir().unwrap();
        let escapes = "path escapes the package";
        let links = "symlinks, hard links and special files are not allowed";
        let cases: Vec<(Vec<u8>, String)> = vec![
            (
                raw_entry("../evil", tar::EntryType::Regular),
                format!("package: entry \"../evil\": {escapes}"),
            ),
            (
                raw_entry("a/../../evil", tar::EntryType::Regular),
                format!("package: entry \"a/../../evil\": {escapes}"),
            ),
            (
                raw_entry("/abs", tar::EntryType::Regular),
                format!("package: entry \"/abs\": {escapes}"),
            ),
            (
                tar_with(|b| {
                    let mut h = tar::Header::new_gnu();
                    h.set_entry_type(tar::EntryType::Symlink);
                    h.set_size(0);
                    h.set_mode(0o777);
                    h.set_link_name("/etc/passwd").unwrap();
                    h.set_cksum();
                    b.append_data(&mut h, "link", &b""[..]).unwrap();
                }),
                format!("package: entry \"link\": {links}"),
            ),
            (
                raw_entry("hard", tar::EntryType::Link),
                format!("package: entry \"hard\": {links}"),
            ),
            (
                raw_entry("fifo", tar::EntryType::Fifo),
                format!("package: entry \"fifo\": {links}"),
            ),
        ];
        for (bytes, expected) in cases {
            let d = dest.path().join("x");
            let e = unpack(&bytes, &d).unwrap_err().to_string();
            assert_eq!(e, expected);
            assert!(!d.exists(), "nothing is left behind on rejection");
        }
        let e = unpack(b"not a tarball", &dest.path().join("y"))
            .unwrap_err()
            .to_string();
        assert!(e.starts_with("package: "), "{e}");
    }

    #[test]
    fn install_verifies_digests_and_reuses_an_existing_unpack() {
        let src = package_dir();
        let out = tempfile::tempdir().unwrap();
        let tarball = out.path().join("p.tar.gz");
        let digest = create(src.path(), &tarball).unwrap();
        let root = out.path().join("install");
        let (dir, d) =
            install("p", &Source::Tarball(tarball.clone()), Some(&digest), &root).unwrap();
        assert_eq!(dir, root.join("p").join(digest_prefix(&digest)));
        assert_eq!(d.as_deref(), Some(digest.as_str()));
        assert!(dir.join("mise.toml").exists());
        let marker = dir.join("lib/a.py");
        let before = std::fs::metadata(&marker).unwrap().modified().unwrap();
        let (again, _) =
            install("p", &Source::Tarball(tarball.clone()), Some(&digest), &root).unwrap();
        assert_eq!(again, dir);
        assert_eq!(
            std::fs::metadata(&marker).unwrap().modified().unwrap(),
            before,
            "reused, not rewritten"
        );
        let e = install("p", &Source::Tarball(tarball), Some(&"0".repeat(64)), &root).unwrap_err();
        assert!(matches!(e, PluginError::Digest { .. }), "{e}");
        assert_eq!(
            std::fs::read_dir(root.join("p")).unwrap().count(),
            1,
            "no temp dir left"
        );
        let (d, none) = install(
            "p",
            &Source::Directory(src.path().to_path_buf()),
            None,
            &root,
        )
        .unwrap();
        assert_eq!(d, std::fs::canonicalize(src.path()).unwrap());
        assert_eq!(none, None);
    }
}
