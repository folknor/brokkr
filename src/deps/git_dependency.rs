//! `git_dependency` phase.
//!
//! Scans resolved packages for sources starting with `git+`. Cargo encodes
//! git sources as `git+URL[?branch=NAME|tag=NAME|rev=SHA]#RESOLVED_SHA`.
//! The query string carries what the manifest asked for; the fragment is
//! always the actual resolved commit.

use super::{CargoMetadata, GitDependencyEvent};

pub fn run(metadata: &CargoMetadata) -> Vec<GitDependencyEvent> {
    let mut events = Vec::new();
    for pkg in &metadata.packages {
        let Some(source) = &pkg.source else { continue };
        let Some(parsed) = parse_git_source(source) else {
            continue;
        };
        events.push(GitDependencyEvent {
            krate: pkg.name.clone(),
            version: pkg.version.clone(),
            url: parsed.url,
            rev: parsed.rev,
            branch: parsed.branch,
            tag: parsed.tag,
        });
    }
    events.sort_by(|a, b| a.krate.cmp(&b.krate).then(a.version.cmp(&b.version)));
    events
}

struct ParsedGitSource {
    url: String,
    rev: Option<String>,
    branch: Option<String>,
    tag: Option<String>,
}

fn parse_git_source(source: &str) -> Option<ParsedGitSource> {
    let body = source.strip_prefix("git+")?;
    let (url_and_query, fragment) = match body.split_once('#') {
        Some((a, b)) => (a, Some(b)),
        None => (body, None),
    };
    let (url, query) = match url_and_query.split_once('?') {
        Some((u, q)) => (u, Some(q)),
        None => (url_and_query, None),
    };

    let mut branch = None;
    let mut tag = None;
    let mut query_rev = None;
    if let Some(q) = query {
        for pair in q.split('&') {
            if let Some((k, v)) = pair.split_once('=') {
                match k {
                    "branch" => branch = Some(v.to_string()),
                    "tag" => tag = Some(v.to_string()),
                    "rev" => query_rev = Some(v.to_string()),
                    _ => {}
                }
            }
        }
    }

    Some(ParsedGitSource {
        url: url.to_string(),
        rev: fragment.map(str::to_string).or(query_rev),
        branch,
        tag,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_rev_pin() {
        let p = parse_git_source(
            "git+https://github.com/iced-rs/winit.git?rev=05b8ff17#05b8ff17",
        )
        .expect("parses");
        assert_eq!(p.url, "https://github.com/iced-rs/winit.git");
        assert_eq!(p.rev.as_deref(), Some("05b8ff17"));
        assert!(p.branch.is_none());
        assert!(p.tag.is_none());
    }

    #[test]
    fn parses_branch() {
        let p = parse_git_source(
            "git+https://github.com/foo/bar?branch=main#deadbeef",
        )
        .expect("parses");
        assert_eq!(p.url, "https://github.com/foo/bar");
        assert_eq!(p.branch.as_deref(), Some("main"));
        assert_eq!(p.rev.as_deref(), Some("deadbeef"));
    }

    #[test]
    fn parses_tag() {
        let p = parse_git_source("git+https://github.com/foo/bar?tag=v1.0#abc")
            .expect("parses");
        assert_eq!(p.tag.as_deref(), Some("v1.0"));
        assert_eq!(p.rev.as_deref(), Some("abc"));
    }

    #[test]
    fn parses_default_branch() {
        let p = parse_git_source("git+https://github.com/foo/bar#abc").expect("parses");
        assert_eq!(p.url, "https://github.com/foo/bar");
        assert!(p.branch.is_none());
        assert!(p.tag.is_none());
        assert_eq!(p.rev.as_deref(), Some("abc"));
    }

    #[test]
    fn rejects_registry_source() {
        assert!(parse_git_source("registry+https://example.com").is_none());
    }
}
