use iced::advanced::layout::{self, Layout};
use iced::advanced::overlay;
use iced::advanced::renderer::{self, Quad};
use iced::advanced::widget::{Operation, Tree, tree};
use iced::advanced::{Clipboard, Shell, Widget};
use iced::{
    Background, Border, Color, Element, Event, Length, Point, Rectangle, Shadow, Size, Theme,
    Vector, keyboard, mouse,
};

use crate::app::{Message, shortcut_for_key_event_with_palette};
use crate::workspace::{MAX_SPLIT_RATIO, MIN_SPLIT_RATIO, PaneId, SplitAxis, SplitId, TabId};

const DIVIDER_THICKNESS: f32 = 4.0;
const DIVIDER_HIT_PADDING: f32 = 3.0;
const PANE_BORDER_WIDTH: f32 = 2.0;

#[derive(Debug, Default)]
struct SplitDragState {
    dragging: bool,
    hovered: bool,
}

/// Interactive binary split container rendering two child elements separated by a draggable divider.
pub struct SplitView<'a> {
    split_id: SplitId,
    axis: SplitAxis,
    ratio: f32,
    children: [Element<'a, Message, Theme, iced::Renderer>; 2],
}

impl<'a> SplitView<'a> {
    /// Creates a new [`SplitView`] for `split_id` along `axis` with `ratio` in `[0.1, 0.9]`.
    pub fn new(
        split_id: SplitId,
        axis: SplitAxis,
        ratio: f32,
        first: Element<'a, Message, Theme, iced::Renderer>,
        second: Element<'a, Message, Theme, iced::Renderer>,
    ) -> Element<'a, Message, Theme, iced::Renderer> {
        Element::new(Self {
            split_id,
            axis,
            ratio: ratio.clamp(MIN_SPLIT_RATIO, MAX_SPLIT_RATIO),
            children: [first, second],
        })
    }

    fn divider_rects(&self, bounds: Rectangle) -> (Rectangle, Rectangle) {
        let (first_w, first_h) = split_first_size(bounds.size(), self.axis, self.ratio);
        let visual = match self.axis {
            SplitAxis::Vertical => Rectangle::new(
                Point::new(bounds.x + first_w, bounds.y),
                Size::new(DIVIDER_THICKNESS, bounds.height),
            ),
            SplitAxis::Horizontal => Rectangle::new(
                Point::new(bounds.x, bounds.y + first_h),
                Size::new(bounds.width, DIVIDER_THICKNESS),
            ),
        };
        let hit = match self.axis {
            SplitAxis::Vertical => Rectangle::new(
                Point::new(visual.x - DIVIDER_HIT_PADDING, visual.y),
                Size::new(visual.width + DIVIDER_HIT_PADDING * 2.0, visual.height),
            ),
            SplitAxis::Horizontal => Rectangle::new(
                Point::new(visual.x, visual.y - DIVIDER_HIT_PADDING),
                Size::new(visual.width, visual.height + DIVIDER_HIT_PADDING * 2.0),
            ),
        };
        (visual, hit)
    }
}

fn split_first_size(total: Size, axis: SplitAxis, ratio: f32) -> (f32, f32) {
    match axis {
        SplitAxis::Vertical => {
            let available = (total.width - DIVIDER_THICKNESS).max(2.0);
            let first_w = (available * ratio).round().clamp(1.0, available - 1.0);
            (first_w, total.height.max(1.0))
        }
        SplitAxis::Horizontal => {
            let available = (total.height - DIVIDER_THICKNESS).max(2.0);
            let first_h = (available * ratio).round().clamp(1.0, available - 1.0);
            (total.width.max(1.0), first_h)
        }
    }
}

impl Widget<Message, Theme, iced::Renderer> for SplitView<'_> {
    fn tag(&self) -> tree::Tag {
        tree::Tag::of::<SplitDragState>()
    }

    fn state(&self) -> tree::State {
        tree::State::new(SplitDragState::default())
    }

    fn children(&self) -> Vec<Tree> {
        self.children.iter().map(Tree::new).collect()
    }

    fn diff(&self, tree: &mut Tree) {
        tree.diff_children(&self.children);
    }

    fn size(&self) -> Size<Length> {
        Size::new(Length::Fill, Length::Fill)
    }

    fn layout(
        &mut self,
        tree: &mut Tree,
        renderer: &iced::Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        let total = limits.max();
        let (first_w, first_h) = split_first_size(total, self.axis, self.ratio);

        let (second_offset, second_size) = match self.axis {
            SplitAxis::Vertical => {
                let second_w = (total.width - first_w - DIVIDER_THICKNESS).max(1.0);
                (
                    Point::new(first_w + DIVIDER_THICKNESS, 0.0),
                    Size::new(second_w, total.height.max(1.0)),
                )
            }
            SplitAxis::Horizontal => {
                let second_h = (total.height - first_h - DIVIDER_THICKNESS).max(1.0);
                (
                    Point::new(0.0, first_h + DIVIDER_THICKNESS),
                    Size::new(total.width.max(1.0), second_h),
                )
            }
        };

        let first_limits = layout::Limits::new(Size::ZERO, Size::new(first_w, first_h));
        let second_limits = layout::Limits::new(Size::ZERO, second_size);

        let [first, second] = &mut self.children;
        let [first_tree, second_tree] = &mut tree.children[..2] else {
            unreachable!("SplitView always has two child trees");
        };

        let first_node = first
            .as_widget_mut()
            .layout(first_tree, renderer, &first_limits);
        let second_node = second
            .as_widget_mut()
            .layout(second_tree, renderer, &second_limits)
            .move_to(second_offset);

        layout::Node::with_children(total, vec![first_node, second_node])
    }

    fn operate(
        &mut self,
        tree: &mut Tree,
        layout: Layout<'_>,
        renderer: &iced::Renderer,
        operation: &mut dyn Operation,
    ) {
        operation.container(None, layout.bounds());
        operation.traverse(&mut |op| {
            for ((child, child_tree), child_layout) in self
                .children
                .iter_mut()
                .zip(&mut tree.children)
                .zip(layout.children())
            {
                child
                    .as_widget_mut()
                    .operate(child_tree, child_layout, renderer, op);
            }
        });
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
        let state = tree.state.downcast_mut::<SplitDragState>();
        let bounds = layout.bounds();
        let (_visual_rect, hit_rect) = self.divider_rects(bounds);

        match event {
            Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left))
                if cursor.is_over(hit_rect) =>
            {
                state.dragging = true;
                shell.capture_event();
                shell.request_redraw();
                return;
            }
            Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)) if state.dragging => {
                state.dragging = false;
                shell.capture_event();
                shell.request_redraw();
                return;
            }
            Event::Mouse(mouse::Event::CursorMoved { position }) => {
                let is_hovered = cursor.is_over(hit_rect);
                if state.hovered != is_hovered {
                    state.hovered = is_hovered;
                    shell.request_redraw();
                }
                if state.dragging {
                    let ratio = match self.axis {
                        SplitAxis::Vertical => {
                            let available = (bounds.width - DIVIDER_THICKNESS).max(2.0);
                            (position.x - bounds.x) / available
                        }
                        SplitAxis::Horizontal => {
                            let available = (bounds.height - DIVIDER_THICKNESS).max(2.0);
                            (position.y - bounds.y) / available
                        }
                    }
                    .clamp(MIN_SPLIT_RATIO, MAX_SPLIT_RATIO);

                    shell.publish(Message::ResizeSplit {
                        split_id: self.split_id,
                        ratio,
                    });
                    shell.capture_event();
                    return;
                }
            }
            _ => {}
        }

        for ((child, child_tree), child_layout) in self
            .children
            .iter_mut()
            .zip(&mut tree.children)
            .zip(layout.children())
        {
            child.as_widget_mut().update(
                child_tree,
                event,
                child_layout,
                cursor,
                renderer,
                clipboard,
                shell,
                viewport,
            );
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
        let state = tree.state.downcast_ref::<SplitDragState>();
        let (_visual_rect, hit_rect) = self.divider_rects(layout.bounds());
        if state.dragging || cursor.is_over(hit_rect) {
            return match self.axis {
                SplitAxis::Vertical => mouse::Interaction::ResizingHorizontally,
                SplitAxis::Horizontal => mouse::Interaction::ResizingVertically,
            };
        }

        self.children
            .iter()
            .zip(&tree.children)
            .zip(layout.children())
            .map(|((child, child_tree), child_layout)| {
                child
                    .as_widget()
                    .mouse_interaction(child_tree, child_layout, cursor, viewport, renderer)
            })
            .max()
            .unwrap_or_default()
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

        for ((child, child_tree), child_layout) in self
            .children
            .iter()
            .zip(&tree.children)
            .zip(layout.children())
            .filter(|(_, l)| l.bounds().intersects(viewport))
        {
            child
                .as_widget()
                .draw(child_tree, renderer, theme, style, child_layout, cursor, viewport);
        }

        let state = tree.state.downcast_ref::<SplitDragState>();
        let (visual_rect, _) = self.divider_rects(layout.bounds());
        if visual_rect.intersects(viewport) {
            let ext = theme.extended_palette();
            let color = if state.dragging || state.hovered {
                ext.primary.strong.color
            } else {
                ext.background.strong.color
            };
            renderer.fill_quad(
                Quad {
                    bounds: visual_rect,
                    border: Border::default(),
                    shadow: Shadow::default(),
                    snap: true,
                },
                Background::Color(color),
            );
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
        overlay::from_children(
            &mut self.children,
            tree,
            layout,
            renderer,
            viewport,
            translation,
        )
    }
}

/// Wrapper around a single pane's terminal viewport handling click-to-focus and focus/broadcast borders.
pub struct PaneFrame<'a> {
    pane_id: PaneId,
    focused: bool,
    broadcast: bool,
    show_border: bool,
    content: Element<'a, Message, Theme, iced::Renderer>,
}

impl<'a> PaneFrame<'a> {
    /// Wraps `content` inside a clickable focus/border frame for `pane_id`.
    pub fn new(
        pane_id: PaneId,
        focused: bool,
        broadcast: bool,
        show_border: bool,
        content: Element<'a, Message, Theme, iced::Renderer>,
    ) -> Element<'a, Message, Theme, iced::Renderer> {
        Element::new(Self {
            pane_id,
            focused,
            broadcast,
            show_border,
            content,
        })
    }

    fn border_pad(&self) -> f32 {
        if self.show_border || self.broadcast {
            PANE_BORDER_WIDTH
        } else {
            0.0
        }
    }
}

impl Widget<Message, Theme, iced::Renderer> for PaneFrame<'_> {
    fn children(&self) -> Vec<Tree> {
        vec![Tree::new(&self.content)]
    }

    fn diff(&self, tree: &mut Tree) {
        tree.diff_children(std::slice::from_ref(&self.content));
    }

    fn size(&self) -> Size<Length> {
        Size::new(Length::Fill, Length::Fill)
    }

    fn layout(
        &mut self,
        tree: &mut Tree,
        renderer: &iced::Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        let total = limits.max();
        let pad = self.border_pad();
        let inner_size = Size::new(
            (total.width - pad * 2.0).max(1.0),
            (total.height - pad * 2.0).max(1.0),
        );
        let child_node = self
            .content
            .as_widget_mut()
            .layout(
                &mut tree.children[0],
                renderer,
                &layout::Limits::new(Size::ZERO, inner_size),
            )
            .move_to(Point::new(pad, pad));
        layout::Node::with_children(total, vec![child_node])
    }

    fn operate(
        &mut self,
        tree: &mut Tree,
        layout: Layout<'_>,
        renderer: &iced::Renderer,
        operation: &mut dyn Operation,
    ) {
        if let Some(child_layout) = layout.children().next() {
            self.content.as_widget_mut().operate(
                &mut tree.children[0],
                child_layout,
                renderer,
                operation,
            );
        }
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
        if !self.focused
            && matches!(
                event,
                Event::Mouse(mouse::Event::ButtonPressed(
                    mouse::Button::Left | mouse::Button::Right | mouse::Button::Middle
                ))
            )
            && cursor.is_over(layout.bounds())
        {
            shell.publish(Message::FocusPane(self.pane_id));
        }

        if let Some(child_layout) = layout.children().next() {
            self.content.as_widget_mut().update(
                &mut tree.children[0],
                event,
                child_layout,
                cursor,
                renderer,
                clipboard,
                shell,
                viewport,
            );
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
        layout
            .children()
            .next()
            .map(|child_layout| {
                self.content.as_widget().mouse_interaction(
                    &tree.children[0],
                    child_layout,
                    cursor,
                    viewport,
                    renderer,
                )
            })
            .unwrap_or_default()
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

        let bounds = layout.bounds();
        let pad = self.border_pad();
        if pad > 0.0 && bounds.intersects(viewport) {
            let ext = theme.extended_palette();
            let border_color = if self.broadcast {
                Color::from_rgb8(220, 38, 38)
            } else if self.focused {
                ext.primary.strong.color
            } else {
                ext.background.strong.color
            };
            renderer.fill_quad(
                Quad {
                    bounds,
                    border: Border {
                        color: border_color,
                        width: pad,
                        radius: 0.0.into(),
                    },
                    shadow: Shadow::default(),
                    snap: true,
                },
                Background::Color(Color::TRANSPARENT),
            );
        }

        if let Some(child_layout) = layout.children().next() {
            self.content.as_widget().draw(
                &tree.children[0],
                renderer,
                theme,
                style,
                child_layout,
                cursor,
                viewport,
            );
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
        layout.children().next().and_then(|child_layout| {
            self.content.as_widget_mut().overlay(
                &mut tree.children[0],
                child_layout,
                renderer,
                viewport,
                translation,
            )
        })
    }
}

/// Top-level wrapper intercepting workspace keyboard shortcuts before terminal panes consume them.
pub struct ShortcutInterceptor<'a> {
    active_tab: Option<TabId>,
    palette_open: bool,
    content: Element<'a, Message, Theme, iced::Renderer>,
}

impl<'a> ShortcutInterceptor<'a> {
    /// Wraps `content` so workspace shortcuts are captured before reaching inner widgets.
    pub fn new(
        content: Element<'a, Message, Theme, iced::Renderer>,
        active_tab: Option<TabId>,
    ) -> Element<'a, Message, Theme, iced::Renderer> {
        Self::with_palette(content, active_tab, false)
    }

    /// Wraps `content` with Command Palette modal keyboard navigation awareness.
    pub fn with_palette(
        content: Element<'a, Message, Theme, iced::Renderer>,
        active_tab: Option<TabId>,
        palette_open: bool,
    ) -> Element<'a, Message, Theme, iced::Renderer> {
        Element::new(Self {
            active_tab,
            palette_open,
            content,
        })
    }
}

impl Widget<Message, Theme, iced::Renderer> for ShortcutInterceptor<'_> {
    fn children(&self) -> Vec<Tree> {
        vec![Tree::new(&self.content)]
    }

    fn diff(&self, tree: &mut Tree) {
        tree.diff_children(std::slice::from_ref(&self.content));
    }

    fn size(&self) -> Size<Length> {
        Size::new(Length::Fill, Length::Fill)
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
        if let Event::Keyboard(keyboard::Event::KeyPressed { key, modifiers, .. }) = event
            && let Some(msg) =
                shortcut_for_key_event_with_palette(key, *modifiers, self.active_tab, self.palette_open)
        {
            shell.publish(msg);
            shell.capture_event();
            return;
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

/// Wrapper that skips forwarding already-captured events (e.g. from sidebar text inputs) to inner panes.
pub struct CapturedEventGate<'a> {
    content: Element<'a, Message, Theme, iced::Renderer>,
}

impl<'a> CapturedEventGate<'a> {
    /// Wraps `content` so events already captured by sibling controls are not re-processed.
    pub fn new(
        content: Element<'a, Message, Theme, iced::Renderer>,
    ) -> Element<'a, Message, Theme, iced::Renderer> {
        Element::new(Self { content })
    }
}

impl Widget<Message, Theme, iced::Renderer> for CapturedEventGate<'_> {
    fn children(&self) -> Vec<Tree> {
        vec![Tree::new(&self.content)]
    }

    fn diff(&self, tree: &mut Tree) {
        tree.diff_children(std::slice::from_ref(&self.content));
    }

    fn size(&self) -> Size<Length> {
        Size::new(Length::Fill, Length::Fill)
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
        if shell.is_event_captured() {
            return;
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
