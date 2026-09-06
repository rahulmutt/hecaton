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
    ) -> Result<LaunchPlan, MaterializeError> {
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
                with_gh,
                redact_credentials: opts.redact_credentials,
            },
        )?;

        let system = system_tools(&self.layout, id)?;
        Toolchain {
            tools: &self.tools,
            layout: &self.layout,
        }
        .write(id, &paths, &system, &agent.settings.tools, with_gh)?;

        let env = agent_env(id, &paths, &self.layout, &hooks.url, &agent.settings.env);
        let profile = render_profile(
            id,
            &hecaton_grants(&paths, &crew, &self.layout),
            hooks_port(&hooks.url),
            &env,
            &agent.settings.sandbox,
        )?;
        write_profile(id, &paths, &profile)?;

        let resume = wants_continue(&agent.settings.claude, &paths);
        let (script, plan) = render_launch(
            &paths,
            &self.tools,
            &agent.settings.claude.binary,
            &agent.settings.claude.args,
            resume,
        );
        write_launch(id, &paths, &script)?;
        Ok(plan)
    }

    /// The subprocess half of steps 3 and 4.
    pub fn install_and_validate(&self, agent: &ResolvedAgent) -> Result<(), MaterializeError> {
        let paths = self.layout.agent(&agent.id);
        Toolchain {
            tools: &self.tools,
            layout: &self.layout,
        }
        .install(&agent.id, &paths)?;
        validate_profile(&self.tools, &agent.id, &paths)
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
        let plan = self.render_agent(agent, creds, hooks, &RenderOptions::default())?;
        self.install_and_validate(agent)?;
        Ok(plan)
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
