//! Tabby ("Follow the color scheme") UI tokens, dynamic surface derivation, and component styles
//! derived from the active [`ThemePalette`], matching `Eugeny/tabby`'s `themes.service.ts` and
//! `theme.new.scss`.

use std::time::Instant;

use iced::advanced::layout::{self, Layout};
use iced::advanced::overlay;
use iced::advanced::renderer::{self, Quad};
use iced::advanced::widget::{Operation, Tree};
use iced::advanced::{Clipboard, Shell, Widget};
use iced::overlay::menu;
use iced::widget::{
    button, checkbox, column, container, mouse_area, opaque, pick_list, progress_bar, row,
    scrollable, space, stack, text, text_input,
};
use iced::{
    Alignment, Background, Border, Color, Element, Event, Font, Length, Padding, Point, Rectangle,
    Shadow, Size, Theme, Vector, font, keyboard, mouse,
};
use termewnal_core::{
    DEFAULT_SIDEBAR_WIDTH, MAX_SIDEBAR_WIDTH, MIN_SIDEBAR_WIDTH, RgbColor, ThemePalette,
};

/// Tabby desktop typography scale (`14px` base from `$font-size-base: 14rem / 16`).
pub const FONT_TITLE: f32 = 18.0;
pub const FONT_SUBHEAD: f32 = 14.0;
pub const FONT_BODY: f32 = 13.0;
pub const FONT_META: f32 = 11.5;

/// Tabby standard tab strip and top header height (`40px`).
pub const TAB_BAR_HEIGHT: f32 = 40.0;

/// Primary sans-serif UI font.
pub const fn ui_font() -> Font {
    Font::DEFAULT
}

/// Medium/normal UI font for active tabs and controls (kept at `Normal` weight so systems with
/// only Regular/Bold sans faces never drop `Weight::Medium` glyphs in iced/cosmic-text).
pub const fn ui_font_medium() -> Font {
    Font::DEFAULT
}

/// Bold-weight sans-serif UI font for emphasis labels, tab indices, and buttons.
pub const fn ui_font_semibold() -> Font {
    Font {
        weight: font::Weight::Bold,
        ..Font::DEFAULT
    }
}

/// Monospace UI font reserved for terminal output, host/port strings, and keycaps.
pub const fn mono_font() -> Font {
    Font::MONOSPACE
}

/// Responsive window width tiers driving adaptive sidebars, drawers, and modals.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ViewportTier {
    /// Narrow desktop windows and Quake drop-down panes (`< 860px`).
    Compact,
    /// Medium desktop splits (`860px..1179px`).
    Medium,
    /// Full desktop workspace (`>= 1180px`).
    Wide,
}

impl ViewportTier {
    /// Classifies `window_width` into an adaptive [`ViewportTier`].
    pub fn from_width(window_width: f32) -> Self {
        if window_width < 860.0 {
            Self::Compact
        } else if window_width < 1180.0 {
            Self::Medium
        } else {
            Self::Wide
        }
    }
}

/// Computes the default responsive width for the Left Sidebar.
pub fn sidebar_width_for_window(window_width: f32) -> f32 {
    match ViewportTier::from_width(window_width) {
        ViewportTier::Wide => DEFAULT_SIDEBAR_WIDTH,
        ViewportTier::Medium => 224.0,
        ViewportTier::Compact => (window_width * 0.30).clamp(176.0, 212.0),
    }
}

/// Clamps a persisted Left Sidebar width to safe bounds inside `window_width`.
pub fn clamp_sidebar_width(preferred_width: f32, window_width: f32) -> f32 {
    let preferred = if preferred_width.is_finite() {
        preferred_width
    } else {
        DEFAULT_SIDEBAR_WIDTH
    };
    let max_for_window = (window_width - 260.0).max(MIN_SIDEBAR_WIDTH);
    let upper = MAX_SIDEBAR_WIDTH.min(max_for_window);
    let lower = MIN_SIDEBAR_WIDTH.min(upper);
    preferred.clamp(lower, upper)
}

const DIVIDER_DOUBLE_CLICK_MS: u64 = 400;

/// Outcome of pressing a resizable divider handle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DividerPress {
    StartDrag,
    DoubleClickReset,
}

/// Shared widget state for draggable pane/sidebar/drawer dividers with double-click reset.
#[derive(Debug, Default)]
pub struct DividerDragState {
    pub dragging: bool,
    pub hovered: bool,
    last_click_at: Option<Instant>,
}

impl DividerDragState {
    /// Records a left mouse press on the divider and distinguishes drag vs double-click reset.
    pub fn on_left_press(&mut self, now: Instant) -> DividerPress {
        let is_double = self
            .last_click_at
            .is_some_and(|prev| now.duration_since(prev).as_millis() <= u128::from(DIVIDER_DOUBLE_CLICK_MS));
        if is_double {
            self.last_click_at = None;
            self.dragging = false;
            DividerPress::DoubleClickReset
        } else {
            self.last_click_at = Some(now);
            self.dragging = true;
            DividerPress::StartDrag
        }
    }
}

/// Renders a 1px interactive divider quad with accent highlighting on hover/drag.
pub fn draw_interactive_divider(
    renderer: &mut iced::Renderer,
    chrome: ChromePalette,
    bounds: Rectangle,
    state: &DividerDragState,
    base_alpha: f32,
) {
    use iced::advanced::renderer::Renderer as _;

    let color = if state.dragging || state.hovered {
        chrome.accent
    } else if base_alpha < 1.0 && base_alpha >= 0.0 {
        mix(chrome.chrome_bg, chrome.hairline, base_alpha)
    } else {
        chrome.hairline
    };
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

/// Computes a responsive modal width clamped inside `window_width` with comfortable gutters.
pub fn modal_width(target_width: f32, window_width: f32) -> f32 {
    let margin = if window_width < 640.0 { 20.0 } else { 40.0 };
    (window_width - margin).max(260.0).min(target_width)
}

/// Computes a responsive scrollable body height clamped inside `window_height`.
pub fn modal_body_height(target_height: f32, window_height: f32, chrome_reserve: f32) -> f32 {
    (window_height - chrome_reserve).max(140.0).min(target_height)
}

/// Cohesive Tabby color tokens (`--theme-bg*`, `--theme-fg*`, `--theme-primary`, `--theme-secondary`)
/// derived dynamically from the active color scheme.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ChromePalette {
    pub is_light: bool,
    pub term_bg: Color,
    pub bg_more: Color,
    pub chrome_bg: Color,
    pub bg_less: Color,
    pub bg_less_2: Color,
    pub secondary: Color,
    pub secondary_hover: Color,
    pub well_bg: Color,
    pub panel_bg: Color,
    pub card_bg: Color,
    pub card_hover: Color,
    pub input_bg: Color,
    pub border: Color,
    pub border_subtle: Color,
    pub hairline: Color,
    pub text_primary: Color,
    pub text_secondary: Color,
    pub text_muted: Color,
    pub text_emphasis: Color,
    pub active_tab_indicator: Color,
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
    /// Derives the 4-tier OKLCH `ChromePalette` and WCAG-guarded foreground/semantic tokens from `palette`.
    pub fn from_palette(palette: &ThemePalette) -> Self {
        let term_bg = rgb(palette.background);
        let fg = rgb(palette.foreground);
        let raw_accent = rgb(palette.ui_accent);
        let is_light = palette.is_light;

        let (l_base, c_base, h_base) = rgb_to_oklch(term_bg);
        let c_surface = c_base.min(0.024);
        let surface_at = |l: f32| oklch_to_srgb_gamut(l, c_surface, h_base, 1.0);

        let (
            bg_more,
            chrome_bg,
            bg_less,
            well_bg,
            panel_bg,
            card_bg,
            bg_less_2,
            secondary,
            secondary_hover,
            card_hover,
            input_bg,
            hairline,
            border_subtle,
            border,
        ) = if is_light {
            let panel = surface_at((l_base + 0.022).min(1.0));
            let input = surface_at((l_base + 0.012).min(1.0));
            let well = surface_at((l_base - 0.026).max(0.0));
            let chrome = surface_at((l_base - 0.044).max(0.0));
            let more = surface_at((l_base - 0.060).max(0.0));
            let card = surface_at((l_base - 0.086).max(0.0));
            let hover = surface_at((l_base - 0.114).max(0.0));
            let l_chrome = rgb_to_oklch(chrome).0;
            let hair = surface_at((l_chrome - 0.085).max(0.0));
            let sub_b = surface_at((l_chrome - 0.135).max(0.0));
            let main_b = surface_at((l_chrome - 0.220).max(0.0));
            (
                more, chrome, well, well, panel, card, card, card, hover, hover, input, hair,
                sub_b, main_b,
            )
        } else if l_base < 0.165 {
            // Ultra-dark (#000000 / OLED) schemes preserve raw `term_bg` while stepping chrome upward cleanly.
            let anchor = (l_base + 0.045).max(0.150);
            let more = surface_at(anchor);
            let chrome = surface_at(anchor + 0.016);
            let input = surface_at((anchor - 0.025).max(0.110));
            let well = surface_at(anchor + 0.044);
            let panel = surface_at(anchor + 0.066);
            let card = surface_at(anchor + 0.094);
            let hover = surface_at(anchor + 0.122);
            let l_panel = rgb_to_oklch(panel).0;
            let hair = surface_at((l_panel + 0.065).min(0.95));
            let sub_b = surface_at((l_panel + 0.110).min(0.95));
            let main_b = surface_at((l_panel + 0.190).min(0.95));
            (
                more, chrome, well, well, panel, card, card, card, hover, hover, input, hair,
                sub_b, main_b,
            )
        } else {
            let more = surface_at((l_base - 0.050).max(0.110));
            let chrome = surface_at((l_base - 0.034).max(0.125));
            let input = surface_at((l_base - 0.018).max(0.105));
            let well = surface_at((l_base + 0.028).min(0.95));
            let panel = surface_at((l_base + 0.050).min(0.95));
            let card = surface_at((l_base + 0.078).min(0.95));
            let hover = surface_at((l_base + 0.106).min(0.95));
            let l_panel = rgb_to_oklch(panel).0;
            let hair = surface_at((l_panel + 0.075).min(0.95));
            let sub_b = surface_at((l_panel + 0.120).min(0.95));
            let main_b = surface_at((l_panel + 0.200).min(0.95));
            (
                more, chrome, well, well, panel, card, card, card, hover, hover, input, hair,
                sub_b, main_b,
            )
        };

        let contrast_bgs = [term_bg, chrome_bg, panel_bg];
        let text_primary = ensure_contrast_against(fg, &contrast_bgs, 4.5);
        let text_secondary = ensure_contrast_against(
            mix(fg, chrome_bg, if is_light { 0.22 } else { 0.20 }),
            &contrast_bgs,
            4.5,
        );
        let text_muted = ensure_contrast_against(
            mix(fg, chrome_bg, 0.42),
            &contrast_bgs,
            3.1,
        );
        let text_emphasis = ensure_contrast_against(
            oklch_shift_lightness(fg, if is_light { -0.14 } else { 0.14 }),
            &contrast_bgs,
            7.0,
        );

        let semantic_bgs = [chrome_bg, panel_bg, well_bg];
        let accent = ensure_contrast_against(raw_accent, &semantic_bgs, 3.1);
        let active_tab_indicator = if is_light {
            accent
        } else {
            mix(term_bg, accent, 0.65)
        };

        let accent_soft = Color {
            a: if is_light { 0.14 } else { 0.18 },
            ..accent
        };
        let accent_fg = ensure_contrast_against(
            contrast_pick(accent, Color::from_rgb8(0x0b, 0x0c, 0x10), Color::WHITE),
            &[accent],
            4.5,
        );

        let success = ensure_contrast_against(rgb(palette.ansi[2]), &semantic_bgs, 3.1);
        let warning = ensure_contrast_against(rgb(palette.ansi[3]), &semantic_bgs, 3.1);
        let danger = ensure_contrast_against(rgb(palette.ansi[1]), &semantic_bgs, 3.1);
        let soft_alpha = if is_light { 0.14 } else { 0.16 };

        Self {
            is_light,
            term_bg,
            bg_more,
            chrome_bg,
            bg_less,
            bg_less_2,
            secondary,
            secondary_hover,
            well_bg,
            panel_bg,
            card_bg,
            card_hover,
            input_bg,
            border,
            border_subtle,
            hairline,
            text_primary,
            text_secondary,
            text_muted,
            text_emphasis,
            active_tab_indicator,
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

/// Visual tone for Tabby / Bootstrap buttons (`btn-link`, `btn-secondary`, `btn-primary`, `btn-danger`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PillTone {
    Ghost,
    Subtle,
    Primary,
    Danger,
    ActiveAccent,
}

/// Tabby flat `.btn-tab-bar` style used inside the 40px top header (`+`, `Profiles`, `Settings`, etc.).
pub fn tab_bar_button_style(
    chrome: ChromePalette,
    active: bool,
    danger: bool,
) -> impl Fn(&Theme, button::Status) -> button::Style {
    move |_theme, status| {
        let hovered = matches!(status, button::Status::Hovered);
        let pressed = matches!(status, button::Status::Pressed);
        let (bg, fg) = if active && danger {
            (chrome.danger, Color::WHITE)
        } else if pressed {
            (chrome.bg_less_2, chrome.text_emphasis)
        } else if hovered {
            (chrome.secondary_hover, chrome.text_emphasis)
        } else if active {
            (chrome.bg_less_2, chrome.accent)
        } else {
            (Color::TRANSPARENT, chrome.text_secondary)
        };
        button::Style {
            background: Some(Background::Color(bg)),
            text_color: fg,
            border: Border::default(),
            shadow: Shadow::default(),
            snap: true,
        }
    }
}

/// Tabby window-close button style inside the integrated top System Bar (danger red on hover/press).
pub fn tab_bar_close_button_style(
    chrome: ChromePalette,
) -> impl Fn(&Theme, button::Status) -> button::Style {
    move |_theme, status| {
        let hovered = matches!(status, button::Status::Hovered);
        let pressed = matches!(status, button::Status::Pressed);
        let (bg, fg) = if pressed {
            (oklch_shift_lightness(chrome.danger, -0.08), Color::WHITE)
        } else if hovered {
            (chrome.danger, Color::WHITE)
        } else {
            (Color::TRANSPARENT, chrome.text_secondary)
        };
        button::Style {
            background: Some(Background::Color(bg)),
            text_color: fg,
            border: Border::default(),
            shadow: Shadow::default(),
            snap: true,
        }
    }
}

/// Returns a Tabby/Bootstrap button style using `chrome` with crisp rest, hover, and press states.
pub fn pill_button_style(
    chrome: ChromePalette,
    tone: PillTone,
) -> impl Fn(&Theme, button::Status) -> button::Style {
    move |_theme, status| {
        let hovered = matches!(status, button::Status::Hovered);
        let pressed = matches!(status, button::Status::Pressed);
        let disabled = matches!(status, button::Status::Disabled);

        let (bg, text_color, border_color) = match tone {
            PillTone::Ghost => (
                if pressed {
                    chrome.bg_less_2
                } else if hovered {
                    chrome.bg_less
                } else {
                    Color::TRANSPARENT
                },
                if hovered || pressed {
                    chrome.text_emphasis
                } else {
                    chrome.text_secondary
                },
                Color::TRANSPARENT,
            ),
            PillTone::Subtle => (
                if pressed {
                    chrome.bg_less_2
                } else if hovered {
                    chrome.secondary_hover
                } else {
                    chrome.secondary
                },
                chrome.text_primary,
                if chrome.is_light {
                    chrome.border_subtle
                } else {
                    Color::TRANSPARENT
                },
            ),
            PillTone::Primary => (
                if pressed {
                    oklch_shift_lightness(chrome.accent, -0.08)
                } else if hovered {
                    oklch_shift_lightness(chrome.accent, 0.05)
                } else {
                    chrome.accent
                },
                chrome.accent_fg,
                Color::TRANSPARENT,
            ),
            PillTone::Danger => {
                let danger_fg =
                    contrast_pick(chrome.danger, Color::from_rgb8(0x14, 0x16, 0x1f), Color::WHITE);
                (
                    if pressed {
                        oklch_shift_lightness(chrome.danger, -0.08)
                    } else if hovered {
                        oklch_shift_lightness(chrome.danger, 0.05)
                    } else {
                        chrome.danger
                    },
                    danger_fg,
                    Color::TRANSPARENT,
                )
            }
            PillTone::ActiveAccent => (
                if pressed {
                    chrome.bg_more
                } else if hovered {
                    chrome.bg_less_2
                } else {
                    chrome.bg_more
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
                radius: 5.0.into(),
            },
            shadow: Shadow::default(),
            snap: true,
        }
    }
}

/// Tabby `.list-group-item-action` / `.tree-item` button style (`--theme-bg-less` -> `--theme-bg-less-2`).
pub fn list_row_button_style(
    chrome: ChromePalette,
    selected: bool,
) -> impl Fn(&Theme, button::Status) -> button::Style {
    move |_theme, status| {
        let hovered = matches!(status, button::Status::Hovered);
        let pressed = matches!(status, button::Status::Pressed);
        let bg = if selected {
            chrome.bg_less_2
        } else if pressed || hovered {
            chrome.bg_less_2
        } else {
            Color::TRANSPARENT
        };

        button::Style {
            background: Some(Background::Color(bg)),
            text_color: if selected {
                chrome.accent
            } else if hovered {
                chrome.text_emphasis
            } else {
                chrome.text_primary
            },
            border: Border {
                color: Color::TRANSPARENT,
                width: 0.0,
                radius: 5.0.into(),
            },
            shadow: Shadow::default(),
            snap: true,
        }
    }
}

/// Tabby `.nav-pills .nav-link` style for the Settings left rail (`--theme-primary` active fill).
pub fn settings_nav_pill_style(
    chrome: ChromePalette,
    active: bool,
) -> impl Fn(&Theme, button::Status) -> button::Style {
    move |_theme, status| {
        let hovered = matches!(status, button::Status::Hovered | button::Status::Pressed);
        let (bg, fg) = if active {
            (chrome.accent, chrome.accent_fg)
        } else if hovered {
            (chrome.bg_less_2, chrome.text_emphasis)
        } else {
            (Color::TRANSPARENT, chrome.text_secondary)
        };
        button::Style {
            background: Some(Background::Color(bg)),
            text_color: fg,
            border: Border {
                color: Color::TRANSPARENT,
                width: 0.0,
                radius: 5.0.into(),
            },
            shadow: Shadow::default(),
            snap: true,
        }
    }
}

/// Flush Tabby `tab-header` button style (`border-radius: 0`).
pub fn tab_button_style(
    chrome: ChromePalette,
    active: bool,
) -> impl Fn(&Theme, button::Status) -> button::Style {
    move |_theme, status| {
        let hovered = matches!(
            status,
            button::Status::Hovered | button::Status::Pressed
        );
        button::Style {
            background: None,
            text_color: if active || hovered {
                chrome.text_primary
            } else {
                chrome.text_muted
            },
            border: Border::default(),
            shadow: Shadow::default(),
            snap: true,
        }
    }
}

/// Flush Tabby `tab-header` container style: active tab merges into `term_bg` (`--body-bg`)
/// with zero rounded corners or shadows; optional `tab.color` tints the active background by 20%.
pub fn tab_container_style(chrome: ChromePalette, active: bool) -> container::Style {
    tab_container_style_tinted(chrome, active, None)
}

/// Flush Tabby `tab-header` container style with optional custom `tab.color` tint.
pub fn tab_container_style_tinted(
    chrome: ChromePalette,
    active: bool,
    tab_color: Option<Color>,
) -> container::Style {
    let bg = if active {
        match tab_color {
            Some(c) => mix(chrome.term_bg, c, 0.20),
            None => chrome.term_bg,
        }
    } else {
        Color::TRANSPARENT
    };
    container::Style {
        background: Some(Background::Color(bg)),
        text_color: Some(if active {
            chrome.text_primary
        } else {
            chrome.text_muted
        }),
        border: Border {
            color: Color::TRANSPARENT,
            width: 0.0,
            radius: 0.0.into(),
        },
        shadow: Shadow::default(),
        snap: true,
    }
}

/// Flat container style for Tabby `.btn-group` radio toggles (zero sunken track noise).
pub fn segmented_track_style(_chrome: ChromePalette) -> container::Style {
    container::Style {
        background: None,
        text_color: None,
        border: Border::default(),
        shadow: Shadow::default(),
        snap: true,
    }
}

/// Bootstrap `.btn-group > .btn` style inside Tabby settings forms.
pub fn segmented_button_style(
    chrome: ChromePalette,
    active: bool,
    accent_fill: bool,
) -> impl Fn(&Theme, button::Status) -> button::Style {
    move |_theme, status| {
        let hovered = matches!(status, button::Status::Hovered | button::Status::Pressed);
        let (bg, fg, border_color) = if active {
            if accent_fill {
                (chrome.accent, chrome.accent_fg, chrome.accent)
            } else {
                (chrome.bg_less_2, chrome.text_emphasis, chrome.border)
            }
        } else if hovered {
            (chrome.secondary_hover, chrome.text_primary, chrome.border_subtle)
        } else {
            (chrome.secondary, chrome.text_secondary, chrome.border_subtle)
        };
        button::Style {
            background: Some(Background::Color(bg)),
            text_color: fg,
            border: Border {
                color: border_color,
                width: if chrome.is_light || active { 1.0 } else { 0.0 },
                radius: 4.0.into(),
            },
            shadow: Shadow::default(),
            snap: true,
        }
    }
}

/// Tabby / Bootstrap checkbox styling calibrated against `ChromePalette`.
pub fn checkbox_style(
    chrome: ChromePalette,
) -> impl Fn(&Theme, checkbox::Status) -> checkbox::Style {
    move |_theme, status| {
        let (is_checked, hovered, disabled) = match status {
            checkbox::Status::Active { is_checked } => (is_checked, false, false),
            checkbox::Status::Hovered { is_checked } => (is_checked, true, false),
            checkbox::Status::Disabled { is_checked } => (is_checked, false, true),
        };
        let bg = if is_checked {
            chrome.accent
        } else if hovered {
            chrome.secondary_hover
        } else {
            chrome.secondary
        };
        let border_color = if is_checked {
            chrome.accent
        } else if hovered {
            chrome.text_muted
        } else {
            chrome.border
        };
        checkbox::Style {
            background: Background::Color(bg),
            icon_color: chrome.accent_fg,
            border: Border {
                color: border_color,
                width: 1.0,
                radius: 4.0.into(),
            },
            text_color: Some(if disabled {
                chrome.text_muted
            } else {
                chrome.text_primary
            }),
        }
    }
}

/// Renders a Tabby-styled checkbox/toggle with `FONT_BODY` typography.
pub fn apple_checkbox<'a, Msg: 'a>(
    is_checked: bool,
    label: impl Into<String>,
    on_toggle: impl Fn(bool) -> Msg + 'a,
    chrome: ChromePalette,
) -> Element<'a, Msg, Theme, iced::Renderer> {
    checkbox(is_checked)
        .label(label.into())
        .on_toggle(on_toggle)
        .size(16.0)
        .text_size(FONT_BODY)
        .spacing(8.0)
        .font(ui_font())
        .style(checkbox_style(chrome))
        .into()
}

/// Renders a clean Tabby `.modal-header` with light heading typography and flat close button.
pub fn sheet_header<'a, Msg: Clone + 'a>(
    title: impl Into<String>,
    subtitle: impl Into<String>,
    _shortcut: Option<&'static str>,
    on_close: Msg,
    chrome: ChromePalette,
) -> Element<'a, Msg, Theme, iced::Renderer> {
    let sub: String = subtitle.into();
    let mut title_col = column![
        text(title.into())
            .size(FONT_TITLE)
            .font(ui_font())
            .color(chrome.text_emphasis),
    ]
    .spacing(2)
    .width(Length::Fill);

    if !sub.is_empty() {
        title_col = title_col.push(
            text(sub)
                .size(FONT_META)
                .font(ui_font())
                .color(chrome.text_muted),
        );
    }

    row![
        title_col,
        button(text("×").size(16.0).font(ui_font()))
            .padding([3, 9])
            .style(pill_button_style(chrome, PillTone::Ghost))
            .on_press(on_close),
    ]
    .spacing(10)
    .align_y(Alignment::Center)
    .into()
}

/// Tabby `.form-control` input styling (recessed fill, accent blue focus ring).
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
                radius: 5.0.into(),
            },
            icon: chrome.text_muted,
            placeholder: chrome.text_muted,
            value: chrome.text_primary,
            selection: chrome.accent_soft,
        }
    }
}

/// Tabby-styled chevron handle for [`iced::widget::pick_list`] rendered in [`ui_font`] so glyphs never drop.
pub fn tabby_pick_list_handle() -> pick_list::Handle<Font> {
    pick_list::Handle::Dynamic {
        closed: pick_list::Icon {
            font: ui_font(),
            code_point: '▾',
            size: Some(iced::Pixels(12.0)),
            line_height: text::LineHeight::default(),
            shaping: text::Shaping::Advanced,
        },
        open: pick_list::Icon {
            font: ui_font(),
            code_point: '▴',
            size: Some(iced::Pixels(12.0)),
            line_height: text::LineHeight::default(),
            shaping: text::Shaping::Advanced,
        },
    }
}

/// Tabby `.form-select` / `.form-control` dropdown styling matching [`styled_input_style`].
pub fn tabby_pick_list_style(
    chrome: ChromePalette,
) -> impl Fn(&Theme, pick_list::Status) -> pick_list::Style {
    move |_theme, status| {
        let border_color = match status {
            pick_list::Status::Opened { .. } => chrome.accent,
            pick_list::Status::Hovered => chrome.border,
            pick_list::Status::Active => chrome.border_subtle,
        };
        pick_list::Style {
            text_color: chrome.text_primary,
            placeholder_color: chrome.text_muted,
            handle_color: chrome.text_secondary,
            background: Background::Color(chrome.input_bg),
            border: Border {
                color: border_color,
                width: 1.0,
                radius: 5.0.into(),
            },
        }
    }
}

/// Floating Tabby dropdown menu styling for [`iced::widget::pick_list`].
pub fn tabby_menu_style(chrome: ChromePalette) -> impl Fn(&Theme) -> menu::Style {
    move |_theme| menu::Style {
        background: Background::Color(chrome.panel_bg),
        border: Border {
            color: chrome.border,
            width: 1.0,
            radius: 6.0.into(),
        },
        text_color: chrome.text_primary,
        selected_text_color: chrome.accent_fg,
        selected_background: Background::Color(chrome.accent),
        shadow: Shadow::default(),
    }
}

/// Tabby pill toggle switch matching `.form-switch`.
pub fn tabby_switch<'a, Msg: Clone + 'a>(
    is_checked: bool,
    title: impl Into<String>,
    description: Option<&'a str>,
    on_toggle: impl Fn(bool) -> Msg + 'a,
    chrome: ChromePalette,
) -> Element<'a, Msg, Theme, iced::Renderer> {
    let track_bg = if is_checked {
        chrome.accent
    } else if chrome.is_light {
        mix(chrome.chrome_bg, chrome.border, 0.5)
    } else {
        chrome.bg_less_2
    };
    let knob_color = Color::WHITE;
    let knob = container(space::horizontal().width(12).height(12)).style(move |_| container::Style {
        background: Some(Background::Color(knob_color)),
        border: Border {
            radius: 999.0.into(),
            ..Default::default()
        },
        ..Default::default()
    });
    let track_row = if is_checked {
        row![space::horizontal(), knob].align_y(Alignment::Center)
    } else {
        row![knob, space::horizontal()].align_y(Alignment::Center)
    };
    let switch_pill = container(track_row)
        .width(Length::Fixed(34.0))
        .height(Length::Fixed(18.0))
        .padding([3, 3])
        .style(move |_| container::Style {
            background: Some(Background::Color(track_bg)),
            border: Border {
                radius: 999.0.into(),
                ..Default::default()
            },
            ..Default::default()
        });

    let toggle_btn = button(switch_pill)
        .padding(0)
        .style(move |_, _| button::Style {
            background: None,
            ..Default::default()
        })
        .on_press(on_toggle(!is_checked));

    let mut label_col = column![
        text(title.into())
            .size(FONT_BODY)
            .font(ui_font())
            .color(chrome.text_primary),
    ]
    .spacing(2)
    .width(Length::Fill);

    if let Some(desc) = description.filter(|s| !s.is_empty()) {
        label_col = label_col.push(
            text(desc)
                .size(FONT_META)
                .font(ui_font())
                .color(chrome.text_muted),
        );
    }

    row![label_col, toggle_btn]
        .spacing(12)
        .width(Length::Fill)
        .align_y(Alignment::Center)
        .into()
}

struct BottomUnderline<'a, Msg> {
    content: Element<'a, Msg, Theme, iced::Renderer>,
    gap: f32,
    thickness: f32,
    underline_color: Color,
}

impl<'a, Msg: 'a> Widget<Msg, Theme, iced::Renderer> for BottomUnderline<'a, Msg> {
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
        let extra_h = self.gap + self.thickness;
        let child_limits = limits.shrink(Size::new(0.0, extra_h));
        let child_node = self.content.as_widget_mut().layout(
            &mut tree.children[0],
            renderer,
            &child_limits,
        );
        let child_size = child_node.size();
        let intrinsic = Size::new(child_size.width, child_size.height + extra_h);
        let size = limits.resolve(self.size().width, self.size().height, intrinsic);
        layout::Node::with_children(size, vec![child_node])
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
        shell: &mut Shell<'_, Msg>,
        viewport: &Rectangle,
    ) {
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

        if self.underline_color.a > 0.0 {
            let bounds = layout.bounds();
            if bounds.intersects(viewport) && bounds.width > 0.0 {
                renderer.fill_quad(
                    Quad {
                        bounds: Rectangle {
                            x: bounds.x,
                            y: bounds.y + (bounds.height - self.thickness).max(0.0),
                            width: bounds.width,
                            height: self.thickness,
                        },
                        border: Border::default(),
                        shadow: Shadow::default(),
                        snap: true,
                    },
                    Background::Color(self.underline_color),
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
    ) -> Option<overlay::Element<'b, Msg, Theme, iced::Renderer>> {
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

/// Floating menu popover anchored right beneath a trigger widget using an Iced overlay,
/// preventing clipping inside parent scrollables or modal containers.
pub fn anchored_menu<'a, Msg: Clone + 'a>(
    anchor: Element<'a, Msg, Theme, iced::Renderer>,
    open: bool,
    menu: Element<'a, Msg, Theme, iced::Renderer>,
    menu_width: f32,
    on_dismiss: Msg,
) -> Element<'a, Msg, Theme, iced::Renderer> {
    Element::new(AnchoredMenu {
        children: [anchor, menu],
        open,
        menu_width,
        on_dismiss,
    })
}

struct AnchoredMenu<'a, Msg> {
    children: [Element<'a, Msg, Theme, iced::Renderer>; 2],
    open: bool,
    menu_width: f32,
    on_dismiss: Msg,
}

impl<'a, Msg: Clone + 'a> Widget<Msg, Theme, iced::Renderer> for AnchoredMenu<'a, Msg> {
    fn children(&self) -> Vec<Tree> {
        vec![Tree::new(&self.children[0]), Tree::new(&self.children[1])]
    }

    fn diff(&self, tree: &mut Tree) {
        tree.diff_children(&self.children);
    }

    fn size(&self) -> Size<Length> {
        self.children[0].as_widget().size()
    }

    fn layout(
        &mut self,
        tree: &mut Tree,
        renderer: &iced::Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        self.children[0]
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
        self.children[0]
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
        shell: &mut Shell<'_, Msg>,
        viewport: &Rectangle,
    ) {
        self.children[0].as_widget_mut().update(
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
        self.children[0].as_widget().mouse_interaction(
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
        self.children[0].as_widget().draw(
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
    ) -> Option<overlay::Element<'b, Msg, Theme, iced::Renderer>> {
        if !self.open {
            return self.children[0].as_widget_mut().overlay(
                &mut tree.children[0],
                layout,
                renderer,
                viewport,
                translation,
            );
        }

        let bounds = layout.bounds();
        let anchor_pos = Point::new(bounds.x + translation.x, bounds.y + translation.y);
        Some(overlay::Element::new(Box::new(AnchoredMenuOverlay {
            anchor_pos,
            anchor_size: bounds.size(),
            menu: &mut self.children[1],
            tree: &mut tree.children[1],
            menu_width: self.menu_width,
            on_dismiss: self.on_dismiss.clone(),
        })))
    }
}

struct AnchoredMenuOverlay<'a, 'b, Msg> {
    anchor_pos: Point,
    anchor_size: Size,
    menu: &'b mut Element<'a, Msg, Theme, iced::Renderer>,
    tree: &'b mut Tree,
    menu_width: f32,
    on_dismiss: Msg,
}

impl<Msg: Clone> overlay::Overlay<Msg, Theme, iced::Renderer> for AnchoredMenuOverlay<'_, '_, Msg> {
    fn layout(&mut self, renderer: &iced::Renderer, bounds: Size) -> layout::Node {
        let target_w = self.menu_width.min((bounds.width - 16.0).max(180.0));
        let max_h = (bounds.height - 16.0).max(120.0);
        let limits = layout::Limits::new(Size::new(target_w, 0.0), Size::new(target_w, max_h));
        let node = self.menu.as_widget_mut().layout(self.tree, renderer, &limits);
        let size = node.size();
        let x = self
            .anchor_pos
            .x
            .min((bounds.width - size.width - 8.0).max(8.0))
            .max(8.0);
        let below_y = self.anchor_pos.y + self.anchor_size.height + 4.0;
        let above_y = self.anchor_pos.y - size.height - 4.0;
        let y = if below_y + size.height <= bounds.height - 8.0 {
            below_y
        } else if above_y >= 8.0 {
            above_y
        } else {
            (bounds.height - size.height - 8.0).max(8.0)
        };
        node.move_to(Point::new(x, y))
    }

    fn draw(
        &self,
        renderer: &mut iced::Renderer,
        theme: &Theme,
        style: &renderer::Style,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
    ) {
        self.menu
            .as_widget()
            .draw(self.tree, renderer, theme, style, layout, cursor, &layout.bounds());
    }

    fn operate(
        &mut self,
        layout: Layout<'_>,
        renderer: &iced::Renderer,
        operation: &mut dyn Operation,
    ) {
        self.menu
            .as_widget_mut()
            .operate(self.tree, layout, renderer, operation);
    }

    fn update(
        &mut self,
        event: &Event,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        renderer: &iced::Renderer,
        clipboard: &mut dyn Clipboard,
        shell: &mut Shell<'_, Msg>,
    ) {
        self.menu.as_widget_mut().update(
            self.tree,
            event,
            layout,
            cursor,
            renderer,
            clipboard,
            shell,
            &layout.bounds(),
        );
        if shell.is_event_captured() {
            return;
        }
        match event {
            Event::Mouse(mouse::Event::ButtonPressed(_)) if !cursor.is_over(layout.bounds()) => {
                shell.publish(self.on_dismiss.clone());
                shell.capture_event();
            }
            Event::Keyboard(keyboard::Event::KeyPressed {
                key: keyboard::Key::Named(keyboard::key::Named::Escape),
                ..
            }) => {
                shell.publish(self.on_dismiss.clone());
                shell.capture_event();
            }
            _ => {}
        }
    }

    fn mouse_interaction(
        &self,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        renderer: &iced::Renderer,
    ) -> mouse::Interaction {
        if cursor.is_over(layout.bounds()) {
            self.menu.as_widget().mouse_interaction(
                self.tree,
                layout,
                cursor,
                &layout.bounds(),
                renderer,
            )
        } else {
            mouse::Interaction::None
        }
    }

    fn overlay<'b>(
        &'b mut self,
        layout: Layout<'b>,
        renderer: &iced::Renderer,
    ) -> Option<overlay::Element<'b, Msg, Theme, iced::Renderer>> {
        self.menu
            .as_widget_mut()
            .overlay(self.tree, layout, renderer, &layout.bounds(), Vector::ZERO)
    }

    fn index(&self) -> f32 {
        2.0
    }
}

/// Tabby horizontal underline navigation tab (`GENERAL`, `PORTS`, `ADVANCED`, `LOGIN SCRIPTS`).
pub fn tabby_underline_tab<'a, Msg: Clone + 'a>(
    label: &'static str,
    active: bool,
    on_press: Msg,
    chrome: ChromePalette,
) -> Element<'a, Msg, Theme, iced::Renderer> {
    let underline_color = if active {
        chrome.text_emphasis
    } else {
        Color::TRANSPARENT
    };
    let text_color = if active {
        chrome.text_emphasis
    } else {
        chrome.text_muted
    };
    let content: Element<'a, Msg, Theme, iced::Renderer> = Element::new(BottomUnderline {
        content: text(label)
            .size(12.0)
            .font(ui_font_semibold())
            .color(text_color)
            .into(),
        gap: 6.0,
        thickness: 2.0,
        underline_color,
    });
    button(content)
    .padding([4, 2])
    .style(move |_, status| {
        let hovered = matches!(status, button::Status::Hovered | button::Status::Pressed);
        button::Style {
            background: None,
            text_color: if active || hovered {
                chrome.text_emphasis
            } else {
                chrome.text_muted
            },
            border: Border::default(),
            shadow: Shadow::default(),
            snap: true,
        }
    })
    .on_press(on_press)
    .into()
}

/// Tabby segmented tile button style used for `Authentication method` selector bars.
pub fn tabby_auth_tile_style(
    chrome: ChromePalette,
    active: bool,
) -> impl Fn(&Theme, button::Status) -> button::Style {
    move |_theme, status| {
        let hovered = matches!(status, button::Status::Hovered | button::Status::Pressed);
        let bg = if active {
            if chrome.is_light {
                chrome.card_hover
            } else {
                chrome.secondary
            }
        } else if hovered {
            chrome.bg_less_2
        } else {
            Color::TRANSPARENT
        };
        button::Style {
            background: Some(Background::Color(bg)),
            text_color: if active || hovered {
                chrome.text_emphasis
            } else {
                chrome.text_muted
            },
            border: Border {
                color: Color::TRANSPARENT,
                width: 0.0,
                radius: 4.0.into(),
            },
            shadow: Shadow::default(),
            snap: true,
        }
    }
}

/// Subtle `.list-group` / `.collapse-container` surface (`--theme-bg-less`, `6px` radius).
pub fn inset_well_style(chrome: ChromePalette) -> container::Style {
    container::Style {
        background: Some(Background::Color(chrome.well_bg)),
        text_color: Some(chrome.text_primary),
        border: Border {
            color: chrome.hairline,
            width: 1.0,
            radius: 6.0.into(),
        },
        shadow: Shadow::default(),
        snap: true,
    }
}

/// Container style for selectable list rows and grouped settings blocks.
pub fn card_container_style(chrome: ChromePalette, selected: bool) -> container::Style {
    container::Style {
        background: Some(Background::Color(if selected {
            chrome.bg_less_2
        } else {
            chrome.bg_less
        })),
        text_color: Some(chrome.text_primary),
        border: Border {
            color: if selected {
                chrome.accent
            } else {
                chrome.hairline
            },
            width: 1.0,
            radius: 5.0.into(),
        },
        shadow: Shadow::default(),
        snap: true,
    }
}

/// Tabby `.modal-content` surface (`--body-bg` / `$content-bg-solid`, `6px` radius, clean shadow).
pub fn modal_card_style(chrome: ChromePalette, _highlight_accent: bool) -> container::Style {
    container::Style {
        background: Some(Background::Color(chrome.panel_bg)),
        text_color: Some(chrome.text_primary),
        border: Border {
            color: chrome.border_subtle,
            width: 1.0,
            radius: 6.0.into(),
        },
        shadow: Shadow::default(),
        snap: true,
    }
}

/// Tabby subtle `1px` horizontal divider (`--bs-border-color`).
pub fn hairline_h<'a, Msg: 'a>(chrome: ChromePalette) -> Element<'a, Msg, Theme, iced::Renderer> {
    container(space::horizontal().height(1))
        .width(Length::Fill)
        .style(move |_| container::Style {
            background: Some(Background::Color(chrome.hairline)),
            ..Default::default()
        })
        .into()
}

/// Tabby subtle `1px` vertical divider.
pub fn hairline_v<'a, Msg: 'a>(chrome: ChromePalette) -> Element<'a, Msg, Theme, iced::Renderer> {
    container(space::vertical().width(1))
        .height(Length::Fill)
        .style(move |_| container::Style {
            background: Some(Background::Color(chrome.hairline)),
            ..Default::default()
        })
        .into()
}

/// Tabby dark/light scrollbar styling matching `theme.new.scss` (`::-webkit-scrollbar`).
pub fn tabby_scrollable_style(
    chrome: ChromePalette,
) -> impl Fn(&Theme, scrollable::Status) -> scrollable::Style {
    move |_theme, status| {
        let rail_bg = Color::from_rgba(0.0, 0.0, 0.0, if chrome.is_light { 0.04 } else { 0.125 });
        let scroller_color = match status {
            scrollable::Status::Hovered { .. } | scrollable::Status::Dragged { .. } => {
                if chrome.is_light {
                    Color::from_rgba(0.0, 0.0, 0.0, 0.34)
                } else {
                    Color::from_rgba(1.0, 1.0, 1.0, 0.35)
                }
            }
            scrollable::Status::Active { .. } => {
                if chrome.is_light {
                    Color::from_rgba(0.0, 0.0, 0.0, 0.20)
                } else {
                    Color::from_rgba(1.0, 1.0, 1.0, 0.22)
                }
            }
        };
        let rail = scrollable::Rail {
            background: Some(Background::Color(rail_bg)),
            border: Border::default(),
            scroller: scrollable::Scroller {
                background: Background::Color(scroller_color),
                border: Border {
                    radius: 2.0.into(),
                    ..Default::default()
                },
            },
        };
        scrollable::Style {
            container: container::Style::default(),
            vertical_rail: rail,
            horizontal_rail: rail,
            gap: None,
            auto_scroll: scrollable::AutoScroll {
                background: Background::Color(chrome.panel_bg),
                border: Border::default(),
                shadow: Shadow::default(),
                icon: chrome.text_secondary,
            },
        }
    }
}

/// Tabby 3px progress bar styling (`--bs-blue` / `chrome.accent` on subtle track).
pub fn tabby_progress_style(
    chrome: ChromePalette,
) -> impl Fn(&Theme) -> progress_bar::Style {
    colored_progress_style(chrome, chrome.accent)
}

/// Custom-colored rounded progress bar styling on a subtle theme track.
pub fn colored_progress_style(
    chrome: ChromePalette,
    bar_color: Color,
) -> impl Fn(&Theme) -> progress_bar::Style {
    move |_theme| progress_bar::Style {
        background: Background::Color(chrome.bg_less_2),
        bar: Background::Color(bar_color),
        border: Border {
            radius: 3.0.into(),
            ..Default::default()
        },
    }
}

/// Wraps `card` over a click-to-dismiss dimmed backdrop without letting clicks inside `card`
/// bubble through to `on_dismiss`.
pub fn modal_overlay<'a, Msg: Clone + 'a>(
    card: Element<'a, Msg, Theme, iced::Renderer>,
    on_dismiss: Option<Msg>,
    top_padding: f32,
) -> Element<'a, Msg, Theme, iced::Renderer> {
    let scrim_box = container(
        space::horizontal()
            .width(Length::Fill)
            .height(Length::Fill),
    )
    .width(Length::Fill)
    .height(Length::Fill)
    .style(|_theme| container::Style {
        background: Some(Background::Color(Color::from_rgba(0.0, 0.0, 0.0, 0.50))),
        ..Default::default()
    });

    let scrim: Element<'a, Msg, Theme, iced::Renderer> = if let Some(msg) = on_dismiss {
        mouse_area(scrim_box).on_press(msg).into()
    } else {
        scrim_box.into()
    };

    let side_pad = if top_padding > 32.0 { 16.0 } else { 12.0 };
    let mut card_layer = container(opaque(card))
        .width(Length::Fill)
        .height(Length::Fill)
        .center_x(Length::Fill)
        .padding(Padding {
            top: top_padding,
            right: side_pad,
            bottom: 16.0,
            left: side_pad,
        });
    if top_padding <= 24.0 {
        card_layer = card_layer.center_y(Length::Fill);
    }

    opaque(stack![scrim, card_layer]).into()
}

/// Small circular colored status dot (`7px` diameter) for live runtime transport/tunnel state.
pub fn status_dot<'a, Msg: 'a>(color: Color) -> Element<'a, Msg, Theme, iced::Renderer> {
    container(space::horizontal().width(7).height(7))
        .width(7)
        .height(7)
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

/// Bootstrap/Tabby `.badge` pill (`4px` radius, compact).
pub fn badge_pill<'a, Msg: 'a>(
    label: impl Into<String>,
    fg: Color,
    bg: Color,
    border_color: Color,
) -> Element<'a, Msg, Theme, iced::Renderer> {
    container(
        text(label.into())
            .size(FONT_META)
            .font(ui_font_semibold())
            .color(fg),
    )
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

/// Tabby hotkey/shortcut badge (`ENTER`, `Ctrl+K`, etc. — `.badge.text-bg-secondary`).
pub fn keycap_pill<'a, Msg: 'a>(
    shortcut: impl Into<String>,
    chrome: ChromePalette,
) -> Element<'a, Msg, Theme, iced::Renderer> {
    container(
        text(shortcut.into())
            .size(10.5)
            .font(mono_font())
            .color(chrome.text_secondary),
    )
    .padding([2, 6])
    .align_y(Alignment::Center)
    .style(move |_theme| container::Style {
        background: Some(Background::Color(chrome.secondary)),
        border: Border {
            color: chrome.border_subtle,
            width: 1.0,
            radius: 4.0.into(),
        },
        ..Default::default()
    })
    .into()
}

/// Tabby `.form-line` horizontal setting row (`.header` title+description left, control right).
pub fn form_line<'a, Msg: 'a>(
    title: impl Into<String>,
    description: Option<&'a str>,
    control: Element<'a, Msg, Theme, iced::Renderer>,
    compact: bool,
    chrome: ChromePalette,
) -> Element<'a, Msg, Theme, iced::Renderer> {
    let mut header = column![
        text(title.into())
            .size(FONT_SUBHEAD)
            .font(ui_font())
            .color(chrome.text_primary),
    ]
    .spacing(2);
    if let Some(desc) = description.filter(|s| !s.is_empty()) {
        header = header.push(
            text(desc)
                .size(12.0)
                .font(ui_font())
                .color(chrome.text_muted),
        );
    }

    if compact {
        column![header, control]
            .spacing(8)
            .padding([8, 0])
            .width(Length::Fill)
            .into()
    } else {
        row![header.width(Length::Fill), control]
            .spacing(16)
            .padding([8, 0])
            .width(Length::Fill)
            .align_y(Alignment::Center)
            .into()
    }
}

fn rgb(c: RgbColor) -> Color {
    Color::from_rgb8(c.r, c.g, c.b)
}

pub(crate) fn mix(a: Color, b: Color, t: f32) -> Color {
    let t = t.clamp(0.0, 1.0);
    Color {
        r: a.r + (b.r - a.r) * t,
        g: a.g + (b.g - a.g) * t,
        b: a.b + (b.b - a.b) * t,
        a: a.a + (b.a - a.a) * t,
    }
}

fn srgb_to_linear(c: f32) -> f32 {
    if c <= 0.04045 {
        c / 12.92
    } else {
        ((c + 0.055) / 1.055).powf(2.4)
    }
}

fn linear_to_srgb(c: f32) -> f32 {
    let c = c.clamp(0.0, 1.0);
    if c <= 0.0031308 {
        12.92 * c
    } else {
        1.055 * c.powf(1.0 / 2.4) - 0.055
    }
}

fn rgb_to_oklab(c: Color) -> (f32, f32, f32) {
    let r = srgb_to_linear(c.r);
    let g = srgb_to_linear(c.g);
    let b = srgb_to_linear(c.b);
    let l = (0.4122214708 * r + 0.5363325363 * g + 0.0514459929 * b).cbrt();
    let m = (0.2119034982 * r + 0.6806995451 * g + 0.1073969566 * b).cbrt();
    let s = (0.0883024619 * r + 0.2817188376 * g + 0.6299787005 * b).cbrt();
    (
        0.2104542553 * l + 0.7936177850 * m - 0.0040720468 * s,
        1.9779984951 * l - 2.4285922050 * m + 0.4505937099 * s,
        0.0259040371 * l + 0.7827717662 * m - 0.8086757660 * s,
    )
}

fn oklab_to_linear_srgb(l_ok: f32, a_ok: f32, b_ok: f32) -> (f32, f32, f32) {
    let l_ = l_ok + 0.3963377774 * a_ok + 0.2158037573 * b_ok;
    let m_ = l_ok - 0.1055613458 * a_ok - 0.0638541728 * b_ok;
    let s_ = l_ok - 0.0894841775 * a_ok - 1.2914855480 * b_ok;
    let l = l_ * l_ * l_;
    let m = m_ * m_ * m_;
    let s = s_ * s_ * s_;
    (
        4.0767416621 * l - 3.3077115913 * m + 0.2309699292 * s,
        -1.2684380046 * l + 2.6097574011 * m - 0.3413193965 * s,
        -0.0041960863 * l - 0.7034186147 * m + 1.7076147010 * s,
    )
}

fn rgb_to_oklch(c: Color) -> (f32, f32, f32) {
    let (l, a, b) = rgb_to_oklab(c);
    let chroma = (a * a + b * b).sqrt();
    let hue = b.atan2(a);
    (l, chroma, hue)
}

fn in_srgb_gamut(r: f32, g: f32, b: f32) -> bool {
    const EPS: f32 = 1e-4;
    r >= -EPS && r <= 1.0 + EPS && g >= -EPS && g <= 1.0 + EPS && b >= -EPS && b <= 1.0 + EPS
}

fn oklch_to_srgb_gamut(l: f32, chroma: f32, hue: f32, a: f32) -> Color {
    let target_l = l.clamp(0.0, 1.0);
    if target_l <= 1e-5 {
        return Color { r: 0.0, g: 0.0, b: 0.0, a };
    }
    if target_l >= 1.0 - 1e-5 {
        return Color { r: 1.0, g: 1.0, b: 1.0, a };
    }
    let cos_h = hue.cos();
    let sin_h = hue.sin();
    let target_c = chroma.max(0.0);
    let (r0, g0, b0) = oklab_to_linear_srgb(target_l, target_c * cos_h, target_c * sin_h);
    let (r, g, b) = if in_srgb_gamut(r0, g0, b0) {
        (r0, g0, b0)
    } else {
        let mut lo = 0.0_f32;
        let mut hi = target_c;
        for _ in 0..20 {
            let mid = (lo + hi) * 0.5;
            let (rm, gm, bm) = oklab_to_linear_srgb(target_l, mid * cos_h, mid * sin_h);
            if in_srgb_gamut(rm, gm, bm) {
                lo = mid;
            } else {
                hi = mid;
            }
        }
        oklab_to_linear_srgb(target_l, lo * cos_h, lo * sin_h)
    };
    Color {
        r: linear_to_srgb(r),
        g: linear_to_srgb(g),
        b: linear_to_srgb(b),
        a,
    }
}

fn oklch_shift_lightness(c: Color, delta_l: f32) -> Color {
    let (l, chroma, hue) = rgb_to_oklch(c);
    oklch_to_srgb_gamut((l + delta_l).clamp(0.0, 1.0), chroma, hue, c.a)
}

fn relative_luminance(c: Color) -> f32 {
    0.2126 * srgb_to_linear(c.r)
        + 0.7152 * srgb_to_linear(c.g)
        + 0.0722 * srgb_to_linear(c.b)
}

fn contrast_ratio(a: Color, b: Color) -> f32 {
    let la = relative_luminance(a);
    let lb = relative_luminance(b);
    let (hi, lo) = if la >= lb { (la, lb) } else { (lb, la) };
    (hi + 0.05) / (lo + 0.05)
}

fn contrast_pick(bg: Color, dark_candidate: Color, light_candidate: Color) -> Color {
    if contrast_ratio(dark_candidate, bg) >= contrast_ratio(light_candidate, bg) {
        dark_candidate
    } else {
        light_candidate
    }
}

fn min_contrast(fg: Color, bgs: &[Color]) -> f32 {
    bgs.iter()
        .map(|&bg| contrast_ratio(fg, bg))
        .fold(f32::INFINITY, f32::min)
}

fn ensure_contrast_against(fg: Color, bgs: &[Color], min_ratio: f32) -> Color {
    if min_contrast(fg, bgs) >= min_ratio {
        return fg;
    }
    let (l0, chroma, hue) = rgb_to_oklch(fg);

    // Search upward in OKLCH lightness for the closest passing candidate.
    let mut lo = l0;
    let mut hi = 1.0_f32;
    for _ in 0..20 {
        let mid = (lo + hi) * 0.5;
        let cand = oklch_to_srgb_gamut(mid, chroma, hue, fg.a);
        if min_contrast(cand, bgs) >= min_ratio {
            hi = mid;
        } else {
            lo = mid;
        }
    }
    let up_cand = oklch_to_srgb_gamut(hi, chroma, hue, fg.a);
    let up_ok = min_contrast(up_cand, bgs) >= min_ratio;

    // Search downward in OKLCH lightness for the closest passing candidate.
    let mut lo_d = 0.0_f32;
    let mut hi_d = l0;
    for _ in 0..20 {
        let mid = (lo_d + hi_d) * 0.5;
        let cand = oklch_to_srgb_gamut(mid, chroma, hue, fg.a);
        if min_contrast(cand, bgs) >= min_ratio {
            lo_d = mid;
        } else {
            hi_d = mid;
        }
    }
    let down_cand = oklch_to_srgb_gamut(lo_d, chroma, hue, fg.a);
    let down_ok = min_contrast(down_cand, bgs) >= min_ratio;

    match (up_ok, down_ok) {
        (true, true) => {
            if (hi - l0).abs() <= (l0 - lo_d).abs() {
                up_cand
            } else {
                down_cand
            }
        }
        (true, false) => up_cand,
        (false, true) => down_cand,
        (false, false) => {
            let white = Color { a: fg.a, ..Color::WHITE };
            let black = Color { a: fg.a, ..Color::BLACK };
            let mut best = fg;
            let mut best_ratio = min_contrast(fg, bgs);
            for cand in [up_cand, down_cand, white, black] {
                let r = min_contrast(cand, bgs);
                if r > best_ratio {
                    best = cand;
                    best_ratio = r;
                }
            }
            best
        }
    }
}
