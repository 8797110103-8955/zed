//! Real-process checks for package manager selection, installation, and tool commands.
//!
//! These run fake package manager executables through the real command implementation. Each
//! test re-runs itself in a child process with its own data directory, working directory, and
//! `PATH`, because the data directory is process-global and holds the registry of installed
//! package managers. Argument translation, project detection, and registry parsing are covered
//! by the crate's unit tests.

use anyhow::{Context as _, Result, ensure};
use futures::StreamExt as _;
use node_runtime::{
    NodeBinaryOptions, NodeRuntime, PackageManager, PackageManagerInstallEvent,
    PackageManagerRuntime, find_installed_package, project_tool_environment,
};
use semver::Version;
use std::{
    collections::HashMap,
    env, fs,
    future::Future,
    path::{Path, PathBuf},
    process::Output,
    sync::Arc,
};

const CHILD: &str = "ZED_TEST_PACKAGE_MANAGER_CHILD";

/// Runs `body` in a child process whose working directory, data directory, and `PATH` are a
/// fresh temporary directory. With `inherit_path` the child keeps the real `PATH` instead.
fn isolated<Body, BodyFuture>(test: &str, inherit_path: bool, body: Body) -> Result<()>
where
    Body: FnOnce(PathBuf) -> BodyFuture,
    BodyFuture: Future<Output = Result<()>>,
{
    if env::var_os(CHILD).is_some() {
        let directory = env::current_dir()?;
        paths::set_custom_data_dir(&directory.join("data").to_string_lossy());
        return smol::block_on(body(directory));
    }
    let directory = tempfile::tempdir()?;
    let mut command = util::command::new_command(env::current_exe()?);
    command
        .args(["--exact", test, "--include-ignored", "--nocapture"])
        .env(CHILD, "1")
        .current_dir(directory.path());
    if !inherit_path {
        command
            .env("PATH", directory.path())
            .env("PATHEXT", ".CMD;.EXE");
    }
    let output = smol::block_on(command.output())?;
    ensure!(
        output.status.success(),
        "{test} failed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(())
}

#[derive(Clone, Copy)]
enum Fake {
    Usable,
    /// Fails even to report its version, like a broken installation.
    Unusable,
    /// Reports a version but fails every operation.
    FailingCommands,
}

/// A fake manager that reports version 4.1.0 and prints `<name> <arguments>` followed by
/// `env=$ZED_PROJECT_TOOL_TEST` for any other invocation.
fn fake_manager(directory: &Path, manager: PackageManager, fake: Fake) -> Result<PathBuf> {
    let (version_exit, command_exit) = match fake {
        Fake::Usable => (0, 0),
        Fake::Unusable => (1, 1),
        Fake::FailingCommands => (0, 1),
    };
    let name = manager.name();
    let source = if cfg!(windows) {
        format!(
            "@echo off\r\nif \"%1\"==\"--version\" (echo 4.1.0& exit /b {version_exit})\r\necho {name} %*\r\necho env=%ZED_PROJECT_TOOL_TEST%\r\nexit /b {command_exit}\r\n"
        )
    } else {
        format!(
            "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then echo 4.1.0; exit {version_exit}; fi\necho {name} \"$@\"\necho \"env=$ZED_PROJECT_TOOL_TEST\"\nexit {command_exit}\n"
        )
    };
    write_executable(directory, name, &source)
}

fn fake_node(directory: &Path) -> Result<PathBuf> {
    let source = if cfg!(windows) {
        "@echo off\r\necho v26.13.0\r\n"
    } else {
        "#!/bin/sh\necho v26.13.0\n"
    };
    write_executable(directory, "node", source)
}

fn write_executable(directory: &Path, name: &str, source: &str) -> Result<PathBuf> {
    fs::create_dir_all(directory)?;
    let path = directory.join(if cfg!(windows) {
        format!("{name}.cmd")
    } else {
        name.to_string()
    });
    fs::write(&path, source)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755))?;
    }
    Ok(path)
}

fn executable_name(name: &str) -> String {
    if cfg!(windows) {
        format!("{name}.cmd")
    } else {
        name.to_string()
    }
}

fn path_environment(directory: &Path) -> HashMap<String, String> {
    HashMap::from([("PATH".into(), directory.to_string_lossy().into_owned())])
}

fn runtime(
    preference: PackageManager,
) -> (watch::Sender<Option<PackageManager>>, PackageManagerRuntime) {
    let (sender, preference) = watch::channel(Some(preference));
    let runtime = PackageManagerRuntime::new(
        NodeRuntime::unavailable(),
        Arc::new(http_client::BlockedHttpClient),
        preference,
    );
    (sender, runtime)
}

fn repository(path: &Path) -> Result<PathBuf> {
    fs::create_dir_all(path.join(".git"))?;
    Ok(path.to_path_buf())
}

/// The manager a fake executable reported running as.
fn manager_used(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout)
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .to_string()
}

fn run_task(
    runtime: &PackageManagerRuntime,
    project: &Path,
    environment: HashMap<String, String>,
    preference: PackageManager,
) -> impl Future<Output = Result<node_runtime::PackageManagerInvocation>> {
    let runtime = runtime.clone();
    let project = project.to_path_buf();
    async move {
        runtime
            .task_command(
                &project,
                &["run".into(), "test".into()],
                environment,
                preference,
                &[],
            )
            .await
    }
}

#[test]
fn undeclared_tasks_use_the_first_installed_manager_in_priority_order() -> Result<()> {
    isolated(
        "undeclared_tasks_use_the_first_installed_manager_in_priority_order",
        false,
        |root| async move {
            let system = root.join("system");
            let project = repository(&root.join("project"))?;
            for manager in PackageManager::INSTALLABLE {
                fake_manager(&system, manager, Fake::Usable)?;
            }
            let (_, runtime) = runtime(PackageManager::Auto);
            for expected in PackageManager::INSTALLABLE {
                let task = run_task(
                    &runtime,
                    &project,
                    path_environment(&system),
                    PackageManager::Auto,
                )
                .await?;
                assert_eq!(task.package_manager, expected);
                fs::remove_file(system.join(executable_name(expected.name())))?;
            }
            Ok(())
        },
    )
}

#[test]
fn tools_fall_back_from_an_unavailable_manager_when_nothing_may_be_installed() -> Result<()> {
    isolated(
        "tools_fall_back_from_an_unavailable_manager_when_nothing_may_be_installed",
        false,
        |root| async move {
            let system = root.join("system");
            let project = repository(&root.join("project"))?;
            let (mut preferences, runtime) = runtime(PackageManager::Yarn);
            let runtime = runtime
                .for_project(&project)
                .with_environment(path_environment(&system));

            fake_manager(&system, PackageManager::Pnpm, Fake::Usable)?;
            assert_eq!(manager_used(&info(&runtime).await?), "pnpm");

            fake_manager(&system, PackageManager::Npm, Fake::Usable)?;
            preferences.send(Some(PackageManager::Pnpm))?;
            assert_eq!(manager_used(&info(&runtime).await?), "pnpm");

            fake_manager(&system, PackageManager::Yarn, Fake::Unusable)?;
            preferences.send(Some(PackageManager::Yarn))?;
            assert_eq!(manager_used(&info(&runtime).await?), "pnpm");
            Ok(())
        },
    )
}

#[test]
fn vite_plus_is_probed_without_triggering_first_run_setup() -> Result<()> {
    isolated(
        "vite_plus_is_probed_without_triggering_first_run_setup",
        false,
        |root| async move {
            let system = root.join("system");
            let project = repository(&root.join("project"))?;
            let source = if cfg!(windows) {
                "@echo off\r\nif defined VP_SELF_SETUP_SUPPORT_CHECK (\r\necho vite-plus-self-setup-v1\r\nexit /b 0\r\n)\r\necho setup>setup-triggered\r\nexit /b 1\r\n"
            } else {
                "#!/bin/sh\nif [ -n \"$VP_SELF_SETUP_SUPPORT_CHECK\" ]; then\necho vite-plus-self-setup-v1\nexit 0\nfi\necho setup > setup-triggered\nexit 1\n"
            };
            write_executable(&system, "vp", source)?;
            let (_, runtime) = runtime(PackageManager::Auto);
            let task = run_task(
                &runtime,
                &project,
                path_environment(&system),
                PackageManager::Vp,
            )
            .await?;
            assert_eq!(task.package_manager, PackageManager::Vp);
            assert!(!project.join("setup-triggered").exists());
            assert!(!root.join("setup-triggered").exists());
            Ok(())
        },
    )
}

#[test]
fn missing_managers_and_failed_commands_are_reported() -> Result<()> {
    isolated(
        "missing_managers_and_failed_commands_are_reported",
        false,
        |root| async move {
            let system = root.join("system");
            fs::create_dir_all(&system)?;
            let project = repository(&root.join("project"))?;
            let (_, runtime) = runtime(PackageManager::Auto);
            let error = run_task(
                &runtime,
                &project,
                path_environment(&system),
                PackageManager::Vp,
            )
            .await
            .err()
            .context("no manager is installed")?;
            assert!(format!("{error:#}").contains("preferred: vp"));

            fake_manager(&system, PackageManager::Pnpm, Fake::FailingCommands)?;
            let error = runtime
                .for_project(&project)
                .with_environment(path_environment(&system))
                .run_package_manager_subcommand(None, "install", &[])
                .await
                .err()
                .context("install failure must propagate")?;
            assert!(error.to_string().contains("pnpm failed"));
            Ok(())
        },
    )
}

#[test]
fn project_tool_paths_respect_repository_boundaries_and_system_fallback() -> Result<()> {
    isolated(
        "project_tool_paths_respect_repository_boundaries_and_system_fallback",
        false,
        |root| async move {
            let repository = repository(&root.join("repository"))?;
            let project = repository.join("packages/project");
            let system = root.join("system");
            fs::create_dir_all(&project)?;
            let environment = project_tool_environment(&project, path_environment(&system)).await?;
            let paths = env::split_paths(environment.get("PATH").context("missing PATH")?)
                .collect::<Vec<_>>();
            assert_eq!(
                paths,
                vec![
                    project.join("node_modules/.bin"),
                    repository.join("packages/node_modules/.bin"),
                    repository.join("node_modules/.bin"),
                    system.clone(),
                ]
            );

            fake_manager(&system, PackageManager::Pnpm, Fake::Usable)?;
            let (_, runtime) = runtime(PackageManager::Auto);
            let task = run_task(&runtime, &project, environment, PackageManager::Pnpm).await?;
            assert_eq!(task.program, "pnpm");
            let task_paths =
                env::split_paths(task.env.get("PATH").context("missing PATH")?).collect::<Vec<_>>();
            assert!(task_paths.contains(&system));

            let loose = root.join("loose/project");
            fs::create_dir_all(&loose)?;
            let environment =
                project_tool_environment(&loose, HashMap::from([("PATH".into(), String::new())]))
                    .await?;
            let paths = env::split_paths(environment.get("PATH").context("missing PATH")?)
                .collect::<Vec<_>>();
            assert_eq!(paths.first(), Some(&loose.join("node_modules/.bin")));
            assert!(!paths.contains(&root.join("loose/node_modules/.bin")));
            Ok(())
        },
    )
}

#[test]
fn installed_packages_prefer_project_and_hoisted_tools_before_global_ones() -> Result<()> {
    isolated(
        "installed_packages_prefer_project_and_hoisted_tools_before_global_ones",
        false,
        |root| async move {
            let repository = repository(&root.join("repository"))?;
            let project = repository.join("packages/project");
            let system = root.join("system");
            let global = root.join("global/node_modules");
            for path in [&project, &global] {
                fs::create_dir_all(path)?;
            }
            let package = "@scope/server";
            for (node_modules, version) in [
                (project.join("node_modules"), "1.0.0"),
                (repository.join("node_modules"), "2.0.0"),
                (global.clone(), "3.0.0"),
            ] {
                fs::create_dir_all(node_modules.join(package))?;
                fs::write(
                    node_modules.join(package).join("package.json"),
                    format!(r#"{{"version":"{version}"}}"#),
                )?;
            }
            let source = if cfg!(windows) {
                format!("@echo off\r\necho {}\r\n", global.display())
            } else {
                format!("#!/bin/sh\nprintf '%s\\n' '{}'\n", global.display())
            };
            write_executable(&system, "npm", &source)?;
            let environment = path_environment(&system);
            for (node_modules, expected) in [
                (project.join("node_modules"), Version::new(1, 0, 0)),
                (repository.join("node_modules"), Version::new(2, 0, 0)),
                (global, Version::new(3, 0, 0)),
            ] {
                assert_eq!(
                    find_installed_package(&project, package, environment.clone()).await?,
                    Some((node_modules.clone(), expected))
                );
                fs::remove_file(node_modules.join(package).join("package.json"))?;
            }
            assert!(
                find_installed_package(&project, package, environment)
                    .await?
                    .is_none()
            );
            Ok(())
        },
    )
}

async fn info(runtime: &PackageManagerRuntime) -> Result<Output> {
    runtime
        .run_package_manager_subcommand(None, "info", &[])
        .await
}

async fn install(runtime: &PackageManagerRuntime) -> Result<Output> {
    runtime
        .run_package_manager_subcommand(None, "install", &[])
        .await
}

#[test]
fn publishes_missing_manager_errors_and_clears_them_after_installation() -> Result<()> {
    isolated(
        "publishes_missing_manager_errors_and_clears_them_after_installation",
        false,
        |directory| async move {
            fake_node(&directory)?;
            let node_runtime = NodeRuntime::new(
                None,
                watch::channel(Some(NodeBinaryOptions {
                    allow_path_lookup: true,
                }))
                .1,
            );
            let (mut preferences, preference) = watch::channel(Some(PackageManager::Auto));
            let runtime = PackageManagerRuntime::new(
                node_runtime,
                Arc::new(http_client::BlockedHttpClient),
                preference,
            )
            .for_project(&directory);

            let mut errors = runtime.errors();
            assert!(errors.borrow().is_none());
            let error = info(&runtime)
                .await
                .err()
                .context("missing package managers must be reported")?;
            assert!(error.to_string().contains("Install it manually"));
            assert!(error.to_string().contains("https://pnpm.io/installation"));
            assert_eq!(errors.borrow().as_deref(), Some(&error.to_string()));

            let (events, mut requests) = futures::channel::mpsc::unbounded();
            assert!(
                info(&runtime.clone().with_installation_events(events))
                    .await
                    .is_err(),
                "downloading pnpm is blocked in tests"
            );
            assert!(
                !matches!(
                    requests.try_recv(),
                    Ok(PackageManagerInstallEvent::Select { .. })
                ),
                "tool installs download pnpm without prompting when no manager is installed"
            );

            fake_manager(&directory, PackageManager::Npm, Fake::Usable)?;
            let (events, mut requests) = futures::channel::mpsc::unbounded();
            let automatic = runtime.clone().with_installation_events(events);
            assert_eq!(manager_used(&info(&automatic).await?), "npm");
            assert_eq!(
                run_task(
                    &automatic,
                    &directory,
                    path_environment(&directory),
                    PackageManager::Auto
                )
                .await?
                .package_manager,
                PackageManager::Npm
            );
            assert!(
                requests.try_recv().is_err(),
                "using an existing manager must not trigger installation"
            );

            fs::remove_file(directory.join(executable_name("npm")))?;
            fake_manager(&directory, PackageManager::Pnpm, Fake::Usable)?;
            assert_eq!(manager_used(&info(&runtime).await?), "pnpm");
            assert!(errors.borrow().is_none());

            let yarn_project = repository(&directory.join("yarn-project"))?;
            let bun_project = repository(&directory.join("bun-project"))?;
            for (project, manager) in [
                (&yarn_project, PackageManager::Yarn),
                (&bun_project, PackageManager::Bun),
            ] {
                fs::write(
                    project.join("package.json"),
                    serde_json::to_vec(&serde_json::json!({
                        "packageManager": format!("{}@4.1.0", manager.name()),
                    }))?,
                )?;
                fake_manager(&directory, manager, Fake::Usable)?;
            }
            for project in [&yarn_project, &bun_project] {
                assert_eq!(
                    manager_used(&install(&runtime.for_project(project)).await?),
                    "pnpm",
                    "auto tool installs use the first installed manager, not the project's"
                );
            }

            let yarn_runtime = runtime.for_project(&yarn_project);
            preferences.send(Some(PackageManager::Bun))?;
            assert_eq!(manager_used(&install(&yarn_runtime).await?), "bun");
            preferences.send(Some(PackageManager::Auto))?;
            fake_manager(&directory, PackageManager::Yarn, Fake::Unusable)?;
            assert_ne!(manager_used(&install(&yarn_runtime).await?), "yarn");
            assert!(
                run_task(
                    &yarn_runtime,
                    &yarn_project,
                    path_environment(&directory),
                    PackageManager::Auto
                )
                .await
                .is_err(),
                "project tasks must not fall back to another package manager"
            );

            let local_tools = yarn_project.join("node_modules/.bin");
            fake_manager(&local_tools, PackageManager::Yarn, Fake::Usable)?;
            let mut environment = path_environment(&directory);
            environment.insert("ZED_PROJECT_TOOL_TEST".into(), "configured".into());
            let yarn_runtime = yarn_runtime.with_environment(environment.clone());
            // Node.js is checked with exactly the PATH its processes receive.
            assert_eq!(
                yarn_runtime.binary_path().await?,
                directory.join(executable_name("node"))
            );
            let output = install(&yarn_runtime).await?;
            assert_eq!(manager_used(&output), "pnpm");
            assert!(String::from_utf8_lossy(&output.stdout).contains("env=configured"));
            let task = run_task(
                &yarn_runtime,
                &yarn_project,
                environment,
                PackageManager::Auto,
            )
            .await?;
            assert_eq!(task.program, "yarn");
            let task_paths =
                env::split_paths(task.env.get("PATH").context("missing PATH")?).collect::<Vec<_>>();
            assert_eq!(task_paths.first(), Some(&local_tools));
            Ok(())
        },
    )
}

/// An installer that reports `global` as its global directory and, when asked to install
/// anything, logs its arguments and puts a usable pnpm there.
fn fake_installer(
    system: &Path,
    global: &Path,
    installer: PackageManager,
    major: u64,
    log: &Path,
) -> Result<()> {
    fs::create_dir_all(global)?;
    let template = global.join("template");
    let target = global.join(executable_name("pnpm"));
    fs::write(
        &template,
        if cfg!(windows) {
            "@echo off\r\nif \"%1\"==\"--version\" (echo 12.0.0& exit /b 0)\r\necho pnpm %*\r\n"
        } else {
            "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then echo 12.0.0; exit 0; fi\necho pnpm \"$@\"\n"
        },
    )?;
    let prefix = if cfg!(windows) {
        global
    } else {
        global.parent().context("missing prefix")?
    };
    let source = if cfg!(windows) {
        format!(
            "@echo off\r\nif \"%1\"==\"--version\" (echo {major}.0.0 & exit /b 0)\r\nif \"%1\"==\"prefix\" (echo {prefix} & exit /b 0)\r\nif \"%1\"==\"bin\" (echo {global} & exit /b 0)\r\nif \"%1\"==\"pm\" (echo {global} & exit /b 0)\r\nif \"%1\"==\"global\" if \"%2\"==\"bin\" (echo {global} & exit /b 0)\r\necho %*>>\"{log}\"\r\ncopy /y \"{template}\" \"{target}\" >nul\r\n",
            prefix = prefix.display(),
            global = global.display(),
            log = log.display(),
            template = template.display(),
            target = target.display(),
        )
    } else {
        format!(
            "#!/bin/sh\ncase \"$1:$2\" in\n--version:*) echo {major}.0.0;;\nprefix:*) printf '%s\\n' '{prefix}';;\nbin:*|pm:*|global:bin) printf '%s\\n' '{global}';;\n*) printf '%s\\n' \"$*\" >> '{log}'; /bin/cp '{template}' '{target}'; /bin/chmod +x '{target}';;\nesac\n",
            prefix = prefix.display(),
            global = global.display(),
            log = log.display(),
            template = template.display(),
            target = target.display(),
        )
    };
    write_executable(system, installer.name(), &source)?;
    Ok(())
}

#[test]
fn missing_selected_manager_is_installed_once_through_an_existing_manager() -> Result<()> {
    isolated(
        "missing_selected_manager_is_installed_once_through_an_existing_manager",
        false,
        |root| async move {
            for (installer, major) in [
                (PackageManager::Vp, 1),
                (PackageManager::Bun, 1),
                (PackageManager::Npm, 10),
                (PackageManager::Yarn, 1),
            ] {
                let project = repository(&root.join(format!("{}-{major}", installer.name())))?;
                let system = project.join("system");
                let global = if installer == PackageManager::Vp {
                    project.join("vp-home/bin")
                } else {
                    project.join("global/bin")
                };
                let log = project.join("install.log");
                fake_installer(&system, &global, installer, major, &log)?;
                let (events, mut requests) = futures::channel::mpsc::unbounded();
                let (_, runtime) = runtime(PackageManager::Pnpm);
                let runtime = runtime
                    .for_project(&project)
                    .with_environment(HashMap::from([
                        ("PATH".into(), system.to_string_lossy().into_owned()),
                        (
                            "VP_HOME".into(),
                            project.join("vp-home").to_string_lossy().into_owned(),
                        ),
                    ]))
                    .with_installation_events(events);

                let (first, second) = futures::join!(info(&runtime), info(&runtime));
                assert_eq!(manager_used(&first?), "pnpm");
                assert_eq!(manager_used(&second?), "pnpm");
                match requests
                    .next()
                    .await
                    .context("missing installation event")?
                {
                    PackageManagerInstallEvent::Installed {
                        manager,
                        directory,
                        installed_via_manager,
                    } => {
                        assert_eq!(manager, PackageManager::Pnpm);
                        assert_eq!(directory, global);
                        assert!(installed_via_manager);
                    }
                    PackageManagerInstallEvent::Select { .. } => {
                        anyhow::bail!("an existing manager must be used without prompting")
                    }
                }
                let arguments = fs::read_to_string(&log)?;
                assert_eq!(
                    arguments.lines().count(),
                    1,
                    "concurrent requests must install once"
                );
                assert!(arguments.contains("global") && arguments.contains("pnpm"));
            }
            Ok(())
        },
    )
}

#[test]
fn undeclared_tasks_ask_which_manager_to_download_and_honor_cancellation() -> Result<()> {
    isolated(
        "undeclared_tasks_ask_which_manager_to_download_and_honor_cancellation",
        false,
        |root| async move {
            let project = repository(&root.join("project"))?;
            let empty = HashMap::from([("PATH".into(), String::new())]);
            let (events, mut requests) = futures::channel::mpsc::unbounded();
            let (_, runtime) = runtime(PackageManager::Auto);
            let runtime = runtime
                .for_project(&project)
                .with_environment(empty.clone())
                .with_installation_events(events);
            let selection = async {
                let Some(PackageManagerInstallEvent::Select { response }) = requests.next().await
                else {
                    anyhow::bail!("missing selection request")
                };
                response
                    .send(None)
                    .map_err(|_| anyhow::anyhow!("prompt closed"))
            };
            let (result, response) = futures::join!(
                run_task(&runtime, &project, empty, PackageManager::Auto),
                selection
            );
            response?;
            let error = result.err().context("cancel must stop installation")?;
            assert!(format!("{error:#}").contains("cancelled"));
            Ok(())
        },
    )
}

#[test]
fn queued_tasks_share_a_cancelled_selection_instead_of_prompting_again() -> Result<()> {
    isolated(
        "queued_tasks_share_a_cancelled_selection_instead_of_prompting_again",
        false,
        |root| async move {
            let project = repository(&root.join("project"))?;
            let empty = HashMap::from([("PATH".into(), String::new())]);
            let (events, mut requests) = futures::channel::mpsc::unbounded();
            let (_, runtime) = runtime(PackageManager::Auto);
            let runtime = runtime
                .for_project(&project)
                .with_environment(empty.clone())
                .with_installation_events(events);
            let selection = async {
                let Some(PackageManagerInstallEvent::Select { response }) = requests.next().await
                else {
                    anyhow::bail!("missing selection request")
                };
                response
                    .send(None)
                    .map_err(|_| anyhow::anyhow!("prompt closed"))
            };
            let (first, second, response) = futures::join!(
                run_task(&runtime, &project, empty.clone(), PackageManager::Auto),
                run_task(&runtime, &project, empty.clone(), PackageManager::Auto),
                selection
            );
            response?;
            for result in [first, second] {
                let error = result.err().context("cancel must stop every queued request")?;
                assert!(format!("{error:#}").contains("cancelled"));
            }
            assert!(
                requests.try_recv().is_err(),
                "requests queued behind a cancelled prompt must not prompt again"
            );

            let selection = async {
                let Some(PackageManagerInstallEvent::Select { response }) = requests.next().await
                else {
                    anyhow::bail!("a new request must prompt again")
                };
                response
                    .send(None)
                    .map_err(|_| anyhow::anyhow!("prompt closed"))
            };
            let (result, response) = futures::join!(
                run_task(&runtime, &project, empty, PackageManager::Auto),
                selection
            );
            response?;
            assert!(result.is_err());
            Ok(())
        },
    )
}

#[test]
fn tools_download_pnpm_with_its_official_installer_when_nothing_is_installed() -> Result<()> {
    isolated(
        "tools_download_pnpm_with_its_official_installer_when_nothing_is_installed",
        false,
        |root| async move {
            let project = repository(&root.join("project"))?;
            let source = if cfg!(windows) {
                "$directory = Join-Path $env:PNPM_HOME 'bin'\nNew-Item -ItemType Directory -Force -Path $directory | Out-Null\nSet-Content -LiteralPath (Join-Path $directory 'pnpm.cmd') -Encoding ASCII -Value \"@echo off`r`necho 12.0.0`r`n\"\n"
            } else {
                "/bin/mkdir -p \"$PNPM_HOME/bin\"\nprintf '#!/bin/sh\\necho 12.0.0\\n' > \"$PNPM_HOME/bin/pnpm\"\n/bin/chmod +x \"$PNPM_HOME/bin/pnpm\"\n"
            };
            let http = http_client::FakeHttpClient::create(move |request| async move {
                let expected = if cfg!(windows) {
                    "https://get.pnpm.io/install.ps1"
                } else {
                    "https://get.pnpm.io/install.sh"
                };
                assert_eq!(request.uri().to_string(), expected);
                Ok(http_client::Response::builder()
                    .status(200)
                    .body(http_client::AsyncBody::from(source))?)
            });
            let pnpm_home = root.join("pnpm home");
            let (events, mut requests) = futures::channel::mpsc::unbounded();
            let runtime = PackageManagerRuntime::new(
                NodeRuntime::unavailable(),
                http,
                watch::channel(Some(PackageManager::Auto)).1,
            )
            .for_project(&project)
            .with_environment(HashMap::from([
                ("PATH".into(), String::new()),
                ("PNPM_HOME".into(), pnpm_home.to_string_lossy().into_owned()),
            ]))
            .with_installation_events(events);

            info(&runtime).await?;
            assert!(
                pnpm_home
                    .join("bin")
                    .join(executable_name("pnpm"))
                    .is_file()
            );
            match requests.next().await {
                Some(PackageManagerInstallEvent::Installed {
                    manager,
                    directory,
                    installed_via_manager,
                }) => {
                    assert_eq!(manager, PackageManager::Pnpm);
                    assert_eq!(directory, pnpm_home.join("bin"));
                    assert!(!installed_via_manager);
                }
                _ => anyhow::bail!("downloading pnpm must not prompt"),
            }
            Ok(())
        },
    )
}

#[test]
#[ignore = "requires system Node.js, a package manager, and registry access"]
fn installs_and_queries_tools_with_system_package_manager() -> Result<()> {
    isolated(
        "installs_and_queries_tools_with_system_package_manager",
        true,
        |root| async move {
            let manager = PackageManager::from_name(
                &env::var("ZED_TEST_PACKAGE_MANAGER").unwrap_or_else(|_| "pnpm".into()),
            )
            .context("unsupported test package manager")?;
            which::which(manager.name()).context("test package manager must be installed")?;
            which::which("node").context("test requires system Node.js")?;
            let project = repository(&root.join("project"))?;
            let package = root.join("package");
            let tools = root.join("tools");
            fs::create_dir_all(&package)?;
            let manifest = serde_json::to_string(
                &serde_json::json!({"packageManager": format!("{}@1.0.0", manager.name())}),
            )?;
            fs::write(project.join("package.json"), &manifest)?;
            fs::write(
                package.join("package.json"),
                r#"{"name":"zed-tool-install-test","version":"1.2.3","bin":"cli.js"}"#,
            )?;
            fs::write(package.join("cli.js"), "console.log('installed');\n")?;
            let node_runtime = NodeRuntime::new(
                None,
                watch::channel(Some(NodeBinaryOptions {
                    allow_path_lookup: true,
                }))
                .1,
            );
            let runtime = PackageManagerRuntime::new(
                node_runtime,
                Arc::new(http_client::BlockedHttpClient),
                watch::channel(Some(manager)).1,
            )
            .for_project(&project);

            let version = format!("file:{}", package.to_string_lossy().replace('\\', "/"));
            runtime
                .package_manager_install_packages(&tools, &[("zed-tool-install-test", &version)])
                .await?;
            assert_eq!(
                runtime
                    .package_manager_installed_version(&tools, "zed-tool-install-test")
                    .await?,
                Some(Version::new(1, 2, 3))
            );
            let executable = node_runtime::read_package_executable(
                tools.join("node_modules"),
                "zed-tool-install-test",
            )
            .await?;
            assert!(executable.is_file());
            assert!(
                runtime
                    .package_manager_latest_version("is-number")
                    .await?
                    .major
                    > 0
            );
            assert_eq!(fs::read_to_string(project.join("package.json"))?, manifest);
            assert!(!project.join("node_modules").exists());
            Ok(())
        },
    )
}
