//! Port of `packages/coding-agent/src/utils/git.ts` (`parseGitUrl` and
//! helpers) @ pi 0.82.1 (2efa728), including the subset of
//! `hosted-git-info@9.0.3` (`lib/from-url.js` / `lib/parse-url.js` /
//! `lib/hosts.js`) that `parseGitUrl` relies on: the five built-in hosts
//! (github / gist / bitbucket / gitlab / sourcehut), the github-shorthand
//! correction, and the scp-style URL fixups.
//!
//! Intentional differences:
//! - URL parsing uses the `url` crate (WHATWG URL, same standard as the
//!   JS `URL` constructor upstream uses).
//! - `decodeURIComponent` is reimplemented as strict percent-decoding;
//!   malformed sequences make the candidate fail, like the upstream
//!   `URIError` catch.

/// `GitSource` (git.ts:7-19).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitSource {
    /// Clone URL (always valid for `git clone`, without ref suffix).
    pub repo: String,
    /// Git host domain (e.g. `github.com`).
    pub host: String,
    /// Repository path (e.g. `user/repo`).
    pub path: String,
    /// Git ref (branch, tag, commit) when specified.
    pub ref_: Option<String>,
    /// True when a ref was specified (package is not auto-updated).
    pub pinned: bool,
}

/// `decodeURIComponent` (strict): `%XX` bytes decoded as UTF-8; `+` stays
/// literal; malformed input returns `None` (the upstream `URIError` case).
fn decode_uri_component(value: &str) -> Option<String> {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            if i + 2 >= bytes.len() {
                return None;
            }
            let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).ok()?;
            let byte = u8::from_str_radix(hex, 16).ok()?;
            out.push(byte);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

struct SplitRef {
    repo: String,
    ref_: Option<String>,
}

/// `splitRef` (git.ts:21-74): split a trailing `@ref` off the repo path for
/// scp-like, protocol and shorthand forms.
fn split_ref(url: &str) -> SplitRef {
    // Scp-like `git@host:path[@ref]`.
    if let Some(rest) = url.strip_prefix("git@") {
        if let Some(colon) = rest.find(':') {
            let path_with_maybe_ref = &rest[colon + 1..];
            if let Some(at) = path_with_maybe_ref.find('@') {
                let repo_path = &path_with_maybe_ref[..at];
                let ref_ = &path_with_maybe_ref[at + 1..];
                if !repo_path.is_empty() && !ref_.is_empty() {
                    return SplitRef {
                        repo: format!("git@{}:{}", &rest[..colon], repo_path),
                        ref_: Some(ref_.to_string()),
                    };
                }
            }
            return SplitRef {
                repo: url.to_string(),
                ref_: None,
            };
        }
    }

    if url.contains("://") {
        if let Ok(parsed) = url::Url::parse(url) {
            let path_with_maybe_ref = parsed.path().trim_start_matches('/');
            if let Some(at) = path_with_maybe_ref.find('@') {
                let repo_path = &path_with_maybe_ref[..at];
                let ref_ = &path_with_maybe_ref[at + 1..];
                if !repo_path.is_empty() && !ref_.is_empty() {
                    let mut rebuilt = parsed.clone();
                    rebuilt.set_path(&format!("/{repo_path}"));
                    rebuilt.set_fragment(None);
                    let repo = rebuilt
                        .as_str()
                        .strip_suffix('/')
                        .unwrap_or_else(|| rebuilt.as_str())
                        .to_string();
                    return SplitRef {
                        repo,
                        ref_: Some(ref_.to_string()),
                    };
                }
            }
        }
        return SplitRef {
            repo: url.to_string(),
            ref_: None,
        };
    }

    // Shorthand `host/path[@ref]`.
    let Some(slash) = url.find('/') else {
        return SplitRef {
            repo: url.to_string(),
            ref_: None,
        };
    };
    let host = &url[..slash];
    let path_with_maybe_ref = &url[slash + 1..];
    if let Some(at) = path_with_maybe_ref.find('@') {
        let repo_path = &path_with_maybe_ref[..at];
        let ref_ = &path_with_maybe_ref[at + 1..];
        if !repo_path.is_empty() && !ref_.is_empty() {
            return SplitRef {
                repo: format!("{host}/{repo_path}"),
                ref_: Some(ref_.to_string()),
            };
        }
    }
    SplitRef {
        repo: url.to_string(),
        ref_: None,
    }
}

/// `hasUnsafeGitInstallPart` (git.ts:84-102).
fn has_unsafe_git_install_part(value: &str, allow_slash: bool) -> bool {
    let Some(decoded) = decode_uri_component(value) else {
        return true;
    };
    for candidate in [value, decoded.as_str()] {
        if candidate.contains('\0') || candidate.contains('\\') || candidate.starts_with('/') {
            return true;
        }
        if !allow_slash && candidate.contains('/') {
            return true;
        }
        if candidate.split('/').any(|segment| segment == "..") {
            return true;
        }
    }
    false
}

/// `buildGitSource` (git.ts:104-124).
fn build_git_source(repo: &str, host: &str, path: &str, ref_: Option<String>) -> Option<GitSource> {
    if path.starts_with('/') {
        return None;
    }
    let normalized_path = path
        .strip_suffix(".git")
        .unwrap_or(path)
        .trim_start_matches('/');
    if host.is_empty() || normalized_path.is_empty() || normalized_path.split('/').count() < 2 {
        return None;
    }
    if has_unsafe_git_install_part(host, false) || has_unsafe_git_install_part(path, true) {
        return None;
    }
    Some(GitSource {
        repo: repo.to_string(),
        host: host.to_string(),
        path: normalized_path.to_string(),
        pinned: ref_.is_some(),
        ref_,
    })
}

// ---------------------------------------------------------------------------
// hosted-git-info@9.0.3 subset (lib/parse-url.js, lib/from-url.js, hosts.js)
// ---------------------------------------------------------------------------

/// The `GitHost.#protocols` keys (index.js:44-52) plus host shortcuts.
const KNOWN_PROTOCOLS: [&str; 12] = [
    "git+ssh:",
    "ssh:",
    "git+https:",
    "git:",
    "http:",
    "https:",
    "git+http:",
    "github:",
    "gist:",
    "bitbucket:",
    "gitlab:",
    "sourcehut:",
];

/// One parsed hosted URL (the `GitHost` fields `parseGitUrl` consumes).
struct HostedInfo {
    domain: &'static str,
    user: Option<String>,
    project: String,
    committish: Option<String>,
}

/// JS `String.prototype.indexOf` returning `-1` for "not found".
fn index_of(haystack: &str, needle: char) -> isize {
    haystack.find(needle).map(|i| i as isize).unwrap_or(-1)
}

/// `lastIndexOfBefore` (parse-url.js:3-6).
fn last_index_of_before(value: &str, ch: char, before: char) -> isize {
    let start = index_of(value, before);
    let haystack = match start {
        -1 => value,
        start => &value[..start as usize],
    };
    haystack.rfind(ch).map(|i| i as isize).unwrap_or(-1)
}

/// `isGitHubShorthand` (from-url.js:6-34): bare `user/repo` detection.
fn is_github_shorthand(arg: &str) -> bool {
    let first_hash = index_of(arg, '#');
    let first_slash = index_of(arg, '/');
    let second_slash = if first_slash < 0 {
        -1
    } else {
        arg[first_slash as usize + 1..]
            .find('/')
            .map(|i| i as isize + first_slash + 1)
            .unwrap_or(-1)
    };
    let first_colon = index_of(arg, ':');
    let first_space = arg
        .find(|c: char| c.is_whitespace())
        .map(|i| i as isize)
        .unwrap_or(-1);
    let first_at = index_of(arg, '@');

    let space_only_after_hash = first_space == -1 || (first_hash > -1 && first_space > first_hash);
    let at_only_after_hash = first_at == -1 || (first_hash > -1 && first_at > first_hash);
    let colon_only_after_hash = first_colon == -1 || (first_hash > -1 && first_colon > first_hash);
    let second_slash_only_after_hash =
        second_slash == -1 || (first_hash > -1 && second_slash > first_hash);
    let has_slash = first_slash > 0;
    let does_not_end_with_slash = if first_hash > -1 {
        arg.as_bytes()[first_hash as usize - 1] != b'/'
    } else {
        !arg.ends_with('/')
    };
    let does_not_start_with_dot = !arg.starts_with('.');

    space_only_after_hash
        && has_slash
        && does_not_end_with_slash
        && does_not_start_with_dot
        && at_only_after_hash
        && colon_only_after_hash
        && second_slash_only_after_hash
}

/// `correctProtocol` (parse-url.js:16-42).
fn correct_protocol(arg: &str) -> String {
    let first_colon = arg.find(':').map(|i| i as isize).unwrap_or(-1);
    let proto_end = (first_colon + 1).max(0) as usize;
    let proto = &arg[..proto_end.min(arg.len())];
    if first_colon >= 0 && KNOWN_PROTOCOLS.contains(&proto) {
        return arg.to_string();
    }

    if first_colon >= 0 && arg[first_colon as usize..].starts_with("://") {
        // `<foo>://<bar>` is already a valid URL.
        return arg.to_string();
    }

    let first_at = index_of(arg, '@');
    if first_at > -1 {
        if first_at > first_colon {
            // `<foo>:<bar>@<baz>` — assume a git+ssh URL.
            return format!("git+ssh://{arg}");
        }
        // `git@github.com:user/repo` — handled by `correct_url`.
        return arg.to_string();
    }

    // Correct `<foo>:<bar>` to `<foo>://<bar>`.
    format!("{}//{}", &arg[..proto_end], &arg[proto_end..])
}

/// `correctUrl` (parse-url.js:45-76): fix scp-style URLs so they parse.
fn correct_url(giturl: &str) -> String {
    let mut giturl = giturl.to_string();
    let first_at = last_index_of_before(&giturl, '@', '#');
    let last_colon_before_hash = last_index_of_before(&giturl, ':', '#');

    if last_colon_before_hash > first_at {
        // `host:user/repo` style — replace the last `:` with `/`.
        let at = last_colon_before_hash as usize;
        giturl = format!("{}/{}", &giturl[..at], &giturl[at + 1..]);
    }

    if last_index_of_before(&giturl, ':', '#') == -1 && !giturl.contains("//") {
        // No `:` at all — `user@host/repo`; prepend a protocol.
        giturl = format!("git+ssh://{giturl}");
    }
    giturl
}

/// `parseUrl` (parse-url.js:78-81).
fn parse_url(giturl: &str) -> Option<url::Url> {
    let with_protocol = correct_protocol(giturl);
    url::Url::parse(&with_protocol)
        .ok()
        .or_else(|| url::Url::parse(&correct_url(&with_protocol)).ok())
}

/// JS `"a/b".split('/', limit)` for the leading segments the host
/// `extract` functions consume.
fn split_segments(pathname: &str, limit: usize) -> Vec<&str> {
    pathname.split('/').take(limit).collect()
}

fn strip_git_suffix(project: &str) -> &str {
    project.strip_suffix(".git").unwrap_or(project)
}

/// The per-host `extract` functions (hosts.js), returning
/// `(user, project, committish)` raw (still percent-encoded) parts.
fn extract_host_segments(
    host: &str,
    url: &url::Url,
) -> Option<(Option<String>, String, Option<String>)> {
    let hash = url.fragment().unwrap_or("");
    match host {
        // hosts.js:55-74 (github).
        "github" => {
            let segments = split_segments(url.path(), 5);
            let user = segments.get(1).copied().unwrap_or("");
            let project = segments.get(2).copied().unwrap_or("");
            let type_ = segments.get(3).copied().unwrap_or("");
            let mut committish = segments.get(4).copied().unwrap_or("");
            if !type_.is_empty() && type_ != "tree" {
                return None;
            }
            if type_.is_empty() {
                committish = hash;
            }
            let project = strip_git_suffix(project);
            if user.is_empty() || project.is_empty() {
                return None;
            }
            Some((
                Some(user.to_string()),
                project.to_string(),
                Some(committish.to_string()),
            ))
        }
        // hosts.js:88-106 (bitbucket) and :205-223 (sourcehut).
        "bitbucket" | "sourcehut" => {
            let rejected_aux = if host == "bitbucket" {
                "get"
            } else {
                "archive"
            };
            let segments = split_segments(url.path(), 4);
            let user = segments.get(1).copied().unwrap_or("");
            let project = segments.get(2).copied().unwrap_or("");
            let aux = segments.get(3).copied().unwrap_or("");
            if aux == rejected_aux {
                return None;
            }
            let project = strip_git_suffix(project);
            if user.is_empty() || project.is_empty() {
                return None;
            }
            Some((
                Some(user.to_string()),
                project.to_string(),
                Some(hash.to_string()),
            ))
        }
        // hosts.js:114-132 (gitlab): subgroups join into `user`.
        "gitlab" => {
            let path = &url.path()[1.min(url.path().len())..];
            if path.contains("/-/") || path.contains("/archive.tar.gz") {
                return None;
            }
            let mut segments: Vec<&str> = path.split('/').collect();
            let project = strip_git_suffix(segments.pop().unwrap_or(""));
            let user = segments.join("/");
            if user.is_empty() || project.is_empty() {
                return None;
            }
            Some((Some(user), project.to_string(), Some(hash.to_string())))
        }
        // hosts.js:167-189 (gist): `user` may be absent.
        "gist" => {
            let segments = split_segments(url.path(), 4);
            let mut user = segments.get(1).copied().unwrap_or("").to_string();
            let mut project = segments.get(2).copied().unwrap_or("").to_string();
            let aux = segments.get(3).copied().unwrap_or("");
            if aux == "raw" {
                return None;
            }
            if project.is_empty() {
                if user.is_empty() {
                    return None;
                }
                project = user.clone();
                user = String::new();
            }
            let project = strip_git_suffix(&project).to_string();
            let user = if user.is_empty() { None } else { Some(user) };
            Some((user, project, Some(hash.to_string())))
        }
        _ => None,
    }
}

/// `(hostname, protocol)` host table entry: `byDomain` / `byShortcut` and
/// the per-host allowed `protocols` list (hosts.js).
fn lookup_host(shortcut: &str, hostname: &str) -> Option<(&'static str, bool)> {
    // Returns (host name, matched-via-shortcut).
    let by_shortcut = [
        ("github:", "github"),
        ("gist:", "gist"),
        ("bitbucket:", "bitbucket"),
        ("gitlab:", "gitlab"),
        ("sourcehut:", "sourcehut"),
    ];
    if let Some((_, name)) = by_shortcut.iter().find(|(key, _)| *key == shortcut) {
        return Some((name, true));
    }
    let hostname = hostname
        .strip_prefix("www.")
        .unwrap_or(hostname)
        .to_lowercase();
    let by_domain = [
        ("github.com", "github"),
        ("gist.github.com", "gist"),
        ("bitbucket.org", "bitbucket"),
        ("gitlab.com", "gitlab"),
        ("git.sr.ht", "sourcehut"),
    ];
    by_domain
        .iter()
        .find(|(domain, _)| *domain == hostname)
        .map(|(_, name)| (*name, false))
}

fn host_protocols(host: &str) -> &'static [&'static str] {
    match host {
        "github" => &["git:", "http:", "git+ssh:", "git+https:", "ssh:", "https:"],
        "gist" => &["git:", "git+ssh:", "git+https:", "ssh:", "https:"],
        "bitbucket" | "gitlab" => &["git+ssh:", "git+https:", "ssh:", "https:"],
        // sourcehut
        _ => &["git+ssh:", "https:"],
    }
}

/// `fromUrl` (from-url.js:36-122).
fn hosted_from_url(giturl: &str) -> Option<HostedInfo> {
    if giturl.is_empty() {
        return None;
    }
    let corrected = if is_github_shorthand(giturl) {
        format!("github:{giturl}")
    } else {
        giturl.to_string()
    };
    let parsed = parse_url(&corrected)?;
    let protocol = format!("{}:", parsed.scheme());
    let hostname = parsed.host_str().unwrap_or("");
    let (host, via_shortcut) = lookup_host(&protocol, hostname)?;

    if via_shortcut {
        // Shortcut branch (from-url.js:68-96).
        let mut pathname = parsed.path().trim_start_matches('/').to_string();
        if let Some(at) = pathname.find('@') {
            // Auth is ignored for shortcuts, so just trim it out.
            pathname = pathname[at + 1..].to_string();
        }
        let (user, project) = match pathname.rfind('/') {
            Some(last_slash) => {
                let user = decode_uri_component(&pathname[..last_slash])?;
                let project = decode_uri_component(&pathname[last_slash + 1..])?;
                (if user.is_empty() { None } else { Some(user) }, project)
            }
            None => (None, decode_uri_component(&pathname)?),
        };
        let project = strip_git_suffix(&project).to_string();
        let committish = match parsed.fragment().filter(|hash| !hash.is_empty()) {
            Some(hash) => Some(decode_uri_component(hash)?),
            None => None,
        };
        return Some(HostedInfo {
            domain: match host {
                "github" => "github.com",
                "gist" => "gist.github.com",
                "bitbucket" => "bitbucket.org",
                "gitlab" => "gitlab.com",
                _ => "git.sr.ht",
            },
            user,
            project,
            committish,
        });
    }

    if !host_protocols(host).contains(&protocol.as_str()) {
        return None;
    }
    let (user, project, committish) = extract_host_segments(host, &parsed)?;
    Some(HostedInfo {
        domain: match host {
            "github" => "github.com",
            "gist" => "gist.github.com",
            "bitbucket" => "bitbucket.org",
            "gitlab" => "gitlab.com",
            _ => "git.sr.ht",
        },
        user: user.and_then(|u| decode_uri_component(&u)),
        project: decode_uri_component(&project)?,
        committish: committish.and_then(|c| decode_uri_component(&c)),
    })
}

/// `parseGenericGitUrl` (git.ts:126-163).
fn parse_generic_git_url(url: &str) -> Option<GitSource> {
    let split = split_ref(url);
    let mut repo = split.repo.clone();
    let host;
    let path;

    if let Some(rest) = split.repo.strip_prefix("git@") {
        let colon = rest.find(':')?;
        host = rest[..colon].to_string();
        path = rest[colon + 1..].to_string();
    } else if split.repo.starts_with("https://")
        || split.repo.starts_with("http://")
        || split.repo.starts_with("ssh://")
        || split.repo.starts_with("git://")
    {
        let parsed = url::Url::parse(&split.repo).ok()?;
        host = parsed.host_str().unwrap_or("").to_string();
        path = parsed.path().trim_start_matches('/').to_string();
    } else {
        let slash = split.repo.find('/')?;
        host = split.repo[..slash].to_string();
        path = split.repo[slash + 1..].to_string();
        if !host.contains('.') && host != "localhost" {
            return None;
        }
        repo = format!("https://{}", split.repo);
    }

    build_git_source(&repo, &host, &path, split.ref_)
}

/// `parseGitUrl` (git.ts:172-226).
///
/// Rules: with the `git:` prefix all historical shorthand forms are
/// accepted; without it only explicit protocol URLs (`https?`/`ssh`/`git`).
pub fn parse_git_url(source: &str) -> Option<GitSource> {
    let trimmed = source.trim();
    let has_git_prefix = trimmed.starts_with("git:");
    let url = if has_git_prefix {
        trimmed[4..].trim()
    } else {
        trimmed
    };

    if !has_git_prefix {
        let lower = url.to_lowercase();
        let has_protocol = lower.starts_with("http://")
            || lower.starts_with("https://")
            || lower.starts_with("ssh://")
            || lower.starts_with("git://");
        if !has_protocol {
            return None;
        }
    }

    let split = split_ref(url);

    // Hosted candidates: `repo#ref` first, then the raw URL (git.ts:183-205).
    let mut hosted_candidates: Vec<String> = Vec::new();
    if let Some(ref_) = &split.ref_ {
        hosted_candidates.push(format!("{}#{}", split.repo, ref_));
    }
    hosted_candidates.push(url.to_string());
    for candidate in &hosted_candidates {
        if let Some(info) = hosted_from_url(candidate) {
            if split.ref_.is_some() && info.project.contains('@') {
                continue;
            }
            let use_https_prefix = !split.repo.starts_with("http://")
                && !split.repo.starts_with("https://")
                && !split.repo.starts_with("ssh://")
                && !split.repo.starts_with("git://")
                && !split.repo.starts_with("git@");
            let repo = if use_https_prefix {
                format!("https://{}", split.repo)
            } else {
                split.repo.clone()
            };
            // JS template literal: a missing user stringifies as `null`.
            let path = format!(
                "{}/{}",
                info.user.as_deref().unwrap_or("null"),
                info.project
            );
            let ref_ = info
                .committish
                .filter(|c| !c.is_empty())
                .or_else(|| split.ref_.clone());
            if let Some(source) = build_git_source(&repo, info.domain, &path, ref_) {
                return Some(source);
            }
        }
    }

    // HTTPS candidates (git.ts:207-223).
    let mut https_candidates: Vec<String> = Vec::new();
    if let Some(ref_) = &split.ref_ {
        https_candidates.push(format!("https://{}#{}", split.repo, ref_));
    }
    https_candidates.push(format!("https://{url}"));
    for candidate in &https_candidates {
        if let Some(info) = hosted_from_url(candidate) {
            if split.ref_.is_some() && info.project.contains('@') {
                continue;
            }
            let repo = format!("https://{}", split.repo);
            let path = format!(
                "{}/{}",
                info.user.as_deref().unwrap_or("null"),
                info.project
            );
            let ref_ = info
                .committish
                .filter(|c| !c.is_empty())
                .or_else(|| split.ref_.clone());
            if let Some(source) = build_git_source(&repo, info.domain, &path, ref_) {
                return Some(source);
            }
        }
    }

    parse_generic_git_url(url)
}

#[cfg(test)]
mod tests {
    //! Port of `packages/coding-agent/test/package-manager-ssh.test.ts`
    //! (git source parsing) plus hosted-git-info subset edge cases.

    use super::*;

    #[test]
    fn test_protocol_urls_without_git_prefix() {
        let https = parse_git_url("https://github.com/user/repo").unwrap();
        assert_eq!(https.host, "github.com");
        assert_eq!(https.path, "user/repo");
        assert_eq!(https.repo, "https://github.com/user/repo");

        let ssh = parse_git_url("ssh://git@github.com/user/repo").unwrap();
        assert_eq!(ssh.host, "github.com");
        assert_eq!(ssh.path, "user/repo");
        assert_eq!(ssh.repo, "ssh://git@github.com/user/repo");
    }

    #[test]
    fn test_shorthand_with_git_prefix() {
        let scp = parse_git_url("git:git@github.com:user/repo").unwrap();
        assert_eq!(scp.repo, "git@github.com:user/repo");
        assert_eq!(scp.host, "github.com");
        assert_eq!(scp.path, "user/repo");
        assert!(!scp.pinned);

        let shorthand = parse_git_url("git:github.com/user/repo").unwrap();
        assert_eq!(shorthand.repo, "https://github.com/user/repo");
        assert_eq!(shorthand.host, "github.com");

        let with_ref = parse_git_url("git:github.com/user/repo@v1").unwrap();
        assert_eq!(with_ref.ref_.as_deref(), Some("v1"));
        assert!(with_ref.pinned);
        assert_eq!(with_ref.repo, "https://github.com/user/repo");
    }

    #[test]
    fn test_unsupported_forms_without_git_prefix() {
        assert!(parse_git_url("git@github.com:user/repo").is_none());
        assert!(parse_git_url("github.com/user/repo").is_none());
    }

    #[test]
    fn test_bare_user_repo_shorthand_via_git_prefix() {
        // `git:user/repo` hits hosted-git-info's github shorthand; the repo
        // URL keeps the raw `https://user/repo` form (upstream quirk).
        let parsed = parse_git_url("git:user/repo").unwrap();
        assert_eq!(parsed.host, "github.com");
        assert_eq!(parsed.path, "user/repo");
        assert_eq!(parsed.repo, "https://user/repo");
    }

    #[test]
    fn test_gitlab_subgroup_paths() {
        let parsed = parse_git_url("https://gitlab.com/group/sub/repo").unwrap();
        assert_eq!(parsed.host, "gitlab.com");
        assert_eq!(parsed.path, "group/sub/repo");
    }

    #[test]
    fn test_unknown_hosts_fall_back_to_generic() {
        let parsed = parse_git_url("git:git@myhost:user/repo").unwrap();
        assert_eq!(parsed.host, "myhost");
        assert_eq!(parsed.path, "user/repo");
        assert_eq!(parsed.repo, "git@myhost:user/repo");

        let parsed = parse_git_url("git:myhost.local/user/repo").unwrap();
        assert_eq!(parsed.host, "myhost.local");
        assert_eq!(parsed.repo, "https://myhost.local/user/repo");
    }

    #[test]
    fn test_dot_git_suffix_stripped() {
        let parsed = parse_git_url("https://github.com/user/repo.git").unwrap();
        assert_eq!(parsed.path, "user/repo");
        assert_eq!(parsed.repo, "https://github.com/user/repo.git");
    }

    #[test]
    fn test_unsafe_parts_rejected() {
        // Dot segments normalize away during URL parsing, leaving a path
        // with fewer than two segments.
        assert!(parse_git_url("https://github.com/user/../repo").is_none());
        // Percent-decoded `..` path segments are rejected outright.
        assert!(parse_git_url("https://github.com/user/%2E%2E/repo").is_none());
    }
}
