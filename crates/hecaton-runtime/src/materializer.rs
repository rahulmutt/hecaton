//! `Runtime`: the `Materializer` over real tools. `render_agent` is the
//! pure-ish half (files only) shared with `hecaton dev materialize`.

use hecaton_api::{CredentialBundle, GitAuth, GitSettings};
use hecaton_core::{
    AgentId, CrewRef, HookTarget, Keep, LaunchPlan, MaterializeError, Materializer, RepoRef,
    ResolvedAgent,
};

use crate::env::agent_env;
use crate::home::{HomeInputs, write_home};
use crate::launch::{hooks_port, render_launch, wants_continue, write_launch};
use crate::layout::StateLayout;
use crate::sandbox::{hecaton_grants, render_profile, validate_profile, write_profile};
use crate::toolchain::{Toolchain, system_tools};
use crate::tools::ToolPaths;
use crate::workspace::Workspace;

pub struct Runtime {
    pub layout: StateLayout,
    pub tools: ToolPaths,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct RenderOptions {
    pub redact_credentials: bool,
}

/// What `render_agent` produced and whether the installable inputs moved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderOutcome {
    pub plan: LaunchPlan,
    /// `mise.toml` or `nono-profile.json` changed: the install marker was
    /// removed and `install_and_validate` will run the tools again.
    pub toolchain_changed: bool,
}

impl Runtime {
    pub fn new(layout: StateLayout, tools: ToolPaths) -> Self {
        Self { layout, tools }
    }

    /// Steps 2–5 of the pipeline without subprocesses: home, `mise.toml`,
    /// `nono-profile.json`, `launch.sh`. Idempotent.
    pub fn render_agent(
        &self,
        agent: &ResolvedAgent,
        creds: &CredentialBundle,
        hooks: &HookTarget,
        opts: &RenderOptions,
    ) -> Result<RenderOutcome, MaterializeError> {
        let id = &agent.id;
        let paths = self.layout.agent(id);
        let crew = self.layout.crew(&id.crew_ref());
        let with_gh = agent.git.auth == GitAuth::Gh;

        write_home(
            id,
            &paths,
            &HomeInputs {
                settings: &agent.settings.claude.settings,
                creds,
                hooks,
                git: &agent.git,
                relay: &self.tools.hecaton,
                with_gh,
                redact_credentials: opts.redact_credentials,
            },
        )?;

        let system = system_tools(&self.layout, id)?;
        let tools_changed = Toolchain {
            tools: &self.tools,
            layout: &self.layout,
        }
        .write(id, &paths, &system, &agent.settings.tools, with_gh)?;

        let env = agent_env(
            id,
            &paths,
            &self.layout,
            &hooks.url,
            &hooks.secret,
            &agent.settings.env,
        );
        let profile = render_profile(
            id,
            &hecaton_grants(&paths, &crew, &self.layout, &self.tools.hecaton),
            hooks_port(&hooks.url),
            &env,
            &agent.settings.sandbox,
        )?;
        let profile_changed = write_profile(id, &paths, &profile)?;

        let resume = wants_continue(&agent.settings.claude, &paths);
        let (script, plan) = render_launch(
            &paths,
            &self.tools,
            &agent.settings.claude.binary,
            &agent.settings.claude.args,
            resume,
        );
        write_launch(id, &paths, &script)?;

        let toolchain_changed = tools_changed || profile_changed;
        if toolchain_changed
            && let Err(e) = std::fs::remove_file(paths.installed_marker())
            && e.kind() != std::io::ErrorKind::NotFound
        {
            return Err(MaterializeError::Io {
                id: id.to_string(),
                path: paths.installed_marker(),
                message: e.to_string(),
            });
        }
        Ok(RenderOutcome {
            plan,
            toolchain_changed,
        })
    }

    /// The subprocess half of steps 3 and 4. Skipped when the marker from a
    /// previous success exists (Phase 3 spec §6.2); written on success.
    pub fn install_and_validate(&self, agent: &ResolvedAgent) -> Result<(), MaterializeError> {
        let paths = self.layout.agent(&agent.id);
        if paths.installed_marker().exists() {
            return Ok(());
        }
        Toolchain {
            tools: &self.tools,
            layout: &self.layout,
        }
        .install(&agent.id, &paths)?;
        validate_profile(&self.tools, &agent.id, &paths)?;
        std::fs::write(paths.installed_marker(), b"").map_err(|e| MaterializeError::Io {
            id: agent.id.to_string(),
            path: paths.installed_marker(),
            message: e.to_string(),
        })
    }
}

impl Runtime {
    fn workspace(&self, fleet: &hecaton_core::FleetName, git: &GitSettings) -> Workspace<'_> {
        Workspace {
            tools: &self.tools,
            gh_config_dir: (git.auth == GitAuth::Gh).then(|| self.layout.fleet_gh_dir(fleet)),
        }
    }

    fn rm_rf(id: &str, path: &std::path::Path) -> Result<(), MaterializeError> {
        match std::fs::remove_dir_all(path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(MaterializeError::Io {
                id: id.to_string(),
                path: path.to_path_buf(),
                message: e.to_string(),
            }),
        }
    }
}

impl Materializer for Runtime {
    fn ensure_crew(
        &self,
        crew: &CrewRef,
        repo: &RepoRef,
        git_ref: &str,
        git: &GitSettings,
        creds: &CredentialBundle,
    ) -> Result<(), MaterializeError> {
        let id = crew.to_string();
        if git.auth == GitAuth::Gh {
            let token = creds.gh_token.as_deref().ok_or_else(|| MaterializeError::Invalid {
                id: id.clone(),
                message: "git.auth is gh but no gh token was provided (run `gh auth login` on the client)".into(),
            })?;
            Workspace::write_fleet_gh_config(&self.layout.fleet_gh_dir(&crew.fleet), token, &id)?;
        }
        self.workspace(&crew.fleet, git)
            .ensure_repo(&id, &self.layout.crew(crew), repo, git_ref)
    }

    fn materialize(
        &self,
        agent: &ResolvedAgent,
        creds: &CredentialBundle,
        hooks: &HookTarget,
    ) -> Result<LaunchPlan, MaterializeError> {
        let id = agent.id.to_string();
        let crew = self.layout.crew(&agent.id.crew_ref());
        let paths = self.layout.agent(&agent.id);
        self.workspace(&agent.id.fleet, &agent.git)
            .ensure_worktree(
                &id,
                &crew,
                &paths.workspace,
                &agent.branch(),
                &agent.git_ref,
            )?;
        let out = self.render_agent(agent, creds, hooks, &RenderOptions::default())?;
        self.install_and_validate(agent)?;
        Ok(out.plan)
    }

    fn remove_agent(&self, agent: &AgentId) -> Result<(), MaterializeError> {
        let id = agent.to_string();
        let crew = self.layout.crew(&agent.crew_ref());
        let paths = self.layout.agent(agent);
        Workspace {
            tools: &self.tools,
            gh_config_dir: None,
        }
        .remove_worktree(&id, &crew, &paths.workspace)?;
        Self::rm_rf(&id, &paths.root)
    }

    fn remove_crew(&self, crew: &CrewRef, keep: Keep) -> Result<(), MaterializeError> {
        let id = crew.to_string();
        let paths = self.layout.crew(crew);
        if !keep.sessions {
            let agents_dir = paths.root.join("agents");
            if let Ok(entries) = std::fs::read_dir(&agents_dir) {
                for e in entries.flatten() {
                    let agent_id = format!("{id}/{}", e.file_name().to_string_lossy());
                    Workspace {
                        tools: &self.tools,
                        gh_config_dir: None,
                    }
                    .remove_worktree(
                        &agent_id,
                        &paths,
                        &e.path().join("workspace"),
                    )?;
                }
            }
            Self::rm_rf(&id, &agents_dir)?;
        }
        if !keep.repos {
            Self::rm_rf(&id, &paths.repo)?;
        }
        if !keep.repos && !keep.sessions {
            Self::rm_rf(&id, &paths.root)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hecaton_api::{AgentSettings, CrewSpec, FleetSpec};
    use hecaton_core::Fleet;
    use std::collections::BTreeMap;

    fn runtime(root: &std::path::Path) -> Runtime {
        let layout = StateLayout {
            state_root: root.join("state"),
            data_root: root.join("data"),
            config_root: root.join("config"),
        };
        std::fs::create_dir_all(&layout.config_root).unwrap();
        std::fs::write(layout.system_mise_toml(), "[tools]\n").unwrap();
        let tools = ToolPaths {
            git: "/nonexistent/git".into(),
            gh: "/nonexistent/gh".into(),
            mise: "/nonexistent/mise".into(),
            nono: "/nonexistent/nono".into(),
            tmux: "/nonexistent/tmux".into(),
            hecaton: "/nonexistent/hecaton".into(),
        };
        Runtime::new(layout, tools)
    }

    fn agent(tools: &[(&str, &str)]) -> ResolvedAgent {
        let s = AgentSettings {
            tools: tools
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            ..AgentSettings::default()
        };
        let fleet = Fleet::try_from(FleetSpec {
            name: "f".into(),
            crews: BTreeMap::from([(
                "c".to_string(),
                CrewSpec {
                    repo: "acme/x".into(),
                    git_ref: "main".into(),
                    git: GitSettings::default(),
                    agents: BTreeMap::from([("a".to_string(), s)]),
                },
            )]),
        })
        .unwrap();
        ResolvedAgent::from_fleet(&fleet).remove(0)
    }

    fn hooks() -> HookTarget {
        HookTarget {
            url: "http://127.0.0.1:7643".into(),
            secret: "s".into(),
        }
    }

    #[test]
    fn rendering_reports_toolchain_changes_and_clears_the_marker() {
        let dir = tempfile::tempdir().unwrap();
        let rt = runtime(dir.path());
        let creds = CredentialBundle::default();
        let a = agent(&[("node", "22.11.0")]);
        let paths = rt.layout.agent(&a.id);
        let first = rt
            .render_agent(&a, &creds, &hooks(), &RenderOptions::default())
            .unwrap();
        assert!(first.toolchain_changed);
        let again = rt
            .render_agent(&a, &creds, &hooks(), &RenderOptions::default())
            .unwrap();
        assert!(!again.toolchain_changed, "same inputs, same files");
        assert_eq!(again.plan, first.plan);

        std::fs::write(paths.installed_marker(), "").unwrap();
        rt.render_agent(&a, &creds, &hooks(), &RenderOptions::default())
            .unwrap();
        assert!(
            paths.installed_marker().exists(),
            "unchanged render keeps the marker"
        );
        let b = agent(&[("node", "22.12.0")]);
        let changed = rt
            .render_agent(&b, &creds, &hooks(), &RenderOptions::default())
            .unwrap();
        assert!(changed.toolchain_changed);
        assert!(
            !paths.installed_marker().exists(),
            "a new table invalidates the install"
        );
    }

    #[test]
    fn install_is_skipped_when_the_marker_exists() {
        let dir = tempfile::tempdir().unwrap();
        let rt = runtime(dir.path());
        let a = agent(&[]);
        let paths = rt.layout.agent(&a.id);
        rt.render_agent(
            &a,
            &CredentialBundle::default(),
            &hooks(),
            &RenderOptions::default(),
        )
        .unwrap();
        // tools point nowhere: running mise would fail with "cannot execute"
        assert!(rt.install_and_validate(&a).is_err());
        std::fs::write(paths.installed_marker(), "").unwrap();
        rt.install_and_validate(&a).unwrap();
    }
}
