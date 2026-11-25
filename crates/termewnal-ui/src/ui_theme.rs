//! Tabby-inspired unified desktop design system derived from the active [`ThemePalette`].

use iced::widget::{button, container, mouse_area, opaque, stack, text, text_input};
use iced::{Alignment, Background, Border, Color, Element, Length, Padding, Shadow, Theme, Vector};
use termewnal_core::{RgbColor, ThemePalette};

/// Cohesive color tokens for all Termewnal workspace chrome, panels, cards, and modals.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ChromePalette {
    pub is_light: bool,
    pub term_bg: Color,
    pub chrome_bg: Color,
    pub panel_bg: Color,
    pub card_bg: Color,
    pub card_hover: Color,
    pub input_bg: Color,
    pub border: Color,
    pub border_subtle: Color,
    pub text_primary: Color,
    pub text_secondary: Color,
    pub text_muted: Color,
    pub accent: Color,
    pub accent_fg: Color,
    pub accent_soft: Color,
    pub success: Color,
    pub success_soft: Color,
    pub warning: Color,
    pub warning_soft: Color,
    pub danger: Color,
    pub danger_soft: Color,
}

impl ChromePalette {
    /// Derives harmonious Tabby-style chrome colors from `palette`.
    pub fn from_palette(palette: &ThemePalette) -> Self {
        let term_bg = rgb(palette.background);
        let surface = rgb(palette.ui_surface);
        let accent = rgb(palette.ui_accent);
        let fg = rgb(palette.foreground);
        let border_base = rgb(palette.ui_border);

        let (chrome_bg, panel_bg, card_bg, card_hover, input_bg) = if palette.is_light {
            (
                darken(surface, 0.04),
                surface,
                Color::WHITE,
                darken(Color::WHITE, 0.03),
                darken(surface, 0.02),
            )
        } else {
            (
                darken(term_bg, 0.24),
                mix(term_bg, surface, 0.45),
                surface,
                lighten(surface, 0.08),
                darken(term_bg, 0.16),
            )
        };

        let border = Color {
            a: if palette.is_light { 0.85 } else { 0.55 },
            ..border_base
        };
        let border_subtle = Color {
            a: if palette.is_light { 0.45 } else { 0.28 },
            ..border_base
        };
        let accent_soft = Color {
            a: if palette.is_light { 0.14 } else { 0.18 },
            ..accent
        };
        let accent_fg = if luminance(accent) > 0.55 {
            Color::from_rgb8(0x14, 0x16, 0x1f)
        } else {
            Color::WHITE
        };

        let success = if palette.is_light {
            Color::from_rgb8(22, 163, 74)
        } else {
            Color::from_rgb8(74, 222, 128)
        };
        let warning = if palette.is_light {
            Color::from_rgb8(217, 119, 6)
        } else {
            Color::from_rgb8(251, 191, 36)
        };
        let danger = if palette.is_light {
            Color::from_rgb8(220, 38, 38)
        } else {
            Color::from_rgb8(248, 113, 113)
        };
        let soft_alpha = if palette.is_light { 0.14 } else { 0.18 };

        Self {
            is_light: palette.is_light,
            term_bg,
            chrome_bg,
            panel_bg,
            card_bg,
            card_hover,
            input_bg,
            border,
            border_subtle,
            text_primary: fg,
            text_secondary: Color { a: 0.78, ..fg },
            text_muted: Color { a: 0.54, ..fg },
            accent,
            accent_fg,
            accent_soft,
            success,
            success_soft: Color { a: soft_alpha, ..success },
            warning,
            warning_soft: Color { a: soft_alpha, ..warning },
            danger,
            danger_soft: Color { a: soft_alpha, ..danger },
        }
    }
}

/// Visual tone for rounded pill buttons.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PillTone {
    Ghost,
    Subtle,
    Primary,
    Danger,
    ActiveAccent,
}

/// Returns a Tabby-inspired pill button style using `chrome`.
pub fn pill_button_style(
    chrome: ChromePalette,
    tone: PillTone,
) -> impl Fn(&Theme, button::Status) -> button::Style {
    move |_theme, status| {
        let hovered = matches!(
            status,
            button::Status::Hovered | button::Status::Pressed
        );
        let disabled = matches!(status, button::Status::Disabled);

        let (bg, text_color, border_color) = match tone {
            PillTone::Ghost => (
                if hovered {
                    chrome.card_hover
                } else {
                    Color::TRANSPARENT
                },
                if hovered {
                    chrome.text_primary
                } else {
                    chrome.text_secondary
                },
                Color::TRANSPARENT,
            ),
            PillTone::Subtle => (
                if hovered {
                    chrome.card_hover
                } else {
                    chrome.card_bg
                },
                chrome.text_primary,
                if hovered {
                    chrome.border
                } else {
                    chrome.border_subtle
                },
            ),
            PillTone::Primary => (
                if hovered {
                    lighten(chrome.accent, 0.08)
                } else {
                    chrome.accent
                },
                chrome.accent_fg,
                chrome.accent,
            ),
            PillTone::Danger => (
                if hovered {
                    Color::from_rgb8(220, 38, 38)
                } else {
                    Color::from_rgb8(185, 28, 28)
                },
                Color::WHITE,
                Color::from_rgb8(239, 68, 68),
            ),
            PillTone::ActiveAccent => (
                if hovered {
                    Color {
                        a: 0.28,
                        ..chrome.accent
                    }
                } else {
                    chrome.accent_soft
                },
                chrome.text_primary,
                chrome.accent,
            ),
        };

        button::Style {
            background: Some(Background::Color(bg)),
            text_color: if disabled {
                chrome.text_muted
            } else {
                text_color
            },
            border: Border {
                color: border_color,
                width: if border_color == Color::TRANSPARENT {
                    0.0
                } else {
                    1.0
                },
                radius: 6.0.into(),
            },
            shadow: Shadow::default(),
            snap: true,
        }
    }
}

/// Tab button styling modeled after Tabby's connected terminal tabs.
pub fn tab_button_style(
    chrome: ChromePalette,
    active: bool,
) -> impl Fn(&Theme, button::Status) -> button::Style {
    move |_theme, status| {
        let hovered = matches!(
            status,
            button::Status::Hovered | button::Status::Pressed
        );
        let bg = if active {
            chrome.term_bg
        } else if hovered {
            chrome.card_bg
        } else {
            Color::TRANSPARENT
        };
        let border_color = if active {
            chrome.border
        } else if hovered {
            chrome.border_subtle
        } else {
            Color::TRANSPARENT
        };
        button::Style {
            background: Some(Background::Color(bg)),
            text_color: if active || hovered {
                chrome.text_primary
            } else {
                chrome.text_secondary
            },
            border: Border {
                color: border_color,
                width: if active { 1.0 } else { 0.0 },
                radius: iced::border::Radius {
                    top_left: 6.0,
                    top_right: 6.0,
                    bottom_right: 2.0,
                    bottom_left: 2.0,
                },
            },
            shadow: Shadow::default(),
            snap: true,
        }
    }
}

/// Container style for Tabby-style connected tab chips.
pub fn tab_container_style(chrome: ChromePalette, active: bool) -> container::Style {
    container::Style {
        background: Some(Background::Color(if active {
            chrome.term_bg
        } else {
            chrome.card_bg
        })),
        text_color: Some(if active {
            chrome.text_primary
        } else {
            chrome.text_secondary
        }),
        border: Border {
            color: if active {
                chrome.border
            } else {
                chrome.border_subtle
            },
            width: 1.0,
            radius: iced::border::Radius {
                top_left: 7.0,
                top_right: 7.0,
                bottom_right: if active { 0.0 } else { 4.0 },
                bottom_left: if active { 0.0 } else { 4.0 },
            },
        },
        shadow: Shadow::default(),
        snap: true,
    }
}

/// Sunken input style matching `chrome`.
pub fn styled_input_style(
    chrome: ChromePalette,
) -> impl Fn(&Theme, text_input::Status) -> text_input::Style {
    move |_theme, status| {
        let border_color = match status {
            text_input::Status::Focused { .. } => chrome.accent,
            text_input::Status::Hovered => chrome.border,
            text_input::Status::Active | text_input::Status::Disabled => chrome.border_subtle,
        };
        text_input::Style {
            background: Background::Color(chrome.input_bg),
            border: Border {
                color: border_color,
                width: 1.0,
                radius: 6.0.into(),
            },
            icon: chrome.text_muted,
            placeholder: chrome.text_muted,
            value: chrome.text_primary,
            selection: chrome.accent_soft,
        }
    }
}

/// Elevated container card style for list items and settings blocks.
pub fn card_container_style(chrome: ChromePalette, selected: bool) -> container::Style {
    container::Style {
        background: Some(Background::Color(if selected {
            chrome.accent_soft
        } else {
            chrome.card_bg
        })),
        text_color: Some(chrome.text_primary),
        border: Border {
            color: if selected {
                chrome.accent
            } else {
                chrome.border_subtle
            },
            width: 1.0,
            radius: 6.0.into(),
        },
        shadow: Shadow::default(),
        snap: true,
    }
}

/// Elevated modal dialog surface with soft drop shadow.
pub fn modal_card_style(chrome: ChromePalette, highlight_accent: bool) -> container::Style {
    container::Style {
        background: Some(Background::Color(chrome.panel_bg)),
        text_color: Some(chrome.text_primary),
        border: Border {
            color: if highlight_accent {
                chrome.accent
            } else {
                chrome.border
            },
            width: 1.0,
            radius: 10.0.into(),
        },
        shadow: Shadow {
            color: Color::from_rgba(0.0, 0.0, 0.0, 0.55),
            offset: Vector::new(0.0, 10.0),
            blur_radius: 28.0,
        },
        snap: true,
    }
}

/// Wraps `card` over a click-to-dismiss dimmed backdrop without letting clicks inside `card`
/// bubble through to `on_dismiss`.
pub fn modal_overlay<'a, Msg: Clone + 'a>(
    card: Element<'a, Msg, Theme, iced::Renderer>,
    on_dismiss: Option<Msg>,
    top_padding: f32,
) -> Element<'a, Msg, Theme, iced::Renderer> {
    let scrim_box = container(iced::widget::space::horizontal().width(Length::Fill).height(Length::Fill))
        .width(Length::Fill)
        .height(Length::Fill)
        .style(|_theme| container::Style {
            background: Some(Background::Color(Color::from_rgba(0.0, 0.0, 0.0, 0.58))),
            ..Default::default()
        });

    let scrim: Element<'a, Msg, Theme, iced::Renderer> = if let Some(msg) = on_dismiss {
        mouse_area(scrim_box).on_press(msg).into()
    } else {
        scrim_box.into()
    };

    let mut card_layer = container(opaque(card))
        .width(Length::Fill)
        .height(Length::Fill)
        .center_x(Length::Fill)
        .padding(Padding {
            top: top_padding,
            right: 24.0,
            bottom: 24.0,
            left: 24.0,
        });
    if top_padding <= 24.0 {
        card_layer = card_layer.center_y(Length::Fill);
    }

    opaque(stack![scrim, card_layer]).into()
}

/// Small circular colored status dot (`8px` diameter).
pub fn status_dot<'a, Msg: 'a>(color: Color) -> Element<'a, Msg, Theme, iced::Renderer> {
    container(iced::widget::space::horizontal().width(8).height(8))
        .width(8)
        .height(8)
        .style(move |_theme| container::Style {
            background: Some(Background::Color(color)),
            border: Border {
                color: Color::TRANSPARENT,
                width: 0.0,
                radius: 999.0.into(),
            },
            ..Default::default()
        })
        .into()
}

/// Rounded category or status pill badge.
pub fn badge_pill<'a, Msg: 'a>(
    label: impl Into<String>,
    fg: Color,
    bg: Color,
    border_color: Color,
) -> Element<'a, Msg, Theme, iced::Renderer> {
    container(text(label.into()).size(10).color(fg))
        .padding([2, 6])
        .align_y(Alignment::Center)
        .style(move |_theme| container::Style {
            background: Some(Background::Color(bg)),
            border: Border {
                color: border_color,
                width: if border_color == Color::TRANSPARENT {
                    0.0
                } else {
                    1.0
                },
                radius: 4.0.into(),
            },
            ..Default::default()
        })
        .into()
}

/// Keyboard shortcut keycap pill (`Ctrl+K`).
pub fn keycap_pill<'a, Msg: 'a>(
    shortcut: impl Into<String>,
    chrome: ChromePalette,
) -> Element<'a, Msg, Theme, iced::Renderer> {
    badge_pill(shortcut, chrome.text_secondary, chrome.input_bg, chrome.border)
}

fn rgb(c: RgbColor) -> Color {
    Color::from_rgb8(c.r, c.g, c.b)
}

fn mix(a: Color, b: Color, t: f32) -> Color {
    let t = t.clamp(0.0, 1.0);
    Color {
        r: a.r + (b.r - a.r) * t,
        g: a.g + (b.g - a.g) * t,
        b: a.b + (b.b - a.b) * t,
        a: a.a + (b.a - a.a) * t,
    }
}

fn darken(c: Color, amount: f32) -> Color {
    mix(c, Color::BLACK, amount)
}

fn lighten(c: Color, amount: f32) -> Color {
    mix(c, Color::WHITE, amount)
}

fn luminance(c: Color) -> f32 {
    0.2126 * c.r + 0.7152 * c.g + 0.0722 * c.b
}
