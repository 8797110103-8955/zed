//! Real-process checks for how `NodeRuntime` reports and recovers from Node.js availability.
//!
//! Each test re-runs itself in a child process so the data directory, working directory, and
//! inherited `PATH` it sets up stay isolated from the other tests.

use anyhow::{Context as _, Result, ensure};
use node_runtime::{NodeBinaryOptions, NodeRuntime, PackageManager, PackageManagerRuntime};
use std::{collections::HashMap, env, fs, path::Path, sync::Arc};

const CHILD: &str = "ZED_TEST_NODE_RUNTIME_CHILD";

#[test]
fn path_lookup_finds_system_node_without_npm() -> Result<()> {
    let executable_name = if cfg!(windows) { "node.cmd" } else { "node" };
    if env::var_os(CHILD).is_some() {
        let directory = env::current_dir()?;
        paths::set_custom_data_dir(&directory.join("data").to_string_lossy());
        return smol::block_on(async {
            let runtime = NodeRuntime::new(
                None,
                watch::channel(Some(NodeBinaryOptions {
                    allow_path_lookup: true,
                }))
                .1,
            );
            let project_runtime = PackageManagerRuntime::new(
                runtime.clone(),
                Arc::new(http_client::BlockedHttpClient),
                watch::channel(Some(PackageManager::Auto)).1,
            );
            let alternate_directory = directory.join("alternate-node");
            fs::create_dir(&alternate_directory)?;
            fs::write(
                alternate_directory.join(executable_name),
                "project-local Node must not be executed",
            )?;
            let project_file = directory.join("example.json");
            fs::write(&project_file, "{}")?;
            for search_path in [
                String::new(),
                alternate_directory.to_string_lossy().into_owned(),
            ] {
                let project_runtime = project_runtime.with_environment(HashMap::from([
                    ("PATH".into(), search_path),
                    ("NODE_OPTIONS".into(), "--invalid-project-option".into()),
                ]));
                for project_root in [&directory, &project_file] {
                    assert_eq!(
                        project_runtime
                            .for_project(project_root)
                            .binary_path()
                            .await?,
                        directory.join(executable_name),
                        "project environment must not change Node.js for {}",
                        project_root.display()
                    );
                }
            }
            assert_eq!(
                runtime.binary_path().await?,
                directory.join(executable_name)
            );
            assert!(runtime.errors().borrow().is_none());
            Ok(())
        });
    }

    let directory = tempfile::tempdir()?;
    let executable = directory.path().join(executable_name);
    if cfg!(windows) {
        fs::write(&executable, "@echo off\r\necho v22.0.0\r\n")?;
    } else {
        fs::write(&executable, "#!/bin/sh\nprintf 'v22.0.0\\n'\n")?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&executable, fs::Permissions::from_mode(0o755))?;
        }
    }
    run_child_test(
        "path_lookup_finds_system_node_without_npm",
        directory.path(),
    )
}

#[test]
fn missing_node_is_reported_without_download_and_can_be_retried() -> Result<()> {
    if env::var_os(CHILD).is_some() {
        return smol::block_on(async {
            let (mut options, receiver) = watch::channel(Some(NodeBinaryOptions {
                allow_path_lookup: true,
            }));
            let runtime = NodeRuntime::new(None, receiver);
            let mut errors = runtime.errors();
            assert!(errors.borrow().is_none());

            let error = runtime
                .binary_path()
                .await
                .err()
                .context("missing Node.js must fail")?;
            assert!(error.to_string().contains("Install it manually"));
            assert!(error.to_string().contains("https://nodejs.org/en/download"));
            assert_eq!(errors.borrow().as_deref(), Some(&error.to_string()));

            assert!(runtime.binary_path().await.is_err());
            assert!(smol::future::poll_once(errors.changed()).await.is_none());

            options.send(Some(NodeBinaryOptions::default()))?;
            let error = runtime
                .binary_path()
                .await
                .err()
                .context("disabled Node.js must fail")?;
            assert!(error.to_string().contains("https://nodejs.org/en/download"));
            errors.changed().await?;
            assert_eq!(errors.borrow().as_deref(), Some(&error.to_string()));
            Ok(())
        });
    }

    let directory = tempfile::tempdir()?;
    run_child_test(
        "missing_node_is_reported_without_download_and_can_be_retried",
        directory.path(),
    )
}

/// Runs `test` alone with `directory` as both its working directory and its only `PATH` entry.
fn run_child_test(test: &str, directory: &Path) -> Result<()> {
    let output = smol::block_on(
        util::command::new_command(env::current_exe()?)
            .args(["--exact", test, "--nocapture"])
            .env(CHILD, "1")
            .env("PATH", directory)
            .env("PATHEXT", ".CMD;.EXE")
            .current_dir(directory)
            .output(),
    )?;
    ensure!(
        output.status.success(),
        "{test} failed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(())
}
