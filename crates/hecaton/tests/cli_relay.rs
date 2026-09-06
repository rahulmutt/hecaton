#![allow(clippy::unwrap_used, clippy::expect_used)]

use assert_cmd::Command;
use predicates::prelude::*;

#[test]
fn hook_relay_never_fails_the_agent() {
    Command::new(env!("CARGO_BIN_EXE_hecaton"))
        .arg("hook-relay")
        .env("HECATON_API_URL", "http://127.0.0.1:1")
        .env("HECATON_AGENT_ID", "f/c/a")
        .env("HECATON_HOOK_SECRET", "s")
        .write_stdin(r#"{"hook_event_name":"SessionStart"}"#)
        .assert()
        .success()
        .stdout("{}\n")
        .stderr(predicate::str::contains("hecaton hook-relay:"));
    Command::new(env!("CARGO_BIN_EXE_hecaton"))
        .arg("hook-relay")
        .env_remove("HECATON_API_URL")
        .write_stdin("{}")
        .assert()
        .success()
        .stdout("{}\n")
        .stderr(predicate::str::contains("HECATON_API_URL"));
}
