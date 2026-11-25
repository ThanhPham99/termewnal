use std::sync::Arc;

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
use termewnal_pty::{
    CellFlags, CursorShape, NamedColor, Rgb, TermColor, TerminalCell, TerminalSnapshot,
};

use crate::app::{Message, SessionId};

/// Custom `iced::advanced::Widget` rendering a terminal grid and capturing keyboard/IME input.
pub struct TerminalViewport {
    session_id: SessionId,
    snapshot: Arc<TerminalSnapshot>,
    font: Font,
    font_size: f32,
    explicit_focus: Option<bool>,
}

impl TerminalViewport {
    /// Creates a new [`TerminalViewport`] element bound to `session_id` and `snapshot`.
    pub fn new(
        session_id: SessionId,
        snapshot: Arc<TerminalSnapshot>,
        font_size: f32,
    ) -> Element<'static, Message, Theme, iced::Renderer> {
        Element::new(Self {
            session_id,
            snapshot,
            font: Font::MONOSPACE,
            font_size: font_size.max(6.0),
            explicit_focus: None,
        })
    }

    /// Creates a [`TerminalViewport`] element whose focus state is explicitly controlled by a parent pane.
    pub fn with_focus(
        session_id: SessionId,
        snapshot: Arc<TerminalSnapshot>,
        font_size: f32,
        focused: bool,
    ) -> Element<'static, Message, Theme, iced::Renderer> {
        Element::new(Self {
            session_id,
            snapshot,
            font: Font::MONOSPACE,
            font_size: font_size.max(6.0),
            explicit_focus: Some(focused),
        })
    }
}

#[derive(Debug, Clone)]
struct ViewportState {
    focused: bool,
    last_requested_size: Option<(u16, u16)>,
}

impl Default for ViewportState {
    fn default() -> Self {
        Self {
            focused: true,
            last_requested_size: None,
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
        _clipboard: &mut dyn Clipboard,
        shell: &mut Shell<'_, Message>,
        _viewport: &Rectangle,
    ) {
        let state = tree.state.downcast_mut::<ViewportState>();
        let is_focused = self.explicit_focus.unwrap_or(state.focused);
        let bounds = layout.bounds();
        let cell_size = measure_cell_size(self.font, self.font_size);

        match event {
            Event::Window(
                window::Event::Opened { .. }
                | window::Event::Resized(_)
                | window::Event::Rescaled(_)
                | window::Event::RedrawRequested(_),
            ) => {
                let cols = (bounds.width / cell_size.width).floor().max(1.0) as u16;
                let rows = (bounds.height / cell_size.height).floor().max(1.0) as u16;
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
                    if is_focused {
                        let cursor_rect = cursor_pixel_rect(&self.snapshot, bounds, cell_size);
                        shell.request_input_method::<&str>(&InputMethod::Enabled {
                            cursor: cursor_rect,
                            purpose: input_method::Purpose::Terminal,
                            preedit: None,
                        });
                    } else if self.explicit_focus.is_none() {
                        shell.request_input_method::<&str>(&InputMethod::Disabled);
                    }
                }
            }
            Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)) => {
                if self.explicit_focus.is_none() {
                    let was_focused = state.focused;
                    state.focused = cursor.is_over(bounds);
                    if state.focused != was_focused {
                        shell.request_redraw();
                    }
                }
            }
            Event::Keyboard(keyboard::Event::KeyPressed {
                key,
                modified_key,
                modifiers,
                text,
                ..
            }) => {
                if is_focused {
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
            Event::InputMethod(ev) => match ev {
                input_method::Event::Opened | input_method::Event::Closed => {
                    shell.request_redraw();
                }
                input_method::Event::Preedit(_, _) => {
                    shell.request_redraw();
                }
                input_method::Event::Commit(committed) => {
                    if is_focused && !committed.is_empty() {
                        shell.publish(Message::TerminalInput {
                            session_id: self.session_id,
                            bytes: committed.as_bytes().to_vec(),
                        });
                        shell.capture_event();
                    }
                }
            },
            _ => {}
        }
    }

    fn mouse_interaction(
        &self,
        _tree: &Tree,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        _viewport: &Rectangle,
        _renderer: &iced::Renderer,
    ) -> mouse::Interaction {
        if cursor.is_over(layout.bounds()) {
            mouse::Interaction::Text
        } else {
            mouse::Interaction::None
        }
    }

    fn draw(
        &self,
        tree: &Tree,
        renderer: &mut iced::Renderer,
        theme: &Theme,
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
        let Some(clip_bounds) = bounds.intersection(viewport) else {
            return;
        };

        let cell_size = measure_cell_size(self.font, self.font_size);
        let palette = TerminalPalette::from_theme(theme);

        // Paint full viewport background first.
        renderer.fill_quad(
            Quad {
                bounds,
                border: Border::default(),
                shadow: Shadow::default(),
                snap: true,
            },
            Background::Color(palette.background),
        );

        let cols = self.snapshot.cols as usize;
        let rows = self.snapshot.rows as usize;

        for row in 0..rows {
            let y = bounds.y + (row as f32) * cell_size.height;
            if y >= clip_bounds.y + clip_bounds.height {
                break;
            }
            if y + cell_size.height < clip_bounds.y {
                continue;
            }

            let mut col = 0;
            while col < cols {
                let Some(cell) = self.snapshot.cell(col, row) else {
                    break;
                };

                if cell.flags.contains(CellFlags::WIDE_CHAR_SPACER) {
                    col += 1;
                    continue;
                }

                let (fg, bg) = resolve_cell_colors(cell, &palette);

                let cell_width_cells = if cell.flags.contains(CellFlags::WIDE_CHAR) {
                    2.0
                } else {
                    1.0
                };
                let x = bounds.x + (col as f32) * cell_size.width;

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
                            let (next_fg, next_bg) = resolve_cell_colors(next, &palette);
                            if next_fg != run_fg || next_bg != bg {
                                break;
                            }
                            col += 1;
                            run.push(next.c);
                        }
                    }

                    let run_x = bounds.x + (run_start_col as f32) * cell_size.width;
                    renderer.fill_text(
                        Text {
                            content: run,
                            bounds: Size::new(bounds.width, cell_size.height),
                            size: Pixels(self.font_size),
                            line_height: LineHeight::Absolute(Pixels(cell_size.height)),
                            font: self.font,
                            align_x: text::Alignment::Left,
                            align_y: iced::alignment::Vertical::Top,
                            shaping: Shaping::Advanced,
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

        if let Some(cursor_snap) = self.snapshot.cursor
            && cursor_snap.shape != CursorShape::Hidden
        {
            let cursor_rect = cursor_pixel_rect(&self.snapshot, bounds, cell_size);
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

fn cursor_pixel_rect(snapshot: &TerminalSnapshot, bounds: Rectangle, cell_size: Size) -> Rectangle {
    let (col, row) = snapshot
        .cursor
        .map(|c| (c.col as f32, c.row as f32))
        .unwrap_or((0.0, 0.0));
    Rectangle::new(
        Point::new(
            bounds.x + col * cell_size.width,
            bounds.y + row * cell_size.height,
        ),
        cell_size,
    )
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

struct TerminalPalette {
    foreground: Color,
    background: Color,
    cursor: Color,
    ansi: [Color; 16],
}

impl TerminalPalette {
    fn from_theme(theme: &Theme) -> Self {
        let p = theme.palette();
        let is_light = (p.background.r + p.background.g + p.background.b) > 1.5;
        let ansi = if is_light {
            [
                rgb(0x2e, 0x34, 0x40),
                rgb(0xbf, 0x3f, 0x4a),
                rgb(0x3a, 0x8f, 0x4b),
                rgb(0xb0, 0x76, 0x19),
                rgb(0x2b, 0x6c, 0xb0),
                rgb(0x91, 0x41, 0xac),
                rgb(0x23, 0x83, 0x91),
                rgb(0xc8, 0xcd, 0xd4),
                rgb(0x5c, 0x63, 0x70),
                rgb(0xd9, 0x48, 0x54),
                rgb(0x46, 0xa8, 0x59),
                rgb(0xcb, 0x8a, 0x22),
                rgb(0x38, 0x82, 0xcf),
                rgb(0xaa, 0x52, 0xc7),
                rgb(0x2c, 0x9c, 0xac),
                rgb(0xe5, 0xe9, 0xf0),
            ]
        } else {
            [
                rgb(0x1e, 0x22, 0x2a),
                rgb(0xe0, 0x6c, 0x75),
                rgb(0x98, 0xc3, 0x79),
                rgb(0xe5, 0xc0, 0x7b),
                rgb(0x61, 0xaf, 0xef),
                rgb(0xc6, 0x78, 0xdd),
                rgb(0x56, 0xb6, 0xc2),
                rgb(0xab, 0xb2, 0xbf),
                rgb(0x5c, 0x63, 0x70),
                rgb(0xef, 0x7b, 0x83),
                rgb(0xa9, 0xd4, 0x89),
                rgb(0xf2, 0xcc, 0x86),
                rgb(0x73, 0xbf, 0xf7),
                rgb(0xd4, 0x87, 0xeb),
                rgb(0x66, 0xc7, 0xd4),
                rgb(0xff, 0xff, 0xff),
            ]
        };
        Self {
            foreground: p.text,
            background: p.background,
            cursor: p.primary,
            ansi,
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
