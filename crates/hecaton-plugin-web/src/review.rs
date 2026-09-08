//! The review submission (Spec C §4.4): the body the page posts, its
//! limits, and the one message the agent receives through `send_text`.

use hecaton_api::check_path;
use serde::Deserialize;

pub const MAX_COMMENTS: usize = 200;
/// `summary` plus every comment `body`, in bytes.
pub const MAX_BODY_BYTES: usize = 64 << 10;
/// One quoted diff line, in bytes.
pub const MAX_TEXT_BYTES: usize = 4096;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Side {
    Old,
    New,
}

impl std::fmt::Display for Side {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Side::Old => "old",
            Side::New => "new",
        })
    }
}

/// One line comment: where it was made and the diff line it was made
/// on (`text`, sign included), so the agent can find the place even
/// after the tree moved (PC-6).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Comment {
    pub path: String,
    pub side: Side,
    pub line: u64,
    #[serde(default)]
    pub text: String,
    pub body: String,
}

/// What `POST /agents/{id}/review` takes. Every field defaults so a page
/// that loaded no diff can still send a summary.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ReviewBody {
    pub head: String,
    pub base_ref: String,
    pub summary: String,
    pub comments: Vec<Comment>,
}

/// The 400 text, field path first, or `Ok` for a sendable review.
pub fn validate(body: &ReviewBody) -> Result<(), String> {
    if body.comments.is_empty() && body.summary.trim().is_empty() {
        return Err("nothing to send".into());
    }
    if body.comments.len() > MAX_COMMENTS {
        return Err(format!("comments: more than {MAX_COMMENTS}"));
    }
    let mut bytes = body.summary.len();
    for (i, c) in body.comments.iter().enumerate() {
        check_path(&c.path)
            .map_err(|r| format!("comments[{i}].path: workspace: invalid path: {r}"))?;
        if c.path.is_empty() {
            return Err(format!("comments[{i}].path: empty"));
        }
        if c.body.trim().is_empty() {
            return Err(format!("comments[{i}].body: empty"));
        }
        if c.text.len() > MAX_TEXT_BYTES {
            return Err(format!(
                "comments[{i}].text: longer than {MAX_TEXT_BYTES} bytes"
            ));
        }
        bytes += c.body.len();
    }
    if bytes > MAX_BODY_BYTES {
        return Err(format!(
            "summary and comment bodies: longer than {MAX_BODY_BYTES} bytes together"
        ));
    }
    Ok(())
}

/// The one message the agent receives (Spec C §4.4): a header, the
/// comments in file then line order each quoting its line, and the
/// summary as `Overall:` when there is one. No trailing newline: the
/// runner's `submit` adds the Enter.
pub fn render_message(agent: &str, body: &ReviewBody) -> String {
    let mut comments: Vec<&Comment> = body.comments.iter().collect();
    comments.sort_by(|a, b| {
        (a.path.as_str(), a.line, a.side as u8).cmp(&(b.path.as_str(), b.line, b.side as u8))
    });
    let n = comments.len();
    let short: String = body.head.chars().take(7).collect();
    let mut out = format!("Review of {agent}");
    if !body.base_ref.is_empty() {
        out.push_str(&format!(" against {}", body.base_ref));
    }
    if !short.is_empty() {
        out.push_str(&format!(" at {short}"));
    }
    out.push_str(&format!(
        " ({n} comment{})\n",
        if n == 1 { "" } else { "s" }
    ));
    for c in comments {
        out.push_str(&format!(
            "\n{} line {} ({}):\n> {}\n{}\n",
            c.path,
            c.line,
            c.side,
            c.text,
            c.body.trim_end()
        ));
    }
    let summary = body.summary.trim();
    if !summary.is_empty() {
        out.push_str(&format!("\nOverall:\n{summary}\n"));
    }
    out.trim_end().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn body() -> ReviewBody {
        serde_json::from_value(json!({
            "head": "3f9c2a1d5b7e8c0a4f6d2e1b9a8c7d6e5f4a3b2c",
            "base_ref": "origin/main",
            "summary": "Looks close. Please address the comments above and run the tests.\n",
            "comments": [
                { "path": "src/lib.rs", "side": "old", "line": 80, "text": "-    // TODO",
                  "body": "Good riddance, but the docs still mention this." },
                { "path": "src/lib.rs", "side": "new", "line": 42, "text": "+    let x = foo();",
                  "body": "This unwrap can panic on an empty list; return the error instead.\n" }
            ]
        }))
        .unwrap()
    }

    #[test]
    fn the_message_is_the_spec_example() {
        assert_eq!(validate(&body()), Ok(()));
        assert_eq!(
            render_message("e2e/c/alice", &body()),
            "Review of e2e/c/alice against origin/main at 3f9c2a1 (2 comments)\n\
             \n\
             src/lib.rs line 42 (new):\n\
             > +    let x = foo();\n\
             This unwrap can panic on an empty list; return the error instead.\n\
             \n\
             src/lib.rs line 80 (old):\n\
             > -    // TODO\n\
             Good riddance, but the docs still mention this.\n\
             \n\
             Overall:\n\
             Looks close. Please address the comments above and run the tests."
        );
        let mut one = body();
        one.comments.truncate(1);
        one.summary.clear();
        one.base_ref.clear();
        let m = render_message("f/c/a", &one);
        assert!(
            m.starts_with("Review of f/c/a at 3f9c2a1 (1 comment)\n\n"),
            "{m}"
        );
        assert!(!m.contains("Overall:"));
        assert!(!m.ends_with('\n'));
    }

    #[test]
    fn the_limits_are_enforced_with_a_field_path() {
        let mut b = body();
        b.comments.clear();
        b.summary.clear();
        assert_eq!(validate(&b), Err("nothing to send".to_string()));
        b.summary = "just a summary".into();
        assert_eq!(validate(&b), Ok(()), "a summary alone is a review");
        let mut b = body();
        b.comments[1].path = "../etc/passwd".into();
        assert_eq!(
            validate(&b),
            Err("comments[1].path: workspace: invalid path: \"..\" segment".to_string())
        );
        let mut b = body();
        b.comments[0].body = String::new();
        assert_eq!(validate(&b), Err("comments[0].body: empty".to_string()));
        let mut b = body();
        b.comments[0].text = "x".repeat(MAX_TEXT_BYTES + 1);
        assert_eq!(
            validate(&b),
            Err(format!(
                "comments[0].text: longer than {MAX_TEXT_BYTES} bytes"
            ))
        );
        let mut b = body();
        b.comments = std::iter::repeat_n(b.comments[0].clone(), MAX_COMMENTS + 1).collect();
        assert_eq!(
            validate(&b),
            Err(format!("comments: more than {MAX_COMMENTS}"))
        );
        let mut b = body();
        b.summary = "y".repeat(MAX_BODY_BYTES);
        assert_eq!(
            validate(&b),
            Err(format!(
                "summary and comment bodies: longer than {MAX_BODY_BYTES} bytes together"
            ))
        );
        assert!(
            serde_json::from_value::<ReviewBody>(json!({ "comments": [], "nope": 1 })).is_err(),
            "unknown fields are refused"
        );
        let minimal: ReviewBody = serde_json::from_value(json!({})).unwrap();
        assert!(minimal.comments.is_empty() && minimal.head.is_empty());
        assert_eq!(Side::Old.to_string(), "old");
    }
}
