use std::collections::HashMap;
use std::ops::Range;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Instant;

use iced::advanced::clipboard::Kind as ClipboardKind;
use iced::advanced::input_method::{self, InputMethod};
use iced::advanced::layout::{self, Layout};
use iced::advanced::renderer::{self, Quad};
use iced::advanced::text::{self, LineHeight, Paragraph as _, Shaping, Text, Wrapping};
use iced::advanced::widget::tree::{self, Tree};
use iced::advanced::{Clipboard, Shell, Widget};
use iced::keyboard::{self, Key, Modifiers, key::Named};
use iced::{
    Background, Border, Color, Element, Event, Font, Length, Pixels, Point, Rectangle, Shadow,
    Size, Theme, mouse, window,
};
use termewnal_core::{
    BuiltinTheme, CursorStyle, RightClickAction, TerminalConfig, ThemePalette,
};
use termewnal_pty::{
    CellFlags, CompiledHighlightRule, CursorShape, DetectedLink, DetectedLinkKind,
    GridSearchMatch, HighlightStyle, NamedColor, PtyMouseButton, PtyMouseEventKind, Rgb,
    SelectionType, Side, TermColor, TerminalCell, TerminalSnapshot, compile_highlight_rules,
    compute_highlight_grid, encode_mouse_event, link_at_cell,
};

use crate::app::{Message, SessionId};
use crate::terminal_ergonomics::rgb_to_iced;

/// Outcome of applying an [`input_method::Event`] to [`ImeComposition`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImeUpdate {
    Redraw,
    Commit(String),
}

/// Active Vietnamese/CJK IME pre-edit composition buffer for a terminal viewport.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ImeComposition {
    pub content: String,
    pub selection: Option<Range<usize>>,
}

impl ImeComposition {
    /// Updates pre-edit state from an Iced `input_method::Event` and returns the next action.
    pub fn apply_event(&mut self, event: &input_method::Event) -> ImeUpdate {
        match event {
            input_method::Event::Opened => ImeUpdate::Redraw,
            input_method::Event::Closed => {
                self.clear();
                ImeUpdate::Redraw
            }
            input_method::Event::Preedit(text, selection) => {
                self.content = text.clone();
                self.selection = selection.clone();
                ImeUpdate::Redraw
            }
            input_method::Event::Commit(text) => {
                self.clear();
                ImeUpdate::Commit(text.clone())
            }
        }
    }

    /// Clears any active pre-edit string and selection range.
    pub fn clear(&mut self) {
        self.content.clear();
        self.selection = None;
    }

    /// Converts the active pre-edit text into an Iced `Preedit` descriptor when non-empty.
    pub fn to_iced_preedit(&self, font_size: f32) -> Option<input_method::Preedit<&str>> {
        if self.content.is_empty() {
            return None;
        }
        Some(input_method::Preedit {
            content: self.content.as_str(),
            selection: self.selection.clone(),
            text_size: Some(Pixels(font_size)),
        })
    }
}

/// Inner padding (in logical pixels) around the terminal grid matching Tabby's 15px terminal margin.
pub const TERMINAL_INNER_PAD: f32 = 14.0;

fn content_bounds(bounds: Rectangle) -> Rectangle {
    let pad = TERMINAL_INNER_PAD;
    if bounds.width <= pad * 2.0 + 8.0 || bounds.height <= pad * 2.0 + 8.0 {
        return bounds;
    }
    Rectangle::new(
        Point::new(bounds.x + pad, bounds.y + pad),
        Size::new(bounds.width - pad * 2.0, bounds.height - pad * 2.0),
    )
}

/// Computes the pixel anchor rectangle for the OS IME candidate popup at the active cursor cell.
pub fn ime_cursor_anchor_rect(
    snapshot: &TerminalSnapshot,
    bounds: Rectangle,
    cell_size: Size,
) -> Rectangle {
    let (col, row) = snapshot
        .cursor
        .map(|c| (c.col as f32, c.row as f32))
        .unwrap_or_else(|| (0.0, snapshot.rows.saturating_sub(1) as f32));

    let x = (bounds.x + col * cell_size.width).min((bounds.x + bounds.width - cell_size.width).max(bounds.x));
    let y = (bounds.y + row * cell_size.height).min((bounds.y + bounds.height - cell_size.height).max(bounds.y));
    Rectangle::new(Point::new(x, y), cell_size)
}

/// Rendering and behavioral configuration passed to [`TerminalViewport`].
#[derive(Debug, Clone)]
pub struct TerminalViewOptions {
    pub palette: TerminalPalette,
    pub font_family: String,
    pub font_size: f32,
    pub font_ligatures: bool,
    pub cursor_style: CursorStyle,
    pub cursor_blink: bool,
    pub cursor_visible: bool,
    pub copy_on_select: bool,
    pub right_click_action: RightClickAction,
    pub middle_click_paste: bool,
    pub opacity: f32,
    pub highlight_rules: Arc<[CompiledHighlightRule]>,
    pub bell_flash: bool,
    pub ime_enabled: bool,
}

impl PartialEq for TerminalViewOptions {
    fn eq(&self, other: &Self) -> bool {
        self.palette == other.palette
            && self.font_family == other.font_family
            && self.font_size == other.font_size
            && self.font_ligatures == other.font_ligatures
            && self.cursor_style == other.cursor_style
            && self.cursor_blink == other.cursor_blink
            && self.cursor_visible == other.cursor_visible
            && self.copy_on_select == other.copy_on_select
            && self.right_click_action == other.right_click_action
            && self.middle_click_paste == other.middle_click_paste
            && self.opacity == other.opacity
            && Arc::ptr_eq(&self.highlight_rules, &other.highlight_rules)
            && self.bell_flash == other.bell_flash
            && self.ime_enabled == other.ime_enabled
    }
}

impl TerminalViewOptions {
    /// Builds [`TerminalViewOptions`] from persisted [`TerminalConfig`].
    pub fn from_config(cfg: &TerminalConfig, ime_enabled: bool) -> Self {
        Self::from_config_with_runtime(
            cfg,
            ime_enabled,
            Arc::from(compile_highlight_rules(&cfg.highlight_rules)),
            true,
            false,
        )
    }

    /// Builds [`TerminalViewOptions`] with pre-compiled highlight rules and blink/bell state.
    pub fn from_config_with_runtime(
        cfg: &TerminalConfig,
        ime_enabled: bool,
        highlight_rules: Arc<[CompiledHighlightRule]>,
        cursor_visible: bool,
        bell_flash: bool,
    ) -> Self {
        Self {
            palette: TerminalPalette::from_config(cfg),
            font_family: cfg.font_family.clone(),
            font_size: cfg.font_size,
            font_ligatures: cfg.font_ligatures,
            cursor_style: cfg.cursor_style,
            cursor_blink: cfg.cursor_blink,
            cursor_visible: !cfg.cursor_blink || cursor_visible,
            copy_on_select: cfg.copy_on_select,
            right_click_action: cfg.right_click_action,
            middle_click_paste: cfg.middle_click_paste,
            opacity: cfg.window_opacity.clamp(0.3, 1.0),
            highlight_rules,
            bell_flash,
            ime_enabled,
        }
    }
}

/// Resolved terminal color palette derived from [`ThemePalette`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TerminalPalette {
    pub theme: BuiltinTheme,
    pub foreground: Color,
    pub background: Color,
    pub cursor: Color,
    pub selection_bg: Color,
    pub match_bg: Color,
    pub match_fg: Color,
    pub active_match_bg: Color,
    pub active_match_fg: Color,
    pub ui_surface: Color,
    pub ui_accent: Color,
    pub ansi: [Color; 16],
}

impl TerminalPalette {
    /// Builds a [`TerminalPalette`] from [`TerminalConfig::theme`] and optional color overrides.
    pub fn from_config(cfg: &TerminalConfig) -> Self {
        Self::from_core(&ThemePalette::for_name_with_overrides(
            &cfg.theme,
            &cfg.theme_overrides,
        ))
    }

    /// Converts a core [`ThemePalette`] to Iced colors.
    pub fn from_core(core: &ThemePalette) -> Self {
        Self {
            theme: core.theme,
            foreground: rgb_to_iced(core.foreground),
            background: rgb_to_iced(core.background),
            cursor: rgb_to_iced(core.cursor),
            selection_bg: rgb_to_iced(core.selection_bg),
            match_bg: rgb_to_iced(core.match_bg),
            match_fg: rgb_to_iced(core.match_fg),
            active_match_bg: rgb_to_iced(core.active_match_bg),
            active_match_fg: rgb_to_iced(core.active_match_fg),
            ui_surface: rgb_to_iced(core.ui_surface),
            ui_accent: rgb_to_iced(core.ui_accent),
            ansi: core.ansi.map(rgb_to_iced),
        }
    }

    fn resolve(&self, color: TermColor) -> Color {
        match color {
            TermColor::Spec(Rgb { r, g, b }) => rgb(r, g, b),
            TermColor::Named(named) => match named {
                NamedColor::Foreground | NamedColor::BrightForeground => self.foreground,
                NamedColor::Background => self.background,
                NamedColor::Cursor => self.cursor,
                NamedColor::Black => self.ansi[0],
                NamedColor::Red => self.ansi[1],
                NamedColor::Green => self.ansi[2],
                NamedColor::Yellow => self.ansi[3],
                NamedColor::Blue => self.ansi[4],
                NamedColor::Magenta => self.ansi[5],
                NamedColor::Cyan => self.ansi[6],
                NamedColor::White => self.ansi[7],
                NamedColor::BrightBlack | NamedColor::DimBlack => self.ansi[8],
                NamedColor::BrightRed | NamedColor::DimRed => self.ansi[9],
                NamedColor::BrightGreen | NamedColor::DimGreen => self.ansi[10],
                NamedColor::BrightYellow | NamedColor::DimYellow => self.ansi[11],
                NamedColor::BrightBlue | NamedColor::DimBlue => self.ansi[12],
                NamedColor::BrightMagenta | NamedColor::DimMagenta => self.ansi[13],
                NamedColor::BrightCyan | NamedColor::DimCyan => self.ansi[14],
                NamedColor::BrightWhite | NamedColor::DimWhite => self.ansi[15],
                NamedColor::DimForeground => Color {
                    a: 0.7,
                    ..self.foreground
                },
            },
            TermColor::Indexed(idx) => indexed_color(idx, &self.ansi),
        }
    }
}

/// Custom `iced::advanced::Widget` rendering a terminal grid and capturing keyboard/IME/mouse/drop input.
pub struct TerminalViewport {
    session_id: SessionId,
    snapshot: Arc<TerminalSnapshot>,
    options: TerminalViewOptions,
    font: Font,
    search_matches: Arc<[GridSearchMatch]>,
    active_search_match: Option<usize>,
    explicit_focus: Option<bool>,
}

impl TerminalViewport {
    /// Creates a new [`TerminalViewport`] element bound to `session_id` and `snapshot`.
    pub fn new(
        session_id: SessionId,
        snapshot: Arc<TerminalSnapshot>,
        options: TerminalViewOptions,
        search_matches: Arc<[GridSearchMatch]>,
        active_search_match: Option<usize>,
    ) -> Element<'static, Message, Theme, iced::Renderer> {
        Self::build(
            session_id,
            snapshot,
            options,
            search_matches,
            active_search_match,
            None,
        )
    }

    /// Creates a [`TerminalViewport`] element whose focus state is explicitly controlled by a parent pane.
    pub fn with_focus(
        session_id: SessionId,
        snapshot: Arc<TerminalSnapshot>,
        options: TerminalViewOptions,
        search_matches: Arc<[GridSearchMatch]>,
        active_search_match: Option<usize>,
        focused: bool,
    ) -> Element<'static, Message, Theme, iced::Renderer> {
        Self::build(
            session_id,
            snapshot,
            options,
            search_matches,
            active_search_match,
            Some(focused),
        )
    }

    fn build(
        session_id: SessionId,
        snapshot: Arc<TerminalSnapshot>,
        mut options: TerminalViewOptions,
        search_matches: Arc<[GridSearchMatch]>,
        active_search_match: Option<usize>,
        explicit_focus: Option<bool>,
    ) -> Element<'static, Message, Theme, iced::Renderer> {
        let font = resolve_font(&options.font_family);
        options.font_size = options.font_size.max(6.0);

        Element::new(Self {
            session_id,
            snapshot,
            options,
            font,
            search_matches,
            active_search_match,
            explicit_focus,
        })
    }
}

#[derive(Debug, Clone)]
struct ViewportState {
    focused: bool,
    modifiers: Modifiers,
    last_requested_size: Option<(u16, u16)>,
    ime: ImeComposition,
    cursor_cell: Option<(usize, usize)>,
    hovered_link: Option<DetectedLink>,
    dragging_selection: bool,
    last_click: Option<(Instant, usize, usize, u8)>,
    held_mouse_button: Option<PtyMouseButton>,
    last_motion_cell: Option<(usize, usize)>,
}

impl Default for ViewportState {
    fn default() -> Self {
        Self {
            focused: true,
            modifiers: Modifiers::empty(),
            last_requested_size: None,
            ime: ImeComposition::default(),
            cursor_cell: None,
            hovered_link: None,
            dragging_selection: false,
            last_click: None,
            held_mouse_button: None,
            last_motion_cell: None,
        }
    }
}

impl Widget<Message, Theme, iced::Renderer> for TerminalViewport {
    fn tag(&self) -> tree::Tag {
        tree::Tag::of::<ViewportState>()
    }

    fn state(&self) -> tree::State {
        tree::State::new(ViewportState::default())
    }

    fn size(&self) -> Size<Length> {
        Size::new(Length::Fill, Length::Fill)
    }

    fn layout(
        &mut self,
        _tree: &mut Tree,
        _renderer: &iced::Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        layout::Node::new(limits.max())
    }

    fn update(
        &mut self,
        tree: &mut Tree,
        event: &Event,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        _renderer: &iced::Renderer,
        clipboard: &mut dyn Clipboard,
        shell: &mut Shell<'_, Message>,
        _viewport: &Rectangle,
    ) {
        let state = tree.state.downcast_mut::<ViewportState>();
        let is_focused = self.explicit_focus.unwrap_or(state.focused);
        let bounds = layout.bounds();
        let inner = content_bounds(bounds);
        let cell_size = measure_cell_size(self.font, self.options.font_size);

        match event {
            Event::Window(
                window::Event::Opened { .. }
                | window::Event::Resized(_)
                | window::Event::Rescaled(_)
                | window::Event::RedrawRequested(_),
            ) => {
                let cols = (inner.width / cell_size.width).floor().max(1.0) as u16;
                let rows = (inner.height / cell_size.height).floor().max(1.0) as u16;
                if (cols, rows) == (self.snapshot.cols, self.snapshot.rows) {
                    state.last_requested_size = Some((cols, rows));
                } else if state.last_requested_size != Some((cols, rows)) {
                    state.last_requested_size = Some((cols, rows));
                    shell.publish(Message::ViewportResized {
                        session_id: self.session_id,
                        cols,
                        rows,
                    });
                }

                if matches!(event, Event::Window(window::Event::RedrawRequested(_))) {
                    if self.options.ime_enabled && is_focused {
                        let cursor_rect = ime_cursor_anchor_rect(&self.snapshot, inner, cell_size);
                        let preedit = state.ime.to_iced_preedit(self.options.font_size);
                        shell.request_input_method(&InputMethod::Enabled {
                            cursor: cursor_rect,
                            purpose: input_method::Purpose::Terminal,
                            preedit,
                        });
                    } else if self.options.ime_enabled && self.explicit_focus.is_none() && !is_focused {
                        shell.request_input_method::<&str>(&InputMethod::Disabled);
                    }
                }
            }
            Event::Window(window::Event::FileDropped(path)) => {
                let over_pane = cursor.is_over(bounds)
                    || (matches!(cursor, mouse::Cursor::Unavailable) && is_focused);
                if over_pane {
                    shell.publish(Message::FileDropped {
                        session_id: self.session_id,
                        path: path.clone(),
                        shift_held: state.modifiers.shift(),
                    });
                    shell.capture_event();
                }
            }
            Event::Mouse(mouse::Event::CursorMoved { position }) => {
                let prev_hover = state.hovered_link.clone();
                let pty_mouse_active = self.snapshot.mouse_mode && !state.modifiers.shift();
                if bounds.contains(*position) {
                    let (col, row, side) = point_to_cell(*position, inner, cell_size, &self.snapshot);
                    state.cursor_cell = Some((col, row));
                    state.hovered_link = if state.modifiers.control() {
                        link_at_cell(&self.snapshot, col, row)
                    } else {
                        None
                    };
                    if state.dragging_selection {
                        shell.publish(Message::SelectionUpdate {
                            session_id: self.session_id,
                            col,
                            row,
                            side,
                        });
                    } else if pty_mouse_active
                        && (state.held_mouse_button.is_some() || self.snapshot.mouse_motion)
                        && state.last_motion_cell != Some((col, row))
                    {
                        let btn = state.held_mouse_button.unwrap_or(PtyMouseButton::Left);
                        state.last_motion_cell = Some((col, row));
                        self.forward_pty_mouse(
                            btn,
                            PtyMouseEventKind::Drag,
                            col,
                            row,
                            state.modifiers,
                            shell,
                        );
                    }
                } else {
                    state.cursor_cell = None;
                    state.last_motion_cell = None;
                    state.hovered_link = None;
                }
                if state.hovered_link != prev_hover {
                    shell.request_redraw();
                }
            }
            Event::Mouse(mouse::Event::CursorLeft) => {
                state.cursor_cell = None;
                state.last_motion_cell = None;
                if state.hovered_link.take().is_some() {
                    shell.request_redraw();
                }
            }
            Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)) => {
                let clicked_inside = cursor.is_over(bounds);
                if self.explicit_focus.is_none() {
                    let was_focused = state.focused;
                    state.focused = clicked_inside;
                    if state.focused != was_focused {
                        shell.request_redraw();
                    }
                }
                if !clicked_inside {
                    state.ime.clear();
                    state.dragging_selection = false;
                    return;
                }

                shell.publish(Message::TerminalFocused);
                if let Some(pos) = cursor.position_in(bounds) {
                    let abs = Point::new(bounds.x + pos.x, bounds.y + pos.y);
                    let (col, row, side) = point_to_cell(abs, inner, cell_size, &self.snapshot);
                    if state.modifiers.control()
                        && let Some(detected) = link_at_cell(&self.snapshot, col, row)
                    {
                        match detected.kind {
                            DetectedLinkKind::WebUrl => {
                                shell.publish(Message::OpenUrl(detected.target));
                            }
                            DetectedLinkKind::LocalFile {
                                path,
                                line,
                                col: file_col,
                            } => {
                                shell.publish(Message::OpenLocalFile {
                                    session_id: self.session_id,
                                    path,
                                    line,
                                    col: file_col,
                                });
                            }
                        }
                        shell.capture_event();
                        return;
                    }

                    if self.snapshot.mouse_mode && !state.modifiers.shift() {
                        state.held_mouse_button = Some(PtyMouseButton::Left);
                        state.last_motion_cell = Some((col, row));
                        self.forward_pty_mouse(
                            PtyMouseButton::Left,
                            PtyMouseEventKind::Press,
                            col,
                            row,
                            state.modifiers,
                            shell,
                        );
                        shell.capture_event();
                        return;
                    }

                    let click_count = next_click_count(&mut state.last_click, col, row);
                    let selection_type = match click_count {
                        2 => SelectionType::Semantic,
                        3 => SelectionType::Lines,
                        _ => SelectionType::Simple,
                    };
                    state.dragging_selection = true;
                    shell.publish(Message::SelectionStart {
                        session_id: self.session_id,
                        col,
                        row,
                        side,
                        selection_type,
                    });
                    shell.capture_event();
                }
            }
            Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)) => {
                if state.held_mouse_button == Some(PtyMouseButton::Left) {
                    state.held_mouse_button = None;
                    state.last_motion_cell = None;
                    if let Some(pos) = cursor.position_in(bounds) {
                        let abs = Point::new(bounds.x + pos.x, bounds.y + pos.y);
                        let (col, row, _) = point_to_cell(abs, inner, cell_size, &self.snapshot);
                        self.forward_pty_mouse(
                            PtyMouseButton::Left,
                            PtyMouseEventKind::Release,
                            col,
                            row,
                            state.modifiers,
                            shell,
                        );
                        shell.capture_event();
                    }
                }
                if state.dragging_selection {
                    state.dragging_selection = false;
                    shell.publish(Message::SelectionFinish {
                        session_id: self.session_id,
                    });
                    shell.capture_event();
                }
            }
            Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Middle)) => {
                if cursor.is_over(bounds) {
                    if self.snapshot.mouse_mode && !state.modifiers.shift() {
                        if let Some(pos) = cursor.position_in(bounds) {
                            let abs = Point::new(bounds.x + pos.x, bounds.y + pos.y);
                            let (col, row, _) =
                                point_to_cell(abs, inner, cell_size, &self.snapshot);
                            state.held_mouse_button = Some(PtyMouseButton::Middle);
                            state.last_motion_cell = Some((col, row));
                            self.forward_pty_mouse(
                                PtyMouseButton::Middle,
                                PtyMouseEventKind::Press,
                                col,
                                row,
                                state.modifiers,
                                shell,
                            );
                            shell.capture_event();
                        }
                    } else if self.options.middle_click_paste {
                        publish_clipboard_paste(self.session_id, clipboard, shell);
                        shell.capture_event();
                    }
                }
            }
            Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Middle)) => {
                if state.held_mouse_button == Some(PtyMouseButton::Middle) {
                    state.held_mouse_button = None;
                    state.last_motion_cell = None;
                    if let Some(pos) = cursor.position_in(bounds) {
                        let abs = Point::new(bounds.x + pos.x, bounds.y + pos.y);
                        let (col, row, _) = point_to_cell(abs, inner, cell_size, &self.snapshot);
                        self.forward_pty_mouse(
                            PtyMouseButton::Middle,
                            PtyMouseEventKind::Release,
                            col,
                            row,
                            state.modifiers,
                            shell,
                        );
                        shell.capture_event();
                    }
                }
            }
            Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Right)) => {
                if cursor.is_over(bounds) {
                    if self.snapshot.mouse_mode && !state.modifiers.shift() {
                        if let Some(pos) = cursor.position_in(bounds) {
                            let abs = Point::new(bounds.x + pos.x, bounds.y + pos.y);
                            let (col, row, _) =
                                point_to_cell(abs, inner, cell_size, &self.snapshot);
                            state.held_mouse_button = Some(PtyMouseButton::Right);
                            state.last_motion_cell = Some((col, row));
                            self.forward_pty_mouse(
                                PtyMouseButton::Right,
                                PtyMouseEventKind::Press,
                                col,
                                row,
                                state.modifiers,
                                shell,
                            );
                            shell.capture_event();
                        }
                    } else {
                        match self.options.right_click_action {
                            RightClickAction::Paste => {
                                publish_clipboard_paste(self.session_id, clipboard, shell);
                                shell.capture_event();
                            }
                            RightClickAction::Menu => {
                                if let Some(pos) = cursor.position() {
                                    shell.publish(Message::OpenTerminalContextMenu {
                                        session_id: self.session_id,
                                        position: pos,
                                    });
                                    shell.capture_event();
                                }
                            }
                            RightClickAction::Off => {}
                        }
                    }
                }
            }
            Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Right)) => {
                if state.held_mouse_button == Some(PtyMouseButton::Right) {
                    state.held_mouse_button = None;
                    state.last_motion_cell = None;
                    if let Some(pos) = cursor.position_in(bounds) {
                        let abs = Point::new(bounds.x + pos.x, bounds.y + pos.y);
                        let (col, row, _) = point_to_cell(abs, inner, cell_size, &self.snapshot);
                        self.forward_pty_mouse(
                            PtyMouseButton::Right,
                            PtyMouseEventKind::Release,
                            col,
                            row,
                            state.modifiers,
                            shell,
                        );
                        shell.capture_event();
                    }
                }
            }
            Event::Mouse(mouse::Event::WheelScrolled { delta }) => {
                if cursor.is_over(bounds) {
                    let delta_lines = match delta {
                        mouse::ScrollDelta::Lines { y, .. } => (*y * 3.0).round() as i32,
                        mouse::ScrollDelta::Pixels { y, .. } => {
                            (*y / cell_size.height.max(1.0)).round() as i32
                        }
                    };
                    if delta_lines != 0 {
                        if self.snapshot.mouse_mode && !state.modifiers.shift() {
                            let (col, row) = cursor
                                .position_in(bounds)
                                .map(|pos| {
                                    let abs = Point::new(bounds.x + pos.x, bounds.y + pos.y);
                                    let (c, r, _) =
                                        point_to_cell(abs, inner, cell_size, &self.snapshot);
                                    (c, r)
                                })
                                .or(state.cursor_cell)
                                .unwrap_or((0, 0));
                            let btn = if delta_lines > 0 {
                                PtyMouseButton::WheelUp
                            } else {
                                PtyMouseButton::WheelDown
                            };
                            for _ in 0..delta_lines.unsigned_abs().clamp(1, 3) {
                                self.forward_pty_mouse(
                                    btn,
                                    PtyMouseEventKind::Press,
                                    col,
                                    row,
                                    state.modifiers,
                                    shell,
                                );
                            }
                            shell.capture_event();
                        } else {
                            shell.publish(Message::ScrollViewport {
                                session_id: self.session_id,
                                delta_lines,
                            });
                            shell.capture_event();
                        }
                    }
                }
            }
            Event::Keyboard(keyboard::Event::ModifiersChanged(modifiers)) => {
                state.modifiers = *modifiers;
                let prev = state.hovered_link.clone();
                state.hovered_link = if modifiers.control()
                    && let Some((col, row)) = state.cursor_cell
                {
                    link_at_cell(&self.snapshot, col, row)
                } else {
                    None
                };
                if state.hovered_link != prev {
                    shell.request_redraw();
                }
            }
            Event::Keyboard(keyboard::Event::KeyPressed {
                key,
                modified_key,
                modifiers,
                text,
                ..
            }) => {
                state.modifiers = *modifiers;
                if self.options.ime_enabled && is_focused && !shell.is_event_captured() {
                    if modifiers.shift()
                        && !modifiers.control()
                        && !modifiers.alt()
                        && let Key::Named(named) = key
                    {
                        match named {
                            Named::PageUp => {
                                shell.publish(Message::ScrollViewport {
                                    session_id: self.session_id,
                                    delta_lines: self.snapshot.rows as i32,
                                });
                                shell.capture_event();
                                return;
                            }
                            Named::PageDown => {
                                shell.publish(Message::ScrollViewport {
                                    session_id: self.session_id,
                                    delta_lines: -(self.snapshot.rows as i32),
                                });
                                shell.capture_event();
                                return;
                            }
                            Named::Home => {
                                shell.publish(Message::ScrollToTop(self.session_id));
                                shell.capture_event();
                                return;
                            }
                            Named::End => {
                                shell.publish(Message::ScrollToBottom(self.session_id));
                                shell.capture_event();
                                return;
                            }
                            _ => {}
                        }
                    }

                    let bytes =
                        key_event_to_bytes(key, modified_key, *modifiers, text.as_deref());
                    if !bytes.is_empty() {
                        shell.publish(Message::TerminalInput {
                            session_id: self.session_id,
                            bytes,
                        });
                        shell.capture_event();
                    }
                }
            }
            Event::InputMethod(ev) => {
                if !self.options.ime_enabled || !is_focused {
                    if matches!(ev, input_method::Event::Closed) {
                        state.ime.clear();
                    }
                    return;
                }
                match state.ime.apply_event(ev) {
                    ImeUpdate::Redraw => {
                        shell.request_redraw();
                        if matches!(ev, input_method::Event::Preedit(_, _)) {
                            shell.capture_event();
                        }
                    }
                    ImeUpdate::Commit(committed) => {
                        if !committed.is_empty() {
                            shell.publish(Message::ImeCommitted {
                                session_id: self.session_id,
                                text: committed,
                            });
                            shell.capture_event();
                        }
                        shell.request_redraw();
                    }
                }
            }
            _ => {}
        }
    }

    fn mouse_interaction(
        &self,
        tree: &Tree,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        _viewport: &Rectangle,
        _renderer: &iced::Renderer,
    ) -> mouse::Interaction {
        let state = tree.state.downcast_ref::<ViewportState>();
        if cursor.is_over(layout.bounds()) {
            if state.hovered_link.is_some() {
                mouse::Interaction::Pointer
            } else {
                mouse::Interaction::Text
            }
        } else {
            mouse::Interaction::None
        }
    }

    fn draw(
        &self,
        tree: &Tree,
        renderer: &mut iced::Renderer,
        _theme: &Theme,
        _style: &renderer::Style,
        layout: Layout<'_>,
        _cursor: mouse::Cursor,
        viewport: &Rectangle,
    ) {
        use iced::advanced::renderer::Renderer as _;
        use iced::advanced::text::Renderer as _;

        let state = tree.state.downcast_ref::<ViewportState>();
        let is_focused = self.explicit_focus.unwrap_or(state.focused);
        let bounds = layout.bounds();
        let inner = content_bounds(bounds);
        let Some(clip_bounds) = bounds.intersection(viewport) else {
            return;
        };

        let cell_size = measure_cell_size(self.font, self.options.font_size);
        let palette = &self.options.palette;
        let shaping = if self.options.font_ligatures {
            Shaping::Advanced
        } else {
            Shaping::Auto
        };

        // Paint full viewport background first with configured opacity.
        let canvas_bg = Color {
            a: palette.background.a * self.options.opacity,
            ..palette.background
        };
        renderer.fill_quad(
            Quad {
                bounds,
                border: Border::default(),
                shadow: Shadow::default(),
                snap: true,
            },
            Background::Color(canvas_bg),
        );

        let cols = self.snapshot.cols as usize;
        let rows = self.snapshot.rows as usize;
        let highlight_grid = compute_highlight_grid(&self.snapshot, &self.options.highlight_rules);
        let bold_font = Font {
            weight: iced::font::Weight::Bold,
            ..self.font
        };

        for row in 0..rows {
            let y = inner.y + (row as f32) * cell_size.height;
            if y >= clip_bounds.y + clip_bounds.height {
                break;
            }
            if y + cell_size.height < clip_bounds.y {
                continue;
            }

            let grid_line = self.snapshot.top_line + row as i32;
            let mut col = 0;
            while col < cols {
                let Some(cell) = self.snapshot.cell(col, row) else {
                    break;
                };

                if cell.flags.contains(CellFlags::WIDE_CHAR_SPACER) {
                    col += 1;
                    continue;
                }

                let hl = highlight_grid.get(row * cols + col).copied().flatten();
                let (fg, bg, run_bold) =
                    self.resolve_cell_appearance(cell, palette, grid_line, col, row, cols, hl);

                let cell_width_cells = if cell.flags.contains(CellFlags::WIDE_CHAR) {
                    2.0
                } else {
                    1.0
                };
                let x = inner.x + (col as f32) * cell_size.width;

                if bg != palette.background {
                    renderer.fill_quad(
                        Quad {
                            bounds: Rectangle::new(
                                Point::new(x, y),
                                Size::new(cell_size.width * cell_width_cells, cell_size.height),
                            ),
                            border: Border::default(),
                            shadow: Shadow::default(),
                            snap: true,
                        },
                        Background::Color(bg),
                    );
                }

                if cell.c != ' ' && cell.c != '\0' {
                    let mut run = String::new();
                    run.push(cell.c);
                    let run_start_col = col;
                    let run_fg = fg;
                    let first_ascii = cell.c.is_ascii();

                    if !cell.flags.contains(CellFlags::WIDE_CHAR) {
                        while col + 1 < cols {
                            let Some(next) = self.snapshot.cell(col + 1, row) else {
                                break;
                            };
                            if next.c == ' '
                                || next.c == '\0'
                                || next.flags.contains(CellFlags::WIDE_CHAR)
                                || next.flags.contains(CellFlags::WIDE_CHAR_SPACER)
                            {
                                break;
                            }
                            if !self.options.font_ligatures && (next.c.is_ascii() != first_ascii) {
                                break;
                            }
                            let next_hl = highlight_grid.get(row * cols + col + 1).copied().flatten();
                            let (next_fg, next_bg, next_bold) = self.resolve_cell_appearance(
                                next,
                                palette,
                                grid_line,
                                col + 1,
                                row,
                                cols,
                                next_hl,
                            );
                            if next_fg != run_fg || next_bg != bg || next_bold != run_bold {
                                break;
                            }
                            col += 1;
                            run.push(next.c);
                        }
                    }

                    let run_x = inner.x + (run_start_col as f32) * cell_size.width;
                    renderer.fill_text(
                        Text {
                            content: run,
                            bounds: Size::new(inner.width, cell_size.height),
                            size: Pixels(self.options.font_size),
                            line_height: LineHeight::Absolute(Pixels(cell_size.height)),
                            font: if run_bold { bold_font } else { self.font },
                            align_x: text::Alignment::Left,
                            align_y: iced::alignment::Vertical::Top,
                            shaping,
                            wrapping: Wrapping::None,
                        },
                        Point::new(run_x, y),
                        run_fg,
                        clip_bounds,
                    );
                }

                col += 1;
            }
        }

        // Underline hovered URL or local file path on Ctrl+Hover.
        if let Some(hovered) = &state.hovered_link {
            for span in &hovered.spans {
                let y = inner.y + (span.row as f32 + 1.0) * cell_size.height - 2.0;
                let x = inner.x + (span.start_col as f32) * cell_size.width;
                let width = ((span.end_col - span.start_col + 1) as f32) * cell_size.width;
                renderer.fill_quad(
                    Quad {
                        bounds: Rectangle::new(Point::new(x, y), Size::new(width, 2.0)),
                        border: Border::default(),
                        shadow: Shadow::default(),
                        snap: true,
                    },
                    Background::Color(palette.ui_accent),
                );
            }
        }

        if let Some(cursor_snap) = self.snapshot.cursor
            && cursor_snap.shape != CursorShape::Hidden
            && (!is_focused || self.options.cursor_visible)
        {
            let cursor_rect = ime_cursor_anchor_rect(&self.snapshot, inner, cell_size);
            let cursor_color = Color {
                a: if is_focused { 0.75 } else { 0.35 },
                ..palette.cursor
            };

            let quad = match cursor_snap.shape {
                CursorShape::Block => Quad {
                    bounds: cursor_rect,
                    border: Border::default(),
                    shadow: Shadow::default(),
                    snap: true,
                },
                CursorShape::HollowBlock => Quad {
                    bounds: cursor_rect,
                    border: Border {
                        color: palette.cursor,
                        width: 1.0,
                        radius: 0.0.into(),
                    },
                    shadow: Shadow::default(),
                    snap: true,
                },
                CursorShape::Beam => Quad {
                    bounds: Rectangle::new(
                        cursor_rect.position(),
                        Size::new(2.0, cursor_rect.height),
                    ),
                    border: Border::default(),
                    shadow: Shadow::default(),
                    snap: true,
                },
                CursorShape::Underline => Quad {
                    bounds: Rectangle::new(
                        Point::new(cursor_rect.x, cursor_rect.y + cursor_rect.height - 2.0),
                        Size::new(cursor_rect.width, 2.0),
                    ),
                    border: Border::default(),
                    shadow: Shadow::default(),
                    snap: true,
                },
                CursorShape::Hidden => unreachable!(),
            };

            let fill = if matches!(cursor_snap.shape, CursorShape::HollowBlock) {
                Background::Color(Color::TRANSPARENT)
            } else {
                Background::Color(cursor_color)
            };
            renderer.fill_quad(quad, fill);
        }

        // Subtle scrollbar thumb and scrollback offset badge when scrolled up in history.
        if self.snapshot.history_size > 0 && bounds.height > 24.0 && bounds.width > 20.0 {
            let total_lines = (self.snapshot.history_size + rows).max(1) as f32;
            let track_h = (bounds.height - 8.0).max(12.0);
            let thumb_h = ((rows as f32 / total_lines) * track_h).clamp(18.0, track_h);
            let scroll_ratio = (self.snapshot.display_offset as f32
                / self.snapshot.history_size.max(1) as f32)
                .clamp(0.0, 1.0);
            let thumb_y = bounds.y + 4.0 + (1.0 - scroll_ratio) * (track_h - thumb_h);
            let thumb_alpha = if self.snapshot.display_offset > 0 { 0.55 } else { 0.22 };
            renderer.fill_quad(
                Quad {
                    bounds: Rectangle::new(
                        Point::new(bounds.x + bounds.width - 6.0, thumb_y),
                        Size::new(3.0, thumb_h),
                    ),
                    border: Border {
                        color: Color::TRANSPARENT,
                        width: 0.0,
                        radius: 2.0.into(),
                    },
                    shadow: Shadow::default(),
                    snap: true,
                },
                Background::Color(Color {
                    a: thumb_alpha,
                    ..palette.foreground
                }),
            );
        }

        // Render active visual bell flash border if triggered recently.
        if self.options.bell_flash && bounds.width > 4.0 && bounds.height > 4.0 {
            renderer.fill_quad(
                Quad {
                    bounds,
                    border: Border {
                        color: Color {
                            a: 0.75,
                            ..palette.ui_accent
                        },
                        width: 2.0,
                        radius: 0.0.into(),
                    },
                    shadow: Shadow::default(),
                    snap: true,
                },
                Background::Color(Color {
                    a: 0.06,
                    ..palette.ui_accent
                }),
            );
        }

        // Render active Vietnamese/CJK IME pre-edit overlay inline at the cursor cell.
        if is_focused && !state.ime.content.is_empty() {
            draw_ime_preedit(
                renderer,
                &state.ime.content,
                self.font,
                self.options.font_size,
                cell_size,
                inner,
                clip_bounds,
                &self.snapshot,
                palette,
            );
        }
    }
}

impl TerminalViewport {
    fn forward_pty_mouse(
        &self,
        button: PtyMouseButton,
        kind: PtyMouseEventKind,
        col: usize,
        row: usize,
        modifiers: Modifiers,
        shell: &mut Shell<'_, Message>,
    ) {
        let bytes = encode_mouse_event(
            button,
            kind,
            col,
            row,
            modifiers.shift(),
            modifiers.alt(),
            modifiers.control(),
            self.snapshot.mouse_sgr,
        );
        if !bytes.is_empty() {
            shell.publish(Message::TerminalInput {
                session_id: self.session_id,
                bytes,
            });
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn resolve_cell_appearance(
        &self,
        cell: &TerminalCell,
        palette: &TerminalPalette,
        grid_line: i32,
        col: usize,
        row: usize,
        cols: usize,
        highlight: Option<HighlightStyle>,
    ) -> (Color, Color, bool) {
        let (mut fg, mut bg) = resolve_cell_colors(cell, palette);
        let mut bold = cell.flags.contains(CellFlags::BOLD);

        if let Some(hl) = highlight {
            if let Some(hfg) = hl.fg {
                fg = rgb_to_iced(hfg);
            }
            if let Some(hbg) = hl.bg {
                bg = rgb_to_iced(hbg);
            }
            if hl.bold {
                bold = true;
            }
        }

        if let Some((idx, _)) = self
            .search_matches
            .iter()
            .enumerate()
            .find(|(_, m)| m.contains(grid_line, col, cols))
        {
            if Some(idx) == self.active_search_match {
                fg = palette.active_match_fg;
                bg = palette.active_match_bg;
            } else {
                fg = palette.match_fg;
                bg = palette.match_bg;
            }
        } else if self.snapshot.is_cell_selected(col, row) {
            bg = palette.selection_bg;
        }

        (fg, bg, bold)
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_ime_preedit(
    renderer: &mut iced::Renderer,
    content: &str,
    font: Font,
    font_size: f32,
    cell_size: Size,
    bounds: Rectangle,
    clip_bounds: Rectangle,
    snapshot: &TerminalSnapshot,
    palette: &TerminalPalette,
) {
    use iced::advanced::renderer::Renderer as _;
    use iced::advanced::text::Renderer as _;

    type P = <iced::Renderer as text::Renderer>::Paragraph;
    let probe = P::with_text(Text {
        content,
        bounds: Size::INFINITE,
        size: Pixels(font_size),
        line_height: LineHeight::Absolute(Pixels(cell_size.height)),
        font,
        align_x: text::Alignment::Left,
        align_y: iced::alignment::Vertical::Top,
        shaping: Shaping::Advanced,
        wrapping: Wrapping::None,
    });
    let width = probe.min_bounds().width.max(cell_size.width) + 4.0;
    let anchor = ime_cursor_anchor_rect(snapshot, bounds, cell_size);
    let max_x = (bounds.x + bounds.width - width).max(bounds.x);
    let x = anchor.x.min(max_x);
    let y = anchor.y;

    let box_bounds = Rectangle::new(Point::new(x, y), Size::new(width, cell_size.height));
    renderer.fill_quad(
        Quad {
            bounds: box_bounds,
            border: Border::default(),
            shadow: Shadow::default(),
            snap: true,
        },
        Background::Color(palette.ui_surface),
    );
    renderer.fill_quad(
        Quad {
            bounds: Rectangle::new(
                Point::new(x, y + cell_size.height - 2.0),
                Size::new(width, 2.0),
            ),
            border: Border::default(),
            shadow: Shadow::default(),
            snap: true,
        },
        Background::Color(palette.ui_accent),
    );
    renderer.fill_text(
        Text {
            content: content.to_string(),
            bounds: Size::new(width, cell_size.height),
            size: Pixels(font_size),
            line_height: LineHeight::Absolute(Pixels(cell_size.height)),
            font,
            align_x: text::Alignment::Left,
            align_y: iced::alignment::Vertical::Top,
            shaping: Shaping::Advanced,
            wrapping: Wrapping::None,
        },
        Point::new(x + 2.0, y),
        palette.foreground,
        clip_bounds,
    );
}

fn publish_clipboard_paste(
    session_id: SessionId,
    clipboard: &dyn Clipboard,
    shell: &mut Shell<'_, Message>,
) {
    if let Some(text) = clipboard.read(ClipboardKind::Standard) {
        shell.publish(Message::PasteText { session_id, text });
    } else {
        shell.publish(Message::PasteFromClipboard { session_id });
    }
}

fn point_to_cell(
    point: Point,
    bounds: Rectangle,
    cell_size: Size,
    snapshot: &TerminalSnapshot,
) -> (usize, usize, Side) {
    let local_x = (point.x - bounds.x).max(0.0);
    let local_y = (point.y - bounds.y).max(0.0);
    let max_col = snapshot.cols.saturating_sub(1) as usize;
    let max_row = snapshot.rows.saturating_sub(1) as usize;
    let col = ((local_x / cell_size.width.max(1.0)).floor() as usize).min(max_col);
    let row = ((local_y / cell_size.height.max(1.0)).floor() as usize).min(max_row);
    let cell_offset_x = local_x - (col as f32) * cell_size.width;
    let side = if cell_offset_x < cell_size.width * 0.5 {
        Side::Left
    } else {
        Side::Right
    };
    (col, row, side)
}

fn next_click_count(
    last_click: &mut Option<(Instant, usize, usize, u8)>,
    col: usize,
    row: usize,
) -> u8 {
    let now = Instant::now();
    let count = match *last_click {
        Some((prev_time, prev_col, prev_row, prev_count))
            if prev_col == col
                && prev_row == row
                && now.duration_since(prev_time).as_millis() <= 400 =>
        {
            if prev_count >= 3 { 1 } else { prev_count + 1 }
        }
        _ => 1,
    };
    *last_click = Some((now, col, row, count));
    count
}

fn resolve_font(font_family: &str) -> Font {
    let trimmed = font_family.trim();
    if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("monospace") {
        return Font::MONOSPACE;
    }

    static INTERNER: OnceLock<Mutex<HashMap<String, &'static str>>> = OnceLock::new();
    let map = INTERNER.get_or_init(|| Mutex::new(HashMap::new()));
    let mut guard = map.lock().expect("font interner mutex poisoned");
    let static_name = if let Some(&existing) = guard.get(trimmed) {
        existing
    } else {
        let leaked: &'static str = Box::leak(trimmed.to_string().into_boxed_str());
        guard.insert(trimmed.to_string(), leaked);
        leaked
    };

    Font {
        family: iced::font::Family::Name(static_name),
        ..Font::MONOSPACE
    }
}

fn resolve_cell_colors(cell: &TerminalCell, palette: &TerminalPalette) -> (Color, Color) {
    let mut fg = palette.resolve(cell.fg);
    let mut bg = palette.resolve(cell.bg);
    if cell.flags.contains(CellFlags::INVERSE) {
        std::mem::swap(&mut fg, &mut bg);
    }
    if cell.flags.contains(CellFlags::DIM) {
        fg.a *= 0.66;
    }
    (fg, bg)
}

fn measure_cell_size(font: Font, font_size: f32) -> Size {
    type P = <iced::Renderer as text::Renderer>::Paragraph;
    let probe = P::with_text(Text {
        content: "MMMMMMMMMM",
        bounds: Size::INFINITE,
        size: Pixels(font_size),
        line_height: LineHeight::Relative(1.2),
        font,
        align_x: text::Alignment::Left,
        align_y: iced::alignment::Vertical::Top,
        shaping: Shaping::Basic,
        wrapping: Wrapping::None,
    });
    let bounds = probe.min_bounds();
    let width = (bounds.width / 10.0).max(4.0);
    let height = bounds.height.max(font_size * 1.2);
    Size::new(width, height)
}

/// Translates an Iced keyboard press event into terminal PTY byte sequences.
pub fn key_event_to_bytes(
    key: &Key,
    modified_key: &Key,
    modifiers: Modifiers,
    text: Option<&str>,
) -> Vec<u8> {
    if modifiers.logo() {
        return Vec::new();
    }

    if modifiers.control()
        && let Some(ctrl_bytes) = control_key_bytes(key, modified_key)
    {
        return if modifiers.alt() {
            let mut out = vec![0x1b];
            out.extend_from_slice(&ctrl_bytes);
            out
        } else {
            ctrl_bytes
        };
    }

    if let Key::Named(named) = key {
        return named_key_bytes(*named, modifiers);
    }

    if let Some(txt) = text.filter(|t| !t.is_empty()) {
        if modifiers.alt() {
            let mut out = Vec::with_capacity(1 + txt.len());
            out.push(0x1b);
            out.extend_from_slice(txt.as_bytes());
            return out;
        }
        return txt.as_bytes().to_vec();
    }

    if let Key::Character(c) = modified_key.as_ref() {
        if modifiers.alt() {
            let mut out = Vec::with_capacity(1 + c.len());
            out.push(0x1b);
            out.extend_from_slice(c.as_bytes());
            return out;
        }
        return c.as_bytes().to_vec();
    }

    Vec::new()
}

fn control_key_bytes(key: &Key, modified_key: &Key) -> Option<Vec<u8>> {
    if let Key::Named(Named::Space) = key {
        return Some(vec![0x00]);
    }
    let ch = match (key.as_ref(), modified_key.as_ref()) {
        (Key::Character(s), _) | (_, Key::Character(s)) => s.chars().next(),
        _ => None,
    }?;
    let byte = match ch {
        'a'..='z' => (ch as u8) - b'a' + 1,
        'A'..='Z' => (ch as u8) - b'A' + 1,
        '@' | ' ' => 0x00,
        '[' => 0x1b,
        '\\' => 0x1c,
        ']' => 0x1d,
        '^' => 0x1e,
        '_' => 0x1f,
        '?' => 0x7f,
        _ => return None,
    };
    Some(vec![byte])
}

fn named_key_bytes(named: Named, modifiers: Modifiers) -> Vec<u8> {
    let mod_param = xterm_modifier_param(modifiers);
    match named {
        Named::Enter => b"\r".to_vec(),
        Named::Tab => {
            if modifiers.shift() {
                b"\x1b[Z".to_vec()
            } else {
                b"\t".to_vec()
            }
        }
        Named::Backspace => {
            if modifiers.alt() {
                b"\x1b\x7f".to_vec()
            } else {
                b"\x7f".to_vec()
            }
        }
        Named::Escape => b"\x1b".to_vec(),
        Named::Space => {
            if modifiers.alt() {
                b"\x1b ".to_vec()
            } else {
                b" ".to_vec()
            }
        }
        Named::ArrowUp => arrow_seq(b'A', mod_param),
        Named::ArrowDown => arrow_seq(b'B', mod_param),
        Named::ArrowRight => arrow_seq(b'C', mod_param),
        Named::ArrowLeft => arrow_seq(b'D', mod_param),
        Named::Home => arrow_seq(b'H', mod_param),
        Named::End => arrow_seq(b'F', mod_param),
        Named::Insert => csi_tilde(2, mod_param),
        Named::Delete => csi_tilde(3, mod_param),
        Named::PageUp => csi_tilde(5, mod_param),
        Named::PageDown => csi_tilde(6, mod_param),
        Named::F1 => ss3_or_csi(b'P', mod_param),
        Named::F2 => ss3_or_csi(b'Q', mod_param),
        Named::F3 => ss3_or_csi(b'R', mod_param),
        Named::F4 => ss3_or_csi(b'S', mod_param),
        Named::F5 => csi_tilde(15, mod_param),
        Named::F6 => csi_tilde(17, mod_param),
        Named::F7 => csi_tilde(18, mod_param),
        Named::F8 => csi_tilde(19, mod_param),
        Named::F9 => csi_tilde(20, mod_param),
        Named::F10 => csi_tilde(21, mod_param),
        Named::F11 => csi_tilde(23, mod_param),
        Named::F12 => csi_tilde(24, mod_param),
        _ => Vec::new(),
    }
}

fn xterm_modifier_param(modifiers: Modifiers) -> Option<u8> {
    let mut code = 1_u8;
    if modifiers.shift() {
        code += 1;
    }
    if modifiers.alt() {
        code += 2;
    }
    if modifiers.control() {
        code += 4;
    }
    (code > 1).then_some(code)
}

fn arrow_seq(dir: u8, mod_param: Option<u8>) -> Vec<u8> {
    match mod_param {
        Some(m) => format!("\x1b[1;{m}{}", dir as char).into_bytes(),
        None => vec![0x1b, b'[', dir],
    }
}

fn csi_tilde(num: u8, mod_param: Option<u8>) -> Vec<u8> {
    match mod_param {
        Some(m) => format!("\x1b[{num};{m}~").into_bytes(),
        None => format!("\x1b[{num}~").into_bytes(),
    }
}

fn ss3_or_csi(code: u8, mod_param: Option<u8>) -> Vec<u8> {
    match mod_param {
        Some(m) => format!("\x1b[1;{m}{}", code as char).into_bytes(),
        None => vec![0x1b, b'O', code],
    }
}

fn indexed_color(idx: u8, ansi: &[Color; 16]) -> Color {
    match idx {
        0..=15 => ansi[idx as usize],
        16..=231 => {
            let n = idx - 16;
            let r = n / 36;
            let g = (n % 36) / 6;
            let b = n % 6;
            let comp = |v: u8| if v == 0 { 0 } else { 55 + v * 40 };
            rgb(comp(r), comp(g), comp(b))
        }
        232..=255 => {
            let v = 8 + (idx - 232) * 10;
            rgb(v, v, v)
        }
    }
}

fn rgb(r: u8, g: u8, b: u8) -> Color {
    Color::from_rgb8(r, g, b)
}
