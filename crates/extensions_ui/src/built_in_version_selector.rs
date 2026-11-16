use std::sync::Arc;

use gpui::{
    App, Context, DismissEvent, Entity, EventEmitter, Focusable, Task, WeakEntity, Window,
    prelude::*,
};
use language::LanguageServerName;
use picker::{Picker, PickerDelegate};
use project::built_in_language_servers::BuiltInLanguageServers;
use semver::Version;
use ui::{ListItem, ListItemSpacing, prelude::*};
use util::ResultExt as _;
use workspace::ModalView;

pub struct BuiltInVersionSelector {
    picker: Entity<Picker<BuiltInVersionDelegate>>,
}

impl ModalView for BuiltInVersionSelector {}
impl EventEmitter<DismissEvent> for BuiltInVersionSelector {}

impl Focusable for BuiltInVersionSelector {
    fn focus_handle(&self, cx: &App) -> gpui::FocusHandle {
        self.picker.focus_handle(cx)
    }
}

impl Render for BuiltInVersionSelector {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        v_flex().child(self.picker.clone())
    }
}

impl BuiltInVersionSelector {
    pub fn new(
        store: Entity<BuiltInLanguageServers>,
        name: LanguageServerName,
        versions: Vec<Version>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let selector = cx.weak_entity();
        let picker = cx.new(|cx| {
            Picker::uniform_list(
                BuiltInVersionDelegate {
                    store,
                    name,
                    matches: versions.clone(),
                    versions,
                    selected_index: 0,
                    selector,
                },
                window,
                cx,
            )
        });
        Self { picker }
    }
}

struct BuiltInVersionDelegate {
    store: Entity<BuiltInLanguageServers>,
    name: LanguageServerName,
    versions: Vec<Version>,
    matches: Vec<Version>,
    selected_index: usize,
    selector: WeakEntity<BuiltInVersionSelector>,
}

impl PickerDelegate for BuiltInVersionDelegate {
    type ListItem = ListItem;
    fn name() -> &'static str {
        "built-in language server version selector"
    }
    fn placeholder_text(&self, _: &mut Window, _: &mut App) -> Arc<str> {
        format!("Select {} version...", self.name).into()
    }
    fn match_count(&self) -> usize {
        self.matches.len()
    }
    fn selected_index(&self) -> usize {
        self.selected_index
    }
    fn set_selected_index(&mut self, index: usize, _: &mut Window, _: &mut Context<Picker<Self>>) {
        self.selected_index = index;
    }
    fn update_matches(
        &mut self,
        query: String,
        _: &mut Window,
        _: &mut Context<Picker<Self>>,
    ) -> Task<()> {
        let query = query.trim_start_matches('v');
        self.matches = self
            .versions
            .iter()
            .filter(|version| version.to_string().contains(query))
            .cloned()
            .collect();
        self.selected_index = 0;
        Task::ready(())
    }
    fn confirm(&mut self, _: bool, window: &mut Window, cx: &mut Context<Picker<Self>>) {
        if let Some(version) = self.matches.get(self.selected_index).cloned() {
            self.store.update(cx, |store, cx| {
                store.install(self.name.clone(), Some(version), cx)
            });
        }
        self.dismissed(window, cx);
    }
    fn dismissed(&mut self, _: &mut Window, cx: &mut Context<Picker<Self>>) {
        self.selector
            .update(cx, |_, cx| cx.emit(DismissEvent))
            .log_err();
    }
    fn render_match(
        &self,
        index: usize,
        selected: bool,
        _: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> Option<ListItem> {
        let version = self.matches.get(index)?.to_string();
        let installed = self
            .store
            .read(cx)
            .state(&self.name)
            .and_then(|state| state.version.as_deref())
            == Some(version.as_str());
        Some(
            ListItem::new(index)
                .inset(true)
                .spacing(ListItemSpacing::Sparse)
                .toggle_state(selected)
                .child(Label::new(format!("v{version}")))
                .when(installed, |item| {
                    item.end_slot(Label::new("Installed").color(Color::Muted))
                }),
        )
    }
}
