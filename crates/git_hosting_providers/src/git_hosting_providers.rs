mod providers;
mod settings;

use std::sync::Arc;

use anyhow::Context as _;
use anyhow::Result;
use git::{GitHostingProviderRegistry, RemoteUrl, parse_git_remote_url};
use gpui::App;
use url::Url;

pub use crate::providers::*;
pub use crate::settings::*;

/// Initializes the Git hosting providers.
pub fn init(cx: &mut App) {
    crate::settings::init(cx);

    let provider_registry = GitHostingProviderRegistry::global(cx);
    provider_registry.register_hosting_provider(Arc::new(Azure));
    provider_registry.register_hosting_provider(Arc::new(Bitbucket::public_instance()));
    provider_registry.register_hosting_provider(Arc::new(Chromium));
    provider_registry.register_hosting_provider(Arc::new(Forgejo::public_instance()));
    provider_registry.register_hosting_provider(Arc::new(Gitea::public_instance()));
    provider_registry.register_hosting_provider(Arc::new(Gitee));
    provider_registry.register_hosting_provider(Arc::new(Github::public_instance()));
    provider_registry.register_hosting_provider(Arc::new(Gitlab::public_instance()));
    provider_registry.register_hosting_provider(Arc::new(SourceHut::public_instance()));
    provider_registry.register_hosting_provider(Arc::new(Tangled::public_instance()));
}

/// Registers additional Git hosting providers.
pub fn register_additional_providers<'a>(
    provider_registry: Arc<GitHostingProviderRegistry>,
    remote_urls: impl IntoIterator<Item = &'a str>,
) {
    for remote_url in remote_urls {
        if parse_git_remote_url(provider_registry.clone(), remote_url).is_some() {
            continue;
        }
        if let Ok(gitlab_self_hosted) = Gitlab::from_remote_url(remote_url) {
            provider_registry.register_hosting_provider(Arc::new(gitlab_self_hosted));
        } else if let Ok(github_self_hosted) = Github::from_remote_url(remote_url) {
            provider_registry.register_hosting_provider(Arc::new(github_self_hosted));
        } else if let Ok(forgejo_self_hosted) = Forgejo::from_remote_url(remote_url) {
            provider_registry.register_hosting_provider(Arc::new(forgejo_self_hosted));
        } else if let Ok(gitea_self_hosted) = Gitea::from_remote_url(remote_url) {
            provider_registry.register_hosting_provider(Arc::new(gitea_self_hosted));
        } else if let Ok(bitbucket_self_hosted) = Bitbucket::from_remote_url(remote_url) {
            provider_registry.register_hosting_provider(Arc::new(bitbucket_self_hosted));
        } else if let Ok(sourcehut_self_hosted) = SourceHut::from_remote_url(remote_url) {
            provider_registry.register_hosting_provider(Arc::new(sourcehut_self_hosted));
        }
    }
}

pub fn get_host_from_git_remote_url(remote_url: &str) -> Result<String> {
    remote_url
        .parse::<RemoteUrl>()?
        .host_str()
        .map(str::to_string)
        .context("URL has no host")
}

fn get_web_base_url_from_git_remote_url(remote_url: &str) -> Result<Url> {
    let remote = remote_url.parse::<RemoteUrl>()?;
    let host = remote.host_str().context("URL has no host")?;
    if matches!(remote.scheme(), "http" | "https") {
        Ok(Url::parse(&remote.origin().ascii_serialization())?)
    } else {
        Ok(Url::parse(&format!("https://{host}"))?)
    }
}

fn remote_matches_base_url(remote: &RemoteUrl, base_url: &Url, public_host: Option<&str>) -> bool {
    let Some(host) = remote.host_str() else {
        return false;
    };
    if Some(host) != base_url.host_str() {
        return false;
    }
    // SSH and Git transport ports do not identify the web interface's port.
    if !matches!(remote.scheme(), "http" | "https") {
        return true;
    }
    // Public instances keep their canonical web URL for HTTP clones on standard ports.
    remote.origin() == base_url.origin()
        || (public_host == Some(host) && remote.port().is_none() && base_url.port().is_none())
}

#[cfg(test)]
mod tests {
    use super::*;
    use git::{BuildCommitPermalinkParams, GitHostingProvider};
    use pretty_assertions::assert_eq;

    fn self_hosted_provider(
        kind: &str,
        remote: &str,
    ) -> Result<Arc<dyn GitHostingProvider + Send + Sync>> {
        Ok(match kind {
            "gitlab" => Arc::new(Gitlab::from_remote_url(remote)?),
            "github" => Arc::new(Github::from_remote_url(remote)?),
            "forgejo" => Arc::new(Forgejo::from_remote_url(remote)?),
            "gitea" => Arc::new(Gitea::from_remote_url(remote)?),
            "bitbucket" => Arc::new(Bitbucket::from_remote_url(remote)?),
            "sourcehut" => Arc::new(SourceHut::from_remote_url(remote)?),
            _ => anyhow::bail!("unknown provider {kind}"),
        })
    }

    #[test]
    fn test_self_hosted_web_origins() -> Result<()> {
        for kind in [
            "gitlab",
            "github",
            "forgejo",
            "gitea",
            "bitbucket",
            "sourcehut",
        ] {
            let registry = Arc::new(GitHostingProviderRegistry::new());
            let owner = if kind == "sourcehut" { "~team" } else { "team" };
            for (transport, base) in [
                ("ssh://git@", "https"),
                ("http://", "http"),
                ("https://", "https"),
            ] {
                let port = if base == "http" { 8080 } else { 8443 };
                let remote = format!("{transport}{kind}.example:{port}/{owner}/repo");
                register_additional_providers(registry.clone(), [remote.as_str()]);
                let (provider, parsed) = parse_git_remote_url(registry.clone(), &remote)
                    .context("registered provider")?;
                let expected_base = if transport.starts_with("ssh") {
                    format!("https://{kind}.example/")
                } else {
                    format!("{base}://{kind}.example:{port}/")
                };
                assert_eq!(provider.base_url().as_str(), expected_base);
                let expected_path = match kind {
                    "gitlab" => "team/repo/-/commit/abc123",
                    "bitbucket" => "projects/team/repos/repo/commits/abc123",
                    "sourcehut" => "~team/repo/commit/abc123",
                    _ => "team/repo/commit/abc123",
                };
                assert_eq!(
                    provider
                        .build_commit_permalink(
                            &parsed,
                            BuildCommitPermalinkParams { sha: "abc123" }
                        )
                        .as_str(),
                    format!("{expected_base}{expected_path}"),
                );
                let other_port = format!("{base}://{kind}.example:9999/{owner}/repo");
                assert!(provider.parse_remote_url(&other_port).is_none());
                let other_scheme = if base == "http" { "https" } else { "http" };
                let other_scheme = format!("{other_scheme}://{kind}.example:{port}/{owner}/repo");
                assert!(provider.parse_remote_url(&other_scheme).is_none());
            }
            assert_eq!(registry.list_hosting_providers().len(), 3);
            let other_port = format!("http://{kind}.example:9999/{owner}/repo");
            register_additional_providers(registry.clone(), [other_port.as_str()]);
            let (provider, _) = parse_git_remote_url(registry.clone(), &other_port)
                .context("provider on another port")?;
            assert_eq!(
                provider.base_url().as_str(),
                format!("http://{kind}.example:9999/")
            );
            let remote = format!("http://{kind}.example:8080/{owner}/repo");
            register_additional_providers(registry.clone(), [remote.as_str()]);
            assert_eq!(registry.list_hosting_providers().len(), 4);
        }
        Ok(())
    }

    #[test]
    fn test_public_provider_transport_compatibility() {
        let providers: Vec<Box<dyn GitHostingProvider>> = vec![
            Box::new(Gitlab::public_instance()),
            Box::new(Github::public_instance()),
            Box::new(Forgejo::public_instance()),
            Box::new(Gitea::public_instance()),
            Box::new(Bitbucket::public_instance()),
            Box::new(SourceHut::public_instance()),
            Box::new(Tangled::public_instance()),
        ];
        for provider in providers {
            let base = provider.base_url();
            let host = base.host_str().expect("public host");
            for remote in [
                format!("http://{host}/~team/repo"),
                format!("https://{host}/~team/repo"),
                format!("ssh://git@{host}:2222/~team/repo"),
            ] {
                assert!(provider.parse_remote_url(&remote).is_some(), "{remote}");
            }
            assert!(
                provider
                    .parse_remote_url(&format!("https://{host}:8443/~team/repo"))
                    .is_none()
            );
        }
    }

    #[test]
    fn test_web_base_url_discards_credentials_and_transport_ports() -> Result<()> {
        for (remote, expected) in [
            (
                "http://user:secret@gitlab.example:8080/team/repo.git?x=1#fragment",
                "http://gitlab.example:8080/",
            ),
            (
                "https://gitlab.example:443/team/repo.git",
                "https://gitlab.example/",
            ),
            (
                "ssh://git@gitlab.example:2222/team/repo.git",
                "https://gitlab.example/",
            ),
            (
                "git@gitlab.example:team/repo.git",
                "https://gitlab.example/",
            ),
            ("http://[::1]:8080/team/repo.git", "http://[::1]:8080/"),
        ] {
            assert_eq!(
                get_web_base_url_from_git_remote_url(remote)?.as_str(),
                expected
            );
        }
        Ok(())
    }

    #[test]
    fn test_configured_providers_match_web_ports_and_ssh() -> Result<()> {
        let base = Url::parse("http://review.example:8080/")?;
        let providers: Vec<Box<dyn GitHostingProvider>> = vec![
            Box::new(Gerrit::new("Gerrit", base.clone())),
            Box::new(Gitiles::new("Gitiles", base.clone())),
            Box::new(Tangled::new("Tangled", base)),
        ];
        for provider in providers {
            assert!(
                provider
                    .parse_remote_url("http://review.example:8080/team/repo")
                    .is_some()
            );
            assert!(
                provider
                    .parse_remote_url("http://review.example:8081/team/repo")
                    .is_none()
            );
            assert!(
                provider
                    .parse_remote_url("https://review.example:8080/team/repo")
                    .is_none()
            );
            assert!(
                provider
                    .parse_remote_url("ssh://git@review.example:29418/team/repo")
                    .is_some()
            );
        }
        Ok(())
    }

    #[test]
    fn test_self_hosted_avatar_requests_preserve_web_origin() -> Result<()> {
        futures::executor::block_on(async {
            for kind in ["gitlab", "gitea", "forgejo", "bitbucket"] {
                for scheme in ["http", "https"] {
                    let origin = format!("{scheme}://{kind}.example:8080");
                    let provider = self_hosted_provider(kind, &format!("{origin}/team/repo"))?;
                    let expected_origin = origin.clone();
                    let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
                    let client = http_client::FakeHttpClient::create({
                        let requests = requests.clone();
                        move |request| {
                            let uri = request.uri().to_string();
                            assert!(uri.starts_with(&format!("{expected_origin}/")), "{uri}");
                            requests.lock().expect("request log").push(uri);
                            async move {
                                Ok(http_client::Response::builder().status(200).body(http_client::AsyncBody::from(
                                    r#"{"author_email":"user+tag@example.com","author":{"id":1,"login":"user","avatar_url":"https://avatars.example/user"},"avatar_url":"https://avatars.example/user"}"#
                                ))?)
                            }
                        }
                    });
                    provider
                        .commit_author_avatar_url("team", "repo", "abc123".into(), None, client)
                        .await?;
                    let requests = requests.lock().expect("request log");
                    let expected_path = match kind {
                        "gitlab" => "/api/v4/projects/team%2Frepo/repository/commits/abc123",
                        "bitbucket" => {
                            "/rest/api/latest/projects/team/repos/repo/commits/abc123?avatarSize=128"
                        }
                        _ => {
                            "/api/v1/repos/team/repo/git/commits/abc123?stat=false&verification=false&files=false"
                        }
                    };
                    assert_eq!(requests.first(), Some(&format!("{origin}{expected_path}")));
                    assert_eq!(requests.len(), if kind == "gitlab" { 2 } else { 1 });
                    if kind == "gitlab" {
                        let avatar_request = requests.last().context("avatar request")?;
                        let avatar_url = Url::parse(avatar_request)?;
                        assert!(
                            avatar_url
                                .query_pairs()
                                .any(|(key, value)| key == "email"
                                    && value == "user+tag@example.com")
                        );
                    }
                }
            }
            Ok(())
        })
    }

    #[test]
    fn test_get_host_from_git_remote_url() {
        let tests = [
            (
                "https://jlannister@github.com/some-org/some-repo.git",
                Some("github.com".to_string()),
            ),
            (
                "git@github.com:zed-industries/zed.git",
                Some("github.com".to_string()),
            ),
            (
                "git@my.super.long.subdomain.com:zed-industries/zed.git",
                Some("my.super.long.subdomain.com".to_string()),
            ),
            (
                "first.last@gitlab.example.com:group/repo.git",
                Some("gitlab.example.com".to_string()),
            ),
            (
                "gitlab.example.com:group/repo.git",
                Some("gitlab.example.com".to_string()),
            ),
            (
                "persistent-https::https://gitlab.example.com/group/repo.git",
                Some("gitlab.example.com".to_string()),
            ),
            ("git@[::1]:team/repo.git", Some("[::1]".to_string())),
            (r"C:\repo.git", None),
            ("file:///srv/repo.git", None),
        ];

        for (remote_url, expected_host) in tests {
            let host = get_host_from_git_remote_url(remote_url).ok();
            assert_eq!(host, expected_host);
        }
    }
}
