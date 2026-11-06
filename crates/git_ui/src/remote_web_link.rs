use std::net::IpAddr;
use std::sync::Arc;

use git::{
    BuildCommitPermalinkParams, GitHostingProviderRegistry, RemoteUrl, parse_git_remote_url,
};
use gpui::SharedString;
use ui::IconName;
use url::Url;

/// A browser link to a commit, derived from one or more Git remotes that
/// resolve to the same web origin and group.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RemoteWebLink {
    pub remote_name: SharedString,
    pub group: SharedString,
    pub provider_name: SharedString,
    pub icon: IconName,
    pub url: String,
    /// For remotes that don't reveal the web scheme (SSH, `git://`, ...) on an
    /// unknown host, `url` guesses `http://` and this holds the `https://`
    /// alternative.
    pub secure_url: Option<String>,
}

const SECOND_LEVEL_DOMAINS: &[&str] = &[
    "ac", "co", "com", "edu", "gen", "go", "gob", "gov", "info", "ltd", "mil", "ne", "net", "nom",
    "or", "org", "plc", "sch",
];

/// Resolves every remote that can be turned into a web URL into a commit link,
/// keeping only the first remote (in the given order) for each web origin and group.
pub(crate) fn remote_web_links(
    remotes: &[(SharedString, String)],
    sha: &str,
    provider_registry: Arc<GitHostingProviderRegistry>,
) -> Vec<RemoteWebLink> {
    let mut seen_keys: Vec<(String, String)> = Vec::new();
    let mut links = Vec::new();
    for (remote_name, remote_url) in remotes {
        let Some((key, link)) =
            remote_web_link(remote_name, remote_url, sha, provider_registry.clone())
        else {
            continue;
        };
        if seen_keys.contains(&key) {
            continue;
        }
        seen_keys.push(key);
        links.push(link);
    }
    links
}

fn remote_web_link(
    remote_name: &SharedString,
    remote_url: &str,
    sha: &str,
    provider_registry: Arc<GitHostingProviderRegistry>,
) -> Option<((String, String), RemoteWebLink)> {
    let remote = remote_url.parse::<RemoteUrl>().ok()?;
    let scheme = match remote.scheme() {
        "http" | "https" => remote.scheme(),
        "ssh" | "git" | "git+ssh" | "ssh+git" | "ftp" | "ftps" => "http",
        _ => return None,
    };
    let host = remote.host_str()?.to_lowercase();

    if let Some((provider, parsed)) = parse_git_remote_url(provider_registry, remote_url) {
        let provider_name = provider.name();
        let group = parsed.owner.to_string();
        let url = provider
            .build_commit_permalink(&parsed, BuildCommitPermalinkParams { sha })
            .to_string();
        return Some((
            (
                provider.base_url().origin().ascii_serialization(),
                group.clone(),
            ),
            RemoteWebLink {
                remote_name: remote_name.clone(),
                group: group.into(),
                icon: ui::git_hosting_provider_icon(&provider_name),
                provider_name: provider_name.into(),
                url,
                secure_url: None,
            },
        ));
    }

    let path = remote.path().trim_matches('/');
    let path = path.strip_suffix(".git").unwrap_or(path);
    let (group, repo) = path.rsplit_once('/')?;
    if group.is_empty() || repo.is_empty() {
        return None;
    }
    let mut url = Url::parse(&format!("{scheme}://{host}/")).ok()?;
    // Transport ports do not identify the port of the web interface.
    if matches!(remote.scheme(), "http" | "https") {
        url.set_port(remote.port()).ok()?;
    }
    url.set_path(&format!("{group}/{repo}/commit/{sha}"));
    let secure_url = if matches!(remote.scheme(), "http" | "https") {
        None
    } else {
        let mut secure_url = url.clone();
        secure_url.set_scheme("https").ok()?;
        Some(secure_url.to_string())
    };
    let provider_name = site_name(host.trim_start_matches('[').trim_end_matches(']'));
    Some((
        (url.origin().ascii_serialization(), group.to_string()),
        RemoteWebLink {
            remote_name: remote_name.clone(),
            group: group.to_string().into(),
            provider_name: provider_name.into(),
            icon: IconName::GitGraph,
            url: url.to_string(),
            secure_url,
        },
    ))
}

/// Picks the registrable label of a host, so `aaa.bbb.com.cn` becomes `Bbb`.
fn site_name(host: &str) -> String {
    if host.parse::<IpAddr>().is_ok() {
        return host.to_string();
    }
    let mut labels = host
        .split('.')
        .filter(|label| !label.is_empty())
        .collect::<Vec<_>>();
    if labels.len() > 1 {
        let top_level = labels.pop().unwrap_or_default();
        if labels.len() > 1
            && top_level.len() == 2
            && labels
                .last()
                .is_some_and(|label| SECOND_LEVEL_DOMAINS.contains(label))
        {
            labels.pop();
        }
    }
    let Some(label) = labels.last() else {
        return host.to_string();
    };
    let mut chars = label.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => host.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_site_name() {
        assert_eq!(site_name("aaa.bbb.com.cn"), "Bbb");
        assert_eq!(site_name("git.company.co.uk"), "Company");
        assert_eq!(site_name("code.example.com"), "Example");
        assert_eq!(site_name("example.io"), "Example");
        assert_eq!(site_name("devbox"), "Devbox");
        assert_eq!(site_name("10.0.0.1"), "10.0.0.1");
    }

    #[test]
    fn test_remote_web_links() {
        let registry = Arc::new(GitHostingProviderRegistry::new());
        registry.register_hosting_provider(Arc::new(
            git_hosting_providers::Gitlab::from_remote_url("git@gitlab.corp.net:a/b/c.git")
                .expect("self-hosted GitLab"),
        ));
        let remotes = [
            (
                "fork".into(),
                "git@code.example.com.cn:Team/repo.git".into(),
            ),
            (
                "mirror".into(),
                "ssh://git@code.example.com.cn/Team/repo.git".into(),
            ),
            (
                "upstream".into(),
                "https://git.example.org:8443/Core/repo".into(),
            ),
            ("selfhosted".into(), "git@gitlab.corp.net:a/b/c.git".into()),
            ("local".into(), "/srv/repo.git".into()),
        ];
        let links = remote_web_links(&remotes, "abc123", registry);
        assert_eq!(
            links,
            vec![
                RemoteWebLink {
                    remote_name: "fork".into(),
                    group: "Team".into(),
                    provider_name: "Example".into(),
                    icon: IconName::GitGraph,
                    url: "http://code.example.com.cn/Team/repo/commit/abc123".into(),
                    secure_url: Some("https://code.example.com.cn/Team/repo/commit/abc123".into()),
                },
                RemoteWebLink {
                    remote_name: "upstream".into(),
                    group: "Core".into(),
                    provider_name: "Example".into(),
                    icon: IconName::GitGraph,
                    url: "https://git.example.org:8443/Core/repo/commit/abc123".into(),
                    secure_url: None,
                },
                RemoteWebLink {
                    remote_name: "selfhosted".into(),
                    group: "a/b".into(),
                    provider_name: "GitLab Self-Hosted".into(),
                    icon: IconName::Gitlab,
                    url: "https://gitlab.corp.net/a/b/c/-/commit/abc123".into(),
                    secure_url: None,
                },
            ]
        );
    }

    #[test]
    fn test_registered_provider_links() {
        use git_hosting_providers::{Azure, Gerrit, Github};

        let registry = Arc::new(GitHostingProviderRegistry::new());
        registry.register_hosting_provider(Arc::new(Azure));
        registry.register_hosting_provider(Arc::new(Github::public_instance()));
        registry.set_setting_providers([Arc::new(Gerrit::new(
            "Gerrit",
            Url::parse("https://review.example.com/").expect("Gerrit base URL"),
        ))
            as Arc<dyn git::GitHostingProvider + Send + Sync>]);

        assert!(
            remote_web_link(
                &"local".into(),
                "file://github.com/owner/repo.git",
                "abc123",
                registry.clone(),
            )
            .is_none()
        );

        for (remote_url, expected_url, expected_group, expected_icon) in [
            (
                "https://dev.azure.com/organization/project/_git/repo",
                "https://dev.azure.com/organization/project/_git/repo/commit/abc123",
                "organization/project",
                IconName::Link,
            ),
            (
                "git@ssh.dev.azure.com:v3/organization/project/repo",
                "https://dev.azure.com/organization/project/_git/repo/commit/abc123",
                "organization/project",
                IconName::Link,
            ),
            (
                "ssh://git@review.example.com:29418/repo.git",
                "https://review.example.com/repo/+/abc123",
                "",
                IconName::Gerrit,
            ),
            (
                "github.com:owner/repo.git",
                "https://github.com/owner/repo/commit/abc123",
                "owner",
                IconName::Github,
            ),
            (
                "git@GitHub.com:owner/repo.git",
                "https://github.com/owner/repo/commit/abc123",
                "owner",
                IconName::Github,
            ),
            (
                "helper::git@github.com:owner/repo.git",
                "https://github.com/owner/repo/commit/abc123",
                "owner",
                IconName::Github,
            ),
        ] {
            let (_, link) =
                remote_web_link(&"upstream".into(), remote_url, "abc123", registry.clone())
                    .expect("registered provider link");
            assert_eq!(link.url, expected_url, "{remote_url}");
            assert_eq!(link.group.as_ref(), expected_group, "{remote_url}");
            assert_eq!(link.icon, expected_icon, "{remote_url}");
            assert_eq!(link.secure_url, None, "{remote_url}");
        }
    }

    #[test]
    fn test_self_hosted_links_keep_distinct_web_origins() {
        let registry = Arc::new(GitHostingProviderRegistry::new());
        let remotes = [
            (
                "ssh".into(),
                "ssh://git@gitlab.example:2222/team/repo.git".to_string(),
            ),
            (
                "http".into(),
                "http://gitlab.example:8080/team/repo.git".to_string(),
            ),
            (
                "https".into(),
                "https://gitlab.example:8443/team/repo.git".to_string(),
            ),
            (
                "mirror".into(),
                "http://gitlab.example:8080/team/repo.git".to_string(),
            ),
        ];
        git_hosting_providers::register_additional_providers(
            registry.clone(),
            remotes.iter().map(|(_, url)| url.as_str()),
        );
        let links = remote_web_links(&remotes, "abc123", registry);
        let urls = links
            .iter()
            .map(|link| link.url.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            urls,
            [
                "https://gitlab.example/team/repo/-/commit/abc123",
                "http://gitlab.example:8080/team/repo/-/commit/abc123",
                "https://gitlab.example:8443/team/repo/-/commit/abc123",
            ]
        );
    }

    #[test]
    fn test_generic_remote_links() {
        let registry = Arc::new(GitHostingProviderRegistry::new());
        for (remote_url, expected_url, expected_secure_url) in [
            (
                "ssh://git@Example.com:2222/team/repo.git",
                "http://example.com/team/repo/commit/abc123",
                Some("https://example.com/team/repo/commit/abc123"),
            ),
            (
                "http://example.com:8080/team/sub/repo.git/",
                "http://example.com:8080/team/sub/repo/commit/abc123",
                None,
            ),
            (
                "git@[::1]:team/repo.git",
                "http://[::1]/team/repo/commit/abc123",
                Some("https://[::1]/team/repo/commit/abc123"),
            ),
        ] {
            let (_, link) = remote_web_link(&"fork".into(), remote_url, "abc123", registry.clone())
                .expect("generic link");
            assert_eq!(link.url, expected_url, "{remote_url}");
            assert_eq!(
                link.secure_url.as_deref(),
                expected_secure_url,
                "{remote_url}"
            );
        }
        for remote_url in [
            "file:///srv/team/repo.git",
            "/srv/team/repo.git",
            r"C:\srv\team\repo.git",
            "C:/srv/team/repo.git",
            "git@example.com:repo.git",
        ] {
            assert!(
                remote_web_link(&"local".into(), remote_url, "abc123", registry.clone()).is_none(),
                "{remote_url}"
            );
        }
    }
}
