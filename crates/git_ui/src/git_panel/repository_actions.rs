use super::GitPanel;
use editor::Editor;
use git::{StashAll, StashApply, StashPop, StashStaged, StashTracked};
use gpui::{DismissEvent, Entity, EventEmitter, FocusHandle, Focusable, Task, WeakEntity};
use project::git_store::Repository;
use ui::{ContextMenu, ContextMenuEntry, prelude::*};
use util::ResultExt;
use workspace::{ModalView, Workspace};

struct RepositoryInputModal {
    title: SharedString,
    editors: Vec<Entity<Editor>>,
    required_fields: usize,
    sender: Option<futures::channel::oneshot::Sender<Vec<String>>>,
}

impl RepositoryInputModal {
    fn focus_field(&self, backwards: bool, window: &mut Window, cx: &mut Context<Self>) {
        if self.editors.is_empty() {
            return;
        }
        let current = self
            .editors
            .iter()
            .position(|editor| editor.focus_handle(cx).is_focused(window))
            .unwrap_or(0);
        let next = if backwards {
            (current + self.editors.len() - 1) % self.editors.len()
        } else {
            (current + 1) % self.editors.len()
        };
        if let Some(editor) = self.editors.get(next) {
            editor.focus_handle(cx).focus(window, cx);
        }
    }

    fn confirm(&mut self, _: &menu::Confirm, _: &mut Window, cx: &mut Context<Self>) {
        let values: Vec<_> = self
            .editors
            .iter()
            .map(|editor| editor.read(cx).text(cx).trim().to_owned())
            .collect();
        if values
            .iter()
            .take(self.required_fields)
            .any(String::is_empty)
        {
            return;
        }
        if let Some(sender) = self.sender.take() {
            if sender.send(values).is_err() {
                log::debug!("Repository input receiver was closed");
            }
        }
        cx.emit(DismissEvent);
    }

    fn cancel(&mut self, _: &menu::Cancel, _: &mut Window, cx: &mut Context<Self>) {
        self.sender.take();
        cx.emit(DismissEvent);
    }
}

impl EventEmitter<DismissEvent> for RepositoryInputModal {}
impl ModalView for RepositoryInputModal {}
impl Focusable for RepositoryInputModal {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.editors
            .first()
            .map(|editor| editor.focus_handle(cx))
            .unwrap_or_else(|| cx.focus_handle())
    }
}

impl Render for RepositoryInputModal {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .key_context("RepositoryInputModal")
            .on_action(cx.listener(Self::confirm))
            .on_action(cx.listener(Self::cancel))
            .on_action(cx.listener(|this, _: &menu::SelectNext, window, cx| {
                this.focus_field(false, window, cx)
            }))
            .on_action(cx.listener(|this, _: &menu::SelectPrevious, window, cx| {
                this.focus_field(true, window, cx)
            }))
            .elevation_2(cx)
            .w(rems(34.))
            .p_3()
            .gap_3()
            .child(Headline::new(self.title.clone()).size(HeadlineSize::XSmall))
            .children(self.editors.iter().cloned())
            .child(Button::new("confirm-repository-input", "Confirm").on_click(
                cx.listener(|this, _, window, cx| this.confirm(&menu::Confirm, window, cx)),
            ))
    }
}

pub(super) fn repository_input(
    title: &'static str,
    placeholders: &[&'static str],
    required_fields: usize,
    workspace: WeakEntity<Workspace>,
    window: &mut Window,
    cx: &mut App,
) -> Task<Option<Vec<String>>> {
    let placeholders = placeholders.to_vec();
    let (sender, receiver) = futures::channel::oneshot::channel();
    if workspace
        .update(cx, |workspace, cx| {
            workspace.toggle_modal(window, cx, |window, cx| RepositoryInputModal {
                title: title.into(),
                editors: placeholders
                    .into_iter()
                    .map(|placeholder| {
                        cx.new(|cx| {
                            let mut editor = Editor::single_line(window, cx);
                            editor.set_placeholder_text(placeholder, window, cx);
                            editor
                        })
                    })
                    .collect(),
                required_fields,
                sender: Some(sender),
            });
        })
        .log_err()
        .is_none()
    {
        return Task::ready(None);
    }
    cx.background_spawn(async move { receiver.await.ok() })
}

#[derive(Clone, Copy)]
pub(super) enum RepositoryMenuAction {
    CreateBranch,
    DeleteBranch,
    AddRemote,
    RemoveRemote,
    DropStash,
    ClearStashes,
    Sync,
    PullFrom,
}

pub(super) fn repository_branch_status(summary: git::status::GitSummary) -> (&'static str, Color) {
    use git::status::TrackedSummary;
    if summary.conflict > 0 {
        return ("!", Color::Conflict);
    }
    let staged = summary.index != TrackedSummary::UNCHANGED;
    let unstaged = summary.worktree != TrackedSummary::UNCHANGED || summary.untracked > 0;
    let suffix = match (unstaged, staged) {
        (true, true) => "*+",
        (true, false) => "*",
        (false, true) => "+",
        (false, false) => "",
    };
    let color = if summary.count == 0 {
        Color::Default
    } else if summary.index.deleted > 0 || summary.worktree.deleted > 0 {
        Color::Deleted
    } else if summary.index.modified > 0 || summary.worktree.modified > 0 {
        Color::Modified
    } else {
        Color::Created
    };
    (suffix, color)
}

fn repository_menu_entry(
    menu: ContextMenu,
    label: &'static str,
    panel: WeakEntity<GitPanel>,
    repository: Entity<Repository>,
    action: RepositoryMenuAction,
    disabled: bool,
) -> ContextMenu {
    menu.item(
        ContextMenuEntry::new(label)
            .disabled(disabled)
            .handler(move |window, cx| {
                panel
                    .update(cx, |panel, cx| {
                        panel.repository_menu_action(repository.clone(), action, window, cx);
                    })
                    .log_err();
            }),
    )
}

pub(super) fn repository_action_submenus(
    menu: ContextMenu,
    panel: WeakEntity<GitPanel>,
    repository: Entity<Repository>,
    focus_handle: FocusHandle,
    cx: &App,
) -> ContextMenu {
    let state = repository.read(cx);
    let has_branch = state.branch.is_some();
    let has_stash = !state.cached_stash().entries.is_empty();
    let writable = panel
        .read_with(cx, |panel, cx| panel.has_write_access(cx))
        .unwrap_or(false);
    let remote_enabled = writable
        && panel
            .read_with(cx, |panel, cx| {
                panel.can_push_and_pull(cx) && panel.pending_remote_operation.is_none()
            })
            .unwrap_or(false);
    menu.submenu("Pull, Push", {
        let panel = panel.clone();
        let repository = repository.clone();
        let focus_handle = focus_handle.clone();
        move |menu, _, _| {
            repository_menu_entry(
                menu.context(focus_handle.clone()),
                "Sync",
                panel.clone(),
                repository.clone(),
                RepositoryMenuAction::Sync,
                !remote_enabled || !has_branch,
            )
            .separator()
            .action_disabled_when(!remote_enabled || !has_branch, "Pull", Box::new(git::Pull))
            .action_disabled_when(
                !remote_enabled || !has_branch,
                "Pull (Rebase)",
                Box::new(git::PullRebase),
            )
            .map(|menu| {
                repository_menu_entry(
                    menu,
                    "Pull From…",
                    panel.clone(),
                    repository.clone(),
                    RepositoryMenuAction::PullFrom,
                    !remote_enabled || !has_branch,
                )
            })
            .separator()
            .action_disabled_when(!remote_enabled || !has_branch, "Push", Box::new(git::Push))
            .action_disabled_when(
                !remote_enabled || !has_branch,
                "Push To…",
                Box::new(git::PushTo),
            )
            .action_disabled_when(
                !remote_enabled || !has_branch,
                "Force Push",
                Box::new(git::ForcePush),
            )
            .separator()
            .action_disabled_when(!remote_enabled, "Fetch From…", Box::new(git::FetchFrom))
            .action_disabled_when(
                !remote_enabled,
                "Fetch All Remotes",
                Box::new(git::Fetch),
            )
        }
    })
    .submenu("Branch", {
        let panel = panel.clone();
        let repository = repository.clone();
        let focus_handle = focus_handle.clone();
        move |menu, _, _| {
            repository_menu_entry(
                menu.context(focus_handle.clone()),
                "Create Branch…",
                panel.clone(),
                repository.clone(),
                RepositoryMenuAction::CreateBranch,
                !writable,
            )
            .action_disabled_when(
                !writable,
                "Checkout Branch…",
                Box::new(zed_actions::git::Switch),
            )
            .action_disabled_when(
                !writable || !has_branch,
                "Rename Branch…",
                Box::new(git::RenameBranch::default()),
            )
            .map(|menu| {
                repository_menu_entry(
                    menu,
                    "Delete Branch…",
                    panel.clone(),
                    repository.clone(),
                    RepositoryMenuAction::DeleteBranch,
                    !writable,
                )
            })
            .separator()
            .action_disabled_when(
                !has_branch,
                "Copy Branch Name",
                Box::new(git::CopyBranchName),
            )
        }
    })
    .submenu("Remote", {
        let panel = panel.clone();
        let repository = repository.clone();
        move |menu, _, _| {
            repository_menu_entry(
                menu,
                "Add Remote…",
                panel.clone(),
                repository.clone(),
                RepositoryMenuAction::AddRemote,
                !remote_enabled,
            )
            .map(|menu| {
                repository_menu_entry(
                    menu,
                    "Remove Remote…",
                    panel.clone(),
                    repository.clone(),
                    RepositoryMenuAction::RemoveRemote,
                    !remote_enabled,
                )
            })
        }
    })
    .submenu("Stash", move |menu, _, _| {
        menu.context(focus_handle.clone())
            .action_disabled_when(!writable, "Stash All…", Box::new(StashAll))
            .action_disabled_when(!writable, "Stash Tracked…", Box::new(StashTracked))
            .action_disabled_when(!writable, "Stash Staged…", Box::new(StashStaged))
            .separator()
            .action_disabled_when(
                !writable || !has_stash,
                "Apply Latest Stash",
                Box::new(StashApply),
            )
            .action_disabled_when(
                !writable || !has_stash,
                "Pop Latest Stash",
                Box::new(StashPop),
            )
            .action_disabled_when(
                !has_stash,
                "View Stashes…",
                Box::new(zed_actions::git::ViewStash),
            )
            .map(|menu| {
                repository_menu_entry(
                    menu,
                    "Drop Stash…",
                    panel.clone(),
                    repository.clone(),
                    RepositoryMenuAction::DropStash,
                    !writable || !has_stash,
                )
            })
            .map(|menu| {
                repository_menu_entry(
                    menu,
                    "Clear Stashes…",
                    panel.clone(),
                    repository.clone(),
                    RepositoryMenuAction::ClearStashes,
                    !writable || !has_stash,
                )
            })
    })
}

#[cfg(test)]
mod tests {
    use super::super::tests::{init_test, setup_git_panel_with_changes};
    use super::*;
    use gpui::TestAppContext;
    use serde_json::json;

    #[test]
    fn test_repository_branch_status() {
        use git::status::{GitSummary, TrackedSummary};
        assert_eq!(
            repository_branch_status(GitSummary::UNCHANGED),
            ("", Color::Default)
        );
        assert_eq!(
            repository_branch_status(GitSummary::UNTRACKED),
            ("*", Color::Created)
        );
        assert_eq!(
            repository_branch_status(GitSummary {
                worktree: TrackedSummary::MODIFIED,
                count: 1,
                ..GitSummary::UNCHANGED
            }),
            ("*", Color::Modified)
        );
        assert_eq!(
            repository_branch_status(GitSummary {
                index: TrackedSummary::ADDED,
                count: 1,
                ..GitSummary::UNCHANGED
            }),
            ("+", Color::Created)
        );
        assert_eq!(
            repository_branch_status(GitSummary {
                index: TrackedSummary::MODIFIED,
                worktree: TrackedSummary::DELETED,
                count: 1,
                ..GitSummary::UNCHANGED
            }),
            ("*+", Color::Deleted)
        );
        assert_eq!(
            repository_branch_status(GitSummary::CONFLICT),
            ("!", Color::Conflict)
        );
    }

    #[gpui::test]
    async fn test_repository_input_validation_and_cancel(cx: &mut TestAppContext) {
        init_test(cx);
        let (_, _, _, panel, mut cx) =
            setup_git_panel_with_changes(cx, json!({ ".git": {} }), &[]).await;
        panel.update_in(&mut cx, |_, window, cx| {
            let (sender, mut receiver) = futures::channel::oneshot::channel();
            let modal = cx.new(|cx| RepositoryInputModal {
                title: "Add Remote".into(),
                editors: vec![
                    cx.new(|cx| Editor::single_line(window, cx)),
                    cx.new(|cx| Editor::single_line(window, cx)),
                ],
                required_fields: 2,
                sender: Some(sender),
            });
            modal.update(cx, |modal, cx| {
                modal
                    .editors
                    .first()
                    .expect("name input")
                    .update(cx, |editor, cx| editor.set_text("origin", window, cx));
                modal.confirm(&menu::Confirm, window, cx);
                assert_eq!(receiver.try_recv().expect("pending input"), None);
                modal
                    .editors
                    .get(1)
                    .expect("URL input")
                    .update(cx, |editor, cx| {
                        editor.set_text(" https://example.com/repository.git ", window, cx)
                    });
                modal.confirm(&menu::Confirm, window, cx);
            });
            assert_eq!(
                receiver.try_recv().expect("submitted input"),
                Some(vec![
                    "origin".to_owned(),
                    "https://example.com/repository.git".to_owned()
                ])
            );
            let (sender, mut receiver) = futures::channel::oneshot::channel();
            modal.update(cx, |modal, cx| {
                modal.sender = Some(sender);
                modal.cancel(&menu::Cancel, window, cx);
            });
            assert!(receiver.try_recv().is_err());
        });
    }
}
