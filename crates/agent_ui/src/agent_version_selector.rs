use std::sync::Arc;

use collections::HashMap;
use fs::Fs;
use gpui::{App, DismissEvent, Entity, EventEmitter, Focusable, Task, WeakEntity, prelude::*};
use picker::{FuzzyVersionMatches, Picker, PickerDelegate};
use semver::Version;
use settings::update_settings_file;
use ui::{HighlightedLabel, ListItem, ListItemSpacing, prelude::*};
use util::ResultExt;
use workspace::ModalView;
use zed_actions::agent::SelectAgent;

pub struct AgentVersionSelector {
    picker: Entity<Picker<AgentVersionSelectorDelegate>>,
}

impl ModalView for AgentVersionSelector {}

impl EventEmitter<DismissEvent> for AgentVersionSelector {}

impl Focusable for AgentVersionSelector {
    fn focus_handle(&self, cx: &App) -> gpui::FocusHandle {
        self.picker.focus_handle(cx)
    }
}

impl Render for AgentVersionSelector {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        v_flex().child(self.picker.clone())
    }
}

impl AgentVersionSelector {
    pub fn new(
        delegate: AgentVersionSelectorDelegate,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let picker = cx.new(|cx| Picker::uniform_list(delegate, window, cx));
        Self { picker }
    }
}

pub struct AgentVersionSelectorDelegate {
    fs: Arc<dyn Fs>,
    selector: WeakEntity<AgentVersionSelector>,
    agent_id: String,
    versions: Vec<Version>,
    matches: FuzzyVersionMatches,
}

impl AgentVersionSelectorDelegate {
    pub fn new(
        fs: Arc<dyn Fs>,
        selector: WeakEntity<AgentVersionSelector>,
        agent_id: String,
        mut versions: Vec<Version>,
    ) -> Self {
        versions.sort_unstable_by(|left, right| right.cmp(left));
        versions.dedup();

        let matches =
            FuzzyVersionMatches::new(versions.iter().map(|version| format!("v{version}")));

        Self {
            fs,
            selector,
            agent_id,
            versions,
            matches,
        }
    }
}

impl PickerDelegate for AgentVersionSelectorDelegate {
    type ListItem = ui::ListItem;

    fn name() -> &'static str {
        "agent version selector"
    }

    fn placeholder_text(&self, _window: &mut Window, _cx: &mut App) -> Arc<str> {
        "Select agent version...".into()
    }

    fn match_count(&self) -> usize {
        self.matches.matches().len()
    }

    fn selected_index(&self) -> usize {
        self.matches.selected_index()
    }

    fn set_selected_index(
        &mut self,
        index: usize,
        _window: &mut Window,
        _cx: &mut Context<Picker<Self>>,
    ) {
        self.matches.set_selected_index(index);
    }

    fn update_matches(
        &mut self,
        query: String,
        window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> Task<()> {
        let matches = self.matches.filter(query, cx.background_executor().clone());

        cx.spawn_in(window, async move |this, cx| {
            let matches = matches.await;
            this.update(cx, |this, _cx| {
                this.delegate.matches.replace_matches(matches);
            })
            .log_err();
        })
    }

    fn confirm(&mut self, _secondary: bool, window: &mut Window, cx: &mut Context<Picker<Self>>) {
        let Some(version_match) = self.matches.selected_match() else {
            self.dismissed(window, cx);
            return;
        };
        let Some(version) = self.versions.get(version_match.candidate_id) else {
            self.dismissed(window, cx);
            return;
        };

        let agent_id = self.agent_id.clone();
        let version = version.to_string();
        update_settings_file(self.fs.clone(), cx, {
            let agent_id = agent_id.clone();
            move |settings, _| {
                let agent_servers = settings.agent_servers.get_or_insert_default();
                match agent_servers.get_mut(agent_id.as_str()) {
                    Some(settings::CustomAgentServerSettings::Registry {
                        package_version, ..
                    }) => {
                        *package_version = Some(version);
                    }
                    Some(settings::CustomAgentServerSettings::Custom { .. }) => {}
                    None => {
                        agent_servers.insert(
                            agent_id,
                            settings::CustomAgentServerSettings::Registry {
                                package_version: Some(version),
                                env: HashMap::default(),
                                default_mode: None,
                                default_config_options: HashMap::default(),
                                favorite_config_option_values: HashMap::default(),
                            },
                        );
                    }
                }
            }
        });
        window.dispatch_action(Box::new(SelectAgent { agent: agent_id }), cx);
        self.dismissed(window, cx);
    }

    fn dismissed(&mut self, _window: &mut Window, cx: &mut Context<Picker<Self>>) {
        self.selector.update(cx, |_, cx| cx.emit(DismissEvent)).ok();
    }

    fn render_match(
        &self,
        index: usize,
        selected: bool,
        _window: &mut Window,
        _cx: &mut Context<Picker<Self>>,
    ) -> Option<Self::ListItem> {
        let version_match = self.matches.matches().get(index)?;

        Some(
            ListItem::new(index)
                .inset(true)
                .spacing(ListItemSpacing::Sparse)
                .toggle_state(selected)
                .child(HighlightedLabel::new(
                    version_match.string.clone(),
                    version_match.positions.clone(),
                )),
        )
    }
}

#[cfg(test)]
mod tests {
    use std::{cell::Cell, rc::Rc};

    use fs::FakeFs;
    use gpui::TestAppContext;
    use settings::SettingsStore;

    use super::*;

    #[gpui::test]
    async fn test_agent_version_picker_writes_the_filtered_version(cx: &mut TestAppContext) {
        let fs = FakeFs::new(cx.executor());
        let settings_path = paths::settings_file();
        fs.create_dir(settings_path.parent().expect("settings directory"))
            .await
            .expect("create settings directory");
        fs.insert_file(settings_path, b"{}".to_vec()).await;
        cx.update(|cx| {
            let mut settings = SettingsStore::test(cx);
            settings.watch_settings_files(fs.clone(), cx, |_, _, _| {});
            cx.set_global(settings);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            editor::init(cx);
        });

        let (selector, cx) = cx.add_window_view(|window, cx| {
            let delegate = AgentVersionSelectorDelegate::new(
                fs.clone(),
                cx.weak_entity(),
                "test-agent".to_string(),
                vec![
                    Version::new(1, 0, 0),
                    Version::new(3, 0, 0),
                    Version::new(2, 0, 0),
                ],
            );
            AgentVersionSelector::new(delegate, window, cx)
        });
        let picker = selector.read_with(cx, |selector, _| selector.picker.clone());
        let dismissals = Rc::new(Cell::new(0));
        let _subscription = cx.update(|_, cx| {
            let dismissals = dismissals.clone();
            cx.subscribe(&selector, move |_, _: &DismissEvent, _| {
                dismissals.set(dismissals.get() + 1);
            })
        });

        picker.update_in(cx, |picker, window, cx| {
            picker.set_query("missing", window, cx)
        });
        cx.run_until_parked();
        picker.update_in(cx, |picker, window, cx| {
            picker.delegate.confirm(false, window, cx);
        });
        assert_eq!(dismissals.get(), 1);
        assert_eq!(fs.load(settings_path).await.expect("read settings"), "{}");

        picker.update_in(cx, |picker, window, cx| {
            picker.set_query("2.0.0", window, cx)
        });
        cx.run_until_parked();
        picker.update_in(cx, |picker, window, cx| {
            picker.delegate.confirm(false, window, cx);
        });
        cx.run_until_parked();
        assert_eq!(dismissals.get(), 2);
        let content = fs.load(settings_path).await.expect("load written settings");
        let settings: serde_json::Value =
            serde_json::from_str(&content).expect("valid settings JSON");
        assert_eq!(
            settings["agent_servers"]["test-agent"]["package_version"],
            "2.0.0"
        );
    }
}
