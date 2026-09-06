//! The hook-event port (architecture spec §8; Phase 3 spec P3-6). Handlers
//! are fast and pure; `Outcome` carries only the response until Spec B.

use hecaton_api::HookEvent;
use serde_json::{Value, json};

/// What the daemon answers Claude with. `{}` means allow / no-op.
#[derive(Debug, Clone, PartialEq)]
pub struct Outcome {
    pub response: Value,
}

impl Outcome {
    pub fn allow() -> Self {
        Self {
            response: json!({}),
        }
    }
}

pub trait EventHandler: Send + Sync {
    fn handle(&self, event: &HookEvent) -> Outcome;
}

/// Phase 3's only handler: allow everything, do nothing.
#[derive(Debug, Default, Clone, Copy)]
pub struct PassThrough;

impl EventHandler for PassThrough {
    fn handle(&self, _: &HookEvent) -> Outcome {
        Outcome::allow()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hecaton_api::Timestamp;
    use serde_json::json;

    #[test]
    fn pass_through_allows_everything() {
        let e = HookEvent {
            agent: "f/c/a".into(),
            name: "PreToolUse".into(),
            session_id: None,
            received_at: Timestamp(1),
            payload: json!({ "tool_name": "Bash" }),
        };
        let h: &dyn EventHandler = &PassThrough;
        assert_eq!(h.handle(&e), Outcome::allow());
        assert_eq!(Outcome::allow().response, json!({}));
    }
}
