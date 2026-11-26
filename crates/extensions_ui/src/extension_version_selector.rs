use std::str::FromStr;
use std::sync::Arc;

use cloud_api_types::ExtensionMetadata;
use extension_host::ExtensionStore;
use fs::Fs;
use gpui::{App, DismissEvent, Entity, EventEmitter, Focusable, Task, WeakEntity, prelude::*};
use picker::{FuzzyVersionMatches, Picker, PickerDelegate};
use release_channel::ReleaseChannel;
use semver::Version;
use settings::update_settings_file;
use ui::{HighlightedLabel, ListItem, ListItemSpacing, prelude::*};
use util::ResultExt;
use workspace::ModalView;

pub struct ExtensionVersionSelector {
    picker: Entity<Picker<ExtensionVersionSelectorDelegate>>,
}

impl ModalView for ExtensionVersionSelector {}

impl EventEmitter<DismissEvent> for ExtensionVersionSelector {}

impl Focusable for ExtensionVersionSelector {
    fn focus_handle(&self, cx: &App) -> gpui::FocusHandle {
        self.picker.focus_handle(cx)
    }
}

impl Render for ExtensionVersionSelector {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        v_flex().child(self.picker.clone())
    }
}

impl ExtensionVersionSelector {
    pub fn new(
        delegate: ExtensionVersionSelectorDelegate,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let picker = cx.new(|cx| Picker::uniform_list(delegate, window, cx));
        Self { picker }
    }
}

pub struct ExtensionVersionSelectorDelegate {
    fs: Arc<dyn Fs>,
    selector: WeakEntity<ExtensionVersionSelector>,
    extension_versions: Vec<ExtensionMetadata>,
    matches: FuzzyVersionMatches,
}

impl ExtensionVersionSelectorDelegate {
    pub fn new(
        fs: Arc<dyn Fs>,
        selector: WeakEntity<ExtensionVersionSelector>,
        mut extension_versions: Vec<ExtensionMetadata>,
    ) -> Self {
        extension_versions.sort_unstable_by(|a, b| {
            let a_version = Version::from_str(&a.manifest.version);
            let b_version = Version::from_str(&b.manifest.version);

            match (a_version, b_version) {
                (Ok(a_version), Ok(b_version)) => b_version.cmp(&a_version),
                _ => b.published_at.cmp(&a.published_at),
            }
        });

        let matches = FuzzyVersionMatches::new(
            extension_versions
                .iter()
                .map(|extension| format!("v{}", extension.manifest.version)),
        );

        Self {
            fs,
            selector,
            extension_versions,
            matches,
        }
    }
}

impl PickerDelegate for ExtensionVersionSelectorDelegate {
    type ListItem = ui::ListItem;

    fn name() -> &'static str {
        "extension version selector"
    }

    fn placeholder_text(&self, _window: &mut Window, _cx: &mut App) -> Arc<str> {
        "Select extension version...".into()
    }

    fn match_count(&self) -> usize {
        self.matches.matches().len()
    }

    fn selected_index(&self) -> usize {
        self.matches.selected_index()
    }

    fn set_selected_index(
        &mut self,
        ix: usize,
        _window: &mut Window,
        _cx: &mut Context<Picker<Self>>,
    ) {
        self.matches.set_selected_index(ix);
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
        let Some(extension_version) = self
            .matches
            .selected_match()
            .and_then(|version_match| self.extension_versions.get(version_match.candidate_id))
        else {
            self.dismissed(window, cx);
            return;
        };

        if !extension_host::is_version_compatible(ReleaseChannel::global(cx), extension_version) {
            return;
        }

        let extension_store = ExtensionStore::global(cx);
        extension_store.update(cx, |store, cx| {
            let extension_id = extension_version.id.clone();
            let version = extension_version.manifest.version.clone();

            update_settings_file(self.fs.clone(), cx, {
                let extension_id = extension_id.clone();
                move |settings, _| {
                    settings
                        .extension
                        .auto_update_extensions
                        .insert(extension_id, false);
                }
            });

            store.install_extension(extension_id, version, cx);
        });
    }

    fn dismissed(&mut self, _: &mut Window, cx: &mut Context<Picker<Self>>) {
        self.selector.update(cx, |_, cx| cx.emit(DismissEvent)).ok();
    }

    fn render_match(
        &self,
        ix: usize,
        selected: bool,
        _: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> Option<Self::ListItem> {
        let version_match = self.matches.matches().get(ix)?;
        let extension_version = self.extension_versions.get(version_match.candidate_id)?;

        let is_version_compatible =
            extension_host::is_version_compatible(ReleaseChannel::global(cx), extension_version);
        let disabled = !is_version_compatible;

        Some(
            ListItem::new(ix)
                .inset(true)
                .spacing(ListItemSpacing::Sparse)
                .toggle_state(selected)
                .disabled(disabled)
                .child(
                    HighlightedLabel::new(
                        version_match.string.clone(),
                        version_match.positions.clone(),
                    )
                    .when(disabled, |label| label.color(Color::Muted)),
                )
                .end_slot(
                    h_flex()
                        .gap_2()
                        .when(!is_version_compatible, |this| {
                            this.child(Label::new("Incompatible").color(Color::Muted))
                        })
                        .child(
                            Label::new(
                                extension_version
                                    .published_at
                                    .format("%Y-%m-%d")
                                    .to_string(),
                            )
                            .when(disabled, |label| label.color(Color::Muted)),
                        ),
                ),
        )
    }
}

#[cfg(test)]
mod tests {
    use std::{cell::Cell, rc::Rc};

    use fs::FakeFs;
    use gpui::TestAppContext;
    use serde_json::json;
    use settings::SettingsStore;

    use super::*;

    #[gpui::test]
    fn test_extension_version_picker_rejects_incompatible_versions(cx: &mut TestAppContext) {
        let fs = FakeFs::new(cx.executor());
        cx.update(|cx| {
            let settings = SettingsStore::test(cx);
            cx.set_global(settings);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            editor::init(cx);
            release_channel::init_test(Version::new(1, 0, 0), ReleaseChannel::Stable, cx);
        });
        let incompatible: ExtensionMetadata = serde_json::from_value(json!({
            "id": "test-extension",
            "name": "Test Extension",
            "version": "2.0.0",
            "authors": [],
            "repository": "https://example.test/extension",
            "schema_version": i32::MAX,
            "published_at": "2026-01-01T00:00:00Z",
            "download_count": 0,
        }))
        .expect("valid metadata with unsupported schema");
        let (selector, cx) = cx.add_window_view(|window, cx| {
            let delegate =
                ExtensionVersionSelectorDelegate::new(fs, cx.weak_entity(), vec![incompatible]);
            ExtensionVersionSelector::new(delegate, window, cx)
        });
        let picker = selector.read_with(cx, |selector, _| selector.picker.clone());
        let dismissed = Rc::new(Cell::new(false));
        let _subscription = cx.update(|_, cx| {
            let dismissed = dismissed.clone();
            cx.subscribe(&selector, move |_, _: &DismissEvent, _| dismissed.set(true))
        });
        cx.run_until_parked();
        picker.update_in(cx, |picker, window, cx| {
            picker.delegate.confirm(false, window, cx);
        });
        assert!(!dismissed.get());

        picker.update_in(cx, |picker, window, cx| {
            picker.set_query("missing", window, cx)
        });
        cx.run_until_parked();
        picker.update_in(cx, |picker, window, cx| {
            assert_eq!(picker.delegate.match_count(), 0);
            picker.delegate.confirm(false, window, cx);
        });
        assert!(dismissed.get());
    }
}
