use anyhow::Result;
#[cfg(not(windows))]
use anyhow::bail;
#[cfg(windows)]
use anyhow::{Context as _, ensure};
use std::path::Path;
#[cfg(windows)]
use std::{collections::HashMap, env};

#[cfg(windows)]
fn read_user_path(key: &windows_registry::Key) -> Result<String> {
    match key.get_string("Path") {
        Ok(path) => Ok(path),
        Err(error)
            if error.code() == windows::Win32::Foundation::ERROR_FILE_NOT_FOUND.to_hresult() =>
        {
            Ok(String::new())
        }
        Err(error) => Err(error.into()),
    }
}

#[cfg(windows)]
fn expanded_path(path: &str) -> Result<String> {
    use windows::{Win32::System::Environment::ExpandEnvironmentStringsW, core::HSTRING};
    let mut variables = HashMap::new();
    match windows_registry::CURRENT_USER.open("Environment") {
        Ok(key) => {
            for (name, value) in key.values()? {
                if matches!(
                    value.ty(),
                    windows_registry::Type::String | windows_registry::Type::ExpandString
                ) && !name.eq_ignore_ascii_case("Path")
                {
                    variables.insert(name.to_uppercase(), String::try_from(value)?);
                }
            }
        }
        Err(error)
            if error.code() == windows::Win32::Foundation::ERROR_FILE_NOT_FOUND.to_hresult() => {}
        Err(error) => return Err(error.into()),
    }
    let input = HSTRING::from(expand_user_variables(path, &variables));
    let length = unsafe { ExpandEnvironmentStringsW(&input, None) };
    ensure!(length != 0, "could not expand the user PATH");
    let mut output = vec![0u16; length as usize];
    let written = unsafe { ExpandEnvironmentStringsW(&input, Some(&mut output)) };
    ensure!(
        written != 0 && written <= length,
        "could not expand the user PATH"
    );
    Ok(String::from_utf16(
        output
            .get(..written.saturating_sub(1) as usize)
            .context("invalid expanded PATH length")?,
    )?)
}

#[cfg(windows)]
fn expand_user_variables(path: &str, variables: &HashMap<String, String>) -> String {
    let mut expanded = path.to_owned();
    for _ in 0..8 {
        let mut result = String::new();
        let mut remaining = expanded.as_str();
        while let Some((prefix, after_open)) = remaining.split_once('%') {
            result.push_str(prefix);
            if let Some((name, suffix)) = after_open.split_once('%') {
                if let Some(value) = variables.get(&name.to_uppercase()) {
                    result.push_str(value);
                } else {
                    result.push('%');
                    result.push_str(name);
                    result.push('%');
                }
                remaining = suffix;
            } else {
                result.push('%');
                result.push_str(after_open);
                remaining = "";
                break;
            }
        }
        result.push_str(remaining);
        if result == expanded {
            break;
        }
        expanded = result;
    }
    expanded
}

#[cfg(windows)]
fn path_contains_directory(path: &str, directory: &Path) -> Result<bool> {
    let normalize = |path: &Path| {
        path.to_string_lossy()
            .trim()
            .trim_matches('"')
            .replace('/', "\\")
            .trim_end_matches('\\')
            .to_lowercase()
    };
    let directory = normalize(directory);
    Ok(env::split_paths(&expanded_path(path)?).any(|path| normalize(&path) == directory))
}

pub async fn installation_in_user_path(directory: &Path) -> Result<bool> {
    #[cfg(windows)]
    {
        let directory = directory.to_path_buf();
        return smol::unblock(move || {
            let key = match windows_registry::CURRENT_USER.open("Environment") {
                Ok(key) => key,
                Err(error)
                    if error.code()
                        == windows::Win32::Foundation::ERROR_FILE_NOT_FOUND.to_hresult() =>
                {
                    return Ok(false);
                }
                Err(error) => return Err(error.into()),
            };
            path_contains_directory(&read_user_path(&key)?, &directory)
        })
        .await;
    }
    #[cfg(not(windows))]
    {
        let _directory = directory;
        Ok(true)
    }
}

pub async fn add_installation_to_user_path(directory: &Path) -> Result<()> {
    #[cfg(windows)]
    {
        static UPDATE_LOCK: smol::lock::Mutex<()> = smol::lock::Mutex::new(());
        let _guard = UPDATE_LOCK.lock().await;
        let directory = directory.to_path_buf();
        return smol::unblock(move || {
            use windows::{core::w, Win32::{Foundation::{LPARAM, WPARAM}, UI::WindowsAndMessaging::{HWND_BROADCAST, SendMessageTimeoutW, SMTO_ABORTIFHUNG, WM_SETTINGCHANGE}}};
            let key = windows_registry::CURRENT_USER.create("Environment")?;
            let path = read_user_path(&key)?;
            if let Some(path) = updated_user_path(&path, &directory)? {
                key.set_expand_string("Path", path)?;
                let result = unsafe {
                    SendMessageTimeoutW(HWND_BROADCAST, WM_SETTINGCHANGE, WPARAM(0), LPARAM(w!("Environment").as_ptr() as isize), SMTO_ABORTIFHUNG, 5000, None)
                };
                if result.0 == 0 {
                    log::warn!("User PATH was updated, but broadcasting the environment change timed out or failed");
                }
            }
            Ok(())
        }).await;
    }
    #[cfg(not(windows))]
    {
        let _directory = directory;
        bail!("user environment PATH installation is only supported on Windows")
    }
}

#[cfg(windows)]
fn updated_user_path(path: &str, directory: &Path) -> Result<Option<String>> {
    if path_contains_directory(path, directory)? {
        return Ok(None);
    }
    let directory = directory
        .to_str()
        .context("installation path is not valid Unicode")?;
    ensure!(
        !directory.contains([';', '\0']),
        "installation path cannot be added to PATH"
    );
    let separator = if path.is_empty() || path.ends_with(';') {
        ""
    } else {
        ";"
    };
    Ok(Some(format!("{path}{separator}{directory}")))
}

#[cfg(test)]
mod tests {
    #[cfg(windows)]
    use super::*;
    #[cfg(windows)]
    use std::path::PathBuf;

    #[test]
    #[cfg(windows)]
    fn user_path_preserves_existing_entries_and_avoids_duplicates() -> Result<()> {
        let directory = Path::new(r"C:\Tools\pnpm");
        assert!(path_contains_directory(
            r#"C:\Other;"c:/TOOLS/pnpm/""#,
            directory
        )?);
        assert!(!path_contains_directory(r"C:\Tools\pnpm-old", directory)?);
        assert_eq!(updated_user_path(r"C:\TOOLS\PNPM\", directory)?, None);
        assert_eq!(
            updated_user_path("", directory)?.as_deref(),
            Some(r"C:\Tools\pnpm")
        );
        assert_eq!(
            updated_user_path(r"%USERPROFILE%\bin;C:\Other;", directory)?.as_deref(),
            Some(r"%USERPROFILE%\bin;C:\Other;C:\Tools\pnpm")
        );
        assert_eq!(
            updated_user_path(r"C:\Other", directory)?.as_deref(),
            Some(r"C:\Other;C:\Tools\pnpm")
        );
        let profile = env::var_os("USERPROFILE").context("missing USERPROFILE")?;
        assert!(path_contains_directory(
            r"%USERPROFILE%\bin",
            &PathBuf::from(profile).join("bin")
        )?);
        assert!(updated_user_path("", Path::new(r"C:\Tools;Other")).is_err());
        let variables = HashMap::from([
            ("PNPM_HOME".into(), r"%TOOLS%\pnpm".into()),
            ("TOOLS".into(), r"C:\Tools".into()),
        ]);
        assert_eq!(
            expand_user_variables(r"%pnpm_home%\bin;%UNKNOWN%\bin", &variables),
            r"C:\Tools\pnpm\bin;%UNKNOWN%\bin"
        );
        assert_eq!(
            expand_user_variables("100% ready", &variables),
            "100% ready"
        );
        Ok(())
    }
}
