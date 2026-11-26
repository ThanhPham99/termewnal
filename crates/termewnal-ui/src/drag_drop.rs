//! Pointer-driven Drag & Drop coordination across tabs, sidebar profiles/groups, and workspace panes.

use std::path::PathBuf;

use iced::advanced::layout::{self, Layout};
use iced::advanced::overlay;
use iced::advanced::renderer::{self, Quad};
use iced::advanced::widget::{Operation, Tree, tree};
use iced::advanced::{Clipboard, Shell, Widget};
use iced::widget::{container, opaque, row, text};
use iced::{
    Alignment, Background, Border, Color, Element, Event, Length, Padding, Point, Rectangle,
    Shadow, Size, Theme, Vector, keyboard, mouse,
};
use termewnal_core::ThemePalette;

use crate::app::Message;
use crate::ui_theme::{ChromePalette, FONT_META, badge_pill, ui_font_medium, ui_font_semibold};
use crate::workspace::{PaneId, SplitAxis, TabId};

/// Minimum pointer travel (logical pixels) before a press transitions into an active drag.
const DRAG_THRESHOLD_PX: f32 = 6.0;

/// Payload carried by an in-app pointer drag operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DragPayload {
    Tab { tab_id: TabId, label: String },
    SavedProfile { profile_id: String, label: String },
    SystemSsh { alias: String, label: String },
}

impl DragPayload {
    /// Human-readable title displayed inside the floating drag badge.
    pub fn display_label(&self) -> &str {
        match self {
            Self::Tab { label, .. }
            | Self::SavedProfile { label, .. }
            | Self::SystemSsh { label, .. } => label,
        }
    }

    /// Short category prefix shown beside the dragged item title.
    pub fn kind_badge(&self) -> &'static str {
        match self {
            Self::Tab { .. } => "Tab",
            Self::SavedProfile { .. } => "Profile",
            Self::SystemSsh { .. } => "SSH",
        }
    }

    /// Returns `true` if dropping `self` onto `target` is meaningful.
    pub fn accepts(&self, target: &DropTarget) -> bool {
        match self {
            Self::Tab { tab_id, .. } => match target {
                DropTarget::TabSlot {
                    tab_id: target_id, ..
                } => target_id != tab_id,
                DropTarget::TabStripEnd => true,
                DropTarget::ProfileGroup(_) | DropTarget::WorkspacePane { .. } => false,
            },
            Self::SavedProfile { .. } | Self::SystemSsh { .. } => match target {
                DropTarget::TabSlot { .. } => false,
                DropTarget::TabStripEnd
                | DropTarget::ProfileGroup(_)
                | DropTarget::WorkspacePane { .. } => true,
            },
        }
    }
}

/// Interactive drop target highlighted while dragging a compatible [`DragPayload`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DropTarget {
    /// Reorder a dragged tab before (`after = false`) or after (`after = true`) `tab_id`.
    TabSlot { tab_id: TabId, after: bool },
    /// Drop onto the tab strip bar (moves a dragged tab to end, or opens a launched session tab).
    TabStripEnd,
    /// Drop onto a Saved Profiles group header/well (`None` = Ungrouped).
    ProfileGroup(Option<String>),
    /// Drop onto the terminal workspace pane or empty canvas.
    WorkspacePane {
        pane_id: Option<PaneId>,
        split_axis: Option<SplitAxis>,
    },
}

/// Active pointer-drag session stored on [`crate::app::AppState`].
#[derive(Debug, Clone, PartialEq)]
pub struct ActiveDrag {
    pub payload: DragPayload,
    pub cursor: Point,
    pub hovered_target: Option<DropTarget>,
}

/// Window-level internal Drag & Drop plus OS external file-hover feedback state.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DragDropState {
    pub active: Option<ActiveDrag>,
    pub external_file_hover: Option<PathBuf>,
}

impl DragDropState {
    /// Returns `true` when an in-app drag gesture is in progress.
    pub fn is_dragging(&self) -> bool {
        self.active.is_some()
    }

    /// Returns the currently dragged payload, if any.
    pub fn active_payload(&self) -> Option<&DragPayload> {
        self.active.as_ref().map(|drag| &drag.payload)
    }

    /// Returns the currently hovered drop target, if any.
    pub fn hovered_target(&self) -> Option<&DropTarget> {
        self.active
            .as_ref()
            .and_then(|drag| drag.hovered_target.as_ref())
    }

    /// Returns `true` when dragging a payload compatible with `target`.
    pub fn accepts(&self, target: &DropTarget) -> bool {
        self.active
            .as_ref()
            .is_some_and(|drag| drag.payload.accepts(target))
    }

    /// Starts an active drag gesture from `payload` at `cursor`.
    pub fn begin(&mut self, payload: DragPayload, cursor: Point) {
        self.active = Some(ActiveDrag {
            payload,
            cursor,
            hovered_target: None,
        });
    }

    /// Updates cursor coordinates while dragging.
    pub fn update_cursor(&mut self, cursor: Point) {
        if let Some(drag) = &mut self.active {
            drag.cursor = cursor;
        }
    }

    /// Sets the active hovered drop target if compatible with the dragged payload.
    pub fn set_hovered(&mut self, target: Option<DropTarget>) {
        if let Some(drag) = &mut self.active {
            drag.hovered_target = target.filter(|t| drag.payload.accepts(t));
        }
    }

    /// Order-independent clear: only clears `hovered_target` if it still matches `target`.
    pub fn clear_hovered_if_matches(&mut self, target: &DropTarget) {
        if let Some(drag) = &mut self.active
            && drag.hovered_target.as_ref() == Some(target)
        {
            drag.hovered_target = None;
        }
    }

    /// Cancels an active drag without performing a drop.
    pub fn cancel(&mut self) {
        self.active = None;
    }

    /// Ends the active drag and returns `(payload, hovered_target)` for handling.
    pub fn take_drop(&mut self) -> Option<(DragPayload, Option<DropTarget>)> {
        self.active
            .take()
            .map(|drag| (drag.payload, drag.hovered_target))
    }

    /// Records an OS file hovering over the window.
    pub fn set_external_file_hovered(&mut self, path: PathBuf) {
        self.external_file_hover = Some(path);
    }

    /// Clears any active OS file-hover state.
    pub fn clear_external_file_hover(&mut self) {
        self.external_file_hover = None;
    }
}

#[derive(Debug, Default)]
struct DragSourceLocal {
    pressed_at: Option<Point>,
    dragging: bool,
}

/// Wraps a draggable UI control (tab chip, saved profile item, System SSH item).
///
/// Normal clicks (`< 6px` movement) pass untouched to inner buttons; moving beyond the drag
/// threshold promotes to an in-app drag and cleanly resets inner button press state on release.
pub struct DragSource<'a> {
    payload: DragPayload,
    enabled: bool,
    dim_when_active: bool,
    content: Element<'a, Message, Theme, iced::Renderer>,
}

impl<'a> DragSource<'a> {
    pub fn new(
        payload: DragPayload,
        enabled: bool,
        dim_when_active: bool,
        content: Element<'a, Message, Theme, iced::Renderer>,
    ) -> Element<'a, Message, Theme, iced::Renderer> {
        Element::new(Self {
            payload,
            enabled,
            dim_when_active,
            content,
        })
    }
}

impl Widget<Message, Theme, iced::Renderer> for DragSource<'_> {
    fn tag(&self) -> tree::Tag {
        tree::Tag::of::<DragSourceLocal>()
    }

    fn state(&self) -> tree::State {
        tree::State::new(DragSourceLocal::default())
    }

    fn children(&self) -> Vec<Tree> {
        vec![Tree::new(&self.content)]
    }

    fn diff(&self, tree: &mut Tree) {
        tree.diff_children(std::slice::from_ref(&self.content));
    }

    fn size(&self) -> Size<Length> {
        self.content.as_widget().size()
    }

    fn layout(
        &mut self,
        tree: &mut Tree,
        renderer: &iced::Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        self.content
            .as_widget_mut()
            .layout(&mut tree.children[0], renderer, limits)
    }

    fn operate(
        &mut self,
        tree: &mut Tree,
        layout: Layout<'_>,
        renderer: &iced::Renderer,
        operation: &mut dyn Operation,
    ) {
        self.content
            .as_widget_mut()
            .operate(&mut tree.children[0], layout, renderer, operation);
    }

    fn update(
        &mut self,
        tree: &mut Tree,
        event: &Event,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        renderer: &iced::Renderer,
        clipboard: &mut dyn Clipboard,
        shell: &mut Shell<'_, Message>,
        viewport: &Rectangle,
    ) {
        let local = tree.state.downcast_mut::<DragSourceLocal>();
        if !self.enabled {
            local.pressed_at = None;
            local.dragging = false;
        }

        match event {
            Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left))
                if self.enabled && cursor.is_over(layout.bounds()) =>
            {
                local.pressed_at = cursor.position();
                local.dragging = false;
            }
            Event::Mouse(mouse::Event::CursorMoved { position })
                if self.enabled && local.pressed_at.is_some() && !local.dragging =>
            {
                if let Some(origin) = local.pressed_at
                    && origin.distance(*position) >= DRAG_THRESHOLD_PX
                {
                    local.dragging = true;
                    shell.publish(Message::DragStart {
                        payload: self.payload.clone(),
                        position: *position,
                    });
                    shell.capture_event();
                    shell.request_redraw();
                    return;
                }
            }
            Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)) => {
                if local.dragging {
                    local.dragging = false;
                    local.pressed_at = None;
                    // Reset inner button's pressed flag without triggering its `on_press` action.
                    self.content.as_widget_mut().update(
                        &mut tree.children[0],
                        event,
                        layout,
                        mouse::Cursor::Unavailable,
                        renderer,
                        clipboard,
                        shell,
                        viewport,
                    );
                    shell.publish(Message::DragFinish);
                    shell.capture_event();
                    shell.request_redraw();
                    return;
                }
                local.pressed_at = None;
            }
            _ => {}
        }

        self.content.as_widget_mut().update(
            &mut tree.children[0],
            event,
            layout,
            cursor,
            renderer,
            clipboard,
            shell,
            viewport,
        );
    }

    fn mouse_interaction(
        &self,
        tree: &Tree,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
        renderer: &iced::Renderer,
    ) -> mouse::Interaction {
        let local = tree.state.downcast_ref::<DragSourceLocal>();
        if local.dragging {
            return mouse::Interaction::Grabbing;
        }
        self.content.as_widget().mouse_interaction(
            &tree.children[0],
            layout,
            cursor,
            viewport,
            renderer,
        )
    }

    fn draw(
        &self,
        tree: &Tree,
        renderer: &mut iced::Renderer,
        theme: &Theme,
        style: &renderer::Style,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
    ) {
        use iced::advanced::renderer::Renderer as _;

        self.content.as_widget().draw(
            &tree.children[0],
            renderer,
            theme,
            style,
            layout,
            cursor,
            viewport,
        );
        if self.dim_when_active {
            let bounds = layout.bounds();
            if bounds.intersects(viewport) {
                renderer.fill_quad(
                    Quad {
                        bounds,
                        border: Border {
                            radius: 6.0.into(),
                            ..Default::default()
                        },
                        shadow: Shadow::default(),
                        snap: true,
                    },
                    Background::Color(Color {
                        a: 0.24,
                        ..Color::BLACK
                    }),
                );
            }
        }
    }

    fn overlay<'b>(
        &'b mut self,
        tree: &'b mut Tree,
        layout: Layout<'b>,
        renderer: &iced::Renderer,
        viewport: &Rectangle,
        translation: Vector,
    ) -> Option<overlay::Element<'b, Message, Theme, iced::Renderer>> {
        self.content.as_widget_mut().overlay(
            &mut tree.children[0],
            layout,
            renderer,
            viewport,
            translation,
        )
    }
}

/// Interactive drop zone behavior parameterized cleanly by target kind.
#[derive(Debug, Clone)]
pub enum DropZoneKind {
    TabSlot {
        tab_id: TabId,
        vertical: bool,
        accepting_tab_drag: bool,
        hovered_slot: Option<(TabId, bool)>,
        accent: Color,
    },
    TabStripBar {
        accepting_drag: bool,
        hovered_strip: bool,
        accent: Color,
        soft_bg: Color,
    },
    ProfileGroup {
        group: Option<String>,
        accepting_drag: bool,
        is_hovered: bool,
    },
    WorkspacePane {
        pane_id: Option<PaneId>,
        accepting_drag: bool,
        hovered_zone: Option<(Option<PaneId>, Option<SplitAxis>)>,
        accent: Color,
        soft_bg: Color,
    },
}

/// Wraps any target surface to detect hover/leave transitions and paint crisp drop feedback.
pub struct DropTargetZone<'a> {
    kind: DropZoneKind,
    content: Element<'a, Message, Theme, iced::Renderer>,
}

impl<'a> DropTargetZone<'a> {
    pub fn new(
        kind: DropZoneKind,
        content: Element<'a, Message, Theme, iced::Renderer>,
    ) -> Element<'a, Message, Theme, iced::Renderer> {
        Element::new(Self { kind, content })
    }

    fn accepting(&self) -> bool {
        match &self.kind {
            DropZoneKind::TabSlot {
                accepting_tab_drag, ..
            } => *accepting_tab_drag,
            DropZoneKind::TabStripBar { accepting_drag, .. }
            | DropZoneKind::ProfileGroup { accepting_drag, .. }
            | DropZoneKind::WorkspacePane { accepting_drag, .. } => *accepting_drag,
        }
    }

    fn compute_hovered_target(&self, bounds: Rectangle, pos: Point) -> DropTarget {
        match &self.kind {
            DropZoneKind::TabSlot {
                tab_id, vertical, ..
            } => {
                let after = if *vertical {
                    pos.y >= bounds.y + bounds.height * 0.5
                } else {
                    pos.x >= bounds.x + bounds.width * 0.5
                };
                DropTarget::TabSlot {
                    tab_id: *tab_id,
                    after,
                }
            }
            DropZoneKind::TabStripBar { .. } => DropTarget::TabStripEnd,
            DropZoneKind::ProfileGroup { group, .. } => DropTarget::ProfileGroup(group.clone()),
            DropZoneKind::WorkspacePane { pane_id, .. } => {
                let split_axis = if pane_id.is_some() && bounds.width > 1.0 && bounds.height > 1.0
                {
                    let rel_x = ((pos.x - bounds.x) / bounds.width).clamp(0.0, 1.0);
                    let rel_y = ((pos.y - bounds.y) / bounds.height).clamp(0.0, 1.0);
                    if rel_x >= 0.62 {
                        Some(SplitAxis::Vertical)
                    } else if rel_y >= 0.62 {
                        Some(SplitAxis::Horizontal)
                    } else {
                        None
                    }
                } else {
                    None
                };
                DropTarget::WorkspacePane {
                    pane_id: *pane_id,
                    split_axis,
                }
            }
        }
    }

    fn current_hovered_target(&self) -> Option<DropTarget> {
        match &self.kind {
            DropZoneKind::TabSlot {
                tab_id,
                hovered_slot,
                ..
            } => hovered_slot.and_then(|(id, after)| {
                (id == *tab_id).then_some(DropTarget::TabSlot { tab_id: id, after })
            }),
            DropZoneKind::TabStripBar { hovered_strip, .. } => {
                hovered_strip.then_some(DropTarget::TabStripEnd)
            }
            DropZoneKind::ProfileGroup {
                group, is_hovered, ..
            } => is_hovered.then(|| DropTarget::ProfileGroup(group.clone())),
            DropZoneKind::WorkspacePane {
                pane_id,
                hovered_zone,
                ..
            } => hovered_zone.and_then(|(pid, split_axis)| {
                (pid == *pane_id).then_some(DropTarget::WorkspacePane {
                    pane_id: pid,
                    split_axis,
                })
            }),
        }
    }
}

impl Widget<Message, Theme, iced::Renderer> for DropTargetZone<'_> {
    fn children(&self) -> Vec<Tree> {
        vec![Tree::new(&self.content)]
    }

    fn diff(&self, tree: &mut Tree) {
        tree.diff_children(std::slice::from_ref(&self.content));
    }

    fn size(&self) -> Size<Length> {
        self.content.as_widget().size()
    }

    fn layout(
        &mut self,
        tree: &mut Tree,
        renderer: &iced::Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        self.content
            .as_widget_mut()
            .layout(&mut tree.children[0], renderer, limits)
    }

    fn operate(
        &mut self,
        tree: &mut Tree,
        layout: Layout<'_>,
        renderer: &iced::Renderer,
        operation: &mut dyn Operation,
    ) {
        self.content
            .as_widget_mut()
            .operate(&mut tree.children[0], layout, renderer, operation);
    }

    fn update(
        &mut self,
        tree: &mut Tree,
        event: &Event,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        renderer: &iced::Renderer,
        clipboard: &mut dyn Clipboard,
        shell: &mut Shell<'_, Message>,
        viewport: &Rectangle,
    ) {
        self.content.as_widget_mut().update(
            &mut tree.children[0],
            event,
            layout,
            cursor,
            renderer,
            clipboard,
            shell,
            viewport,
        );

        if !self.accepting() {
            return;
        }

        if let Event::Mouse(mouse::Event::CursorMoved { position }) = event {
            let bounds = layout.bounds();
            let current = self.current_hovered_target();
            if cursor.is_over(bounds) {
                if matches!(self.kind, DropZoneKind::TabStripBar { .. }) && shell.is_event_captured()
                {
                    return;
                }
                let next = self.compute_hovered_target(bounds, *position);
                if current.as_ref() != Some(&next) {
                    shell.publish(Message::DragHoverChanged(next));
                }
                if matches!(self.kind, DropZoneKind::TabSlot { .. }) {
                    shell.capture_event();
                }
            } else if let Some(prev) = current {
                shell.publish(Message::DragLeaveTarget(prev));
            }
        }
    }

    fn mouse_interaction(
        &self,
        tree: &Tree,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
        renderer: &iced::Renderer,
    ) -> mouse::Interaction {
        self.content.as_widget().mouse_interaction(
            &tree.children[0],
            layout,
            cursor,
            viewport,
            renderer,
        )
    }

    fn draw(
        &self,
        tree: &Tree,
        renderer: &mut iced::Renderer,
        theme: &Theme,
        style: &renderer::Style,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
    ) {
        use iced::advanced::renderer::Renderer as _;

        self.content.as_widget().draw(
            &tree.children[0],
            renderer,
            theme,
            style,
            layout,
            cursor,
            viewport,
        );

        let bounds = layout.bounds();
        if !bounds.intersects(viewport) {
            return;
        }

        match &self.kind {
            DropZoneKind::TabSlot {
                tab_id,
                vertical,
                accepting_tab_drag,
                hovered_slot,
                accent,
            } if *accepting_tab_drag => {
                if let Some((hid, after)) = hovered_slot
                    && hid == tab_id
                {
                    let bar = if *vertical {
                        let y = if *after {
                            bounds.y + bounds.height - 2.0
                        } else {
                            bounds.y - 1.0
                        };
                        Rectangle {
                            x: bounds.x,
                            y,
                            width: bounds.width,
                            height: 3.0,
                        }
                    } else {
                        let x = if *after {
                            bounds.x + bounds.width - 1.5
                        } else {
                            bounds.x - 1.5
                        };
                        Rectangle {
                            x,
                            y: bounds.y + 2.0,
                            width: 3.0,
                            height: (bounds.height - 4.0).max(2.0),
                        }
                    };
                    renderer.fill_quad(
                        Quad {
                            bounds: bar,
                            border: Border {
                                radius: 1.5.into(),
                                ..Default::default()
                            },
                            shadow: Shadow::default(),
                            snap: true,
                        },
                        Background::Color(*accent),
                    );
                }
            }
            DropZoneKind::TabStripBar {
                accepting_drag,
                hovered_strip,
                accent,
                soft_bg,
            } if *accepting_drag && *hovered_strip => {
                renderer.fill_quad(
                    Quad {
                        bounds,
                        border: Border {
                            color: *accent,
                            width: 1.5,
                            radius: 6.0.into(),
                        },
                        shadow: Shadow::default(),
                        snap: true,
                    },
                    Background::Color(*soft_bg),
                );
            }
            DropZoneKind::WorkspacePane {
                pane_id,
                accepting_drag,
                hovered_zone,
                accent,
                soft_bg,
            } if *accepting_drag => {
                if let Some((hid, split_axis)) = hovered_zone
                    && hid == pane_id
                {
                    let zone = match split_axis {
                        Some(SplitAxis::Vertical) => Rectangle {
                            x: bounds.x + bounds.width * 0.5,
                            y: bounds.y + 3.0,
                            width: bounds.width * 0.5 - 3.0,
                            height: (bounds.height - 6.0).max(2.0),
                        },
                        Some(SplitAxis::Horizontal) => Rectangle {
                            x: bounds.x + 3.0,
                            y: bounds.y + bounds.height * 0.5,
                            width: (bounds.width - 6.0).max(2.0),
                            height: bounds.height * 0.5 - 3.0,
                        },
                        None => Rectangle {
                            x: bounds.x + 2.0,
                            y: bounds.y + 2.0,
                            width: (bounds.width - 4.0).max(2.0),
                            height: (bounds.height - 4.0).max(2.0),
                        },
                    };
                    renderer.fill_quad(
                        Quad {
                            bounds: zone,
                            border: Border {
                                color: *accent,
                                width: 2.0,
                                radius: 8.0.into(),
                            },
                            shadow: Shadow::default(),
                            snap: true,
                        },
                        Background::Color(*soft_bg),
                    );
                }
            }
            _ => {}
        }
    }

    fn overlay<'b>(
        &'b mut self,
        tree: &'b mut Tree,
        layout: Layout<'b>,
        renderer: &iced::Renderer,
        viewport: &Rectangle,
        translation: Vector,
    ) -> Option<overlay::Element<'b, Message, Theme, iced::Renderer>> {
        self.content.as_widget_mut().overlay(
            &mut tree.children[0],
            layout,
            renderer,
            viewport,
            translation,
        )
    }
}

/// Top-level coordinator tracking pointer movement and drop/cancel while an internal drag is active.
pub struct DragCoordinator<'a> {
    is_dragging: bool,
    content: Element<'a, Message, Theme, iced::Renderer>,
}

impl<'a> DragCoordinator<'a> {
    pub fn new(
        is_dragging: bool,
        content: Element<'a, Message, Theme, iced::Renderer>,
    ) -> Element<'a, Message, Theme, iced::Renderer> {
        Element::new(Self {
            is_dragging,
            content,
        })
    }
}

impl Widget<Message, Theme, iced::Renderer> for DragCoordinator<'_> {
    fn children(&self) -> Vec<Tree> {
        vec![Tree::new(&self.content)]
    }

    fn diff(&self, tree: &mut Tree) {
        tree.diff_children(std::slice::from_ref(&self.content));
    }

    fn size(&self) -> Size<Length> {
        self.content.as_widget().size()
    }

    fn layout(
        &mut self,
        tree: &mut Tree,
        renderer: &iced::Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        self.content
            .as_widget_mut()
            .layout(&mut tree.children[0], renderer, limits)
    }

    fn operate(
        &mut self,
        tree: &mut Tree,
        layout: Layout<'_>,
        renderer: &iced::Renderer,
        operation: &mut dyn Operation,
    ) {
        self.content
            .as_widget_mut()
            .operate(&mut tree.children[0], layout, renderer, operation);
    }

    fn update(
        &mut self,
        tree: &mut Tree,
        event: &Event,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        renderer: &iced::Renderer,
        clipboard: &mut dyn Clipboard,
        shell: &mut Shell<'_, Message>,
        viewport: &Rectangle,
    ) {
        if self.is_dragging
            && let Event::Keyboard(keyboard::Event::KeyPressed {
                key: keyboard::Key::Named(keyboard::key::Named::Escape),
                ..
            }) = event
        {
            shell.publish(Message::DragCancel);
            shell.capture_event();
            return;
        }

        if self.is_dragging && let Event::Mouse(mouse::Event::CursorMoved { position }) = event {
            shell.publish(Message::DragCursorMoved(*position));
        }

        self.content.as_widget_mut().update(
            &mut tree.children[0],
            event,
            layout,
            cursor,
            renderer,
            clipboard,
            shell,
            viewport,
        );

        if self.is_dragging
            && !shell.is_event_captured()
            && let Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)) = event
        {
            shell.publish(Message::DragFinish);
            shell.capture_event();
        }
    }

    fn mouse_interaction(
        &self,
        tree: &Tree,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
        renderer: &iced::Renderer,
    ) -> mouse::Interaction {
        if self.is_dragging {
            return mouse::Interaction::Grabbing;
        }
        self.content.as_widget().mouse_interaction(
            &tree.children[0],
            layout,
            cursor,
            viewport,
            renderer,
        )
    }

    fn draw(
        &self,
        tree: &Tree,
        renderer: &mut iced::Renderer,
        theme: &Theme,
        style: &renderer::Style,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
    ) {
        self.content.as_widget().draw(
            &tree.children[0],
            renderer,
            theme,
            style,
            layout,
            cursor,
            viewport,
        );
    }

    fn overlay<'b>(
        &'b mut self,
        tree: &'b mut Tree,
        layout: Layout<'b>,
        renderer: &iced::Renderer,
        viewport: &Rectangle,
        translation: Vector,
    ) -> Option<overlay::Element<'b, Message, Theme, iced::Renderer>> {
        self.content.as_widget_mut().overlay(
            &mut tree.children[0],
            layout,
            renderer,
            viewport,
            translation,
        )
    }
}

/// Renders the floating drag pill (for internal drag or external OS file hover) above the window.
pub fn render_drag_overlay<'a>(
    state: &DragDropState,
    window_width: f32,
    window_height: f32,
    palette: &ThemePalette,
) -> Option<Element<'a, Message, Theme, iced::Renderer>> {
    let chrome = ChromePalette::from_palette(palette);

    if let Some(drag) = &state.active {
        let hint = match (&drag.payload, &drag.hovered_target) {
            (DragPayload::Tab { .. }, Some(DropTarget::TabSlot { .. })) => "Move tab here",
            (DragPayload::Tab { .. }, Some(DropTarget::TabStripEnd)) => "Move tab to end",
            (DragPayload::SavedProfile { .. }, Some(DropTarget::ProfileGroup(Some(g)))) => {
                return Some(build_drag_badge(
                    drag.payload.kind_badge(),
                    drag.payload.display_label().to_string(),
                    format!("Move to \"{g}\""),
                    drag.cursor,
                    window_width,
                    window_height,
                    chrome,
                ));
            }
            (DragPayload::SavedProfile { .. }, Some(DropTarget::ProfileGroup(None))) => {
                "Move to Ungrouped"
            }
            (DragPayload::SystemSsh { .. }, Some(DropTarget::ProfileGroup(Some(g)))) => {
                return Some(build_drag_badge(
                    drag.payload.kind_badge(),
                    drag.payload.display_label().to_string(),
                    format!("Clone to \"{g}\""),
                    drag.cursor,
                    window_width,
                    window_height,
                    chrome,
                ));
            }
            (DragPayload::SystemSsh { .. }, Some(DropTarget::ProfileGroup(None))) => {
                "Clone to Saved Profiles"
            }
            (_, Some(DropTarget::TabStripEnd)) => "Open in New Tab",
            (
                _,
                Some(DropTarget::WorkspacePane {
                    split_axis: Some(SplitAxis::Vertical),
                    ..
                }),
            ) => "Split Right",
            (
                _,
                Some(DropTarget::WorkspacePane {
                    split_axis: Some(SplitAxis::Horizontal),
                    ..
                }),
            ) => "Split Down",
            (_, Some(DropTarget::WorkspacePane { split_axis: None, .. })) => "Open in New Tab",
            (DragPayload::Tab { .. }, _) => "Drop on tab bar to reorder",
            (DragPayload::SavedProfile { .. }, _) => "Drop on group, tab bar, or pane",
            (DragPayload::SystemSsh { .. }, _) => "Drop on group to clone, or pane/tab bar",
        };
        return Some(build_drag_badge(
            drag.payload.kind_badge(),
            drag.payload.display_label().to_string(),
            hint.to_string(),
            drag.cursor,
            window_width,
            window_height,
            chrome,
        ));
    }

    if let Some(path) = &state.external_file_hover {
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("File");
        let banner = container(
            row![
                badge_pill(
                    "Drop File",
                    chrome.accent,
                    chrome.accent_soft,
                    Color::TRANSPARENT,
                ),
                text(format!("Drop \"{name}\" onto a terminal pane or SFTP drawer"))
                    .size(FONT_META)
                    .font(ui_font_medium())
                    .color(chrome.text_primary),
            ]
            .spacing(8)
            .align_y(Alignment::Center),
        )
        .padding([7, 12])
        .style(move |_| container::Style {
            background: Some(Background::Color(chrome.panel_bg)),
            border: Border {
                color: chrome.accent,
                width: 1.5,
                radius: 10.0.into(),
            },
            shadow: Shadow {
                color: Color {
                    a: 0.28,
                    ..Color::BLACK
                },
                offset: Vector::new(0.0, 6.0),
                blur_radius: 16.0,
            },
            ..Default::default()
        });
        let top_banner = container(opaque(banner))
            .width(Length::Fill)
            .height(Length::Fill)
            .align_x(Alignment::Center)
            .align_y(Alignment::Start)
            .padding(Padding {
                top: 14.0,
                right: 12.0,
                bottom: 12.0,
                left: 12.0,
            });
        return Some(top_banner.into());
    }

    None
}

fn build_drag_badge<'a>(
    kind: &'static str,
    label: String,
    hint: String,
    cursor: Point,
    window_width: f32,
    window_height: f32,
    chrome: ChromePalette,
) -> Element<'a, Message, Theme, iced::Renderer> {
    let pill = container(
        row![
            badge_pill(kind, chrome.accent, chrome.accent_soft, Color::TRANSPARENT),
            text(label)
                .size(FONT_META)
                .font(ui_font_semibold())
                .color(chrome.text_primary),
            text("·").size(FONT_META).color(chrome.text_muted),
            text(hint).size(FONT_META).color(chrome.text_secondary),
        ]
        .spacing(6)
        .align_y(Alignment::Center),
    )
    .padding([6, 10])
    .style(move |_| container::Style {
        background: Some(Background::Color(chrome.panel_bg)),
        border: Border {
            color: chrome.accent,
            width: 1.5,
            radius: 10.0.into(),
        },
        shadow: Shadow {
            color: Color {
                a: 0.30,
                ..Color::BLACK
            },
            offset: Vector::new(0.0, 6.0),
            blur_radius: 16.0,
        },
        ..Default::default()
    });

    let left = (cursor.x + 14.0).clamp(8.0, (window_width - 260.0).max(8.0));
    let top = (cursor.y + 14.0).clamp(8.0, (window_height - 44.0).max(8.0));
    container(opaque(pill))
        .width(Length::Fill)
        .height(Length::Fill)
        .padding(Padding {
            top,
            left,
            right: 8.0,
            bottom: 8.0,
        })
        .into()
}
