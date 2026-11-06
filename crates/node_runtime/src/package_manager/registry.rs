use anyhow::{Context as _, Result, bail};
use chrono::{DateTime, Utc};
use semver::{Version, VersionReq};
use serde::Deserialize;
use std::collections::HashMap;

pub enum VersionStrategy<'a> {
    Pin(&'a Version),
    Latest(&'a Version),
}

pub(super) fn should_install_package_version(
    installed_version: &Version,
    version_strategy: VersionStrategy<'_>,
) -> bool {
    match version_strategy {
        VersionStrategy::Pin(pinned_version) => installed_version != pinned_version,
        VersionStrategy::Latest(latest_version) => installed_version < latest_version,
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct PackageInfo {
    #[serde(default)]
    pub(super) dist_tags: PackageDistTags,
    versions: Vec<Version>,
    #[serde(default, deserialize_with = "deserialize_package_info_time")]
    time: HashMap<String, String>,
}

/// Parse PackageInfo from npm info --json output, handling both v11 and >= v12 formats.
pub(super) fn deserialize_package_info_from_response(
    data: &[u8],
) -> Result<PackageInfo, serde_json::Error> {
    let value: serde_json::Value = match serde_json::from_slice(data) {
        Ok(value) => value,
        Err(error) => {
            // Yarn Classic emits one JSON record per line, including progress records.
            let Some(value) = data
                .split(|byte| *byte == b'\n')
                .filter_map(|line| serde_json::from_slice::<serde_json::Value>(line).ok())
                .find(|value| {
                    value.get("type").and_then(serde_json::Value::as_str) == Some("inspect")
                })
            else {
                return Err(error);
            };
            value
        }
    };
    if value.get("type").and_then(serde_json::Value::as_str) == Some("inspect") {
        return PackageInfo::deserialize(&value["data"]);
    }

    // npm >= 12 returns an array with one object: [ { ... } ]
    if let serde_json::Value::Array(arr) = &value {
        if arr.len() == 1 {
            return PackageInfo::deserialize(&arr[0]);
        }
    }

    // npm <= v11 returns a bare JSON object: { ... }
    PackageInfo::deserialize(value)
}

#[derive(Debug, Deserialize, Default)]
pub struct PackageDistTags {
    pub(super) latest: Option<Version>,
}

// Some registries put non-string values in the `time` map: JFrog Artifactory emits
// `"unpublished": null`, and npm itself reports `unpublished` as an object when a
// package has had versions unpublished. Only version keys map to the RFC 3339 strings
// we read, so keep the string entries and drop the rest rather than failing to parse
// the entire `npm info` response (which would block language server installation).
fn deserialize_package_info_time<'de, D>(
    deserializer: D,
) -> Result<HashMap<String, String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let entries = HashMap::<String, serde_json::Value>::deserialize(deserializer)?;
    Ok(entries
        .into_iter()
        .filter_map(|(key, value)| match value {
            serde_json::Value::String(value) => Some((key, value)),
            _ => None,
        })
        .collect())
}

#[derive(Debug, Deserialize)]
struct NpmConfig {
    #[serde(default)]
    before: Option<String>,
}

/// Reads the `before` cutoff from `npm config list --json` output.
///
/// `npm config get before` renders Date values for display; the JSON output keeps the computed
/// cutoff in the same ISO format used by `npm info --json` release times.
pub(super) fn parse_npm_config_before(stdout: &[u8]) -> Result<Option<String>> {
    let config: NpmConfig = serde_json::from_slice(stdout)?;
    Ok(config
        .before
        .filter(|before| !before.trim().is_empty() && before != "null"))
}

pub(super) fn select_package_version(
    package_name: &str,
    mut info: PackageInfo,
    before: Option<&str>,
    version_requirement: Option<&VersionReq>,
) -> Result<Version> {
    if let Some(version_requirement) = version_requirement {
        info.versions
            .retain(|version| version_requirement.matches(version));
        info.versions.sort();
        info.dist_tags.latest = info
            .dist_tags
            .latest
            .take()
            .filter(|version| version_requirement.matches(version));
    }

    if let Some(before) = before
        && !info.time.is_empty()
    {
        let before_timestamp = DateTime::parse_from_rfc3339(before)
            .with_context(|| format!("parsing npm before config timestamp {before:?}"))?
            .with_timezone(&Utc);
        let latest_version = info.dist_tags.latest.as_ref();

        if let Some(version) = latest_version
            && package_version_was_published_before(version, &info.time, &before_timestamp)?
        {
            return Ok(version.clone());
        }

        for version in info.versions.iter().rev() {
            if is_allowed_package_version_before(
                version,
                latest_version,
                &info.time,
                &before_timestamp,
                version_requirement.is_some(),
            )? {
                return Ok(version.clone());
            }
        }

        bail!("no version found for package {package_name} before {before}");
    }

    info.dist_tags
        .latest
        .or_else(|| info.versions.pop())
        .with_context(|| format!("no version found for package {package_name}"))
}

fn is_allowed_package_version_before(
    version: &Version,
    latest_version: Option<&Version>,
    published_at_by_version: &HashMap<String, String>,
    before: &DateTime<Utc>,
    allow_prereleases: bool,
) -> Result<bool> {
    if (!allow_prereleases && !version.pre.is_empty())
        || latest_version.is_some_and(|latest_version| version > latest_version)
    {
        return Ok(false);
    }

    package_version_was_published_before(version, published_at_by_version, before)
}

fn package_version_was_published_before(
    version: &Version,
    published_at_by_version: &HashMap<String, String>,
    before: &DateTime<Utc>,
) -> Result<bool> {
    let Some(published_at) = published_at_by_version.get(&version.to_string()) else {
        return Ok(false);
    };
    let published_at = DateTime::parse_from_rfc3339(published_at)
        .with_context(|| format!("parsing package release timestamp for version {version}"))?
        .with_timezone(&Utc);
    Ok(&published_at <= before)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_latest_version_strategy_accepts_newer_installed_versions() -> Result<()> {
        let target_version = Version::parse("2.0.0")?;

        assert!(!should_install_package_version(
            &Version::parse("2.0.0")?,
            VersionStrategy::Latest(&target_version)
        ));
        assert!(should_install_package_version(
            &Version::parse("1.0.0")?,
            VersionStrategy::Latest(&target_version)
        ));
        assert!(!should_install_package_version(
            &Version::parse("3.0.0")?,
            VersionStrategy::Latest(&target_version)
        ));

        Ok(())
    }

    #[test]
    fn test_select_package_version_uses_dist_tag_without_before() -> Result<()> {
        let info: PackageInfo = serde_json::from_str(
            r#"{
                "dist-tags": { "latest": "3.0.0" },
                "versions": ["1.0.0", "2.0.0", "3.0.0"],
                "time": {
                    "1.0.0": "2024-01-01T00:00:00.000Z",
                    "2.0.0": "2024-02-01T00:00:00.000Z",
                    "3.0.0": "2024-03-01T00:00:00.000Z"
                }
            }"#,
        )?;

        assert_eq!(
            select_package_version("test-package", info, None, None)?,
            Version::parse("3.0.0")?
        );
        Ok(())
    }

    #[test]
    fn test_npm_info_skips_non_string_time_entries() -> Result<()> {
        // Registries such as JFrog Artifactory include `"unpublished": null` in `time`;
        // parsing must tolerate this rather than rejecting the whole response.
        let info: PackageInfo = serde_json::from_str(
            r#"{
                "dist-tags": { "latest": "2.0.0" },
                "versions": ["1.0.0", "2.0.0"],
                "time": {
                    "unpublished": null,
                    "created": "2024-01-01T00:00:00.000Z",
                    "modified": "2024-02-01T00:00:00.000Z",
                    "1.0.0": "2024-01-01T00:00:00.000Z",
                    "2.0.0": "2024-02-01T00:00:00.000Z"
                }
            }"#,
        )?;

        assert_eq!(
            select_package_version("test-package", info, Some("2024-02-15T00:00:00.000Z"), None)?,
            Version::parse("2.0.0")?
        );
        Ok(())
    }

    #[test]
    fn test_select_package_version_uses_latest_before_npm_before_config() -> Result<()> {
        let info: PackageInfo = serde_json::from_str(
            r#"{
                "dist-tags": { "latest": "3.0.0" },
                "versions": ["1.0.0", "2.0.0", "3.0.0"],
                "time": {
                    "1.0.0": "2024-01-01T00:00:00.000Z",
                    "2.0.0": "2024-02-01T00:00:00.000Z",
                    "3.0.0": "2024-03-01T00:00:00.000Z"
                }
            }"#,
        )?;

        assert_eq!(
            select_package_version("test-package", info, Some("2024-02-15T00:00:00.000Z"), None)?,
            Version::parse("2.0.0")?
        );
        Ok(())
    }

    #[test]
    fn test_select_package_version_keeps_allowed_latest_dist_tag() -> Result<()> {
        let info: PackageInfo = serde_json::from_str(
            r#"{
                "dist-tags": { "latest": "2.0.0" },
                "versions": ["1.0.0", "2.0.0", "3.0.0"],
                "time": {
                    "1.0.0": "2024-01-01T00:00:00.000Z",
                    "2.0.0": "2024-02-01T00:00:00.000Z",
                    "3.0.0": "2024-03-01T00:00:00.000Z"
                }
            }"#,
        )?;

        assert_eq!(
            select_package_version("test-package", info, Some("2024-02-15T00:00:00.000Z"), None)?,
            Version::parse("2.0.0")?
        );
        Ok(())
    }

    #[test]
    fn test_select_package_version_keeps_allowed_prerelease_latest_dist_tag() -> Result<()> {
        let info: PackageInfo = serde_json::from_str(
            r#"{
                "dist-tags": { "latest": "2.0.0-beta.1" },
                "versions": ["1.0.0", "2.0.0-beta.1"],
                "time": {
                    "1.0.0": "2024-01-01T00:00:00.000Z",
                    "2.0.0-beta.1": "2024-02-01T00:00:00.000Z"
                }
            }"#,
        )?;

        assert_eq!(
            select_package_version("test-package", info, Some("2024-02-15T00:00:00.000Z"), None)?,
            Version::parse("2.0.0-beta.1")?
        );
        Ok(())
    }

    #[test]
    fn test_select_package_version_ignores_prereleases_before_cutoff() -> Result<()> {
        let info: PackageInfo = serde_json::from_str(
            r#"{
                "dist-tags": { "latest": "2.0.0" },
                "versions": ["1.0.0", "2.0.0-beta.1", "2.0.0"],
                "time": {
                    "1.0.0": "2024-01-01T00:00:00.000Z",
                    "2.0.0-beta.1": "2024-02-01T00:00:00.000Z",
                    "2.0.0": "2024-03-01T00:00:00.000Z"
                }
            }"#,
        )?;

        assert_eq!(
            select_package_version("test-package", info, Some("2024-02-15T00:00:00.000Z"), None)?,
            Version::parse("1.0.0")?
        );
        Ok(())
    }

    #[test]
    fn test_select_package_version_ignores_versions_above_latest_dist_tag() -> Result<()> {
        let info: PackageInfo = serde_json::from_str(
            r#"{
                "dist-tags": { "latest": "2.0.0" },
                "versions": ["1.0.0", "2.0.0", "3.0.0"],
                "time": {
                    "1.0.0": "2024-01-01T00:00:00.000Z",
                    "2.0.0": "2024-03-01T00:00:00.000Z",
                    "3.0.0": "2024-02-01T00:00:00.000Z"
                }
            }"#,
        )?;

        assert_eq!(
            select_package_version("test-package", info, Some("2024-02-15T00:00:00.000Z"), None)?,
            Version::parse("1.0.0")?
        );
        Ok(())
    }

    #[test]
    fn test_select_package_version_errors_when_no_version_matches_before() -> Result<()> {
        let info: PackageInfo = serde_json::from_str(
            r#"{
                "dist-tags": { "latest": "2.0.0" },
                "versions": ["1.0.0", "2.0.0"],
                "time": {
                    "1.0.0": "2024-01-01T00:00:00.000Z",
                    "2.0.0": "2024-02-01T00:00:00.000Z"
                }
            }"#,
        )?;

        let Err(error) =
            select_package_version("test-package", info, Some("2023-12-01T00:00:00.000Z"), None)
        else {
            bail!("expected cutoff to reject all package versions");
        };
        assert_eq!(
            error.to_string(),
            "no version found for package test-package before 2023-12-01T00:00:00.000Z"
        );
        Ok(())
    }

    #[test]
    fn test_select_package_version_selects_latest_matching_requirement() -> Result<()> {
        let info: PackageInfo = serde_json::from_str(
            r#"{
                "dist-tags": { "latest": "7.0.0" },
                "versions": ["6.0.3", "7.0.0", "5.9.3", "6.0.2"]
            }"#,
        )?;
        let version_requirement = VersionReq::parse("^6")?;

        assert_eq!(
            select_package_version("test-package", info, None, Some(&version_requirement))?,
            Version::parse("6.0.3")?
        );
        Ok(())
    }

    #[test]
    fn test_select_package_version_applies_before_to_matching_versions() -> Result<()> {
        let info: PackageInfo = serde_json::from_str(
            r#"{
                "dist-tags": { "latest": "7.0.0" },
                "versions": ["6.0.3", "7.0.0", "6.0.2"],
                "time": {
                    "6.0.2": "2024-02-01T00:00:00.000Z",
                    "6.0.3": "2024-03-01T00:00:00.000Z",
                    "7.0.0": "2024-04-01T00:00:00.000Z"
                }
            }"#,
        )?;
        let version_requirement = VersionReq::parse("^6")?;

        assert_eq!(
            select_package_version(
                "test-package",
                info,
                Some("2024-02-15T00:00:00.000Z"),
                Some(&version_requirement),
            )?,
            Version::parse("6.0.2")?
        );
        Ok(())
    }

    #[test]
    fn test_select_package_version_allows_requested_prerelease_before_cutoff() -> Result<()> {
        let info: PackageInfo = serde_json::from_str(
            r#"{
                "dist-tags": { "latest": "7.0.0" },
                "versions": ["7.1.0-beta.1", "7.1.0-beta.2", "7.0.0"],
                "time": {
                    "7.0.0": "2024-01-01T00:00:00.000Z",
                    "7.1.0-beta.1": "2024-02-01T00:00:00.000Z",
                    "7.1.0-beta.2": "2024-03-01T00:00:00.000Z"
                }
            }"#,
        )?;
        let version_requirement = VersionReq::parse(">=7.1.0-beta.1, <7.1.0")?;

        assert_eq!(
            select_package_version(
                "test-package",
                info,
                Some("2024-02-15T00:00:00.000Z"),
                Some(&version_requirement),
            )?,
            Version::parse("7.1.0-beta.1")?
        );
        Ok(())
    }

    #[test]
    fn test_select_package_version_errors_without_matching_version() -> Result<()> {
        let info: PackageInfo = serde_json::from_str(
            r#"{
                "dist-tags": { "latest": "7.0.0" },
                "versions": ["5.9.3", "7.0.0"]
            }"#,
        )?;
        let version_requirement = VersionReq::parse("^6")?;

        let error = select_package_version("test-package", info, None, Some(&version_requirement))
            .expect_err("expected version requirement to reject all package versions");
        assert_eq!(
            error.to_string(),
            "no version found for package test-package"
        );
        Ok(())
    }

    #[test]
    fn test_pinned_version_strategy_replaces_different_installed_version() -> Result<()> {
        let pinned_version = Version::parse("6.0.3")?;

        assert!(!should_install_package_version(
            &pinned_version,
            VersionStrategy::Pin(&pinned_version)
        ));
        assert!(should_install_package_version(
            &Version::parse("7.0.0")?,
            VersionStrategy::Pin(&pinned_version)
        ));
        Ok(())
    }

    #[test]
    fn test_deserialize_npm_info_npm11_format() -> Result<()> {
        let json = r#"{
            "dist-tags": { "latest": "3.0.0" },
            "versions": ["1.0.0", "2.0.0", "3.0.0"]
        }"#;

        let info = deserialize_package_info_from_response(json.as_bytes())?;
        assert_eq!(info.dist_tags.latest, Some(Version::parse("3.0.0")?));
        assert_eq!(
            info.versions,
            vec![
                Version::parse("1.0.0")?,
                Version::parse("2.0.0")?,
                Version::parse("3.0.0")?
            ]
        );
        Ok(())
    }

    #[test]
    fn test_deserialize_npm_v12_format() -> Result<()> {
        let json = r#"[
            {
                "dist-tags": { "latest": "3.0.0" },
                "versions": ["1.0.0", "2.0.0", "3.0.0"]
            }
        ]"#;

        let info = deserialize_package_info_from_response(json.as_bytes())?;
        assert_eq!(info.dist_tags.latest, Some(Version::parse("3.0.0")?));
        assert_eq!(
            info.versions,
            vec![
                Version::parse("1.0.0")?,
                Version::parse("2.0.0")?,
                Version::parse("3.0.0")?
            ]
        );
        Ok(())
    }
}
