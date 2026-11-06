//! The settings tab for settings shared by every folder of a multi-folder
//! project, stored in the project's `.code-workspace` file.

use crate::SettingsUiFile;
use anyhow::{Context as _, Result};
use gpui::{
    AnyElement, App, Context, IntoElement, ParentElement, Render, SharedString, Styled, Window,
    WindowHandle, div,
};
use project::WorktreeId;
use settings::{SettingsContent, SettingsFile, SettingsStore};
use ui::{
    Button, ButtonCommon, ButtonStyle, Clickable, ElevationIndex, InteractiveElement, Label,
    LabelSize, StatefulInteractiveElement, h_flex, prelude::*, tooltip_container,
};
use util::ResultExt as _;
use workspace::{MultiWorkspace, OpenOptions, OpenVisible, notifications::DetachAndPromptErr};

const UNSAVED_WORKSPACE_LABEL: &str = "Project";

/// The workspace file name, or `None` while the project has no workspace file.
/// It is display data only, so it never makes two tabs unequal.
#[derive(Clone, Debug, Default)]
pub(crate) struct WorkspaceTabLabel(pub(crate) Option<SharedString>);

impl PartialEq for WorkspaceTabLabel {
    fn eq(&self, _: &Self) -> bool {
        true
    }
}

impl WorkspaceTabLabel {
    pub(crate) fn is_saved(&self) -> bool {
        self.0.is_some()
    }

    pub(crate) fn display_name(&self) -> String {
        self.0
            .as_ref()
            .map(ToString::to_string)
            .unwrap_or_else(|| UNSAVED_WORKSPACE_LABEL.to_string())
    }
}

/// The workspace tab of the window's active project, which only exists when the
/// project has several folders.
pub(crate) fn workspace_tab(
    original_window: Option<&WindowHandle<MultiWorkspace>>,
    cx: &App,
) -> Option<SettingsUiFile> {
    let workspace = original_window?.read(cx).ok()?.workspace().read(cx);
    let project = workspace.project().read(cx);
    if !project.is_local() {
        return None;
    }
    let folders = project
        .visible_worktrees(cx)
        .map(|worktree| worktree.read(cx).id())
        .collect::<Vec<WorktreeId>>();
    if folders.len() < 2 {
        return None;
    }
    let first_folder = *folders.first()?;
    let anchor = match cx
        .global::<SettingsStore>()
        .workspace_settings_file(first_folder)
    {
        Some(SettingsFile::Workspace(anchor)) => anchor,
        _ => first_folder,
    };
    Some(SettingsUiFile::Workspace((
        anchor,
        WorkspaceTabLabel(workspace.code_workspace_name()),
    )))
}

/// Shows why an unsaved workspace can't be edited and offers to create its file.
pub(crate) fn with_create_workspace_popover(
    id: impl Into<SharedString>,
    element: AnyElement,
    original_window: Option<WindowHandle<MultiWorkspace>>,
) -> AnyElement {
    div()
        .id(id.into())
        .child(element)
        .hoverable_tooltip(move |_, cx| {
            cx.new(|_| CreateCodeWorkspacePopover { original_window })
                .into()
        })
        .into_any_element()
}

struct CreateCodeWorkspacePopover {
    original_window: Option<WindowHandle<MultiWorkspace>>,
}

impl Render for CreateCodeWorkspacePopover {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let original_window = self.original_window;
        tooltip_container(cx, move |container, _cx| {
            container
                .gap_1p5()
                .max_w_80()
                .child(Label::new(
                    "Settings shared by all folders are stored in a .code-workspace file. \
                     Save this project as a workspace to edit them.",
                ))
                .child(
                    h_flex().w_full().justify_end().child(
                        Button::new("create-code-workspace", "Create Workspace")
                            .style(ButtonStyle::Filled)
                            .layer(ElevationIndex::ModalSurface)
                            .label_size(LabelSize::Small)
                            .on_click(move |_, _, cx| create_code_workspace(original_window, cx)),
                    ),
                )
        })
    }
}

fn create_code_workspace(original_window: Option<WindowHandle<MultiWorkspace>>, cx: &mut App) {
    let Some(original_window) = original_window else {
        return;
    };
    original_window
        .update(cx, |multi_workspace, window, cx| {
            window.activate_window();
            multi_workspace
                .workspace()
                .clone()
                .update(cx, |workspace, cx| {
                    workspace
                        .create_code_workspace(window, cx)
                        .detach_and_prompt_err(
                            "Failed to create workspace",
                            window,
                            cx,
                            |_, _, _| None,
                        );
                });
        })
        .log_err();
}

pub(crate) fn edit_button_label(label: &WorkspaceTabLabel) -> String {
    format!("Edit in {}", label.display_name())
}

pub(crate) fn open_workspace_file(
    original_window: Option<WindowHandle<MultiWorkspace>>,
    cx: &mut App,
) {
    let Some(original_window) = original_window else {
        return;
    };
    original_window
        .update(cx, |multi_workspace, window, cx| {
            multi_workspace
                .workspace()
                .clone()
                .update(cx, |workspace, cx| {
                    let Some(path) = workspace.code_workspace_path() else {
                        return;
                    };
                    workspace
                        .open_abs_path(
                            path.to_path_buf(),
                            OpenOptions {
                                visible: Some(OpenVisible::None),
                                ..Default::default()
                            },
                            window,
                            cx,
                        )
                        .detach_and_log_err(cx);
                });
            window.activate_window();
        })
        .log_err();
}

pub(crate) fn update_workspace_settings(
    original_window: Option<WindowHandle<MultiWorkspace>>,
    update: Box<dyn FnOnce(&mut SettingsContent, &App)>,
    cx: &mut App,
) -> Result<()> {
    let workspace = original_window
        .context("No window to save workspace settings for")?
        .read(cx)?
        .workspace()
        .clone();
    workspace.update(cx, |workspace, cx| {
        workspace.update_code_workspace_settings(update, cx)
    })
}
