use super::{
    PackageManager,
    detection::{PackageManagerFileStamp, package_manager_file_stamp},
    environment::path_variable,
};
use anyhow::Result;
use smol::lock::Mutex;
use std::{
    collections::HashMap,
    env,
    ffi::OsString,
    path::{Path, PathBuf},
    sync::LazyLock,
};

type ExecutableKey = (PackageManager, PathBuf, OsString);

static EXECUTABLE_CACHE: LazyLock<Mutex<HashMap<ExecutableKey, CachedPackageManagerExecutable>>> =
    LazyLock::new(Mutex::default);

struct CachedPackageManagerExecutable {
    executable: AvailablePackageManager,
    stamp: PackageManagerFileStamp,
}

pub(super) async fn forget_directory(directory: &Path) {
    EXECUTABLE_CACHE
        .lock()
        .await
        .retain(|(_, cached_directory, _), _| cached_directory != directory);
}

#[derive(Clone)]
pub(super) struct AvailablePackageManager {
    pub(super) manager: PackageManager,
    pub(super) path: PathBuf,
    pub(super) major_version: u64,
}

#[derive(Debug, thiserror::Error)]
#[error(
    "No usable package manager was found (preferred: {preferred}). Install it manually and retry: https://pnpm.io/installation. {details}"
)]
pub(super) struct PackageManagerUnavailable {
    preferred: &'static str,
    details: String,
}

pub(super) fn append_package_manager_fallbacks(candidates: &mut Vec<PackageManager>) {
    for manager in PackageManager::INSTALLABLE {
        if !candidates.contains(&manager) {
            candidates.push(manager);
        }
    }
}

pub(super) async fn find_package_manager(
    candidates: &[PackageManager],
    directory: &Path,
    command_env: &HashMap<String, String>,
) -> Result<AvailablePackageManager> {
    let search_path = path_variable(command_env)
        .map(OsString::from)
        .or_else(|| env::var_os("PATH"));
    let mut failures = Vec::new();
    for &manager in candidates {
        let executable = manager.name();
        let path = smol::unblock({
            let directory = directory.to_path_buf();
            let search_path = search_path.clone();
            move || -> Result<PathBuf> {
                let path = which::which_in(executable, search_path, &directory)?;
                Ok(std::path::absolute(directory.join(path))?)
            }
        })
        .await;
        let Ok(path) = path else { continue };
        let Some(stamp) = package_manager_file_stamp(&path).await? else {
            continue;
        };
        let cache_key = (
            manager,
            directory.to_path_buf(),
            search_path.clone().unwrap_or_default(),
        );
        {
            let cache = EXECUTABLE_CACHE.lock().await;
            if let Some(previous) = cache.get(&cache_key)
                && previous.executable.path == path
                && previous.stamp == stamp
            {
                return Ok(previous.executable.clone());
            }
        }
        let mut command = util::command::new_command(&path);
        command
            .arg("--version")
            .envs(command_env)
            .kill_on_drop(true)
            .current_dir(directory);
        if manager == PackageManager::Vp {
            // Recent Vite+ releases otherwise install their runtime even for --version.
            command.env("VP_SELF_SETUP_SUPPORT_CHECK", "1");
        }
        // node_runtime has no GPUI executor, and this only bounds a real child process.
        #[allow(clippy::disallowed_methods)]
        let timeout = smol::Timer::after(std::time::Duration::from_secs(5));
        let output = futures::future::select(Box::pin(command.output()), Box::pin(timeout)).await;
        match output {
            futures::future::Either::Left((Ok(output), _)) if output.status.success() => {
                let major_version = if manager == PackageManager::Vp
                    && output.stdout.trim_ascii() == b"vite-plus-self-setup-v1"
                {
                    Some(0)
                } else {
                    String::from_utf8_lossy(&output.stdout)
                        .split_whitespace()
                        .find_map(|value| {
                            semver::Version::parse(value.trim_start_matches('v')).ok()
                        })
                        .map(|version| version.major)
                };
                if let Some(major_version) = major_version {
                    let executable = AvailablePackageManager {
                        manager,
                        path,
                        major_version,
                    };
                    EXECUTABLE_CACHE.lock().await.insert(
                        cache_key,
                        CachedPackageManagerExecutable {
                            executable: executable.clone(),
                            stamp,
                        },
                    );
                    return Ok(executable);
                }
                failures.push(format!("{} returned an invalid version", manager.name()));
            }
            futures::future::Either::Left((Ok(output), _)) => failures.push(format!(
                "{}: {}",
                manager.name(),
                String::from_utf8_lossy(&output.stderr).trim()
            )),
            futures::future::Either::Left((Err(error), _)) => {
                failures.push(format!("{}: {error}", manager.name()))
            }
            futures::future::Either::Right(_) => {
                failures.push(format!("{} did not respond", manager.name()))
            }
        }
    }
    Err(PackageManagerUnavailable {
        preferred: candidates
            .first()
            .copied()
            .unwrap_or(PackageManager::Pnpm)
            .name(),
        details: failures.join("; "),
    }
    .into())
}
