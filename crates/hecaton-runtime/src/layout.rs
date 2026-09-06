//! Where everything lives (Phase 2 spec §4.1).

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use hecaton_core::{AgentId, CrewRef, FleetName};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StateLayout {
    pub state_root: PathBuf,
    pub data_root: PathBuf,
    pub config_root: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CrewPaths {
    pub root: PathBuf,
    pub repo: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentPaths {
    pub root: PathBuf,
    pub home: PathBuf,
    pub workspace: PathBuf,
    /// nono's own `$HOME` — its state root must not overlap `home/` (P2-5).
    pub nono_home: PathBuf,
    pub mise_toml: PathBuf,
    pub profile: PathBuf,
    pub launch: PathBuf,
    pub logs: PathBuf,
}

impl StateLayout {
    /// XDG resolution: `$XDG_{STATE,DATA,CONFIG}_HOME/hecaton`, defaulting to
    /// `~/.local/state`, `~/.local/share`, `~/.config`.
    pub fn from_env(home: &Path, env: impl Fn(&str) -> Option<OsString>) -> Self {
        let pick = |var: &str, default: PathBuf| {
            env(var)
                .map(PathBuf::from)
                .unwrap_or(default)
                .join("hecaton")
        };
        Self {
            state_root: pick("XDG_STATE_HOME", home.join(".local").join("state")),
            data_root: pick("XDG_DATA_HOME", home.join(".local").join("share")),
            config_root: pick("XDG_CONFIG_HOME", home.join(".config")),
        }
    }

    pub fn mise_data_dir(&self) -> PathBuf {
        self.data_root.join("mise")
    }
    pub fn system_mise_toml(&self) -> PathBuf {
        self.config_root.join("mise.toml")
    }
    /// `server/`: token, vault key, endpoint, pid, log (Phase 3 spec §3.3, §5).
    pub fn server_dir(&self) -> PathBuf {
        self.state_root.join("server")
    }
    pub fn fleets_dir(&self) -> PathBuf {
        self.state_root.join("fleets")
    }
    pub fn fleet_dir(&self, f: &FleetName) -> PathBuf {
        self.fleets_dir().join(f.as_str())
    }
    pub fn fleet_gh_dir(&self, f: &FleetName) -> PathBuf {
        self.fleet_dir(f).join("gh")
    }
    pub fn crew(&self, c: &CrewRef) -> CrewPaths {
        let root = self.fleet_dir(&c.fleet).join("crews").join(c.crew.as_str());
        CrewPaths {
            repo: root.join("repo"),
            root,
        }
    }
    pub fn agent(&self, id: &AgentId) -> AgentPaths {
        let root = self
            .crew(&id.crew_ref())
            .root
            .join("agents")
            .join(id.agent.as_str());
        AgentPaths {
            home: root.join("home"),
            workspace: root.join("workspace"),
            nono_home: root.join("nono"),
            mise_toml: root.join("mise.toml"),
            profile: root.join("nono-profile.json"),
            launch: root.join("launch.sh"),
            logs: root.join("logs"),
            root,
        }
    }
}

impl AgentPaths {
    /// Written after `mise install` and `nono profile validate` succeed for
    /// the current `mise.toml` + `nono-profile.json`; removed when either
    /// rendered file changes (Phase 3 spec §6.2).
    pub fn installed_marker(&self) -> PathBuf {
        self.root.join(".installed")
    }

    pub fn claude_dir(&self) -> PathBuf {
        self.home.join(".claude")
    }
    /// The agent's temp dir, 0700 inside `home/`. The sandbox grants nothing
    /// under `/tmp`, and claude refuses to start when its temp dir
    /// (`/tmp/claude-<uid>` by default) is unreachable, so `TMPDIR` and
    /// `CLAUDE_CODE_TMPDIR` point here.
    pub fn tmp_dir(&self) -> PathBuf {
        self.home.join("tmp")
    }
    pub fn claude_projects(&self) -> PathBuf {
        self.claude_dir().join("projects")
    }
    pub fn xdg_config(&self) -> PathBuf {
        self.home.join(".config")
    }
    pub fn xdg_data(&self) -> PathBuf {
        self.home.join(".local").join("share")
    }
    pub fn xdg_state(&self) -> PathBuf {
        self.home.join(".local").join("state")
    }
    pub fn xdg_cache(&self) -> PathBuf {
        self.home.join(".cache")
    }
    pub fn gh_dir(&self) -> PathBuf {
        self.xdg_config().join("gh")
    }
    pub fn mise_config_dir(&self) -> PathBuf {
        self.xdg_config().join("mise")
    }
    pub fn mise_state_dir(&self) -> PathBuf {
        self.xdg_state().join("mise")
    }
    pub fn mise_cache_dir(&self) -> PathBuf {
        self.xdg_cache().join("mise")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn no_env(_: &str) -> Option<OsString> {
        None
    }

    #[test]
    fn defaults_to_xdg_dirs_under_home() {
        let l = StateLayout::from_env(Path::new("/h"), no_env);
        assert_eq!(l.state_root, PathBuf::from("/h/.local/state/hecaton"));
        assert_eq!(l.data_root, PathBuf::from("/h/.local/share/hecaton"));
        assert_eq!(l.config_root, PathBuf::from("/h/.config/hecaton"));
    }

    #[test]
    fn honours_xdg_variables() {
        let l = StateLayout::from_env(Path::new("/h"), |k| {
            (k == "XDG_STATE_HOME").then(|| OsString::from("/xs"))
        });
        assert_eq!(l.state_root, PathBuf::from("/xs/hecaton"));
        assert_eq!(l.data_root, PathBuf::from("/h/.local/share/hecaton"));
    }

    #[test]
    fn agent_paths_follow_the_spec_layout() {
        let l = StateLayout::from_env(Path::new("/h"), no_env);
        let a = l.agent(&"payments/backend/alice".parse().unwrap());
        let base = "/h/.local/state/hecaton/fleets/payments/crews/backend/agents/alice";
        assert_eq!(a.root, PathBuf::from(base));
        assert_eq!(a.home, PathBuf::from(format!("{base}/home")));
        assert_eq!(a.tmp_dir(), PathBuf::from(format!("{base}/home/tmp")));
        assert_eq!(a.nono_home, PathBuf::from(format!("{base}/nono")));
        assert_eq!(
            a.profile,
            PathBuf::from(format!("{base}/nono-profile.json"))
        );
        assert_eq!(
            a.claude_dir(),
            PathBuf::from(format!("{base}/home/.claude"))
        );
        assert_eq!(
            a.mise_cache_dir(),
            PathBuf::from(format!("{base}/home/.cache/mise"))
        );
        assert_eq!(
            l.crew(&"payments/backend".parse().unwrap()).repo,
            PathBuf::from("/h/.local/state/hecaton/fleets/payments/crews/backend/repo")
        );
        assert_eq!(
            l.fleet_gh_dir(&"payments".parse().unwrap()),
            PathBuf::from("/h/.local/state/hecaton/fleets/payments/gh")
        );
        assert_eq!(
            l.mise_data_dir(),
            PathBuf::from("/h/.local/share/hecaton/mise")
        );
        assert_eq!(
            l.server_dir(),
            PathBuf::from("/h/.local/state/hecaton/server")
        );
        assert_eq!(
            l.fleets_dir(),
            PathBuf::from("/h/.local/state/hecaton/fleets")
        );
    }
}
