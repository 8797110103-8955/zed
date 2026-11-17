use std::{ops::Range, time::Duration};

use gpui::{AppContext as _, FontWeight, Task, WeakEntity};
use language::BufferSnapshot;
use multi_buffer::MultiBufferRow;
use text::{BufferId, Point};
use ui::prelude::*;

use crate::{
    Editor,
    display_map::{DisplayRow, DisplaySnapshot, ToDisplayPoint},
};

const REGION_TITLE_HEIGHT: Pixels = px(20.);

#[derive(Default)]
struct RegionParser {
    open: Vec<RegionMarker>,
    matched: Vec<RegionMarker>,
}

#[derive(Debug, PartialEq, Eq)]
struct RegionMarker {
    row: u32,
    title: Option<SharedString>,
}

impl RegionParser {
    fn push_line(&mut self, row: u32, line: &str) {
        match region_directive(line) {
            Some(RegionDirective::Start(title)) => {
                self.open.push(RegionMarker {
                    row,
                    title: (!title.is_empty()).then(|| title.into()),
                });
            }
            Some(RegionDirective::End) => {
                self.matched.extend(self.open.pop());
            }
            None => {}
        }
    }

    fn finish(mut self) -> Vec<RegionMarker> {
        self.matched.sort_unstable_by_key(|marker| marker.row);
        self.matched
    }
}

enum RegionDirective<'a> {
    Start(&'a str),
    End,
}

fn region_directive(line: &str) -> Option<RegionDirective<'_>> {
    let line = line.trim();
    let line = if let Some(comment) = line.strip_prefix("//") {
        comment.trim_start()
    } else if let Some(comment) = line.strip_prefix("/*") {
        comment.strip_suffix("*/").unwrap_or(comment).trim()
    } else if let Some(comment) = line.strip_prefix("<!--") {
        comment.strip_suffix("-->")?.trim()
    } else {
        line
    };
    let (mut word, mut rest) = split_word(line.strip_prefix('#')?);
    if word.eq_ignore_ascii_case("pragma") || word.eq_ignore_ascii_case("parama") {
        (word, rest) = split_word(rest);
    }
    if word.eq_ignore_ascii_case("region") {
        let title = rest.strip_prefix(':').unwrap_or(rest).trim();
        let title = title
            .strip_prefix('"')
            .and_then(|title| title.strip_suffix('"'))
            .unwrap_or(title);
        Some(RegionDirective::Start(title))
    } else if word.eq_ignore_ascii_case("endregion")
        || (word.eq_ignore_ascii_case("end") && split_word(rest).0.eq_ignore_ascii_case("region"))
    {
        Some(RegionDirective::End)
    } else {
        None
    }
}

fn split_word(text: &str) -> (&str, &str) {
    let text = text.trim_start();
    let boundary = text.find(|character: char| character.is_whitespace() || character == ':');
    let (word, rest) = text.split_at(boundary.unwrap_or(text.len()));
    (word, rest.trim_start())
}

#[derive(Default)]
pub(crate) struct MinimapRegions {
    version: Option<(BufferId, clock::Global)>,
    markers: Vec<RegionMarker>,
    task: Option<Task<()>>,
}

impl MinimapRegions {
    pub(super) fn refresh(
        &mut self,
        buffer: &BufferSnapshot,
        parent: WeakEntity<Editor>,
        cx: &mut Context<Editor>,
    ) {
        let buffer_id = buffer.remote_id();
        let edited = self
            .version
            .as_ref()
            .is_some_and(|(id, _)| *id == buffer_id);
        if edited
            && self
                .version
                .as_ref()
                .is_some_and(|(_, version)| version == buffer.version())
        {
            return;
        }
        self.version = Some((buffer_id, buffer.version().clone()));
        self.markers.clear();
        let text = buffer.as_rope().clone();
        self.task = Some(cx.spawn(async move |editor, cx| {
            if edited {
                cx.background_executor()
                    .timer(Duration::from_millis(150))
                    .await;
            }
            let markers = cx
                .background_spawn(async move {
                    let mut parser = RegionParser::default();
                    let mut lines = text.chunks().lines();
                    let mut row = 0;
                    while let Some(line) = lines.next() {
                        parser.push_line(row, line);
                        row += 1;
                    }
                    parser.finish()
                })
                .await;
            if let Some(editor) = editor.upgrade() {
                editor.update(cx, |editor, _| {
                    if let Some(regions) = &mut editor.minimap_regions {
                        regions.markers = markers;
                    }
                });
                if let Some(parent) = parent.upgrade() {
                    parent.update(cx, |_, cx| cx.notify());
                }
            }
        }));
    }

    pub(super) fn overlay(
        &self,
        snapshot: &DisplaySnapshot,
        visible: Range<DisplayRow>,
        scroll_top: f64,
        line_height: Pixels,
        cx: &App,
    ) -> Option<AnyElement> {
        if self.markers.is_empty() || visible.is_empty() {
            return None;
        }
        let first_row = visible.start.as_display_point().to_point(snapshot).row;
        let last_row = DisplayRow(visible.end.0 - 1)
            .as_display_point()
            .to_point(snapshot)
            .row;
        let first_marker = self
            .markers
            .partition_point(|marker| marker.row < first_row);
        let end_marker = self
            .markers
            .partition_point(|marker| marker.row <= last_row);
        if first_marker >= end_marker {
            return None;
        }
        let mut next_title_top = px(0.);
        let mut overlay = div().relative().size_full().overflow_hidden();
        for marker in &self.markers[first_marker..end_marker] {
            if snapshot.is_line_folded(MultiBufferRow(marker.row)) {
                continue;
            }
            let row = Point::new(marker.row, 0).to_display_point(snapshot).row();
            let top = (line_height * (f64::from(row.0) - scroll_top) as f32).max(px(0.));
            if !visible.contains(&row) || top < next_title_top {
                continue;
            }
            next_title_top = top + REGION_TITLE_HEIGHT;
            overlay = overlay.child(render_region_title(
                marker.title.clone().unwrap_or_else(|| "region".into()),
                top,
                cx,
            ));
        }
        (next_title_top > px(0.)).then(|| overlay.into_any_element())
    }
}

fn render_region_title(title: SharedString, top: Pixels, cx: &App) -> impl IntoElement {
    let colors = cx.theme().colors();
    div()
        .absolute()
        .top(top)
        .left_0()
        .right_0()
        .h(REGION_TITLE_HEIGHT)
        .flex()
        .items_center()
        .gap(px(4.))
        .font(theme::theme_settings(cx).ui_font(cx).clone())
        .font_weight(FontWeight::MEDIUM)
        // Pixel units keep titles readable independently of the minimap's rem scaling.
        .text_size(px(9.))
        .text_color(colors.text_muted)
        .line_height(relative(1.2))
        .child(
            div()
                .min_w_0()
                .truncate()
                .debug_selector(|| "minimap-region-title".into())
                .child(title),
        )
        .child(
            div()
                .h_px()
                .min_w(px(10.))
                .flex_1()
                .flex_shrink_0()
                .bg(colors.border_variant),
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_minimap_region_markers() {
        let mut parser = RegionParser::default();
        for (row, line) in [
            "#endregion",
            "#pragma region Outer area",
            "  // #region: REGION A",
            "#if region",
            "#regional not a marker",
            "  //#endregion",
            "#pragma endregion Outer area",
            "#parama region Compatibility",
            "#parama endregion",
            "#Region \"VB title\"",
            "#End Region",
            "/* # region CSS */",
            "/* # endregion */",
            "<!-- #region HTML -->",
            "<!-- #endregion -->",
            "#region Unclosed",
        ]
        .into_iter()
        .enumerate()
        {
            parser.push_line(row as u32, line);
        }
        let region = |row, title: &'static str| RegionMarker {
            row,
            title: Some(title.into()),
        };
        assert_eq!(
            parser.finish(),
            vec![
                region(1, "Outer area"),
                region(2, "Region A"),
                region(7, "Compatibility"),
                region(9, "VB title"),
                region(11, "CSS"),
                region(13, "HTML"),
            ],
        );
    }

    #[cfg(debug_assertions)]
    #[gpui::test]
    fn test_minimap_region_title_renders(cx: &mut gpui::TestAppContext) {
        use gpui::UpdateGlobal as _;
        use settings::SettingsStore;

        crate::editor_tests::init_test(cx, |_| {});
        cx.update(|cx| {
            SettingsStore::update_global(cx, |settings, cx| {
                settings
                    .set_user_settings(r#"{"minimap":{"show":"always"}}"#, cx)
                    .expect("valid minimap settings");
            });
        });
        let (editor, cx) = cx.add_window_view(|window, cx| {
            let buffer = crate::MultiBuffer::build_simple(
                "#pragma region Render pipeline\nint value = 1;\n#pragma endregion",
                cx,
            );
            Editor::new(crate::EditorMode::full(), buffer, None, window, cx)
        });
        cx.run_until_parked();

        let title = cx
            .debug_bounds("minimap-region-title")
            .expect("title should be drawn directly in the minimap");
        assert!(title.size.height >= px(12.));
        editor.read_with(cx, |editor, cx| {
            let minimap = editor
                .minimap()
                .expect("minimap should be visible")
                .read(cx);
            let regions = minimap
                .minimap_regions
                .as_ref()
                .expect("minimap region data");
            assert_eq!(
                regions
                    .markers
                    .first()
                    .and_then(|marker| marker.title.as_deref()),
                Some("Render pipeline")
            );
        });
    }
}
