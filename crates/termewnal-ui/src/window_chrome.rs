//! Integrated Tabby-style client-side System Bar controls, draggable titlebar region, and resize frame.

use std::time::{Duration, Instant};

use iced::advanced::layout::{self, Layout};
use iced::advanced::overlay;
use iced::advanced::renderer::{self, Quad};
use iced::advanced::widget::{Operation, Tree, tree};
use iced::advanced::{Clipboard, Shell, Widget};
use iced::widget::{container, row, space, text};
use iced::{
    Alignment, Background, Border, Color, Element, Event, Length, Point, Rectangle, Shadow, Size,
    Theme, Vector, mouse, window,
};

use crate::app::Message;
use crate::ui_theme::{ChromePalette, ui_font};

const EDGE_RESIZE_HIT_PX: f32 = 5.0;
const DOUBLE_CLICK_MAX_MS: u64 = 350;
const DOUBLE_CLICK_MAX_DIST_PX: f32 = 6.0;
pub(crate) const APP_ICON_SIZE: u32 = 64;
pub(crate) const APP_ICON_64_RGBA: &[u8] = include_bytes!("../../../assets/termewnal-icon-64.rgba");

/// Builds the embedded Termewnal desktop/taskbar [`window::Icon`].
pub fn app_window_icon() -> Option<window::Icon> {
    window::icon::from_rgba(APP_ICON_64_RGBA.to_vec(), APP_ICON_SIZE, APP_ICON_SIZE).ok()
}

/// Builds borderless Tabby-style `iced::window::Settings` for integrated System Bar windows.
pub fn main_window_settings(width: u32, height: u32) -> window::Settings {
    window::Settings {
        decorations: false,
        exit_on_close_request: false,
        icon: app_window_icon(),
        size: Size::new(width.max(200) as f32, height.max(150) as f32),
        ..window::Settings::default()
    }
}

/// Geometric vector window-control glyph drawn cleanly with crisp `1px` quads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowControlGlyph {
    Minimize,
    Maximize,
    Restore,
}

/// Renders a crisp `1px` stroke window control icon (`─`, `□`, `❐`) inheriting button hover color.
pub struct WindowControlIcon {
    glyph: WindowControlGlyph,
}

impl WindowControlIcon {
    pub fn new<'a>(
        glyph: WindowControlGlyph,
    ) -> Element<'a, Message, Theme, iced::Renderer> {
        Element::new(Self { glyph })
    }
}

impl Widget<Message, Theme, iced::Renderer> for WindowControlIcon {
    fn children(&self) -> Vec<Tree> {
        Vec::new()
    }

    fn size(&self) -> Size<Length> {
        Size::new(Length::Fixed(14.0), Length::Fixed(14.0))
    }

    fn layout(
        &mut self,
        _tree: &mut Tree,
        _renderer: &iced::Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        let size = limits.resolve(Length::Fixed(14.0), Length::Fixed(14.0), Size::ZERO);
        layout::Node::new(size)
    }

    fn draw(
        &self,
        _tree: &Tree,
        renderer: &mut iced::Renderer,
        _theme: &Theme,
        style: &renderer::Style,
        layout: Layout<'_>,
        _cursor: mouse::Cursor,
        viewport: &Rectangle,
    ) {
        use iced::advanced::renderer::Renderer as _;

        let bounds = layout.bounds();
        if !bounds.intersects(viewport) {
            return;
        }
        let color = style.text_color;
        let cx = (bounds.x + bounds.width * 0.5).round();
        let cy = (bounds.y + bounds.height * 0.5).round();

        match self.glyph {
            WindowControlGlyph::Minimize => {
                fill_stroke_quad(
                    renderer,
                    Rectangle {
                        x: cx - 5.0,
                        y: cy,
                        width: 10.0,
                        height: 1.0,
                    },
                    color,
                );
            }
            WindowControlGlyph::Maximize => {
                renderer.fill_quad(
                    Quad {
                        bounds: Rectangle {
                            x: cx - 5.0,
                            y: cy - 5.0,
                            width: 10.0,
                            height: 10.0,
                        },
                        border: Border {
                            color,
                            width: 1.0,
                            radius: 0.0.into(),
                        },
                        shadow: Shadow::default(),
                        snap: true,
                    },
                    Background::Color(Color::TRANSPARENT),
                );
            }
            WindowControlGlyph::Restore => {
                let x0 = cx - 5.0;
                let y0 = cy - 5.0;
                // Front bottom-left 8x8 square.
                renderer.fill_quad(
                    Quad {
                        bounds: Rectangle {
                            x: x0,
                            y: y0 + 2.0,
                            width: 8.0,
                            height: 8.0,
                        },
                        border: Border {
                            color,
                            width: 1.0,
                            radius: 0.0.into(),
                        },
                        shadow: Shadow::default(),
                        snap: true,
                    },
                    Background::Color(Color::TRANSPARENT),
                );
                // Back top-right peeking L-strokes (no fill so button hover shines through).
                fill_stroke_quad(
                    renderer,
                    Rectangle {
                        x: x0 + 2.0,
                        y: y0,
                        width: 8.0,
                        height: 1.0,
                    },
                    color,
                );
                fill_stroke_quad(
                    renderer,
                    Rectangle {
                        x: x0 + 2.0,
                        y: y0,
                        width: 1.0,
                        height: 2.0,
                    },
                    color,
                );
                fill_stroke_quad(
                    renderer,
                    Rectangle {
                        x: x0 + 9.0,
                        y: y0,
                        width: 1.0,
                        height: 8.0,
                    },
                    color,
                );
                fill_stroke_quad(
                    renderer,
                    Rectangle {
                        x: x0 + 8.0,
                        y: y0 + 7.0,
                        width: 2.0,
                        height: 1.0,
                    },
                    color,
                );
            }
        }
    }
}

fn fill_stroke_quad(renderer: &mut iced::Renderer, bounds: Rectangle, color: Color) {
    use iced::advanced::renderer::Renderer as _;

    renderer.fill_quad(
        Quad {
            bounds,
            border: Border::default(),
            shadow: Shadow::default(),
            snap: true,
        },
        Background::Color(color),
    );
}

#[derive(Debug, Default)]
struct TitleDragLocal {
    last_press: Option<(Instant, Point)>,
}

/// Draggable empty System Bar region supporting window drag, double-click maximize, and system menu.
pub struct TitleBarDragZone<'a> {
    window_id: window::Id,
    enabled: bool,
    top_resize_guard_px: f32,
    content: Element<'a, Message, Theme, iced::Renderer>,
}

impl<'a> TitleBarDragZone<'a> {
    pub fn new(
        window_id: window::Id,
        enabled: bool,
        content: Element<'a, Message, Theme, iced::Renderer>,
    ) -> Element<'a, Message, Theme, iced::Renderer> {
        Self::with_resize_guard(
            window_id,
            enabled,
            if enabled { EDGE_RESIZE_HIT_PX } else { 0.0 },
            content,
        )
    }

    pub fn with_resize_guard(
        window_id: window::Id,
        enabled: bool,
        top_resize_guard_px: f32,
        content: Element<'a, Message, Theme, iced::Renderer>,
    ) -> Element<'a, Message, Theme, iced::Renderer> {
        Element::new(Self {
            window_id,
            enabled,
            top_resize_guard_px: top_resize_guard_px.max(0.0),
            content,
        })
    }

    /// Evaluates whether the drag spacer accepts a mouse press without overlapping a top resize bezel or captured child.
    pub fn allows_drag_at(
        enabled: bool,
        child_captured: bool,
        top_resize_guard_px: f32,
        bounds: Rectangle,
        cursor_pos: Option<Point>,
    ) -> bool {
        let Some(pos) = cursor_pos else {
            return false;
        };
        enabled
            && !child_captured
            && bounds.contains(pos)
            && pos.y >= bounds.y + top_resize_guard_px.max(0.0)
    }
}

impl Widget<Message, Theme, iced::Renderer> for TitleBarDragZone<'_> {
    fn tag(&self) -> tree::Tag {
        tree::Tag::of::<TitleDragLocal>()
    }

    fn state(&self) -> tree::State {
        tree::State::new(TitleDragLocal::default())
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

        if !Self::allows_drag_at(
            self.enabled,
            shell.is_event_captured(),
            self.top_resize_guard_px,
            layout.bounds(),
            cursor.position(),
        ) {
            return;
        }

        let local = tree.state.downcast_mut::<TitleDragLocal>();
        match event {
            Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)) => {
                let now = Instant::now();
                let pos = cursor.position().unwrap_or_default();
                let is_double = local.last_press.is_some_and(|(prev_t, prev_p)| {
                    now.duration_since(prev_t) <= Duration::from_millis(DOUBLE_CLICK_MAX_MS)
                        && prev_p.distance(pos) <= DOUBLE_CLICK_MAX_DIST_PX
                });
                if is_double {
                    local.last_press = None;
                    shell.publish(Message::WindowToggleMaximize(self.window_id));
                } else {
                    local.last_press = Some((now, pos));
                    shell.publish(Message::WindowDragStart(self.window_id));
                }
                shell.capture_event();
            }
            Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Right)) => {
                shell.publish(Message::WindowShowSystemMenu(self.window_id));
                shell.capture_event();
            }
            _ => {}
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

/// Outer `5px` edge resize frame enabling intuitive edge/corner resizing on `decorations: false` windows.
pub struct BorderlessResizeFrame<'a> {
    window_id: window::Id,
    enabled: bool,
    content: Element<'a, Message, Theme, iced::Renderer>,
}

impl<'a> BorderlessResizeFrame<'a> {
    pub fn new(
        window_id: window::Id,
        enabled: bool,
        content: Element<'a, Message, Theme, iced::Renderer>,
    ) -> Element<'a, Message, Theme, iced::Renderer> {
        Element::new(Self {
            window_id,
            enabled,
            content,
        })
    }

    fn edge_direction(bounds: Rectangle, pos: Point) -> Option<window::Direction> {
        let on_left = pos.x <= bounds.x + EDGE_RESIZE_HIT_PX;
        let on_right = pos.x >= bounds.x + bounds.width - EDGE_RESIZE_HIT_PX;
        let on_top = pos.y <= bounds.y + EDGE_RESIZE_HIT_PX;
        let on_bottom = pos.y >= bounds.y + bounds.height - EDGE_RESIZE_HIT_PX;

        match (on_left, on_right, on_top, on_bottom) {
            (true, false, true, false) => Some(window::Direction::NorthWest),
            (false, true, true, false) => Some(window::Direction::NorthEast),
            (true, false, false, true) => Some(window::Direction::SouthWest),
            (false, true, false, true) => Some(window::Direction::SouthEast),
            (true, false, false, false) => Some(window::Direction::West),
            (false, true, false, false) => Some(window::Direction::East),
            (false, false, true, false) => Some(window::Direction::North),
            (false, false, false, true) => Some(window::Direction::South),
            _ => None,
        }
    }

    /// Resolves whether an uncaptured left press on the outer borderless frame starts edge resizing.
    pub fn resolve_resize_press(
        enabled: bool,
        child_captured: bool,
        is_left_press: bool,
        bounds: Rectangle,
        cursor_pos: Option<Point>,
    ) -> Option<window::Direction> {
        if !enabled || child_captured || !is_left_press {
            return None;
        }
        let pos = cursor_pos?;
        if !bounds.contains(pos) {
            return None;
        }
        Self::edge_direction(bounds, pos)
    }
}

impl Widget<Message, Theme, iced::Renderer> for BorderlessResizeFrame<'_> {
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

        let is_left_press = matches!(
            event,
            Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left))
        );
        if let Some(dir) = Self::resolve_resize_press(
            self.enabled,
            shell.is_event_captured(),
            is_left_press,
            layout.bounds(),
            cursor.position(),
        ) {
            shell.publish(Message::WindowEdgeResize(self.window_id, dir));
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
        let child_cursor = self.content.as_widget().mouse_interaction(
            &tree.children[0],
            layout,
            cursor,
            viewport,
            renderer,
        );
        if child_cursor != mouse::Interaction::None {
            return child_cursor;
        }
        if self.enabled
            && let Some(pos) = cursor.position()
            && layout.bounds().contains(pos)
            && let Some(dir) = Self::edge_direction(layout.bounds(), pos)
        {
            return match dir {
                window::Direction::North | window::Direction::South => {
                    mouse::Interaction::ResizingVertically
                }
                window::Direction::East | window::Direction::West => {
                    mouse::Interaction::ResizingHorizontally
                }
                window::Direction::NorthWest | window::Direction::SouthEast => {
                    mouse::Interaction::ResizingDiagonallyDown
                }
                window::Direction::NorthEast | window::Direction::SouthWest => {
                    mouse::Interaction::ResizingDiagonallyUp
                }
            };
        }

        child_cursor
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

/// Renders the compact 40px top System Title Bar used when the tab strip is docked Bottom/Left/Right.
pub fn render_top_system_title_bar<'a>(
    window_id: window::Id,
    title: String,
    left_controls: Element<'a, Message, Theme, iced::Renderer>,
    right_controls: Element<'a, Message, Theme, iced::Renderer>,
    bar_height: f32,
    chrome: ChromePalette,
    top_resize_guard: bool,
) -> Element<'a, Message, Theme, iced::Renderer> {
    let drag_spacer = TitleBarDragZone::with_resize_guard(
        window_id,
        true,
        if top_resize_guard {
            EDGE_RESIZE_HIT_PX
        } else {
            0.0
        },
        row![
            container(space::horizontal().width(6).height(6)).style(move |_| container::Style {
                background: Some(Background::Color(chrome.accent)),
                border: Border {
                    radius: 999.0.into(),
                    ..Default::default()
                },
                ..Default::default()
            }),
            container(
                text(title)
                    .size(12.0)
                    .font(ui_font())
                    .wrapping(iced::widget::text::Wrapping::None)
                    .color(chrome.text_secondary),
            )
            .width(Length::Fill)
            .clip(true),
        ]
        .spacing(8)
        .padding([0, 12])
        .height(Length::Fixed(bar_height))
        .align_y(Alignment::Center)
        .into(),
    );

    container(
        row![left_controls, drag_spacer, right_controls]
            .spacing(0)
            .height(Length::Fixed(bar_height))
            .align_y(Alignment::Center),
    )
    .width(Length::Fill)
    .height(Length::Fixed(bar_height))
    .style(move |_| container::Style {
        background: Some(Background::Color(chrome.bg_more)),
        ..Default::default()
    })
    .into()
}
