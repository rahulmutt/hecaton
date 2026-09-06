//! Hook ingress (Phase 3 spec §3.5): body validation here; the axum
//! handler joins in `api.rs`'s router (Task 8).

use serde_json::Value;

/// The three things the daemon needs from a hook body.
#[derive(Debug, Clone, PartialEq)]
pub struct ParsedEvent {
    pub name: String,
    pub session_id: Option<String>,
    pub payload: Value,
}

/// Validates at the edge: JSON object with a string `hook_event_name`.
/// Unknown names are fine — Claude's list grows. The error is the 400 body.
pub fn parse_event(body: &[u8]) -> Result<ParsedEvent, String> {
    let payload: Value =
        serde_json::from_slice(body).map_err(|e| format!("body is not JSON: {e}"))?;
    let Value::Object(map) = &payload else {
        return Err("body must be a JSON object".to_string());
    };
    let name = match map.get("hook_event_name") {
        Some(Value::String(s)) => s.clone(),
        _ => return Err("hook_event_name must be a string".to_string()),
    };
    let session_id = map
        .get("session_id")
        .and_then(Value::as_str)
        .map(str::to_string);
    Ok(ParsedEvent {
        name,
        session_id,
        payload,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_name_session_and_keeps_the_payload_raw() {
        let body = json!({ "hook_event_name": "PreToolUse", "session_id": "s1", "tool_input": { "command": "ls" } });
        let e = parse_event(body.to_string().as_bytes()).unwrap();
        assert_eq!(e.name, "PreToolUse");
        assert_eq!(e.session_id.as_deref(), Some("s1"));
        assert_eq!(e.payload, body);
        let e = parse_event(br#"{"hook_event_name":"Whatever"}"#).unwrap();
        assert_eq!((e.name.as_str(), e.session_id), ("Whatever", None));
    }

    #[test]
    fn rejects_non_json_non_objects_and_missing_names() {
        assert!(
            parse_event(b"nope")
                .unwrap_err()
                .starts_with("body is not JSON")
        );
        assert_eq!(
            parse_event(b"[1]").unwrap_err(),
            "body must be a JSON object"
        );
        assert_eq!(
            parse_event(br#"{"x":1}"#).unwrap_err(),
            "hook_event_name must be a string"
        );
        assert_eq!(
            parse_event(br#"{"hook_event_name":7}"#).unwrap_err(),
            "hook_event_name must be a string"
        );
    }
}
