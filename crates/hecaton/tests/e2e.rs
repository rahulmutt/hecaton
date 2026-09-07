//! The Phase 3 journey (spec §8): a real daemon over git, mise, nono and
//! tmux, with `hecaton dev fake-claude` in place of `claude`. Skips without
//! the tools or Landlock; `HECATON_REQUIRE_TOOLS=1` (CI) fails instead.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, Instant};

use hecaton_api::{AgentPhase, FleetPhase};
use hecaton_core::FleetRecord;

const HECATON: &str = env!("CARGO_BIN_EXE_hecaton");

fn tool(name: &str) -> Option<PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|d| d.join(name))
        .find(|p| p.is_file())
}

fn require_or_skip(name: &str, present: bool) -> bool {
    if present {
        return true;
    }
    if std::env::var_os("HECATON_REQUIRE_TOOLS").is_some_and(|v| v == "1") {
        panic!("{name} is required (HECATON_REQUIRE_TOOLS=1) but not available");
    }
    eprintln!("skip: {name} not available");
    false
}

/// See `hecaton-runtime/tests/support/mod.rs::landlock_works` for why the
/// probe home is a sibling of `root`.
fn landlock_works(nono: &Path, root: &Path) -> bool {
    let home = root.with_file_name(format!(
        "{}-nono-probe-home",
        root.file_name().unwrap_or_default().to_string_lossy()
    ));
    fs::create_dir_all(&home).unwrap();
    let ok = Command::new(nono)
        .args(["-s", "run", "--allow-cwd", "--", "/bin/true"])
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", &home)
        .current_dir(root)
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    let _ = fs::remove_dir_all(&home);
    ok
}

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@t")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .env_remove("GIT_PREFIX")
        .env_remove("GIT_COMMON_DIR")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

struct World {
    home: PathBuf,
    socket: String,
    tmux: PathBuf,
}

impl World {
    fn hecaton(&self) -> Command {
        let mut c = Command::new(HECATON);
        c.env("HOME", &self.home)
            .env_remove("XDG_CONFIG_HOME")
            .env_remove("XDG_STATE_HOME")
            .env_remove("XDG_DATA_HOME")
            .env_remove("HECATON_API_URL")
            .env_remove("CLAUDE_CONFIG_DIR")
            .env_remove("GH_CONFIG_DIR");
        c
    }
    fn run(&self, args: &[&str]) -> Output {
        let out = self.hecaton().args(args).output().unwrap();
        eprintln!(
            "$ hecaton {}\n{}{}",
            args.join(" "),
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        out
    }
    fn ok(&self, args: &[&str]) -> String {
        let out = self.run(args);
        assert!(out.status.success(), "hecaton {args:?} failed");
        String::from_utf8_lossy(&out.stdout).into_owned()
    }
    fn state(&self) -> PathBuf {
        self.home.join(".local").join("state").join("hecaton")
    }
    fn agent_dir(&self, agent: &str) -> PathBuf {
        self.state().join("fleets/e2e/crews/c/agents").join(agent)
    }
    fn status(&self) -> FleetRecord {
        serde_json::from_str(&self.ok(&["status", "e2e", "--json"])).unwrap()
    }
    fn window_pids(&self) -> String {
        let out = Command::new(&self.tmux)
            .args([
                "-L",
                &self.socket,
                "list-windows",
                "-t",
                "=e2e/c",
                "-F",
                "#{window_name} #{pane_pid}",
            ])
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).into_owned()
    }
    fn pid_of(&self, window: &str) -> String {
        self.window_pids()
            .lines()
            .find_map(|l| l.strip_prefix(&format!("{window} ")).map(str::to_string))
            .unwrap_or_else(|| panic!("no window {window} in {:?}", self.window_pids()))
    }
}

impl Drop for World {
    fn drop(&mut self) {
        let pid_file = self.state().join("server").join("hecaton.pid");
        if let Ok(pid) = fs::read_to_string(&pid_file) {
            let pid = pid.trim().to_string();
            let _ = Command::new("kill").args(["-TERM", &pid]).status();
            let start = Instant::now();
            while pid_file.exists() && start.elapsed() < Duration::from_secs(5) {
                std::thread::sleep(Duration::from_millis(100));
            }
            // Only escalate if the TERM wait timed out with the daemon
            // still up; a clean shutdown removes the pid file and there is
            // nothing left to kill.
            if pid_file.exists() {
                let _ = Command::new("kill").args(["-KILL", &pid]).status();
            }
        }
        let _ = Command::new(&self.tmux)
            .args(["-L", &self.socket, "kill-server"])
            .status();
    }
}

fn fleet_yaml(bare: &Path, bob_model: Option<&str>, plugins: Option<&str>) -> String {
    let bob = match bob_model {
        Some(m) => {
            format!("      bob: {{ claude: {{ resume: true, settings: {{ model: {m} }} }} }}\n")
        }
        None => "      bob: { claude: { resume: true } }\n".to_string(),
    };
    // only alice takes plugins; bob is the pass-through control
    let alice = match plugins {
        Some(p) => format!("      alice: {{ plugins: {p} }}\n"),
        None => "      alice: {}\n".to_string(),
    };
    format!(
        "apiVersion: hecaton/v1\nkind: Fleet\nname: e2e\ndefaults:\n  claude:\n    binary: \"{HECATON}\"\n    args: [dev, fake-claude, \"--verbose\"]\n    settings: {{ model: sonnet }}\n  tools: {{}}\ncrews:\n  c:\n    repo: \"file://{}\"\n    ref: main\n    git: {{ push: false, auth: none }}\n    agents:\n{alice}{bob}",
        bare.display()
    )
}

fn hook_secrets(w: &World) -> Vec<String> {
    let mut out = Vec::new();
    for a in ["alice", "bob"] {
        let settings =
            fs::read_to_string(w.agent_dir(a).join("home/.claude/settings.json")).unwrap();
        for piece in settings.split("Bearer ").skip(1) {
            out.push(piece.split('"').next().unwrap().to_string());
        }
    }
    assert!(!out.is_empty());
    out
}

/// Polls for a file to exist and be non-empty, up to 30 s. The plugin
/// protocol writes these files after the call that triggered them has
/// already returned, so a single read races the writer.
fn wait_file(path: &Path) -> String {
    wait_file_until(path, |s| !s.trim().is_empty())
}

/// Polls (200 ms, 30 s cap) until the file's whole content satisfies
/// `pred`. A file the writer appends to line by line is read while it is
/// still being written: "non-empty" is not "complete", and an unbuffered
/// `writeln!` can even be caught mid-line, so the caller says what it
/// needs to see before the content is worth parsing.
fn wait_file_until(path: &Path, pred: impl Fn(&str) -> bool) -> String {
    let start = Instant::now();
    let mut last = String::new();
    loop {
        if let Ok(s) = fs::read_to_string(path) {
            if pred(&s) {
                return s;
            }
            last = s;
        }
        assert!(
            start.elapsed() < Duration::from_secs(30),
            "{} never arrived complete; last content:\n{last}",
            path.display()
        );
        std::thread::sleep(Duration::from_millis(200));
    }
}

#[test]
fn serve_up_update_down_journey() {
    let Some(nono) = tool("nono") else {
        assert!(!require_or_skip("nono", false));
        return;
    };
    for t in ["git", "gh", "mise", "tmux"] {
        if !require_or_skip(t, tool(t).is_some()) {
            return;
        }
    }
    let root = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("e2e-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();
    if !require_or_skip("landlock", landlock_works(&nono, &root)) {
        return;
    }
    let w = World {
        home: root.join("home"),
        socket: format!("hecaton-e2e-{}", std::process::id()),
        tmux: tool("tmux").unwrap(),
    };
    fs::create_dir_all(&w.home).unwrap();
    // empty system tool table: nothing to download, `mise exec` still resolves
    let cfg = w.home.join(".config/hecaton");
    fs::create_dir_all(&cfg).unwrap();
    fs::write(cfg.join("mise.toml"), "[tools]\n").unwrap();

    // a repo with one commit and an untrusted mise.toml naming an uninstalled tool
    let work = root.join("work");
    fs::create_dir_all(&work).unwrap();
    git(&work, &["init", "-q", "-b", "main"]);
    fs::write(work.join("README"), "hi\n").unwrap();
    fs::write(work.join("mise.toml"), "[tools]\nnode = \"0.0.1\"\n").unwrap();
    git(&work, &["add", "."]);
    git(&work, &["commit", "-q", "-m", "init"]);
    let bare = root.join("repo.git");
    git(
        &root,
        &[
            "clone",
            "-q",
            "--bare",
            &work.display().to_string(),
            &bare.display().to_string(),
        ],
    );
    let v1 = root.join("fleet.yaml");
    let v2 = root.join("fleet2.yaml");
    fs::write(&v1, fleet_yaml(&bare, None, None)).unwrap();
    fs::write(&v2, fleet_yaml(&bare, Some("opus"), None)).unwrap();

    // serve -d
    let out = w.ok(&[
        "serve",
        "-d",
        "--bind",
        "127.0.0.1:0",
        "--tmux-socket",
        &w.socket,
    ]);
    assert!(out.contains("http://127.0.0.1:"), "{out}");
    let url = fs::read_to_string(w.state().join("server/endpoint"))
        .unwrap()
        .trim()
        .to_string();

    // up → both Ready through the relay
    let out = w.ok(&[
        "up",
        &v1.display().to_string(),
        "--no-host-defaults",
        "--timeout",
        "180s",
    ]);
    assert!(out.contains("e2e  ready"), "{out}");
    let rec = w.status();
    assert_eq!(rec.status.phase, FleetPhase::Ready);
    for a in ["e2e/c/alice", "e2e/c/bob"] {
        assert_eq!(rec.status.agents[a].phase, AgentPhase::Ready, "{a}");
    }
    let alice_hash = rec.status.agents["e2e/c/alice"]
        .applied_hash
        .clone()
        .unwrap();
    let bob_hash = rec.status.agents["e2e/c/bob"].applied_hash.clone().unwrap();
    let alice_pid = w.pid_of("alice");
    // `list` pads NAME to the header's width, so the row is "e2e" + 1 pad + 2
    // separators; `status`'s headline joins with a literal two spaces.
    assert!(w.ok(&["list"]).contains("e2e   ready"));
    let argv = fs::read_to_string(w.agent_dir("alice").join("home/fake-claude.argv")).unwrap();
    assert!(
        argv.contains("--verbose") && !argv.contains("--continue"),
        "{argv}"
    );
    assert!(
        w.agent_dir("alice").join("workspace/README").exists(),
        "worktree checked out"
    );
    assert!(w.agent_dir("alice").join("home/.gitconfig").exists());

    // metrics saw the relay (SessionStart) and the HTTP hook (Notification).
    // `up` returns as soon as `Ready` is set by the SessionStart relay; the
    // fake posts `Notification` just after, so poll rather than scrape once.
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .build()
        .into();
    let session_start = "hecaton_hook_events_total{agent=\"alice\",crew=\"c\",event=\"SessionStart\",fleet=\"e2e\"} 1";
    let notification = "hecaton_hook_events_total{agent=\"alice\",crew=\"c\",event=\"Notification\",fleet=\"e2e\"} 1";
    let start = Instant::now();
    let metrics = loop {
        let metrics = agent
            .get(format!("{url}/metrics"))
            .call()
            .unwrap()
            .body_mut()
            .read_to_string()
            .unwrap();
        if metrics.contains(session_start) && metrics.contains(notification) {
            break metrics;
        }
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "metrics never showed both hooks within 10s; last scrape:\n{metrics}"
        );
        std::thread::sleep(Duration::from_millis(100));
    };
    assert!(metrics.contains(session_start), "{metrics}");
    assert!(metrics.contains(notification), "{metrics}");

    // update bob only
    let out = w.ok(&[
        "update",
        &v2.display().to_string(),
        "--no-host-defaults",
        "--timeout",
        "180s",
    ]);
    assert!(out.contains("e2e  ready"), "{out}");
    let rec = w.status();
    assert_eq!(rec.generation, 2);
    assert_ne!(
        rec.status.agents["e2e/c/bob"]
            .applied_hash
            .as_ref()
            .unwrap(),
        &bob_hash
    );
    assert_eq!(
        rec.status.agents["e2e/c/alice"]
            .applied_hash
            .as_ref()
            .unwrap(),
        &alice_hash
    );
    assert_eq!(
        rec.status.agents["e2e/c/bob"].restarts, 0,
        "a spec change is not a crash"
    );
    assert_eq!(w.pid_of("alice"), alice_pid, "alice untouched");

    // down --keep
    let out = w.ok(&["down", "e2e", "--keep", "--timeout", "60s"]);
    assert!(out.contains("e2e  down"), "{out}");
    assert!(w.state().join("fleets/e2e/crews/c/repo").exists());
    assert!(w.agent_dir("bob").join("home").exists());
    assert!(w.state().join("fleets/e2e/fleet.json").exists());
    assert!(w.window_pids().is_empty(), "session gone");

    // up again re-applies in place; bob resumes
    let out = w.ok(&[
        "up",
        &v2.display().to_string(),
        "--no-host-defaults",
        "--timeout",
        "180s",
    ]);
    assert!(out.contains("e2e  ready"), "{out}");
    assert_eq!(w.status().generation, 3);
    let bob_argv = fs::read_to_string(w.agent_dir("bob").join("home/fake-claude.argv")).unwrap();
    assert!(bob_argv.contains("--continue"), "{bob_argv}");
    let alice_argv =
        fs::read_to_string(w.agent_dir("alice").join("home/fake-claude.argv")).unwrap();
    assert!(!alice_argv.contains("--continue"), "{alice_argv}");

    // no secret leaks into the daemon log or launch scripts
    let secrets = hook_secrets(&w);
    let token = fs::read_to_string(w.state().join("server/token")).unwrap();
    let log = fs::read_to_string(w.state().join("server/server.log")).unwrap();
    for s in secrets
        .iter()
        .chain(std::iter::once(&token.trim().to_string()))
    {
        assert!(!log.contains(s), "secret in server.log");
        for a in ["alice", "bob"] {
            let launch = fs::read_to_string(w.agent_dir(a).join("launch.sh")).unwrap();
            assert!(!launch.contains(s), "secret in {a}'s launch.sh");
        }
    }

    // purge
    let out = w.ok(&["down", "e2e", "--purge", "--timeout", "60s"]);
    assert!(out.contains("e2e: purged"), "{out}");
    assert!(!w.state().join("fleets/e2e").exists());
    let out = w.run(&["status", "e2e"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("not found"));
    drop(w);
    assert!(
        !root
            .join("home/.local/state/hecaton/server/endpoint")
            .exists(),
        "SIGTERM cleaned up"
    );
}

fn plugin_package(dir: &Path, manifest_extra: &str) {
    fs::create_dir_all(dir).unwrap();
    fs::write(
        dir.join("hecaton-plugin.yaml"),
        format!(
            "apiVersion: hecaton/v1\nkind: Plugin\nname: hello\nversion: 0.1.0\nprotocol: 1\nstart: serve\nroutes: false\n{manifest_extra}"
        ),
    )
    .unwrap();
    // an empty tool table (nothing to download) and a task that runs this
    // very binary as the plugin
    fs::write(
        dir.join("mise.toml"),
        format!("[tools]\n\n[tasks.serve]\nrun = \"'{HECATON}' dev fake-plugin\"\n"),
    )
    .unwrap();
}

/// Waits (up to 180 s, the toolchain install is real) for `plugin list` to
/// show `name` in phase `ready`, and returns that listing. On timeout it
/// prints the plugin's own logs — the sandbox is where a start fails.
fn wait_plugin_ready(w: &World, name: &str, plugin_dir: &Path) -> String {
    let start = Instant::now();
    loop {
        let list = w.ok(&["plugin", "list"]);
        if list
            .lines()
            .any(|l| l.starts_with(name) && l.contains("ready"))
        {
            return list;
        }
        assert!(
            start.elapsed() < Duration::from_secs(180),
            "plugin never became ready; last list:\n{list}\nnono.log:\n{}\nmise.toolchain.log:\n{}",
            fs::read_to_string(plugin_dir.join("logs/nono.log")).unwrap_or_default(),
            fs::read_to_string(plugin_dir.join("logs/mise.toolchain.log")).unwrap_or_default()
        );
        std::thread::sleep(Duration::from_millis(250));
    }
}

/// Plugins spec §13 item 1, "done when": a trivial SDK plugin reaches
/// `Ready` through a real `mise run` under nono, and the reserved fleet,
/// removal and purge behave.
#[test]
fn plugin_hello_journey() {
    let Some(nono) = tool("nono") else {
        assert!(!require_or_skip("nono", false));
        return;
    };
    for t in ["git", "gh", "mise", "tmux"] {
        if !require_or_skip(t, tool(t).is_some()) {
            return;
        }
    }
    let root =
        Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("e2e-plugins-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();
    if !require_or_skip("landlock", landlock_works(&nono, &root)) {
        return;
    }
    let w = World {
        home: root.join("home"),
        socket: format!("hecaton-e2e-plugins-{}", std::process::id()),
        tmux: tool("tmux").unwrap(),
    };
    fs::create_dir_all(&w.home).unwrap();
    let cfg = w.home.join(".config/hecaton");
    fs::create_dir_all(&cfg).unwrap();
    fs::write(cfg.join("mise.toml"), "[tools]\n").unwrap();
    let pkg = root.join("hello-pkg");
    plugin_package(&pkg, "");
    fs::write(
        cfg.join("plugins.yaml"),
        format!(
            "plugins:\n  - name: hello\n    source: \"{}\"\n    config: {{ greeting: hi }}\n",
            pkg.display()
        ),
    )
    .unwrap();

    let out = w.ok(&[
        "serve",
        "-d",
        "--bind",
        "127.0.0.1:0",
        "--tmux-socket",
        &w.socket,
    ]);
    assert!(out.contains("http://127.0.0.1:"), "{out}");
    let url = fs::read_to_string(w.state().join("server/endpoint"))
        .unwrap()
        .trim()
        .to_string();
    let plugin_dir = w.state().join("plugins/hello");

    // Ready through hello, with a real loopback listen address
    let list = wait_plugin_ready(&w, "hello", &plugin_dir);
    assert!(list.contains("hello  0.1.0    ready"), "{list}");
    assert!(list.contains("127.0.0.1:"), "{list}");
    assert!(
        !plugin_dir.join("scratch/fake-plugin.bind-failed").exists(),
        "spec §11.1 row 1 FAILED: a sandboxed plugin could not bind a loopback listener; record the verdict and take the Unix-socket fallback in phase 2: {}",
        fs::read_to_string(plugin_dir.join("scratch/fake-plugin.bind-failed")).unwrap_or_default()
    );
    let hello = wait_file(&plugin_dir.join("scratch/fake-plugin.hello"));
    assert!(hello.contains("\"greeting\": \"hi\""), "{hello}");
    let rec: FleetRecord = serde_json::from_str(&w.ok(&["status", "hecaton", "--json"])).unwrap();
    assert_eq!(
        rec.status.agents["hecaton/plugins/hello"].phase,
        AgentPhase::Ready
    );
    assert_eq!(rec.status.phase, FleetPhase::Ready);
    assert!(
        w.ok(&["list"]).contains("no fleets"),
        "plugins are not a fleet row"
    );

    // the token is in the profile (0600) and nowhere else
    let profile_path = plugin_dir.join("nono-profile.json");
    assert_eq!(
        fs::metadata(&profile_path).unwrap().permissions().mode() & 0o777,
        0o600,
        "the profile carries the plugin token"
    );
    let profile = fs::read_to_string(&profile_path).unwrap();
    let token = profile
        .split("\"HECATON_PLUGIN_TOKEN\": \"")
        .nth(1)
        .unwrap()
        .split('"')
        .next()
        .unwrap()
        .to_string();
    assert_eq!(token.len(), 64);
    assert!(
        !fs::read_to_string(plugin_dir.join("launch.sh"))
            .unwrap()
            .contains(&token)
    );
    assert!(
        !fs::read_to_string(w.state().join("server/server.log"))
            .unwrap()
            .contains(&token)
    );
    assert!(!pkg.join("escape").exists());

    // metrics
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .build()
        .into();
    let metrics = agent
        .get(format!("{url}/metrics"))
        .call()
        .unwrap()
        .body_mut()
        .read_to_string()
        .unwrap();
    assert!(
        metrics.contains("hecaton_agents{crew=\"plugins\",fleet=\"hecaton\",phase=\"ready\"} 1"),
        "{metrics}"
    );

    // a user fleet may not take the reserved name
    let reserved = root.join("reserved.yaml");
    fs::write(
        &reserved,
        "apiVersion: hecaton/v1\nkind: Fleet\nname: hecaton\n",
    )
    .unwrap();
    let out = w.run(&[
        "up",
        &reserved.display().to_string(),
        "--no-host-defaults",
        "--no-wait",
    ]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("reserved for the daemon's plugins"));

    // remove: stopped, window gone, state kept
    let out = w.ok(&["plugin", "remove", "hello"]);
    assert!(out.contains("stopped: hello"), "{out}");
    assert_eq!(w.ok(&["plugin", "list"]), "no plugins\n");
    let start = Instant::now();
    loop {
        let windows = Command::new(&w.tmux)
            .args([
                "-L",
                &w.socket,
                "list-windows",
                "-t",
                "=hecaton/plugins",
                "-F",
                "#{window_name}",
            ])
            .output()
            .unwrap();
        if !String::from_utf8_lossy(&windows.stdout).contains("hello") {
            break;
        }
        assert!(
            start.elapsed() < Duration::from_secs(30),
            "plugin window still present"
        );
        std::thread::sleep(Duration::from_millis(250));
    }
    assert!(
        plugin_dir.join("scratch").exists(),
        "state survives removal"
    );

    // Purge on an undeclared plugin deletes the state, first time. nono
    // flushes its audit ledger and session file into `plugins/hello/nono`
    // while it exits, which is after tmux has already dropped the window:
    // the daemon waits for the plugin to leave the record and retries
    // `remove_dir_all` on `Directory not empty`, so one attempt is enough.
    let out = w.ok(&["plugin", "remove", "hello", "--purge"]);
    assert!(out.contains("purged"), "{out}");
    assert!(!plugin_dir.exists(), "purge deletes plugins/hello");
    drop(w);
}

/// Plugins spec §13 item 2a, "done when": through a real daemon, nono and
/// tmux, the SDK plugin blocks a PreToolUse and sends text to fake-claude;
/// activation is pending until the plugin says hello, then active.
#[test]
fn plugin_protocol_journey() {
    let Some(nono) = tool("nono") else {
        assert!(!require_or_skip("nono", false));
        return;
    };
    for t in ["git", "gh", "mise", "tmux"] {
        if !require_or_skip(t, tool(t).is_some()) {
            return;
        }
    }
    let root =
        Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("e2e-protocol-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();
    if !require_or_skip("landlock", landlock_works(&nono, &root)) {
        return;
    }
    let w = World {
        home: root.join("home"),
        socket: format!("hecaton-e2e-protocol-{}", std::process::id()),
        tmux: tool("tmux").unwrap(),
    };
    fs::create_dir_all(&w.home).unwrap();
    let cfg = w.home.join(".config/hecaton");
    fs::create_dir_all(&cfg).unwrap();
    fs::write(cfg.join("mise.toml"), "[tools]\n").unwrap();
    let pkg = root.join("fake-pkg");
    plugin_package(
        &pkg,
        "hooks:\n  intercept: [PreToolUse, Stop]\n  observe: [SessionStart, Notification, PreToolUse, Stop]\nneeds: [actions, kv]\n",
    );
    // the package's manifest names the plugin `fake`
    let manifest = fs::read_to_string(pkg.join("hecaton-plugin.yaml"))
        .unwrap()
        .replace("name: hello", "name: fake");
    fs::write(pkg.join("hecaton-plugin.yaml"), manifest).unwrap();
    fs::write(
        cfg.join("plugins.yaml"),
        format!(
            "plugins:\n  - name: fake\n    source: \"{}\"\n",
            pkg.display()
        ),
    )
    .unwrap();

    // the same bare repo recipe as the first journey
    let work = root.join("work");
    fs::create_dir_all(&work).unwrap();
    git(&work, &["init", "-q", "-b", "main"]);
    fs::write(work.join("README"), "hi\n").unwrap();
    git(&work, &["add", "."]);
    git(&work, &["commit", "-q", "-m", "init"]);
    let bare = root.join("repo.git");
    git(
        &root,
        &[
            "clone",
            "-q",
            "--bare",
            &work.display().to_string(),
            &bare.display().to_string(),
        ],
    );
    let fleet = root.join("fleet.yaml");
    fs::write(&fleet, fleet_yaml(&bare, None, Some("{ fake: {} }"))).unwrap();

    let out = w.ok(&[
        "serve",
        "-d",
        "--bind",
        "127.0.0.1:0",
        "--tmux-socket",
        &w.socket,
    ]);
    assert!(out.contains("http://127.0.0.1:"), "{out}");
    let plugin_dir = w.state().join("plugins/fake");

    // The plugin has to be `Ready` before `up`: `Daemon::apply` activates
    // the pair inline only against a plugin that is already listening, and
    // fake-claude posts its PreToolUse moments after alice starts. A pair
    // left pending would activate at the next hello — too late to block.
    wait_plugin_ready(&w, "fake", &plugin_dir);

    // up waits for the plugin's activation too: the plugin must reach
    // hello (under nono, via mise run) for alice's row to turn active
    let out = w.ok(&[
        "up",
        &fleet.display().to_string(),
        "--no-host-defaults",
        "--timeout",
        "180s",
    ]);
    assert!(out.contains("e2e  ready"), "{out}");
    assert!(out.contains("fake=active"), "{out}");
    let rec = w.status();
    assert_eq!(
        rec.status.agents["e2e/c/alice"].plugins["fake"].state,
        hecaton_api::ActivationState::Active
    );
    assert!(rec.status.agents["e2e/c/bob"].plugins.is_empty());
    let list = w.ok(&["plugin", "list"]);
    assert!(list.contains("fake  ") && list.contains("ready"), "{list}");
    assert!(
        list.lines()
            .nth(1)
            .is_some_and(|l| l.split_whitespace().nth(5) == Some("1")),
        "ACTIVE column: {list}"
    );
    let activations = fs::read_to_string(plugin_dir.join("scratch/activations.jsonl")).unwrap();
    assert!(
        activations.contains("\"agent\":\"e2e/c/alice\""),
        "{activations}"
    );

    // the PreToolUse block came back to fake-claude through the HTTP hook
    let reply = wait_file(
        &w.agent_dir("alice")
            .join("home/fake-claude.PreToolUse.reply"),
    );
    assert!(reply.contains("\"decision\":\"block\""), "{reply}");
    assert!(
        reply.contains("fake-plugin: no recursive deletes"),
        "{reply}"
    );
    let bob_reply = wait_file(&w.agent_dir("bob").join("home/fake-claude.PreToolUse.reply"));
    assert_eq!(bob_reply.trim(), "{}", "bob has no plugin: pass-through");

    // the Stop verdict's send_text reached alice's stdin through tmux
    let stdin = wait_file(&w.agent_dir("alice").join("home/fake-claude.stdin"));
    assert!(stdin.contains("fake-plugin says hi"), "{stdin}");

    // Observers saw alice's events, in order, and none of bob's. The
    // delivery task batches 100 ms after the *first* event is enqueued, so
    // `SessionStart` can be flushed on its own and Stop's intercept
    // (proven above) says nothing about its observe batch: wait until every
    // line parses and alice's `Stop` has landed, or a torn append and a
    // half-written file would panic in the parse below.
    let events = wait_file_until(&plugin_dir.join("scratch/events.jsonl"), |s| {
        let mut lines = s.lines().filter(|l| !l.trim().is_empty()).peekable();
        lines.peek().is_some()
            && lines
                .map(serde_json::from_str::<serde_json::Value>)
                .collect::<Result<Vec<_>, _>>()
                .is_ok_and(|vs| {
                    vs.iter()
                        .any(|v| v["agent"] == "e2e/c/alice" && v["name"] == "Stop")
                })
    });
    let names: Vec<String> = events
        .lines()
        .map(|l| serde_json::from_str::<serde_json::Value>(l).unwrap())
        .map(|v| {
            format!(
                "{} {}",
                v["agent"].as_str().unwrap(),
                v["name"].as_str().unwrap()
            )
        })
        .collect();
    assert!(
        names.iter().all(|n| n.starts_with("e2e/c/alice ")),
        "{names:?}"
    );
    let idx = |name: &str| {
        names
            .iter()
            .position(|n| n.ends_with(name))
            .unwrap_or_else(|| panic!("{name} missing in {names:?}"))
    };
    assert!(idx(" SessionStart") < idx(" PreToolUse") && idx(" PreToolUse") < idx(" Stop"));

    // metrics: the chain ran, the action ran, nothing failed
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .build()
        .into();
    let url = fs::read_to_string(w.state().join("server/endpoint"))
        .unwrap()
        .trim()
        .to_string();
    let metrics = agent
        .get(format!("{url}/metrics"))
        .call()
        .unwrap()
        .body_mut()
        .read_to_string()
        .unwrap();
    assert!(
        metrics.contains(
            "hecaton_plugin_events_total{event=\"PreToolUse\",mode=\"intercept\",plugin=\"fake\"} 1"
        ),
        "{metrics}"
    );
    assert!(
        metrics.contains("hecaton_plugin_actions_total{action=\"send_text\",plugin=\"fake\"} 1"),
        "{metrics}"
    );
    assert!(
        !metrics.contains("hecaton_plugin_intercept_failures_total{plugin=\"fake\""),
        "{metrics}"
    );

    // no token leaks (the plugin's, the agents', the admin's)
    let profile = fs::read_to_string(plugin_dir.join("nono-profile.json")).unwrap();
    let token = profile
        .split("\"HECATON_PLUGIN_TOKEN\": \"")
        .nth(1)
        .unwrap()
        .split('"')
        .next()
        .unwrap()
        .to_string();
    let log = fs::read_to_string(w.state().join("server/server.log")).unwrap();
    assert!(!log.contains(&token));
    for s in hook_secrets(&w) {
        assert!(!log.contains(&s), "hook secret in server.log");
    }

    // removing the plugin while the fleet runs: alice's row disappears with the plugin
    w.ok(&["plugin", "remove", "fake"]);
    let start = Instant::now();
    loop {
        let rec = w.status();
        if rec.status.agents["e2e/c/alice"].plugins.is_empty() {
            break;
        }
        assert!(
            start.elapsed() < Duration::from_secs(30),
            "row still there: {rec:?}"
        );
        std::thread::sleep(Duration::from_millis(250));
    }
    w.ok(&["down", "e2e", "--purge", "--timeout", "60s"]);
    drop(w);
}

/// Where `mise run package-plugins` left the flow package: next to this
/// binary's target dir. `None` when it has not been run.
fn flow_package() -> Option<PathBuf> {
    let dir = Path::new(HECATON).parent()?.parent()?.join("plugins/flow");
    dir.join("bin/hecaton-plugin-flow").exists().then_some(dir)
}

/// alice's `plugins:` block for the journey, in YAML flow style, with the
/// `rm -rf` pattern injectable so the rejection case can break it.
fn flow_block(pattern: &str) -> String {
    format!(
        "{{ flow: {{ initial: working, states: {{ \
           working: {{ on: [ \
             {{ event: PreToolUse, match: {{ /tool_input/command: \"{pattern}\" }}, \
                respond: {{ decision: block, reason: \"flow: no recursive deletes\" }} }}, \
             {{ event: Stop, goto: review, send: {{ text: \"flow says: run the tests\" }} }} ] }}, \
           review: {{ on: [ {{ event: Stop, goto: done }} ] }}, \
           done: {{}} }} }} }}"
    )
}

/// Plugins spec §13 item 2b, "done when" (§17.7): the packaged flow plugin
/// through a real daemon, nono and tmux — a block, a send_text, a
/// transition visible in /metrics and in KV, a rejected `update` that
/// leaves the fleet running, and a reset on `down`.
#[test]
fn flow_journey() {
    let Some(nono) = tool("nono") else {
        assert!(!require_or_skip("nono", false));
        return;
    };
    for t in ["git", "gh", "mise", "tmux"] {
        if !require_or_skip(t, tool(t).is_some()) {
            return;
        }
    }
    let Some(pkg) = flow_package() else {
        assert!(!require_or_skip(
            "target/plugins/flow (run `mise run package-plugins`)",
            false
        ));
        return;
    };
    let root =
        Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("e2e-flow-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();
    if !require_or_skip("landlock", landlock_works(&nono, &root)) {
        return;
    }
    let w = World {
        home: root.join("home"),
        socket: format!("hecaton-e2e-flow-{}", std::process::id()),
        tmux: tool("tmux").unwrap(),
    };
    fs::create_dir_all(&w.home).unwrap();
    let cfg = w.home.join(".config/hecaton");
    fs::create_dir_all(&cfg).unwrap();
    fs::write(cfg.join("mise.toml"), "[tools]\n").unwrap();
    fs::write(
        cfg.join("plugins.yaml"),
        format!(
            "plugins:\n  - name: flow\n    source: \"{}\"\n",
            pkg.display()
        ),
    )
    .unwrap();

    let work = root.join("work");
    fs::create_dir_all(&work).unwrap();
    git(&work, &["init", "-q", "-b", "main"]);
    fs::write(work.join("README"), "hi\n").unwrap();
    git(&work, &["add", "."]);
    git(&work, &["commit", "-q", "-m", "init"]);
    let bare = root.join("repo.git");
    git(
        &root,
        &[
            "clone",
            "-q",
            "--bare",
            &work.display().to_string(),
            &bare.display().to_string(),
        ],
    );
    let fleet = root.join("fleet.yaml");
    fs::write(
        &fleet,
        fleet_yaml(&bare, None, Some(&flow_block("rm -rf.*"))),
    )
    .unwrap();

    let out = w.ok(&[
        "serve",
        "-d",
        "--bind",
        "127.0.0.1:0",
        "--tmux-socket",
        &w.socket,
    ]);
    assert!(out.contains("http://127.0.0.1:"), "{out}");
    let url = fs::read_to_string(w.state().join("server/endpoint"))
        .unwrap()
        .trim()
        .to_string();
    let plugin_dir = w.state().join("plugins/flow");
    // as in plugin_protocol_journey: the pair must activate inline, before
    // fake-claude's PreToolUse fires
    wait_plugin_ready(&w, "flow", &plugin_dir);

    let out = w.ok(&[
        "up",
        &fleet.display().to_string(),
        "--no-host-defaults",
        "--timeout",
        "180s",
    ]);
    assert!(out.contains("e2e  ready"), "{out}");
    assert!(out.contains("flow=active"), "{out}");

    // the block came back through the HTTP hook, with flow's reason; bob is pass-through
    let reply = wait_file(
        &w.agent_dir("alice")
            .join("home/fake-claude.PreToolUse.reply"),
    );
    assert!(reply.contains("\"decision\":\"block\""), "{reply}");
    assert!(reply.contains("flow: no recursive deletes"), "{reply}");
    let bob_reply = wait_file(&w.agent_dir("bob").join("home/fake-claude.PreToolUse.reply"));
    assert_eq!(bob_reply.trim(), "{}", "bob has no plugin: pass-through");

    // Stop → review carried the send_text to alice's stdin through tmux
    let stdin = wait_file(&w.agent_dir("alice").join("home/fake-claude.stdin"));
    assert!(stdin.contains("flow says: run the tests"), "{stdin}");

    // the transition is in the daemon's /metrics (re-exported from the
    // plugin's scrape) and in KV; poll: the KV put follows the verdict
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .build()
        .into();
    let state =
        "hecaton_plugin_flow_state{agent=\"alice\",crew=\"c\",fleet=\"e2e\",state=\"review\"} 1";
    let transition = "hecaton_plugin_flow_transitions_total{agent=\"alice\",crew=\"c\",fleet=\"e2e\",from=\"working\",to=\"review\"} 1";
    let start = Instant::now();
    let metrics = loop {
        let m = agent
            .get(format!("{url}/metrics"))
            .call()
            .unwrap()
            .body_mut()
            .read_to_string()
            .unwrap();
        if m.contains(state) && m.contains(transition) {
            break m;
        }
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "metrics never showed the transition within 10s; last scrape:\n{m}"
        );
        std::thread::sleep(Duration::from_millis(100));
    };
    assert!(
        !metrics.contains("state=\"working\"} 1"),
        "the previous state's series is gone: {metrics}"
    );
    let kv = plugin_dir.join("kv/state/e2e/c/alice");
    let stored = wait_file_until(&kv, |s| s.contains("\"review\""));
    assert!(stored.contains("\"state\":\"review\""), "{stored}");

    // a bad regex is rejected with the full config path and the fleet keeps running
    fs::write(&fleet, fleet_yaml(&bare, None, Some(&flow_block("[")))).unwrap();
    let out = w.run(&[
        "update",
        &fleet.display().to_string(),
        "--no-host-defaults",
        "--timeout",
        "60s",
    ]);
    assert!(!out.status.success(), "update with a bad regex must fail");
    let err = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        err.contains(
            "crews.c.agents.alice.plugins.flow: states.working.on[0].match./tool_input/command: "
        ),
        "{err}"
    );
    let rec = w.status();
    assert_eq!(
        rec.status.phase,
        FleetPhase::Ready,
        "the rejected update changed nothing"
    );
    assert_eq!(
        rec.status.agents["e2e/c/alice"].plugins["flow"].state,
        hecaton_api::ActivationState::Active
    );

    // down deactivates: the key is deleted
    let out = w.ok(&["down", "e2e", "--keep", "--timeout", "60s"]);
    assert!(out.contains("e2e  down"), "{out}");
    let start = Instant::now();
    while kv.exists() {
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "kv/state/e2e/c/alice still present after down"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    drop(w);
}
