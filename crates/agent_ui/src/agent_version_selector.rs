use std::sync::Arc;

use collections::HashMap;
use fs::Fs;
use fuzzy::{StringMatch, StringMatchCandidate};
use gpui::{App, DismissEvent, Entity, EventEmitter, Focusable, Task, WeakEntity, prelude::*};
use picker::{Picker, PickerDelegate};
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
    selected_index: usize,
    matches: Vec<StringMatch>,
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

        let matches = versions
            .iter()
            .map(|version| StringMatch {
                candidate_id: 0,
                score: 0.0,
                positions: Default::default(),
                string: format!("v{version}"),
            })
            .collect();

        Self {
            fs,
            selector,
            agent_id,
            versions,
            selected_index: 0,
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
        self.matches.len()
    }

    fn selected_index(&self) -> usize {
        self.selected_index
    }

    fn set_selected_index(
        &mut self,
        index: usize,
        _window: &mut Window,
        _cx: &mut Context<Picker<Self>>,
    ) {
        self.selected_index = index;
    }

    fn update_matches(
        &mut self,
        query: String,
        window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> Task<()> {
        let background_executor = cx.background_executor().clone();
        let candidates = self
            .versions
            .iter()
            .enumerate()
            .map(|(index, version)| StringMatchCandidate::new(index, &format!("v{version}")))
            .collect::<Vec<_>>();

        cx.spawn_in(window, async move |this, cx| {
            let matches = if query.is_empty() {
                candidates
                    .into_iter()
                    .enumerate()
                    .map(|(index, candidate)| StringMatch {
                        candidate_id: index,
                        string: candidate.string,
                        positions: Vec::new(),
                        score: 0.0,
                    })
                    .collect()
            } else {
                fuzzy::match_strings(
                    &candidates,
                    &query,
                    false,
                    true,
                    100,
                    &Default::default(),
                    background_executor,
                )
                .await
            };

            this.update(cx, |this, _cx| {
                this.delegate.matches = matches;
                this.delegate.selected_index = this
                    .delegate
                    .selected_index
                    .min(this.delegate.matches.len().saturating_sub(1));
            })
            .log_err();
        })
    }

    fn confirm(&mut self, _secondary: bool, window: &mut Window, cx: &mut Context<Picker<Self>>) {
        let Some(version_match) = self.matches.get(self.selected_index) else {
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
            let version = version.clone();
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
        let version_match = self.matches.get(index)?;

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
