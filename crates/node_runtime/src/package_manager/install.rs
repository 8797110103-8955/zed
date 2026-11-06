use super::PackageManager;
use crate::NodeRuntime;
use anyhow::{Context as _, Result, bail, ensure};
use futures::{AsyncReadExt as _, channel::oneshot};
use http_client::{AsyncBody, HttpClient};
use smol::fs;
use std::{
    collections::HashMap,
    env,
    ffi::OsString,
    path::{Path, PathBuf},
};

pub enum PackageManagerInstallEvent {
    Select {
        response: oneshot::Sender<Option<PackageManager>>,
    },
    Installed {
        manager: PackageManager,
        directory: PathBuf,
        installed_via_manager: bool,
    },
}

fn registry_file() -> PathBuf {
    paths::data_dir().join("package_manager/installed-paths.json")
}

pub(super) async fn installed_directories() -> Result<Vec<PathBuf>> {
    match fs::read(registry_file()).await {
        Ok(contents) => Ok(serde_json::from_slice(&contents)?),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(error) => Err(error.into()),
    }
}

pub(super) async fn register_directory(directory: &Path) -> Result<()> {
    let mut directories = installed_directories().await?;
    if !directories.iter().any(|existing| existing == directory) {
        directories.push(directory.to_path_buf());
        let file = registry_file();
        fs::create_dir_all(
            file.parent()
                .context("installation registry has no parent")?,
        )
        .await?;
        fs::write(file, serde_json::to_vec(&directories)?).await?;
    }
    Ok(())
}

fn npm_package(manager: PackageManager) -> Result<&'static str> {
    match manager {
        PackageManager::Pnpm => Ok("pnpm"),
        PackageManager::Vp => Ok("vite-plus"),
        PackageManager::Bun => Ok("bun"),
        PackageManager::Npm => Ok("npm"),
        PackageManager::Yarn => Ok("@yarnpkg/cli-dist"),
        PackageManager::Auto => {
            bail!("{} requires its standalone installer", manager.name())
        }
    }
}

async fn command_output(mut command: util::command::Command) -> Result<String> {
    command.kill_on_drop(true);
    let output = command.output().await?;
    ensure!(
        output.status.success(),
        "package manager installation failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(String::from_utf8(output.stdout)?.trim().to_string())
}

/// Arguments that print where `installer` puts global executables, for managers that report it.
fn global_bin_arguments(installer: PackageManager, major_version: u64) -> &'static [&'static str] {
    match installer {
        PackageManager::Npm => &["prefix", "--global"],
        PackageManager::Pnpm => &["bin", "--global"],
        PackageManager::Bun => &["pm", "bin", "--global"],
        PackageManager::Yarn if major_version <= 1 => &["global", "bin"],
        _ => &[],
    }
}

/// Arguments that install `package` globally. npm and modern Yarn install into `prefix`; the
/// other managers use their own global directory.
fn global_install_arguments(
    installer: PackageManager,
    major_version: u64,
    package: &str,
    prefix: &Path,
) -> Result<Vec<OsString>> {
    let arguments: &[&str] = match installer {
        PackageManager::Npm => &["install", "--global", package, "--prefix"],
        PackageManager::Pnpm | PackageManager::Bun => &["add", "--global", package],
        PackageManager::Vp => &["install", "--global", package],
        PackageManager::Yarn if major_version <= 1 => &["global", "add", package],
        // Modern Yarn removed global add; its Classic CLI still supplies the global install operation.
        PackageManager::Yarn => &[
            "dlx",
            "--package",
            "yarn@1",
            "yarn",
            "global",
            "add",
            package,
            "--prefix",
        ],
        PackageManager::Auto => bail!("unsupported installation manager {}", installer.name()),
    };
    let mut arguments = arguments.iter().map(OsString::from).collect::<Vec<_>>();
    if arguments
        .last()
        .is_some_and(|argument| argument == "--prefix")
    {
        arguments.push(prefix.into());
    }
    Ok(arguments)
}

pub(super) async fn install_with_manager(
    installer: PackageManager,
    major_version: u64,
    executable: &Path,
    manager: PackageManager,
    directory: &Path,
    environment: &HashMap<String, String>,
) -> Result<Vec<PathBuf>> {
    let reported_bin = match global_bin_arguments(installer, major_version) {
        [] => None,
        arguments => {
            let mut command = util::command::new_command(executable);
            command
                .args(arguments)
                .current_dir(directory)
                .envs(environment);
            let bin = PathBuf::from(command_output(command).await?);
            ensure!(
                bin.is_absolute(),
                "{} returned a relative global directory",
                installer.name()
            );
            Some(bin)
        }
    };
    let (prefix, bins) = match (installer, reported_bin) {
        (PackageManager::Npm, Some(prefix)) => {
            let bin = if cfg!(windows) {
                prefix.clone()
            } else {
                prefix.join("bin")
            };
            (prefix, vec![bin])
        }
        (_, Some(bin)) => (bin.clone(), vec![bin]),
        (PackageManager::Yarn, None) => {
            let prefix = paths::home_dir().join(".yarn");
            (prefix.clone(), vec![prefix.join("bin")])
        }
        (PackageManager::Vp, None) => {
            let mut bins = vec![paths::home_dir().join(".vite-plus/bin")];
            if let Some(home) = environment.get("VP_HOME") {
                bins.insert(0, PathBuf::from(home).join("bin"));
            }
            if let Some(local) = env::var_os("LOCALAPPDATA") {
                bins.push(PathBuf::from(local).join("vite-plus/bin"));
            }
            if let Some(bin) = environment.get("VP_BIN_DIR") {
                bins.insert(0, PathBuf::from(bin));
            }
            (PathBuf::new(), bins)
        }
        _ => bail!("unsupported installation manager {}", installer.name()),
    };
    let mut command = util::command::new_command(executable);
    command
        .args(global_install_arguments(
            installer,
            major_version,
            npm_package(manager)?,
            &prefix,
        )?)
        .current_dir(directory)
        .envs(environment);
    command_output(command).await?;
    Ok(bins)
}

async fn download(http: &dyn HttpClient, url: &str) -> Result<Vec<u8>> {
    let mut response = http.get(url, AsyncBody::default(), true).await?;
    ensure!(
        response.status().is_success(),
        "downloading {url} failed: {}",
        response.status()
    );
    let mut contents = Vec::new();
    response.body_mut().read_to_end(&mut contents).await?;
    ensure!(
        !contents.is_empty(),
        "downloaded an empty installer from {url}"
    );
    Ok(contents)
}

fn standalone_source(manager: PackageManager) -> Result<(&'static str, &'static str)> {
    match (manager, cfg!(windows)) {
        (PackageManager::Pnpm, true) => Ok(("https://get.pnpm.io/install.ps1", "PNPM_HOME")),
        (PackageManager::Pnpm, false) => Ok(("https://get.pnpm.io/install.sh", "PNPM_HOME")),
        (PackageManager::Vp, true) => Ok(("https://vite.plus/ps1", "VP_HOME")),
        (PackageManager::Vp, false) => Ok(("https://vite.plus", "VP_HOME")),
        (PackageManager::Bun, true) => Ok(("https://bun.sh/install.ps1", "BUN_INSTALL")),
        (PackageManager::Bun, false) => Ok(("https://bun.sh/install", "BUN_INSTALL")),
        _ => bail!("{} has no standalone native installer", manager.name()),
    }
}

pub(super) async fn install_standalone(
    http: &dyn HttpClient,
    node: &NodeRuntime,
    manager: PackageManager,
    directory: &Path,
    environment: &HashMap<String, String>,
) -> Result<Vec<PathBuf>> {
    if matches!(manager, PackageManager::Npm | PackageManager::Yarn) {
        return install_npm_distribution(http, node, manager, directory, environment)
            .await
            .map(|directory| vec![directory]);
    }
    let (url, variable) = standalone_source(manager)?;
    let home = environment
        .get(variable)
        .map(PathBuf::from)
        .unwrap_or_else(|| match manager {
            PackageManager::Pnpm => {
                if cfg!(windows) {
                    env::var_os("LOCALAPPDATA")
                        .map(PathBuf::from)
                        .unwrap_or_else(|| paths::home_dir().join("AppData/Local"))
                        .join("pnpm")
                } else {
                    paths::home_dir().join(".local/share/pnpm")
                }
            }
            PackageManager::Bun => paths::home_dir().join(".bun"),
            _ => paths::home_dir().join(".vite-plus"),
        });
    let home = std::path::absolute(home)?;
    let temporary = tempfile::tempdir()?;
    let script = temporary.path().join(if cfg!(windows) {
        "install.ps1"
    } else {
        "install.sh"
    });
    let contents = download(http, url).await?;
    fs::write(&script, &contents).await?;
    let shell = if cfg!(windows) {
        env::var_os("SYSTEMROOT")
            .map(PathBuf::from)
            .map(|root| root.join("System32/WindowsPowerShell/v1.0/powershell.exe"))
            .filter(|path| path.is_file())
            .or_else(|| which::which("powershell.exe").ok())
            .context("PowerShell is required by the official installer")?
    } else if manager == PackageManager::Pnpm {
        PathBuf::from("/bin/sh")
    } else {
        which::which("bash").context("Bash is required by the official installer")?
    };
    let mut command = util::command::new_command(shell);
    if cfg!(windows) {
        command.args(["-NoLogo", "-NoProfile", "-NonInteractive", "-Command"])
            .arg("$ErrorActionPreference = 'Stop'; & ([scriptblock]::Create([IO.File]::ReadAllText($env:ZED_PACKAGE_MANAGER_INSTALLER))); if ($LASTEXITCODE) { exit $LASTEXITCODE }");
    } else {
        command.arg(&script);
    }
    command
        .current_dir(directory)
        .envs(environment)
        .env(variable, &home);
    if cfg!(windows) {
        command.env("ZED_PACKAGE_MANAGER_INSTALLER", script);
    }
    command_output(command).await?;
    Ok(vec![home.join("bin"), home])
}

async fn install_npm_distribution(
    http: &dyn HttpClient,
    node: &NodeRuntime,
    manager: PackageManager,
    directory: &Path,
    environment: &HashMap<String, String>,
) -> Result<PathBuf> {
    let node = node
        .binary_path()
        .await
        .context("npm and Yarn require an existing supported Node.js installation")?;
    let metadata: serde_json::Value =
        serde_json::from_slice(&download(http, "https://registry.npmjs.org/npm/latest").await?)?;
    let archive_url = metadata
        .pointer("/dist/tarball")
        .and_then(serde_json::Value::as_str)
        .context("npm metadata contains no archive")?;
    ensure!(
        archive_url.starts_with("https://registry.npmjs.org/npm/-/"),
        "npm returned an unexpected archive URL"
    );
    let temporary = tempfile::tempdir()?;
    let archive = temporary.path().join("npm.tgz");
    fs::write(&archive, download(http, archive_url).await?).await?;
    let mut command = util::command::new_command("tar");
    command
        .arg("-xzf")
        .arg(archive)
        .arg("-C")
        .arg(temporary.path());
    command_output(command).await?;
    let prefix = environment
        .get("NPM_CONFIG_PREFIX")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            if cfg!(windows) {
                env::var_os("APPDATA")
                    .map(PathBuf::from)
                    .unwrap_or_else(|| paths::home_dir().join("AppData/Roaming"))
                    .join("npm")
            } else {
                paths::home_dir().join(".local")
            }
        });
    let prefix = std::path::absolute(prefix)?;
    let mut command = util::command::new_command(node);
    command
        .arg(temporary.path().join("package/bin/npm-cli.js"))
        .args(["install", "--global", npm_package(manager)?, "--prefix"])
        .arg(&prefix)
        .current_dir(directory)
        .envs(environment);
    command_output(command).await?;
    Ok(if cfg!(windows) {
        prefix
    } else {
        prefix.join("bin")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn arguments(installer: PackageManager, major_version: u64) -> Result<Vec<String>> {
        Ok(
            global_install_arguments(installer, major_version, "pkg", Path::new("/prefix"))?
                .into_iter()
                .map(|argument| argument.to_string_lossy().into_owned())
                .collect(),
        )
    }

    #[test]
    fn global_installs_use_each_managers_own_command() -> Result<()> {
        assert_eq!(
            arguments(PackageManager::Pnpm, 10)?,
            ["add", "--global", "pkg"]
        );
        assert_eq!(
            arguments(PackageManager::Bun, 1)?,
            ["add", "--global", "pkg"]
        );
        assert_eq!(
            arguments(PackageManager::Vp, 1)?,
            ["install", "--global", "pkg"]
        );
        assert_eq!(
            arguments(PackageManager::Yarn, 1)?,
            ["global", "add", "pkg"]
        );
        assert_eq!(
            arguments(PackageManager::Npm, 10)?,
            ["install", "--global", "pkg", "--prefix", "/prefix"]
        );
        assert_eq!(
            arguments(PackageManager::Yarn, 4)?,
            [
                "dlx",
                "--package",
                "yarn@1",
                "yarn",
                "global",
                "add",
                "pkg",
                "--prefix",
                "/prefix"
            ],
            "modern Yarn installs through its Classic CLI into the given prefix"
        );
        assert!(arguments(PackageManager::Auto, 0).is_err());
        Ok(())
    }

    #[test]
    fn only_managers_that_report_their_global_directory_are_asked_for_it() {
        assert_eq!(
            global_bin_arguments(PackageManager::Npm, 10),
            ["prefix", "--global"]
        );
        assert_eq!(
            global_bin_arguments(PackageManager::Pnpm, 10),
            ["bin", "--global"]
        );
        assert_eq!(
            global_bin_arguments(PackageManager::Bun, 1),
            ["pm", "bin", "--global"]
        );
        assert_eq!(
            global_bin_arguments(PackageManager::Yarn, 1),
            ["global", "bin"]
        );
        assert!(global_bin_arguments(PackageManager::Yarn, 4).is_empty());
        assert!(global_bin_arguments(PackageManager::Vp, 1).is_empty());
    }

    #[test]
    fn standalone_installers_exist_only_for_native_managers() -> Result<()> {
        for manager in [
            PackageManager::Pnpm,
            PackageManager::Vp,
            PackageManager::Bun,
        ] {
            assert!(standalone_source(manager)?.0.starts_with("https://"));
        }
        for manager in [
            PackageManager::Npm,
            PackageManager::Yarn,
            PackageManager::Auto,
        ] {
            assert!(standalone_source(manager).is_err());
        }
        Ok(())
    }
}
