//! Plugins in the daemon (plugins spec §2.1, §5.2): the declarative file,
//! package install, and the host that drives them as the `hecaton` fleet.

pub mod client;
pub mod config;
pub mod host;
pub mod manifest;
pub mod materializer;
pub mod package;

use std::path::PathBuf;

pub use client::{CallFailure, PluginClient};
pub use config::{Source, load_plugins_file, resolve_source};
pub use host::{PluginHost, PluginHostConfig};
pub use manifest::read_manifest;
pub use materializer::{NullStore, PluginMaterializer};

/// Every plugin failure, with the config path or file first.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PluginError {
    #[error("plugins.yaml: {path}: {message}")]
    Config { path: String, message: String },
    #[error(transparent)]
    Manifest(#[from] hecaton_core::ManifestError),
    #[error("hecaton-plugin.yaml: {0}")]
    ManifestParse(String),
    #[error("{path}: {message}")]
    Io { path: PathBuf, message: String },
    /// The field is `url`, not `source`: thiserror treats a field named
    /// `source` as the underlying `Error`, which a `String` is not.
    #[error("{url}: {message}")]
    Fetch { url: String, message: String },
    #[error("package: {0}")]
    Package(String),
    #[error("digest mismatch (expected {expected}, got {got})")]
    Digest { expected: String, got: String },
    #[error("plugin {0:?} is still declared in plugins.yaml; remove it first")]
    StillDeclared(String),
    #[error("{0}")]
    Internal(String),
}

impl PluginError {
    pub(crate) fn io(path: &std::path::Path, e: impl std::fmt::Display) -> Self {
        Self::Io {
            path: path.to_path_buf(),
            message: e.to_string(),
        }
    }
}
