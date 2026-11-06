use std::str::FromStr;

use derive_more::Deref;
use url::Url;

/// The URL to a Git remote.
#[derive(Debug, PartialEq, Eq, Clone, Deref)]
pub struct RemoteUrl(Url);

impl RemoteUrl {
    fn parse(input: &str) -> Result<Url, url::ParseError> {
        let mut input = input.trim();
        // Remote helpers wrap the address as `<transport>::<address>`.
        while let Some((transport, address)) = input.split_once("::") {
            if transport.is_empty()
                || !transport
                    .chars()
                    .all(|character| character.is_ascii_alphanumeric() || "+-.".contains(character))
            {
                break;
            }
            input = address;
        }

        // Drive paths must not become SSH URLs with a one-letter host.
        if input.split_once(':').is_some_and(|(prefix, _)| {
            prefix.len() == 1
                && prefix
                    .chars()
                    .all(|character| character.is_ascii_alphabetic())
        }) {
            return Err(url::ParseError::RelativeUrlWithoutBase);
        }

        if input.contains("://") || input.starts_with("file:") {
            return Url::parse(input);
        }

        // A colon before any slash denotes SCP syntax, except inside an IPv6 host.
        let mut inside_brackets = false;
        for (index, character) in input.char_indices() {
            match character {
                '[' => inside_brackets = true,
                ']' => inside_brackets = false,
                '/' | '\\' => break,
                ':' if !inside_brackets => {
                    let (authority, _) = input.split_at(index);
                    let path = input
                        .get(index + 1..)
                        .ok_or(url::ParseError::RelativeUrlWithoutBase)?;
                    if authority.is_empty() || path.is_empty() {
                        return Err(url::ParseError::RelativeUrlWithoutBase);
                    }
                    return Url::parse(&format!("ssh://{authority}/{path}"));
                }
                _ => {}
            }
        }

        Url::parse(input)
    }
}

impl FromStr for RemoteUrl {
    type Err = url::ParseError;

    fn from_str(input: &str) -> Result<Self, Self::Err> {
        let mut url = Self::parse(input)?;
        // Unlike HTTP URLs, SSH URLs retain the case of DNS names in `url`.
        // Providers compare hosts directly, so normalize them for every transport.
        if let Some(host) = url.host_str() {
            let host = host.to_ascii_lowercase();
            url.set_host(Some(&host))?;
        }
        Ok(Self(url))
    }
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    #[test]
    fn test_parsing_valid_remote_urls() {
        let valid_urls = vec![
            (
                "https://github.com/octocat/zed.git",
                "https",
                "github.com",
                "/octocat/zed.git",
            ),
            (
                "https://jlannister@github.com/octocat/zed.git",
                "https",
                "github.com",
                "/octocat/zed.git",
            ),
            (
                "git@github.com:octocat/zed.git",
                "ssh",
                "github.com",
                "/octocat/zed.git",
            ),
            (
                "org-000000@github.com:octocat/zed.git",
                "ssh",
                "github.com",
                "/octocat/zed.git",
            ),
            (
                "first.last@gitlab.example.com:group/repo.git",
                "ssh",
                "gitlab.example.com",
                "/group/repo.git",
            ),
            (
                "ssh://git@github.com/octocat/zed.git",
                "ssh",
                "github.com",
                "/octocat/zed.git",
            ),
            (
                "github.com:octocat/zed.git",
                "ssh",
                "github.com",
                "/octocat/zed.git",
            ),
            (
                "git@GitHub.com:octocat/zed.git",
                "ssh",
                "github.com",
                "/octocat/zed.git",
            ),
            ("git@[::1]:team/repo.git", "ssh", "[::1]", "/team/repo.git"),
            (
                "[2001:db8::1]:team/repo.git",
                "ssh",
                "[2001:db8::1]",
                "/team/repo.git",
            ),
            (
                "persistent-https::https://example.com/team/repo",
                "https",
                "example.com",
                "/team/repo",
            ),
            (
                "helper::first.last@gitlab.example.com:group/repo.git",
                "ssh",
                "gitlab.example.com",
                "/group/repo.git",
            ),
            (
                "ssh://git@example.com:2222/team/repo.git",
                "ssh",
                "example.com",
                "/team/repo.git",
            ),
            (
                "file:///path/to/local/zed",
                "file",
                "",
                "/path/to/local/zed",
            ),
        ];

        for (input, expected_scheme, expected_host, expected_path) in valid_urls {
            let parsed = input.parse::<RemoteUrl>().expect("failed to parse URL");
            let url = parsed.0;
            assert_eq!(
                url.scheme(),
                expected_scheme,
                "unexpected scheme for {input:?}",
            );
            assert_eq!(
                url.host_str().unwrap_or(""),
                expected_host,
                "unexpected host for {input:?}",
            );
            assert_eq!(url.path(), expected_path, "unexpected path for {input:?}");
        }
    }

    #[test]
    fn test_parsing_invalid_remote_urls() {
        let invalid_urls = vec![
            "not_a_url",
            "http://",
            "/srv/team/repo.git",
            "./example.com:repo.git",
            r"C:\srv\team\repo.git",
            "C:/srv/team/repo.git",
            "C://srv/team/repo.git",
            r"\\server\team\repo.git",
            "git@[::1:team/repo.git",
            "example.com:",
        ];

        for url in invalid_urls {
            assert!(
                url.parse::<RemoteUrl>().is_err(),
                "expected \"{url}\" to not parse as a Git remote URL",
            );
        }
    }
}
