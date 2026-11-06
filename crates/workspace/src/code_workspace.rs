//! VS Code `.code-workspace` support.
//!
//! A workspace file only exists at runtime: the database stores the folders of a
//! window like any other project, and a window becomes a workspace again when its
//! folders match a workspace file that was opened before.

mod open;
mod registry;
#[cfg(test)]
mod tests;
mod write;

use crate::Workspace;
use gpui::{App, SharedString, Task};
use project::WorktreeId;
use std::path::{Path, PathBuf};

pub(crate) use open::{expand_workspace_paths, is_workspace_file};
pub use registry::code_workspace_for_folders;

pub(crate) struct CodeWorkspace {
    path: PathBuf,
    /// Root paths of the worktrees that belong to the workspace file.
    folders: Vec<PathBuf>,
    /// While the file is being applied, folder changes come from the file itself
    /// and must not be written back to it.
    applying: usize,
    sync_folders: Option<Task<()>>,
    settings_update: Option<Task<()>>,
    _watcher: Task<()>,
}

pub(crate) fn init(cx: &mut App) {
    registry::init(cx);
}

/// The name a workspace file is shown with.
pub fn code_workspace_file_name(path: &Path) -> Option<SharedString> {
    path.file_name()
        .map(|name| SharedString::from(name.to_string_lossy().into_owned()))
}

/// The workspace file name to show for a project with the given folders.
pub fn code_workspace_display_name(folders: &[PathBuf], cx: &App) -> Option<SharedString> {
    code_workspace_for_folders(folders, cx).and_then(|path| code_workspace_file_name(&path))
}

impl Workspace {
    pub fn code_workspace_path(&self) -> Option<&Path> {
        self.code_workspace
            .as_ref()
            .map(|workspace| workspace.path.as_path())
    }

    pub fn code_workspace_name(&self) -> Option<SharedString> {
        self.code_workspace_path()
            .and_then(code_workspace_file_name)
    }

    /// The folder the user is working in: the one containing the active
    /// repository, or the first folder of the project.
    pub fn active_folder_worktree_id(&self, cx: &App) -> Option<WorktreeId> {
        let project = self.project.read(cx);
        let mut folders = project
            .visible_worktrees(cx)
            .filter(|worktree| {
                worktree
                    .read(cx)
                    .root_entry()
                    .is_some_and(|entry| entry.is_dir())
            })
            .collect::<Vec<_>>();
        if let Some(repository) = project.active_repository(cx) {
            let repository_path = repository.read(cx).work_directory_abs_path.clone();
            if let Some(index) = folders.iter().position(|worktree| {
                let worktree_path = worktree.read(cx).abs_path();
                worktree_path == repository_path || worktree_path.starts_with(&repository_path)
            }) {
                folders.swap(0, index);
            }
        }
        folders.first().map(|worktree| worktree.read(cx).id())
    }
}
