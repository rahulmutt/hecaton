//! `plugins.yaml` (plugins spec §2.1): parse, validate, resolve sources.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use hecaton_api::{PluginEntry, PluginsFile};
use hecaton_core::name::validate_name;

use super::PluginError;

/// What an `https://` source is answered with. `ureq` is built without a
/// TLS feature (Phase 3 spec P3-1 forbids the crates), so a fetch could
/// only fail; the format keeps the field and the plumbing below.
pub const NO_TLS: &str =
    "URL sources need a TLS-enabled build; use `plugin package` and a tarball path";

/// Where a package comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    Url(String),
    Tarball(PathBuf),
    /// Used in place, never copied, no digest.
    Directory(PathBuf),
}

fn entry_error(i: usize, field: &str, message: impl Into<String>) -> PluginError {
    PluginError::Config {
        path: format!("plugins[{i}].{field}"),
        message: message.into(),
    }
}

fn is_hex64(s: &str) -> bool {
    s.len() == 64
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Missing file → empty list. Names valid and unique; `sha256` well-formed,
/// and present unless the source is an existing directory.
pub fn load_plugins_file(path: &Path) -> Result<PluginsFile, PluginError> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(PluginsFile::default()),
        Err(e) => return Err(PluginError::io(path, e)),
    };
    let file: PluginsFile = serde_norway::from_str(&text).map_err(|e| PluginError::Config {
        path: "(parse)".into(),
        message: e.to_string(),
    })?;
    let base = path.parent().unwrap_or(Path::new("."));
    // Names first, across the whole file: a duplicate is a property of the
    // list, so it must be reported even when an earlier entry's source or
    // digest is also wrong.
    let mut seen = BTreeSet::new();
    for (i, entry) in file.plugins.iter().enumerate() {
        if let Err(reason) = validate_name(&entry.name) {
            return Err(entry_error(
                i,
                "name",
                format!("invalid plugin name {:?}: {reason}", entry.name),
            ));
        }
        if !seen.insert(entry.name.clone()) {
            return Err(entry_error(i, "name", "duplicate"));
        }
    }
    for (i, entry) in file.plugins.iter().enumerate() {
        if entry.source.trim().is_empty() {
            return Err(entry_error(i, "source", "must not be empty"));
        }
        if entry.source.starts_with("https://") {
            return Err(entry_error(i, "source", NO_TLS));
        }
        if entry.source.contains("://") {
            return Err(entry_error(
                i,
                "source",
                "only https:// URLs, tarball paths and directories are accepted",
            ));
        }
        let is_dir = resolve_path(&entry.source, base).is_dir();
        match &entry.sha256 {
            Some(s) if !is_hex64(s) => {
                return Err(entry_error(i, "sha256", "expected 64 lowercase hex digits"));
            }
            None if !is_dir => {
                return Err(entry_error(
                    i,
                    "sha256",
                    "required for URL and tarball sources",
                ));
            }
            _ => {}
        }
    }
    Ok(file)
}

fn resolve_path(source: &str, base: &Path) -> PathBuf {
    let p = Path::new(source);
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        base.join(p)
    }
}

/// URL, existing tarball, or existing directory; relative paths resolve
/// against the directory holding `plugins.yaml`. `load_plugins_file`
/// rejects URLs before this is reached; the arm is enabled when a TLS
/// provider is added.
pub fn resolve_source(entry: &PluginEntry, base: &Path) -> Result<Source, PluginError> {
    if entry.source.starts_with("https://") {
        return Ok(Source::Url(entry.source.clone()));
    }
    let p = resolve_path(&entry.source, base);
    if p.is_dir() {
        Ok(Source::Directory(p))
    } else if p.is_file() {
        Ok(Source::Tarball(p))
    } else {
        Err(PluginError::Io {
            path: p,
            message: "not found".into(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, text: &str) -> PathBuf {
        let p = dir.join("plugins.yaml");
        std::fs::write(&p, text).unwrap();
        p
    }

    #[test]
    fn a_missing_file_is_an_empty_list() {
        let dir = tempfile::tempdir().unwrap();
        let f = load_plugins_file(&dir.path().join("plugins.yaml")).unwrap();
        assert!(f.plugins.is_empty());
    }

    #[test]
    fn loads_and_validates_entries() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("web")).unwrap();
        std::fs::write(dir.path().join("flow.tar.gz"), b"").unwrap();
        let p = write(
            dir.path(),
            "plugins:\n  - name: flow\n    source: ./flow.tar.gz\n    sha256: \"0000000000000000000000000000000000000000000000000000000000000000\"\n  - name: web\n    source: ./web\n    config: { title: t }\n",
        );
        let f = load_plugins_file(&p).unwrap();
        assert_eq!(f.plugins.len(), 2);
        assert_eq!(
            resolve_source(&f.plugins[0], dir.path()).unwrap(),
            Source::Tarball(dir.path().join("flow.tar.gz"))
        );
        assert_eq!(
            resolve_source(&f.plugins[1], dir.path()).unwrap(),
            Source::Directory(dir.path().join("web"))
        );
    }

    /// The format still describes URL sources and `resolve_source` still
    /// maps one; only the load path refuses them, because this build has
    /// no TLS.
    #[test]
    fn url_sources_are_declared_but_rejected_until_tls() {
        let dir = tempfile::tempdir().unwrap();
        let p = write(
            dir.path(),
            "plugins:\n  - name: flow\n    source: https://x/flow.tar.gz\n    sha256: \"0000000000000000000000000000000000000000000000000000000000000000\"\n",
        );
        assert_eq!(
            load_plugins_file(&p).unwrap_err().to_string(),
            format!("plugins.yaml: plugins[0].source: {NO_TLS}")
        );
        let entry = PluginEntry {
            name: "flow".into(),
            source: "https://x/flow.tar.gz".into(),
            sha256: Some("0".repeat(64)),
            config: serde_json::json!({}),
        };
        assert_eq!(
            resolve_source(&entry, dir.path()).unwrap(),
            Source::Url("https://x/flow.tar.gz".into())
        );
    }

    #[test]
    fn errors_name_the_entry() {
        let dir = tempfile::tempdir().unwrap();
        let cases = [
            (
                "plugins:\n  - name: Web\n    source: ./web\n",
                "plugins.yaml: plugins[0].name: invalid plugin name \"Web\": contains characters other than a-z, 0-9 and '-'",
            ),
            (
                "plugins:\n  - name: a\n    source: ./a\n  - name: a\n    source: ./b\n",
                "plugins.yaml: plugins[1].name: duplicate",
            ),
            (
                "plugins:\n  - name: a\n    source: ./a.tar.gz\n",
                "plugins.yaml: plugins[0].sha256: required for URL and tarball sources",
            ),
            (
                "plugins:\n  - name: a\n    source: ./a.tar.gz\n    sha256: xyz\n",
                "plugins.yaml: plugins[0].sha256: expected 64 lowercase hex digits",
            ),
            (
                "plugins:\n  - name: a\n    source: \"\"\n",
                "plugins.yaml: plugins[0].source: must not be empty",
            ),
            (
                "plugins:\n  - name: a\n    source: http://x/a.tar.gz\n    sha256: \"0000000000000000000000000000000000000000000000000000000000000000\"\n",
                "plugins.yaml: plugins[0].source: only https:// URLs, tarball paths and directories are accepted",
            ),
        ];
        for (text, expected) in cases {
            let p = write(dir.path(), text);
            assert_eq!(
                load_plugins_file(&p).unwrap_err().to_string(),
                expected,
                "{text}"
            );
        }
        let p = write(dir.path(), "plugin: []\n");
        let e = load_plugins_file(&p).unwrap_err().to_string();
        assert!(e.starts_with("plugins.yaml: "), "{e}");
        assert!(e.contains("unknown field"), "{e}");
    }

    #[test]
    fn a_missing_tarball_path_is_reported_at_resolve_time() {
        let dir = tempfile::tempdir().unwrap();
        let entry = PluginEntry {
            name: "a".into(),
            source: "./a.tar.gz".into(),
            sha256: Some("0".repeat(64)),
            config: serde_json::json!({}),
        };
        let e = resolve_source(&entry, dir.path()).unwrap_err().to_string();
        assert!(e.ends_with("a.tar.gz: not found"), "{e}");
        std::fs::write(dir.path().join("a.tar.gz"), b"").unwrap();
        assert_eq!(
            resolve_source(&entry, dir.path()).unwrap(),
            Source::Tarball(dir.path().join("a.tar.gz"))
        );
        // absolute paths are used as-is
        let abs = PluginEntry {
            source: dir.path().join("a.tar.gz").display().to_string(),
            ..entry
        };
        assert_eq!(
            resolve_source(&abs, Path::new("/elsewhere")).unwrap(),
            Source::Tarball(dir.path().join("a.tar.gz"))
        );
    }
}
