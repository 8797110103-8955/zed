// FORK: Zoom, fit-to-view and resize handling are reimplemented here after
// `image_viewer::ImageView`; see the FORK notes at the top of
// `crates/image_viewer/src/image_viewer.rs` for the gaps found there.

use std::collections::HashSet;
use std::mem;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use editor::Editor;
use file_icons::FileIcons;
use gpui::{
    AnyElement, App, Bounds, Context, DispatchPhase, Element, ElementId, Entity, EntityId,
    EventEmitter, FocusHandle, Focusable, Global, GlobalElementId, InspectorElementId, IntoElement,
    LayoutId, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, ParentElement, PinchEvent,
    Pixels, Point, Render, RenderImage, ScrollDelta, ScrollEvent, Size, Style, Styled,
    Subscription, Task, WeakEntity, Window, div, img, point, px, size,
};
use language::{Buffer, BufferEvent};
use multi_buffer::MultiBuffer;
use project::ProjectPath;
use ui::{Tooltip, prelude::*};
use workspace::item::{Item, ItemHandle};
use workspace::notifications::NotifyResultExt;
use workspace::{Pane, Workspace};

use crate::{OpenFollowingPreview, OpenPreview, OpenPreviewToTheSide};

/// Zoom is relative to the "contain" fit, so 1.0 fills the view.
const MIN_RELATIVE_ZOOM: f32 = 0.05;
const MAX_RELATIVE_ZOOM: f32 = 100.0;
const SCROLL_LINE_MULTIPLIER: f32 = 20.0;
const FIT_EPSILON: f32 = 0.001;
/// Re-rendering is skipped while the needed resolution stays this close to the
/// rendered one.
const RENDER_SCALE_TOLERANCE: f32 = 0.05;
const MIN_RENDER_SCALE: f32 = 0.1;
/// Bounds the longest side of the rasterized SVG to keep it within the atlas.
const MAX_RENDER_DIMENSION: f32 = 8192.0;
const RERENDER_DELAY: Duration = Duration::from_millis(120);

/// Editors the user switched to from a preview, which must not be turned back
/// into previews when they are added to a pane.
#[derive(Default)]
struct SourceModeEditors(HashSet<EntityId>);

impl Global for SourceModeEditors {}

#[derive(Clone)]
struct SvgPreviewImage {
    image: Arc<RenderImage>,
    size: Size<f32>,
}

pub struct SvgPreviewView {
    focus_handle: FocusHandle,
    workspace: WeakEntity<Workspace>,
    mode: SvgPreviewMode,
    buffer: Option<Entity<Buffer>>,
    current_svg: Option<Result<SvgPreviewImage, SharedString>>,
    relative_zoom: f32,
    /// Measured in unscaled image pixels so that resizing the view scales the pan
    /// together with the image.
    pan_offset: Point<f32>,
    /// The zoom and pan to return to when leaving the fit state.
    restore_view: Option<(f32, Point<f32>)>,
    /// Device pixels per SVG unit wanted by the latest render request.
    requested_render_scale: f32,
    last_mouse_position: Option<Point<Pixels>>,
    container_bounds: Option<Bounds<Pixels>>,
    _refresh: Task<()>,
    _buffer_subscription: Option<Subscription>,
    _workspace_subscription: Option<Subscription>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum SvgPreviewMode {
    /// The preview will always show the contents of the provided editor.
    Default,
    /// The preview will "follow" the last active editor of an SVG file.
    Follow,
}

impl SvgPreviewView {
    pub fn new(
        mode: SvgPreviewMode,
        active_buffer: Entity<MultiBuffer>,
        workspace_handle: WeakEntity<Workspace>,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) -> Entity<Self> {
        cx.new(|cx| {
            let workspace_subscription = if mode == SvgPreviewMode::Follow
                && let Some(workspace) = workspace_handle.upgrade()
            {
                Some(Self::subscribe_to_workspace(workspace, window, cx))
            } else {
                None
            };

            let buffer = active_buffer.read_with(cx, |buffer, _cx| buffer.as_singleton());

            let subscription = buffer
                .as_ref()
                .map(|buffer| Self::create_buffer_subscription(buffer, window, cx));

            let mut this = Self {
                focus_handle: cx.focus_handle(),
                workspace: workspace_handle,
                mode,
                buffer,
                current_svg: None,
                relative_zoom: 1.0,
                pan_offset: Point::default(),
                restore_view: None,
                requested_render_scale: 1.0,
                last_mouse_position: None,
                container_bounds: None,
                _buffer_subscription: subscription,
                _workspace_subscription: workspace_subscription,
                _refresh: Task::ready(()),
            };
            this.render_image(window, cx);

            this
        })
    }

    fn subscribe_to_workspace(
        workspace: Entity<Workspace>,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Subscription {
        cx.subscribe_in(
            &workspace,
            window,
            move |this: &mut SvgPreviewView, workspace, event: &workspace::Event, window, cx| {
                if let workspace::Event::ActiveItemChanged = event {
                    let workspace = workspace.read(cx);
                    if let Some(active_item) = workspace.active_item(cx)
                        && let Some(buffer) = active_item.downcast::<MultiBuffer>()
                        && Self::is_svg_file(&buffer, cx)
                    {
                        let Some(buffer) = buffer.read(cx).as_singleton() else {
                            return;
                        };
                        if this.buffer.as_ref() != Some(&buffer) {
                            this._buffer_subscription =
                                Some(Self::create_buffer_subscription(&buffer, window, cx));
                            this.buffer = Some(buffer);
                            this.render_image(window, cx);
                            cx.notify();
                        }
                    } else {
                        this.set_current(None, window, cx);
                    }
                }
            },
        )
    }

    fn render_image(&mut self, window: &Window, cx: &mut Context<Self>) {
        self.render_image_after(None, window, cx);
    }

    /// Rasterizes the SVG at `requested_render_scale` so it stays sharp at the
    /// current zoom instead of stretching a fixed-size bitmap.
    fn render_image_after(
        &mut self,
        delay: Option<Duration>,
        window: &Window,
        cx: &mut Context<Self>,
    ) {
        let Some(buffer) = self.buffer.as_ref() else {
            return;
        };
        let render_scale = self.requested_render_scale;
        let renderer = cx.svg_renderer();
        let content = buffer.read(cx).snapshot();

        self._refresh = cx.spawn_in(window, async move |this, cx| {
            if let Some(delay) = delay {
                cx.background_executor().timer(delay).await;
            }
            let result = cx
                .background_spawn(async move {
                    let svg = renderer.parse_svg(content.text().as_bytes())?;
                    renderer
                        .render_parsed(&svg, render_scale)
                        .map(|image| SvgPreviewImage {
                            image,
                            size: svg.size(),
                        })
                })
                .await;

            this.update_in(cx, |view, window, cx| {
                let current = result.map_err(|e| e.to_string().into());
                view.set_current(Some(current), window, cx);
            })
            .ok();
        });
    }

    fn request_render_scale(
        &mut self,
        zoom: f32,
        delay: Option<Duration>,
        window: &Window,
        cx: &mut Context<Self>,
    ) {
        let Some(image_size) = self.image_size() else {
            return;
        };
        let max_render_scale = MAX_RENDER_DIMENSION / image_size.width.max(image_size.height);
        let render_scale = (zoom * window.scale_factor())
            .min(max_render_scale)
            .max(MIN_RENDER_SCALE);
        if (render_scale / self.requested_render_scale - 1.0).abs() > RENDER_SCALE_TOLERANCE {
            self.requested_render_scale = render_scale;
            self.render_image_after(delay, window, cx);
        }
    }

    pub fn open_for_project_path(
        project_path: ProjectPath,
        workspace: &mut Workspace,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        let open_buffer = workspace
            .project()
            .update(cx, |project, cx| project.open_buffer(project_path, cx));

        cx.spawn_in(window, async move |workspace, cx| {
            let Some(buffer) = open_buffer
                .await
                .notify_workspace_async_err(workspace.clone(), cx)
            else {
                return;
            };

            workspace
                .update_in(cx, |workspace, window, cx| {
                    let buffer = cx.new(|cx| MultiBuffer::singleton(buffer, cx));
                    let pane = workspace.active_pane().clone();
                    Self::open_preview_in_pane(workspace, buffer, pane, window, cx);
                })
                .ok();
        })
        .detach();
    }

    fn set_current(
        &mut self,
        image: Option<Result<SvgPreviewImage, SharedString>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(Ok(image)) = mem::replace(&mut self.current_svg, image) {
            window.drop_image(image.image).ok();
        }
        cx.notify();
    }

    fn find_existing_preview_item_idx(
        pane: &Pane,
        buffer: &Entity<MultiBuffer>,
        cx: &App,
    ) -> Option<usize> {
        let buffer_id = buffer.read(cx).as_singleton()?.entity_id();
        pane.items_of_type::<SvgPreviewView>()
            .find(|view| {
                view.read(cx)
                    .buffer
                    .as_ref()
                    .is_some_and(|buffer| buffer.entity_id() == buffer_id)
            })
            .and_then(|view| pane.index_for_item(&view))
    }

    pub fn resolve_active_item_as_svg_buffer(
        workspace: &Workspace,
        cx: &mut Context<Workspace>,
    ) -> Option<Entity<MultiBuffer>> {
        workspace
            .active_item(cx)?
            .act_as::<MultiBuffer>(cx)
            .filter(|buffer| Self::is_svg_file(&buffer, cx))
    }

    fn create_svg_view(
        mode: SvgPreviewMode,
        workspace: &mut Workspace,
        buffer: Entity<MultiBuffer>,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) -> Entity<SvgPreviewView> {
        let workspace_handle = workspace.weak_handle();
        SvgPreviewView::new(mode, buffer, workspace_handle, window, cx)
    }

    fn create_buffer_subscription(
        buffer: &Entity<Buffer>,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Subscription {
        cx.subscribe_in(
            buffer,
            window,
            move |this, _buffer, event: &BufferEvent, window, cx| match event {
                BufferEvent::Edited { .. } | BufferEvent::Saved => {
                    this.render_image(window, cx);
                }
                _ => {}
            },
        )
    }

    pub fn is_svg_path(path: &Path) -> bool {
        path.extension()
            .is_some_and(|extension| extension.eq_ignore_ascii_case("svg"))
    }

    pub fn is_svg_file(buffer: &Entity<MultiBuffer>, cx: &App) -> bool {
        buffer
            .read(cx)
            .as_singleton()
            .and_then(|buffer| buffer.read(cx).file())
            .is_some_and(|file| Self::is_svg_path(Path::new(file.file_name(cx))))
    }

    pub fn open_preview_in_pane(
        workspace: &mut Workspace,
        buffer: Entity<MultiBuffer>,
        pane: Entity<Pane>,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        Self::activate_or_add_preview(workspace, buffer, pane, true, window, cx);
    }

    pub fn open_preview_to_the_side_of_pane(
        workspace: &mut Workspace,
        buffer: Entity<MultiBuffer>,
        origin_pane: Entity<Pane>,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        let target_pane = workspace.adjacent_pane_of(&origin_pane, window, cx);
        Self::activate_or_add_preview(workspace, buffer, target_pane, false, window, cx);
    }

    fn activate_or_add_preview(
        workspace: &mut Workspace,
        buffer: Entity<MultiBuffer>,
        pane: Entity<Pane>,
        focus: bool,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        let existing_view_idx = Self::find_existing_preview_item_idx(pane.read(cx), &buffer, cx);
        if let Some(existing_view_idx) = existing_view_idx {
            pane.update(cx, |pane, cx| {
                pane.activate_item(existing_view_idx, focus, focus, window, cx);
            });
        } else {
            let view =
                Self::create_svg_view(SvgPreviewMode::Default, workspace, buffer, window, cx);
            pane.update(cx, |pane, cx| {
                pane.add_item(Box::new(view), focus, focus, None, window, cx)
            });
        }
        cx.notify();
    }

    fn image_size(&self) -> Option<Size<f32>> {
        let Some(Ok(image)) = self.current_svg.as_ref() else {
            return None;
        };
        Some(image.size)
    }

    fn fit_zoom(container_bounds: Bounds<Pixels>, image_size: Size<f32>) -> f32 {
        let container_width: f32 = container_bounds.size.width.into();
        let container_height: f32 = container_bounds.size.height.into();
        (container_width / image_size.width).min(container_height / image_size.height)
    }

    fn current_fit_zoom(&self) -> Option<f32> {
        let fit_zoom = Self::fit_zoom(self.container_bounds?, self.image_size()?);
        (fit_zoom > 0.0).then_some(fit_zoom)
    }

    fn is_fit(&self) -> bool {
        (self.relative_zoom - 1.0).abs() < FIT_EPSILON
            && self.pan_offset.x.abs() < FIT_EPSILON
            && self.pan_offset.y.abs() < FIT_EPSILON
    }

    fn toggle_fit(&mut self, cx: &mut Context<Self>) {
        if self.is_fit() {
            let actual_size_view = self
                .current_fit_zoom()
                .map(|fit_zoom| (1.0 / fit_zoom, Point::default()));
            if let Some((relative_zoom, pan_offset)) = self.restore_view.take().or(actual_size_view)
            {
                self.relative_zoom = relative_zoom.clamp(MIN_RELATIVE_ZOOM, MAX_RELATIVE_ZOOM);
                self.pan_offset = pan_offset;
            }
        } else {
            self.restore_view = Some((self.relative_zoom, self.pan_offset));
            self.relative_zoom = 1.0;
            self.pan_offset = Point::default();
        }
        cx.notify();
    }

    fn zoom_at(&mut self, factor: f32, position: Point<Pixels>, cx: &mut Context<Self>) {
        let (Some(fit_zoom), Some(bounds)) = (self.current_fit_zoom(), self.container_bounds)
        else {
            return;
        };
        let old_zoom = self.relative_zoom;
        self.relative_zoom = (old_zoom * factor).clamp(MIN_RELATIVE_ZOOM, MAX_RELATIVE_ZOOM);
        let zoom_ratio = self.relative_zoom / old_zoom;

        let center = bounds.center();
        let mouse_offset = point(
            f32::from(position.x - center.x) / fit_zoom,
            f32::from(position.y - center.y) / fit_zoom,
        );
        self.pan_offset = point(
            self.pan_offset.x + (mouse_offset.x - self.pan_offset.x) * (1.0 - zoom_ratio),
            self.pan_offset.y + (mouse_offset.y - self.pan_offset.y) * (1.0 - zoom_ratio),
        );
        cx.notify();
    }

    fn handle_scroll_wheel(
        &mut self,
        event: &ScrollEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let delta: f32 = match event.delta {
            ScrollDelta::Pixels(pixels) => pixels.y.into(),
            ScrollDelta::Lines(lines) => lines.y * SCROLL_LINE_MULTIPLIER,
        };
        let factor = if delta > 0.0 {
            1.0 + delta.abs() * 0.01
        } else {
            1.0 / (1.0 + delta.abs() * 0.01)
        };
        self.zoom_at(factor, event.position, cx);
    }

    fn handle_pinch(&mut self, event: &PinchEvent, _window: &mut Window, cx: &mut Context<Self>) {
        self.zoom_at(1.0 + event.delta, event.position, cx);
    }

    fn handle_mouse_down(
        &mut self,
        event: &MouseDownEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.last_mouse_position = Some(event.position);
        cx.notify();
    }

    fn handle_mouse_up(&mut self, _: &MouseUpEvent, _window: &mut Window, cx: &mut Context<Self>) {
        self.last_mouse_position = None;
        cx.notify();
    }

    fn handle_mouse_move(
        &mut self,
        event: &MouseMoveEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(last_position) = self.last_mouse_position else {
            return;
        };
        if let Some(fit_zoom) = self.current_fit_zoom() {
            let delta = event.position - last_position;
            self.pan_offset = point(
                self.pan_offset.x + f32::from(delta.x) / fit_zoom,
                self.pan_offset.y + f32::from(delta.y) / fit_zoom,
            );
        }
        self.last_mouse_position = Some(event.position);
        cx.notify();
    }

    /// Replaces this preview with a regular editor for the same buffer.
    fn show_source(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (Some(buffer), Some(workspace)) = (self.buffer.clone(), self.workspace.upgrade())
        else {
            return;
        };
        let preview_id = cx.entity_id();
        window.defer(cx, move |window, cx| {
            workspace.update(cx, |workspace, cx| {
                let Some(pane) = workspace.pane_for_item_id(preview_id) else {
                    return;
                };
                let project = workspace.project().clone();
                let editor = cx.new(|cx| Editor::for_buffer(buffer, Some(project), window, cx));
                cx.default_global::<SourceModeEditors>()
                    .0
                    .insert(editor.entity_id());
                Self::replace_pane_item(&pane, preview_id, Box::new(editor), true, window, cx);
            });
        });
    }

    fn replace_pane_item(
        pane: &Entity<Pane>,
        old_item_id: EntityId,
        new_item: Box<dyn ItemHandle>,
        focus: bool,
        window: &mut Window,
        cx: &mut App,
    ) {
        pane.update(cx, |pane, cx| {
            let index = pane.items().position(|item| item.item_id() == old_item_id);
            let is_active = pane
                .active_item()
                .is_some_and(|item| item.item_id() == old_item_id);
            let was_preview = pane.is_active_preview_item(old_item_id);
            let new_item_id = new_item.item_id();
            pane.add_item_inner(new_item, focus, focus, is_active, index, window, cx);
            if was_preview {
                pane.replace_preview_item_id(new_item_id, window, cx);
            } else {
                pane.remove_item(old_item_id, false, false, window, cx);
            }
        });
    }

    /// Shows SVG files as a preview by default when an editor for one is
    /// opened in a pane.
    fn preview_added_svg_editor(
        workspace: &mut Workspace,
        item: &dyn ItemHandle,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        if item.downcast::<Editor>().is_none()
            || item.is_dirty(cx)
            || cx
                .try_global::<SourceModeEditors>()
                .is_some_and(|editors| editors.0.contains(&item.item_id()))
        {
            return;
        }
        let Some(buffer) = item.act_as::<MultiBuffer>(cx) else {
            return;
        };
        if !Self::is_svg_file(&buffer, cx) {
            return;
        }
        let Some(pane) = workspace.pane_for(item) else {
            return;
        };
        let focus = item.item_focus_handle(cx).contains_focused(window, cx);

        if let Some(existing_index) =
            Self::find_existing_preview_item_idx(pane.read(cx), &buffer, cx)
        {
            let item_id = item.item_id();
            pane.update(cx, |pane, cx| {
                pane.activate_item(existing_index, focus, focus, window, cx);
                pane.remove_item(item_id, false, false, window, cx);
            });
            return;
        }

        let view = Self::create_svg_view(SvgPreviewMode::Default, workspace, buffer, window, cx);
        Self::replace_pane_item(&pane, item.item_id(), Box::new(view), focus, window, cx);
    }

    pub fn register(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
        cx.subscribe_in(
            &cx.entity(),
            window,
            |workspace, _, event: &workspace::Event, window, cx| {
                if let workspace::Event::ItemAdded { item } = event {
                    Self::preview_added_svg_editor(workspace, item.as_ref(), window, cx);
                }
            },
        )
        .detach();

        workspace.register_action(move |workspace, _: &OpenPreview, window, cx| {
            if let Some(item) = workspace.item_for_action(window, cx)
                && let Some(buffer) = item.act_as::<MultiBuffer>(cx)
                && Self::is_svg_file(&buffer, cx)
                && let Some(pane) = workspace.pane_for_item_id(item.item_id())
            {
                Self::open_preview_in_pane(workspace, buffer, pane, window, cx);
            }
        });

        workspace.register_action(move |workspace, _: &OpenPreviewToTheSide, window, cx| {
            if let Some(buffer) = Self::resolve_active_item_as_svg_buffer(workspace, cx) {
                let pane = workspace.active_pane().clone();
                Self::open_preview_to_the_side_of_pane(workspace, buffer, pane, window, cx);
            }
        });

        workspace.register_action(move |workspace, _: &OpenFollowingPreview, window, cx| {
            if let Some(editor) = Self::resolve_active_item_as_svg_buffer(workspace, cx)
                && Self::is_svg_file(&editor, cx)
            {
                let view =
                    Self::create_svg_view(SvgPreviewMode::Follow, workspace, editor, window, cx);
                workspace.active_pane().update(cx, |pane, cx| {
                    pane.add_item(Box::new(view), true, true, None, window, cx)
                });
                cx.notify();
            }
        });
    }
}

impl Render for SvgPreviewView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors();
        let is_fit = self.is_fit();
        // Occluding keeps clicks on the toolbar from starting a drag or zoom in
        // the preview underneath.
        let toolbar = h_flex()
            .occlude()
            .absolute()
            .top_2()
            .right_2()
            .gap_0p5()
            .p_0p5()
            .rounded_md()
            .border_1()
            .border_color(colors.border_variant)
            .bg(colors.elevated_surface_background)
            .when(self.mode == SvgPreviewMode::Default, |this| {
                this.child(
                    IconButton::new("svg-preview-show-source", IconName::Eye)
                        .icon_size(IconSize::Small)
                        .toggle_state(true)
                        .selected_icon_color(Color::Accent)
                        .tooltip(Tooltip::text("Show Source"))
                        .on_click(cx.listener(|this, _, window, cx| this.show_source(window, cx))),
                )
            })
            .when(self.image_size().is_some(), |this| {
                this.child(
                    IconButton::new("svg-preview-fit", IconName::Maximize)
                        .icon_size(IconSize::Small)
                        .toggle_state(is_fit)
                        .selected_icon_color(Color::Accent)
                        .tooltip(Tooltip::text(if is_fit {
                            "Restore Previous Size"
                        } else {
                            "Fit to View"
                        }))
                        .on_click(cx.listener(|this, _, _, cx| this.toggle_fit(cx))),
                )
            });

        v_flex()
            .id("SvgPreview")
            .key_context("SvgPreview")
            .track_focus(&self.focus_handle(cx))
            .size_full()
            .relative()
            .bg(colors.editor_background)
            .flex()
            .justify_center()
            .items_center()
            .map(|this| match self.current_svg.clone() {
                Some(Ok(_)) => this.child(
                    div()
                        .id("svg-preview-container")
                        .size_full()
                        .overflow_hidden()
                        .cursor(if self.last_mouse_position.is_some() {
                            gpui::CursorStyle::ClosedHand
                        } else {
                            gpui::CursorStyle::OpenHand
                        })
                        .on_scroll(cx.listener(Self::handle_scroll_wheel))
                        .on_pinch(cx.listener(Self::handle_pinch))
                        .on_mouse_down(MouseButton::Left, cx.listener(Self::handle_mouse_down))
                        .on_mouse_down(MouseButton::Middle, cx.listener(Self::handle_mouse_down))
                        .on_mouse_up(MouseButton::Left, cx.listener(Self::handle_mouse_up))
                        .on_mouse_up(MouseButton::Middle, cx.listener(Self::handle_mouse_up))
                        .on_mouse_move(cx.listener(Self::handle_mouse_move))
                        .child(SvgContentElement { view: cx.entity() }),
                ),
                Some(Err(e)) => this.child(div().p_4().child(e).into_any_element()),
                None => this.child(div().p_4().child("No SVG file selected")),
            })
            .child(toolbar)
    }
}

struct SvgContentElement {
    view: Entity<SvgPreviewView>,
}

impl Element for SvgContentElement {
    type RequestLayoutState = ();
    type PrepaintState = Option<(AnyElement, bool)>;

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        (
            window.request_layout(
                Style {
                    size: size(relative(1.).into(), relative(1.).into()),
                    ..Default::default()
                },
                [],
                cx,
            ),
            (),
        )
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        let is_initial_layout = self.view.update(cx, |view, _cx| {
            let is_initial_layout = view.container_bounds.is_none();
            view.container_bounds = Some(bounds);
            is_initial_layout
        });

        let view = self.view.read(cx);
        let Some(Ok(image)) = view.current_svg.clone() else {
            return None;
        };
        let image_size = view.image_size()?;
        let is_dragging = view.last_mouse_position.is_some();

        // Recomputing the fit on every layout keeps the image proportional to
        // the view when the window is resized.
        let fit_zoom = SvgPreviewView::fit_zoom(bounds, image_size);
        let zoom = fit_zoom * view.relative_zoom;
        let delay = (!is_initial_layout).then_some(RERENDER_DELAY);
        self.view.update(cx, |view, cx| {
            view.request_render_scale(zoom, delay, window, cx)
        });
        let view = self.view.read(cx);
        let scaled_width = px(image_size.width * zoom);
        let scaled_height = px(image_size.height * zoom);
        let left = bounds.size.width / 2.0 - scaled_width / 2.0 + px(view.pan_offset.x * fit_zoom);
        let top = bounds.size.height / 2.0 - scaled_height / 2.0 + px(view.pan_offset.y * fit_zoom);

        let mut content = div()
            .relative()
            .size_full()
            .child(
                div()
                    .absolute()
                    .left(left)
                    .top(top)
                    .w(scaled_width)
                    .h(scaled_height)
                    .child(img(image.image).size_full().with_fallback(|| {
                        h_flex()
                            .p_4()
                            .gap_2()
                            .child(Icon::new(IconName::Warning))
                            .child("Failed to load SVG image")
                            .into_any_element()
                    })),
            )
            .into_any_element();
        content.prepaint_as_root(bounds.origin, bounds.size.into(), window, cx);
        Some((content, is_dragging))
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        _bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        let Some((mut content, is_dragging)) = prepaint.take() else {
            return;
        };

        // Stop dragging even when the mouse is released outside the preview.
        if is_dragging {
            let view = self.view.downgrade();
            window.on_mouse_event(move |_event: &MouseUpEvent, phase, _window, cx| {
                if phase == DispatchPhase::Bubble
                    && let Some(view) = view.upgrade()
                {
                    view.update(cx, |view, cx| {
                        view.last_mouse_position = None;
                        cx.notify();
                    });
                }
            });
        }

        content.paint(window, cx);
    }
}

impl Focusable for SvgPreviewView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<()> for SvgPreviewView {}

impl Item for SvgPreviewView {
    type Event = ();

    fn tab_icon(&self, _window: &Window, cx: &App) -> Option<Icon> {
        self.buffer
            .as_ref()
            .and_then(|buffer| buffer.read(cx).file())
            .and_then(|file| FileIcons::get_icon(file.path().as_std_path(), cx))
            .map(Icon::from_path)
            .or_else(|| Some(Icon::new(IconName::Image)))
    }

    fn tab_content_text(&self, _detail: usize, cx: &App) -> SharedString {
        self.buffer
            .as_ref()
            .and_then(|svg_path| svg_path.read(cx).file())
            .map(|name| format!("Preview {}", name.file_name(cx)).into())
            .unwrap_or_else(|| "SVG Preview".into())
    }

    fn telemetry_event_text(&self) -> Option<&'static str> {
        Some("svg preview: open")
    }

    fn to_item_events(_event: &Self::Event, _f: &mut dyn FnMut(workspace::item::ItemEvent)) {}
}
