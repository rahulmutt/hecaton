//! `Runtime`: the `Materializer` over real tools. `render_agent` is the
//! pure-ish half (files only) shared with `hecaton dev materialize`.

use hecaton_api::{CredentialBundle, GitAuth};
use hecaton_core::{HookTarget, LaunchPlan, MaterializeError, ResolvedAgent};

use crate::env::agent_env;
use crate::home::{HomeInputs, write_home};
use crate::launch::{hooks_port, render_launch, wants_continue, write_launch};
use crate::layout::StateLayout;
use crate::sandbox::{hecaton_grants, render_profile, validate_profile, write_profile};
use crate::toolchain::{Toolchain, system_tools};
use crate::tools::ToolPaths;

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
