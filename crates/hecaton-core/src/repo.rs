//! Repository references. GitHub gets first-class treatment (spec: every
//! agent has a git repo, with first-class GitHub support); anything else
//! is passed to git verbatim.

use std::fmt;

/// Why a string is not a repository reference.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid repo {value:?}: {reason}")]
pub struct RepoError {
    pub value: String,
    pub reason: &'static str,
}

/// Where a crew's code lives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RepoRef {
    GitHub { owner: String, name: String },
    Url(String),
}

impl RepoRef {
    /// Accepts `owner/name`, `https://github.com/owner/name[.git]`,
    /// `git@github.com:owner/name[.git]`, or any other `scheme://` / `user@host:` URL.
    pub fn parse(s: &str) -> Result<Self, RepoError> {
        let err = |reason| RepoError {
            value: s.to_string(),
            reason,
        };
        if s.is_empty() {
            return Err(err("empty"));
        }
        if let Some(rest) = s.strip_prefix("https://github.com/") {
            return parse_slug(rest.trim_end_matches(".git"))
                .ok_or_else(|| err("expected owner/name or a clone URL"));
        }
        if let Some(rest) = s.strip_prefix("git@github.com:") {
            return parse_slug(rest.trim_end_matches(".git"))
                .ok_or_else(|| err("expected owner/name or a clone URL"));
        }
        if s.contains("://") || (s.contains('@') && s.contains(':')) {
            return Ok(Self::Url(s.to_string()));
        }
        parse_slug(s).ok_or_else(|| err("expected owner/name or a clone URL"))
    }

    /// URL to hand to `git clone`.
    pub fn clone_url(&self) -> String {
        match self {
            Self::GitHub { owner, name } => format!("https://github.com/{owner}/{name}.git"),
            Self::Url(u) => u.clone(),
        }
    }

    /// `owner/name` for GitHub repos; `None` otherwise.
    pub fn github_slug(&self) -> Option<String> {
        match self {
            Self::GitHub { owner, name } => Some(format!("{owner}/{name}")),
            Self::Url(_) => None,
        }
    }
}

impl fmt::Display for RepoRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.clone_url())
    }
}

fn parse_slug(s: &str) -> Option<RepoRef> {
    let (owner, name) = s.split_once('/')?;
    let ok = |part: &str| {
        !part.is_empty()
            && part
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    };
    (ok(owner) && ok(name)).then(|| RepoRef::GitHub {
        owner: owner.to_string(),
        name: name.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_github_shorthand_and_urls_to_the_same_ref() {
        let expected = RepoRef::GitHub {
            owner: "acme".into(),
            name: "payments-api".into(),
        };
        for s in [
            "acme/payments-api",
            "https://github.com/acme/payments-api",
            "https://github.com/acme/payments-api.git",
            "git@github.com:acme/payments-api.git",
        ] {
            assert_eq!(RepoRef::parse(s).unwrap(), expected, "for {s}");
        }
    }

    #[test]
    fn github_ref_has_https_clone_url_and_slug() {
        let r = RepoRef::parse("acme/payments-api").unwrap();
        assert_eq!(r.clone_url(), "https://github.com/acme/payments-api.git");
        assert_eq!(r.github_slug().as_deref(), Some("acme/payments-api"));
    }

    #[test]
    fn non_github_urls_are_kept_verbatim() {
        let r = RepoRef::parse("https://gitlab.com/acme/x.git").unwrap();
        assert_eq!(r, RepoRef::Url("https://gitlab.com/acme/x.git".into()));
        assert_eq!(r.clone_url(), "https://gitlab.com/acme/x.git");
        assert_eq!(r.github_slug(), None);
    }

    #[test]
    fn rejects_malformed_values() {
        for (bad, reason) in [
            ("", "empty"),
            ("acme", "expected owner/name or a clone URL"),
            ("acme/", "expected owner/name or a clone URL"),
            ("/name", "expected owner/name or a clone URL"),
            ("a/b/c", "expected owner/name or a clone URL"),
            ("acme/pay ments", "expected owner/name or a clone URL"),
        ] {
            let err = RepoRef::parse(bad).unwrap_err();
            assert_eq!(err.value, bad);
            assert_eq!(err.reason, reason, "for {bad:?}");
        }
    }
}
