use super::PackageManager;
use anyhow::{Context as _, Result, bail, ensure};
use http_client::{Host, Url};
use std::{
    collections::HashMap,
    io,
    net::{IpAddr, Ipv4Addr},
    path::{Path, PathBuf},
    process::Output,
};

/// A package manager command handed off to a shell, terminal, or debugger.
///
/// `program` is the bare executable name; `env` carries the `PATH` that resolves it, so the
/// receiver finds the same executable that was checked during resolution.
#[derive(Clone, Debug)]
pub struct PackageManagerInvocation {
    pub package_manager: PackageManager,
    pub program: String,
    pub args: Vec<String>,
    pub env: HashMap<String, String>,
    pub current_dir: PathBuf,
}

/// A package manager command Zed runs itself to install or query tools.
///
/// Spawned directly rather than through a shell, so it keeps the resolved executable: Windows
/// cannot start `.cmd` wrappers such as `pnpm.cmd` by their bare name.
#[derive(Clone, Debug)]
pub(crate) struct ToolCommand {
    pub package_manager: PackageManager,
    pub executable: PathBuf,
    pub args: Vec<String>,
    pub env: HashMap<String, String>,
    pub current_dir: PathBuf,
}

/// Arguments for a tool operation, translated from npm's vocabulary for the selected manager.
pub(super) fn tool_arguments(
    manager: PackageManager,
    major_version: u64,
    subcommand: &str,
    arguments: &[&str],
    directory: &Path,
    cache: &Path,
    proxy: Option<&Url>,
) -> Result<Vec<String>> {
    if manager == PackageManager::Npm {
        return Ok(build_npm_command_args(
            None,
            Some(directory),
            &cache.join("npm"),
            None,
            None,
            proxy,
            subcommand,
            arguments,
        ));
    }
    package_manager_arguments(manager, major_version, subcommand, arguments)
}

/// Settings that keep tool installations isolated from the user's projects and caches.
pub(super) fn apply_tool_environment(
    manager: PackageManager,
    major_version: u64,
    cache: &Path,
    environment: &mut HashMap<String, String>,
) {
    let mut set = |name: &str, value: String| {
        environment.insert(name.into(), value);
    };
    match manager {
        PackageManager::Pnpm | PackageManager::Vp => {
            set("npm_config_node_linker", "hoisted".into());
            set(
                "npm_config_store_dir",
                cache.join("pnpm").to_string_lossy().into_owned(),
            );
            set("npm_config_manage_package_manager_versions", "false".into());
            set("npm_config_package_manager_strict", "false".into());
            if manager == PackageManager::Vp {
                set("YARN_NODE_LINKER", "node-modules".into());
                set("YARN_ENABLE_IMMUTABLE_INSTALLS", "false".into());
            }
        }
        PackageManager::Bun => {
            set(
                "BUN_INSTALL_CACHE_DIR",
                cache.join("bun").to_string_lossy().into_owned(),
            );
        }
        PackageManager::Yarn => {
            set("YARN_IGNORE_PATH", "1".into());
            set(
                "YARN_CACHE_FOLDER",
                cache.join("yarn").to_string_lossy().into_owned(),
            );
            if major_version >= 2 {
                // Adapters resolve executables in node_modules; PnP cannot be used here.
                set("YARN_NODE_LINKER", "node-modules".into());
                set("YARN_ENABLE_IMMUTABLE_INSTALLS", "false".into());
                set("YARN_ENABLE_GLOBAL_CACHE", "false".into());
            }
        }
        PackageManager::Npm | PackageManager::Auto => {}
    }
}

fn package_manager_arguments(
    manager: PackageManager,
    major: u64,
    subcommand: &str,
    arguments: &[&str],
) -> Result<Vec<String>> {
    let mut result: Vec<String> = match (manager, subcommand) {
        (PackageManager::Pnpm, "info") => vec!["view".into()],
        (PackageManager::Pnpm, "install") => vec![
            "--ignore-workspace".into(),
            if arguments.iter().any(|argument| !argument.starts_with('-')) {
                "add"
            } else {
                "install"
            }
            .into(),
        ],
        (PackageManager::Yarn, "info") if major >= 2 => vec!["npm".into(), "info".into()],
        (_, "info") => vec!["info".into()],
        (_, "install") => vec![
            if arguments.iter().any(|argument| !argument.starts_with('-')) {
                "add"
            } else {
                "install"
            }
            .into(),
        ],
        (_, "run-script" | "run") => vec!["run".into()],
        _ => bail!(
            "{} does not support tool operation {subcommand}",
            manager.name()
        ),
    };
    let mut arguments = arguments.iter().copied();
    while let Some(argument) = arguments.next() {
        match argument {
            "--no-package-lock" => {}
            "--fetch-retry-mintimeout" | "--fetch-retry-maxtimeout" | "--fetch-timeout" => {
                arguments.next();
            }
            "--save-exact" => result.push(
                if matches!(manager, PackageManager::Pnpm | PackageManager::Vp) {
                    "--save-exact"
                } else {
                    "--exact"
                }
                .into(),
            ),
            _ => result.push(argument.into()),
        }
    }
    if manager == PackageManager::Yarn && major < 2 {
        result.push("--non-interactive".into());
        if subcommand == "install" {
            result.push("--ignore-workspace-root-check".into());
        }
    }
    Ok(result)
}

pub(super) async fn create_tool_manifest(directory: &Path) -> Result<()> {
    create_if_missing(&directory.join("package.json"), "{\"private\":true}\n").await
}

pub(super) async fn create_if_missing(path: &Path, contents: &str) -> Result<()> {
    let path = path.to_path_buf();
    let contents = contents.to_owned();
    smol::unblock(move || {
        use std::io::Write as _;
        let parent = path
            .parent()
            .context("tool configuration has no parent directory")?;
        let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
        temporary.write_all(contents.as_bytes())?;
        // Publish complete contents so concurrent installers never observe an empty manifest.
        match temporary.persist_noclobber(&path) {
            Ok(_) => Ok(()),
            Err(error) if error.error.kind() == io::ErrorKind::AlreadyExists => Ok(()),
            Err(error) => Err(error.error).with_context(|| format!("creating {}", path.display())),
        }
    })
    .await
}

pub(crate) async fn run_tool_command(invocation: ToolCommand) -> Result<Output> {
    log::debug!(
        "executing {} in {}",
        invocation.package_manager.name(),
        invocation.current_dir.display()
    );
    let mut command = util::command::new_command(&invocation.executable);
    command
        .args(&invocation.args)
        .envs(&invocation.env)
        .current_dir(&invocation.current_dir);
    let output = command
        .output()
        .await
        .with_context(|| format!("starting {}", invocation.executable.display()))?;
    ensure!(
        output.status.success(),
        "{} failed in {}:\nstdout: {}\nstderr: {}",
        invocation.package_manager.name(),
        invocation.current_dir.display(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(output)
}

pub(super) fn proxy_argument(proxy: Option<&Url>) -> Option<String> {
    let mut proxy = proxy.cloned()?;
    // Map proxy settings from `http://localhost:10809` to `http://127.0.0.1:10809`
    // NodeRuntime without environment information can not parse `localhost`
    // correctly.
    // TODO: map to `[::1]` if we are using ipv6
    if matches!(proxy.host(), Some(Host::Domain(domain)) if domain.eq_ignore_ascii_case("localhost"))
    {
        // When localhost is a valid Host, so is `127.0.0.1`
        if proxy.set_ip_host(IpAddr::V4(Ipv4Addr::LOCALHOST)).is_err() {
            log::warn!("could not normalize localhost in package manager proxy URL");
        }
    }

    Some(proxy.as_str().to_string())
}

fn build_npm_command_args(
    entrypoint: Option<&Path>,
    prefix_dir: Option<&Path>,
    cache_dir: &Path,
    user_config: Option<&Path>,
    global_config: Option<&Path>,
    proxy: Option<&Url>,
    subcommand: &str,
    args: &[&str],
) -> Vec<String> {
    let mut command_args = Vec::new();
    if let Some(entrypoint) = entrypoint {
        command_args.push(entrypoint.to_string_lossy().into_owned());
    }
    if let Some(prefix_dir) = prefix_dir {
        command_args.push("--prefix".into());
        command_args.push(prefix_dir.to_string_lossy().into_owned());
    }
    command_args.push(subcommand.to_string());
    command_args.push(format!("--cache={}", cache_dir.display()));
    if let Some(user_config) = user_config {
        command_args.push("--userconfig".into());
        command_args.push(user_config.to_string_lossy().into_owned());
    }
    if let Some(global_config) = global_config {
        command_args.push("--globalconfig".into());
        command_args.push(global_config.to_string_lossy().into_owned());
    }
    if let Some(proxy_arg) = proxy_argument(proxy) {
        command_args.push("--proxy".into());
        command_args.push(proxy_arg);
    }
    command_args.extend(args.into_iter().map(|a| a.to_string()));
    command_args
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn publishes_complete_manifests_without_overwriting_existing_dependencies() -> Result<()> {
        smol::block_on(async {
            let directory = tempfile::tempdir()?;
            futures::future::try_join_all((0..12).map(|_| async {
                create_tool_manifest(directory.path()).await?;
                let contents =
                    smol::fs::read_to_string(directory.path().join("package.json")).await?;
                let manifest: serde_json::Value = serde_json::from_str(&contents)?;
                assert_eq!(
                    manifest.get("private"),
                    Some(&serde_json::Value::Bool(true))
                );
                anyhow::Ok(())
            }))
            .await?;
            let contents = r#"{"private":true,"dependencies":{"existing":"1.0.0"}}"#;
            std::fs::write(directory.path().join("package.json"), contents)?;
            create_tool_manifest(directory.path()).await?;
            assert_eq!(
                std::fs::read_to_string(directory.path().join("package.json"))?,
                contents
            );
            Ok(())
        })
    }

    #[test]
    fn test_proxy_argument_map_localhost_proxy() {
        const CASES: [(&str, &str); 4] = [
            // Map localhost to 127.0.0.1
            ("http://localhost:9090/", "http://127.0.0.1:9090/"),
            ("https://google.com/", "https://google.com/"),
            (
                "http://username:password@proxy.thing.com:8080/",
                "http://username:password@proxy.thing.com:8080/",
            ),
            // Test when localhost is contained within a different part of the URL
            (
                "http://username:localhost@localhost:8080/",
                "http://username:localhost@127.0.0.1:8080/",
            ),
        ];

        for (proxy, mapped_proxy) in CASES {
            let proxy = Url::parse(proxy).unwrap();
            let proxy = proxy_argument(Some(&proxy)).expect("Proxy was not passed correctly");
            assert_eq!(
                proxy, mapped_proxy,
                "Incorrectly mapped localhost to 127.0.0.1"
            );
        }
    }

    #[test]
    fn test_build_npm_command_args_inserts_prefix_before_subcommand() {
        let args = build_npm_command_args(
            None,
            Some(Path::new("/tmp/zed-prefix")),
            Path::new("/tmp/cache"),
            None,
            None,
            None,
            "exec",
            &["--yes", "--", "agent-package"],
        );

        assert_eq!(
            args,
            vec![
                "--prefix".to_string(),
                "/tmp/zed-prefix".to_string(),
                "exec".to_string(),
                "--cache=/tmp/cache".to_string(),
                "--yes".to_string(),
                "--".to_string(),
                "agent-package".to_string(),
            ]
        );
    }

    #[test]
    fn test_build_npm_command_args_keeps_entrypoint_before_prefix() {
        let args = build_npm_command_args(
            Some(Path::new("/tmp/npm-cli.js")),
            Some(Path::new("/tmp/zed-prefix")),
            Path::new("/tmp/cache"),
            None,
            None,
            None,
            "exec",
            &["--yes"],
        );

        assert_eq!(
            args,
            vec![
                "/tmp/npm-cli.js".to_string(),
                "--prefix".to_string(),
                "/tmp/zed-prefix".to_string(),
                "exec".to_string(),
                "--cache=/tmp/cache".to_string(),
                "--yes".to_string(),
            ]
        );
    }
}
