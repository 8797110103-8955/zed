use super::PackageManager;
use anyhow::{Context as _, Result};
use serde_json::Value;
use smol::{fs, lock::Mutex};
use std::{
    collections::HashMap,
    future::Future,
    io,
    path::{Path, PathBuf},
    sync::LazyLock,
    time::SystemTime,
};

static PROJECT_CACHE: LazyLock<Mutex<HashMap<PathBuf, CachedProjectPackageManager>>> =
    LazyLock::new(Mutex::default);

#[derive(Clone)]
struct CachedProjectPackageManager {
    manager: PackageManager,
    sources: Vec<PathBuf>,
    files: Vec<(PathBuf, Option<PackageManagerFileStamp>)>,
}

#[derive(Clone, PartialEq, Eq)]
pub(super) struct PackageManagerFileStamp {
    modified: SystemTime,
    length: u64,
    pub(super) is_directory: bool,
}

pub(super) async fn package_manager_file_stamp(
    path: &Path,
) -> Result<Option<PackageManagerFileStamp>> {
    match fs::metadata(path).await {
        Ok(metadata) => Ok(Some(PackageManagerFileStamp {
            modified: if metadata.is_dir() {
                SystemTime::UNIX_EPOCH
            } else {
                metadata.modified()?
            },
            length: if metadata.is_dir() { 0 } else { metadata.len() },
            is_directory: metadata.is_dir(),
        })),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("reading {}", path.display())),
    }
}

pub async fn detect_package_manager<ReadFile, ReadFuture, IsFile, IsFileFuture>(
    directory: &Path,
    read_file: ReadFile,
    is_file: IsFile,
) -> Result<PackageManager>
where
    ReadFile: Fn(std::path::PathBuf) -> ReadFuture,
    ReadFuture: Future<Output = Result<Option<String>>>,
    IsFile: Fn(std::path::PathBuf) -> IsFileFuture,
    IsFileFuture: Future<Output = bool>,
{
    for directory in directory.ancestors() {
        let manifest_path = directory.join("package.json");
        if let Some(contents) = read_file(manifest_path.clone()).await? {
            let package: Value = serde_json::from_str(&contents)
                .with_context(|| format!("reading {}", manifest_path.display()))?;
            if let Some(manager) = PackageManager::from_package_json(&package) {
                return Ok(manager);
            }
        }
        for (name, manager) in [
            ("pnpm-workspace.yaml", PackageManager::Pnpm),
            ("pnpm-lock.yaml", PackageManager::Pnpm),
            (".pnpmfile.cjs", PackageManager::Pnpm),
            ("bun.lock", PackageManager::Bun),
            ("bun.lockb", PackageManager::Bun),
            ("bunfig.toml", PackageManager::Bun),
            ("package-lock.json", PackageManager::Npm),
            ("npm-shrinkwrap.json", PackageManager::Npm),
            ("yarn.lock", PackageManager::Yarn),
            (".yarnrc.yml", PackageManager::Yarn),
        ] {
            if is_file(directory.join(name)).await {
                return Ok(manager);
            }
        }
        if is_file(directory.join(".git")).await {
            break;
        }
    }
    // Keep cached project hints independent of PATH so installed defaults can change.
    Ok(PackageManager::Auto)
}

pub async fn detect_package_manager_on_disk(directory: &Path) -> Result<PackageManager> {
    cached_project_package_manager(directory, &[]).await
}

pub(super) async fn cached_project_package_manager(
    directory: &Path,
    sources: &[PathBuf],
) -> Result<PackageManager> {
    let directory = if smol::fs::metadata(directory)
        .await
        .with_context(|| format!("reading project directory {}", directory.display()))?
        .is_file()
    {
        directory
            .parent()
            .context("project file has no parent directory")?
    } else {
        directory
    };
    // The global cache lock is held only for lookups and updates, not file
    // system scans, so detection for unrelated projects can run concurrently.
    let previous = PROJECT_CACHE
        .lock()
        .await
        .get(directory)
        .filter(|previous| previous.sources == sources)
        .cloned();
    if let Some(previous) = previous {
        let mut unchanged = true;
        for (path, stamp) in &previous.files {
            if package_manager_file_stamp(path).await?.as_ref() != stamp.as_ref() {
                unchanged = false;
                break;
            }
        }
        if unchanged {
            return Ok(previous.manager);
        }
    }

    let mut files = Vec::new();
    for directory in directory.ancestors() {
        for name in [
            "package.json",
            "pnpm-workspace.yaml",
            "pnpm-lock.yaml",
            ".pnpmfile.cjs",
            "bun.lock",
            "bun.lockb",
            "bunfig.toml",
            "package-lock.json",
            "npm-shrinkwrap.json",
            "yarn.lock",
            ".yarnrc.yml",
            ".vscode/settings.json",
            ".vscode/tasks.json",
            ".vscode/launch.json",
            ".zed/settings.json",
            ".git",
        ] {
            let path = directory.join(name);
            let stamp = package_manager_file_stamp(&path).await?;
            files.push((path, stamp));
        }
        if fs::metadata(directory.join(".git")).await.is_ok() {
            break;
        }
    }
    for path in sources {
        files.push((path.clone(), package_manager_file_stamp(path).await?));
    }
    let manager = detect_package_manager(
        directory,
        |path| async move {
            match smol::fs::read_to_string(&path).await {
                Ok(contents) => Ok(Some(contents)),
                Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
                Err(error) => Err(error).with_context(|| format!("reading {}", path.display())),
            }
        },
        |path| async move { smol::fs::metadata(path).await.is_ok() },
    )
    .await?;
    let mut unchanged = true;
    for (path, stamp) in &files {
        if package_manager_file_stamp(path).await?.as_ref() != stamp.as_ref() {
            unchanged = false;
            break;
        }
    }
    // Corepack shims report the version declared by the project, so a changed declaration
    // invalidates the probes of executables found from this directory.
    super::executable::forget_directory(directory).await;
    if unchanged {
        PROJECT_CACHE.lock().await.insert(
            directory.to_path_buf(),
            CachedProjectPackageManager {
                manager,
                sources: sources.to_vec(),
                files,
            },
        );
    }
    Ok(manager)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_workspace_managers_and_stops_at_repository_boundaries() -> Result<()> {
        smol::block_on(async {
            let directory = tempfile::tempdir()?;
            let root = directory.path().join("workspace");
            let nested = root.join("packages/app/src");
            std::fs::create_dir_all(&nested)?;
            std::fs::create_dir(root.join(".git"))?;
            std::fs::write(
                directory.path().join("package.json"),
                r#"{"packageManager":"yarn@4"}"#,
            )?;
            assert_eq!(
                detect_package_manager_on_disk(&nested).await?,
                PackageManager::Auto
            );
            std::fs::write(root.join("pnpm-workspace.yaml"), "packages: [packages/*]")?;
            assert_eq!(
                detect_package_manager_on_disk(&nested).await?,
                PackageManager::Pnpm
            );
            std::fs::write(root.join("package.json"), r#"{"packageManager":"bun@1"}"#)?;
            assert_eq!(
                detect_package_manager_on_disk(&nested).await?,
                PackageManager::Bun
            );
            std::fs::write(
                root.join("packages/app/package.json"),
                r#"{"devEngines":{"packageManager":[{"name":"unknown"},{"name":"yarn"}]}}"#,
            )?;
            assert_eq!(
                detect_package_manager_on_disk(&nested).await?,
                PackageManager::Bun
            );
            std::fs::write(
                root.join("packages/app/package.json"),
                r#"{"packageManager":"yarn@4"}"#,
            )?;
            assert_eq!(
                detect_package_manager_on_disk(&nested).await?,
                PackageManager::Yarn
            );
            Ok(())
        })
    }

    #[test]
    fn recognizes_only_declared_package_managers() {
        for manager in PackageManager::INSTALLABLE {
            let package = serde_json::json!({"packageManager": format!("{}@1", manager.name()), "dependencies": {"vite-plus": "1"}});
            assert_eq!(PackageManager::from_package_json(&package), Some(manager));
        }
        assert_eq!(
            PackageManager::from_package_json(
                &serde_json::json!({"devDependencies":{"vite-plus":"1"}, "devEngines":{"packageManager":{"name":"bun"}}})
            ),
            None
        );
    }

    #[test]
    fn refreshes_declarations_and_auxiliary_files_without_vite_inference() -> Result<()> {
        smol::block_on(async {
            let directory = tempfile::tempdir()?;
            std::fs::create_dir(directory.path().join(".git"))?;
            for name in ["vite.config.ts", "vite.config.js"] {
                std::fs::write(
                    directory.path().join(name),
                    "import { defineConfig } from 'vite-plus';",
                )?;
            }
            let manifest = directory.path().join("package.json");
            std::fs::write(
                &manifest,
                r#"{"dependencies":{"vite-plus":"1"},"devEngines":{"packageManager":{"name":"bun"}}}"#,
            )?;
            assert_eq!(
                detect_package_manager_on_disk(directory.path()).await?,
                PackageManager::Auto
            );
            for manager in PackageManager::INSTALLABLE {
                std::fs::write(
                    &manifest,
                    serde_json::to_vec(
                        &serde_json::json!({"packageManager":format!("{}@1", manager.name())}),
                    )?,
                )?;
                assert_eq!(
                    detect_package_manager_on_disk(directory.path()).await?,
                    manager
                );
            }
            std::fs::remove_file(manifest)?;
            assert_eq!(
                detect_package_manager_on_disk(directory.path()).await?,
                PackageManager::Auto
            );
            let lockfile = directory.path().join("bun.lock");
            std::fs::write(&lockfile, "")?;
            assert_eq!(
                detect_package_manager_on_disk(directory.path()).await?,
                PackageManager::Bun
            );
            std::fs::write(
                directory.path().join("package.json"),
                r#"{"packageManager":"npm@10"}"#,
            )?;
            assert_eq!(
                detect_package_manager_on_disk(directory.path()).await?,
                PackageManager::Npm
            );
            std::fs::remove_file(directory.path().join("package.json"))?;
            std::fs::remove_file(lockfile)?;
            assert_eq!(
                detect_package_manager_on_disk(directory.path()).await?,
                PackageManager::Auto
            );
            Ok(())
        })
    }

    #[test]
    fn detects_auxiliary_package_manager_files() -> Result<()> {
        smol::block_on(async {
            for (name, expected) in [
                ("pnpm-workspace.yaml", PackageManager::Pnpm),
                ("pnpm-lock.yaml", PackageManager::Pnpm),
                (".pnpmfile.cjs", PackageManager::Pnpm),
                ("bun.lock", PackageManager::Bun),
                ("bun.lockb", PackageManager::Bun),
                ("bunfig.toml", PackageManager::Bun),
                ("package-lock.json", PackageManager::Npm),
                ("npm-shrinkwrap.json", PackageManager::Npm),
                ("yarn.lock", PackageManager::Yarn),
                (".yarnrc.yml", PackageManager::Yarn),
            ] {
                let directory = tempfile::tempdir()?;
                std::fs::create_dir(directory.path().join(".git"))?;
                std::fs::write(directory.path().join(name), "")?;
                assert_eq!(
                    detect_package_manager_on_disk(directory.path()).await?,
                    expected
                );
            }
            Ok(())
        })
    }

    #[test]
    fn reports_invalid_project_configuration() -> Result<()> {
        smol::block_on(async {
            let directory = tempfile::tempdir()?;
            std::fs::write(directory.path().join("package.json"), "{")?;
            let error = detect_package_manager_on_disk(directory.path())
                .await
                .expect_err("invalid manifest must be reported");
            assert!(error.to_string().contains("package.json"));
            Ok(())
        })
    }

    #[test]
    fn detects_manager_when_the_worktree_root_is_a_file() -> Result<()> {
        smol::block_on(async {
            let directory = tempfile::tempdir()?;
            std::fs::create_dir(directory.path().join(".git"))?;
            std::fs::write(directory.path().join("pnpm-lock.yaml"), "")?;
            let file = directory.path().join("example.ts");
            std::fs::write(&file, "")?;
            assert_eq!(
                detect_package_manager_on_disk(&file).await?,
                PackageManager::Pnpm
            );
            Ok(())
        })
    }
}
