use super::environment::{
    path_variable, project_tool_directories, project_tool_environment, without_corepack_downloads,
};
use anyhow::{Context as _, Result, bail};
use futures::AsyncReadExt as _;
use semver::Version;
use serde::Deserialize;
use smol::fs;
use std::{
    collections::HashMap,
    io,
    path::{Path, PathBuf},
};

pub async fn read_package_installed_version(
    node_module_directory: PathBuf,
    name: &str,
) -> Result<Option<Version>> {
    let package_json_path = node_module_directory.join(name).join("package.json");

    let mut file = match fs::File::open(package_json_path).await {
        Ok(file) => file,
        Err(err) => {
            if err.kind() == io::ErrorKind::NotFound {
                return Ok(None);
            }

            Err(err)?
        }
    };

    #[derive(Deserialize)]
    struct PackageJson {
        version: Version,
    }

    let mut contents = String::new();
    file.read_to_string(&mut contents).await?;
    let package_json: PackageJson = serde_json::from_str(&contents)?;
    Ok(Some(package_json.version))
}

pub async fn read_package_executable(
    node_module_directory: PathBuf,
    name: &str,
) -> Result<PathBuf> {
    let package_directory = node_module_directory.join(name);

    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Bin {
        Path(String),
        Named(HashMap<String, String>),
    }

    #[derive(Deserialize)]
    struct PackageJson {
        bin: Option<Bin>,
    }

    let package_json_path = package_directory.join("package.json");
    let mut file = fs::File::open(&package_json_path)
        .await
        .with_context(|| format!("opening {}", package_json_path.display()))?;
    let mut contents = String::new();
    file.read_to_string(&mut contents).await?;
    let package_json: PackageJson = serde_json::from_str(&contents)
        .with_context(|| format!("parsing {}", package_json_path.display()))?;

    let relative_path = match package_json.bin {
        Some(Bin::Path(path)) => path,
        Some(Bin::Named(bins)) => {
            let unscoped_name = name.rsplit('/').next().unwrap_or(name);
            let path = if bins.len() == 1 {
                bins.values().next()
            } else {
                bins.get(unscoped_name)
            };
            path.with_context(|| {
                format!("package {name} declares no executable named {unscoped_name}")
            })?
            .clone()
        }
        None => bail!("package {name} declares no executable"),
    };

    Ok(package_directory.join(relative_path))
}

pub async fn find_installed_package(
    directory: &Path,
    package_name: &str,
    environment: HashMap<String, String>,
) -> Result<Option<(PathBuf, Version)>> {
    let directories = project_tool_directories(directory).await?;
    for bin_directory in &directories {
        let node_modules = bin_directory
            .parent()
            .context("tool directory has no parent")?;
        if let Some(version) =
            read_package_installed_version(node_modules.to_path_buf(), package_name).await?
        {
            return Ok(Some((node_modules.to_path_buf(), version)));
        }
    }
    let environment = project_tool_environment(directory, environment).await?;
    let current_directory = directories
        .first()
        .and_then(|path| path.parent())
        .and_then(|path| path.parent())
        .context("project tool directory is missing")?;
    let npm = smol::unblock({
        let path = path_variable(&environment).map(str::to_owned);
        let directory = current_directory.to_path_buf();
        move || which::which_in("npm", path, directory).ok()
    })
    .await;
    let Some(npm) = npm else { return Ok(None) };
    let output = util::command::new_command(npm)
        .args(["root", "-g"])
        .envs(without_corepack_downloads(environment))
        .current_dir(current_directory)
        .output()
        .await?;
    // A broken global npm setup must not block callers from falling back to
    // downloading the package, so it is treated as "not installed".
    if !output.status.success() {
        log::warn!(
            "reading global npm tools failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        return Ok(None);
    }
    let node_modules = PathBuf::from(String::from_utf8_lossy(&output.stdout).trim());
    if !node_modules.is_absolute() {
        log::warn!(
            "npm returned a relative global package directory: {}",
            node_modules.display()
        );
        return Ok(None);
    }
    Ok(
        read_package_installed_version(node_modules.clone(), package_name)
            .await?
            .map(|version| (node_modules, version)),
    )
}
