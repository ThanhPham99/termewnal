//! Built-in color themes and terminal ANSI palettes for Termewnal.

/// Lightweight RGB color tuple independent of GUI frameworks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RgbColor {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl RgbColor {
    /// Creates an [`RgbColor`] from 8-bit red, green, and blue channels.
    pub const fn new(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b }
    }
}

/// Supported built-in color themes configurable in `config.toml`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum BuiltinTheme {
    Catppuccin,
    TokyoNight,
    Dracula,
    Nord,
    #[default]
    Dark,
    Light,
}

impl BuiltinTheme {
    /// All six built-in Termewnal themes in display order.
    pub const ALL: &'static [Self] = &[
        Self::Catppuccin,
        Self::TokyoNight,
        Self::Dracula,
        Self::Nord,
        Self::Dark,
        Self::Light,
    ];

    /// Canonical theme name suitable for `config.toml` and UI labels.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Catppuccin => "Catppuccin",
            Self::TokyoNight => "Tokyo Night",
            Self::Dracula => "Dracula",
            Self::Nord => "Nord",
            Self::Dark => "Dark",
            Self::Light => "Light",
        }
    }

    /// Parses a built-in theme name (case- and separator-insensitive), returning `None` if unknown.
    pub fn from_name(name: &str) -> Option<Self> {
        let normalized: String = name
            .trim()
            .chars()
            .filter(|c| !c.is_ascii_whitespace() && *c != '_' && *c != '-')
            .map(|c| c.to_ascii_lowercase())
            .collect();

        match normalized.as_str() {
            "catppuccin" | "catppuccinmocha" => Some(Self::Catppuccin),
            "tokyonight" | "tokyonightstorm" => Some(Self::TokyoNight),
            "dracula" => Some(Self::Dracula),
            "nord" => Some(Self::Nord),
            "dark" => Some(Self::Dark),
            "light" => Some(Self::Light),
            _ => None,
        }
    }

    /// Resolves a theme name, falling back to [`BuiltinTheme::Dark`] for unrecognized names.
    pub fn resolve(name: &str) -> Self {
        Self::from_name(name).unwrap_or(Self::Dark)
    }

    /// Returns the terminal and UI color palette associated with this theme.
    pub const fn palette(self) -> ThemePalette {
        match self {
            Self::Catppuccin => CATPPUCCIN_MOCHA,
            Self::TokyoNight => TOKYO_NIGHT,
            Self::Dracula => DRACULA,
            Self::Nord => NORD,
            Self::Dark => DARK,
            Self::Light => LIGHT,
        }
    }
}

/// Complete terminal and overlay palette for a [`BuiltinTheme`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ThemePalette {
    pub theme: BuiltinTheme,
    pub is_light: bool,
    pub background: RgbColor,
    pub foreground: RgbColor,
    pub cursor: RgbColor,
    pub selection_bg: RgbColor,
    pub selection_fg: Option<RgbColor>,
    pub match_bg: RgbColor,
    pub match_fg: RgbColor,
    pub active_match_bg: RgbColor,
    pub active_match_fg: RgbColor,
    pub ui_surface: RgbColor,
    pub ui_border: RgbColor,
    pub ui_accent: RgbColor,
    pub ansi: [RgbColor; 16],
}

impl ThemePalette {
    /// Resolves a palette by theme name, defaulting to [`BuiltinTheme::Dark`] when unrecognized.
    pub fn for_name(name: &str) -> Self {
        BuiltinTheme::resolve(name).palette()
    }
}

const fn rgb(r: u8, g: u8, b: u8) -> RgbColor {
    RgbColor::new(r, g, b)
}

const CATPPUCCIN_MOCHA: ThemePalette = ThemePalette {
    theme: BuiltinTheme::Catppuccin,
    is_light: false,
    background: rgb(0x1e, 0x1e, 0x2e),
    foreground: rgb(0xcd, 0xd6, 0xf4),
    cursor: rgb(0xf5, 0xe0, 0xdc),
    selection_bg: rgb(0x45, 0x47, 0x5a),
    selection_fg: None,
    match_bg: rgb(0x58, 0x4c, 0x2f),
    match_fg: rgb(0xf9, 0xe2, 0xaf),
    active_match_bg: rgb(0xf9, 0xe2, 0xaf),
    active_match_fg: rgb(0x1e, 0x1e, 0x2e),
    ui_surface: rgb(0x31, 0x32, 0x44),
    ui_border: rgb(0x58, 0x5b, 0x70),
    ui_accent: rgb(0x89, 0xb4, 0xfa),
    ansi: [
        rgb(0x45, 0x47, 0x5a),
        rgb(0xf3, 0x8b, 0xa8),
        rgb(0xa6, 0xe3, 0xa1),
        rgb(0xf9, 0xe2, 0xaf),
        rgb(0x89, 0xb4, 0xfa),
        rgb(0xf5, 0xc2, 0xe7),
        rgb(0x94, 0xe2, 0xd5),
        rgb(0xba, 0xc2, 0xde),
        rgb(0x58, 0x5b, 0x70),
        rgb(0xf3, 0x8b, 0xa8),
        rgb(0xa6, 0xe3, 0xa1),
        rgb(0xf9, 0xe2, 0xaf),
        rgb(0x89, 0xb4, 0xfa),
        rgb(0xf5, 0xc2, 0xe7),
        rgb(0x94, 0xe2, 0xd5),
        rgb(0xa6, 0xad, 0xc8),
    ],
};

const TOKYO_NIGHT: ThemePalette = ThemePalette {
    theme: BuiltinTheme::TokyoNight,
    is_light: false,
    background: rgb(0x1a, 0x1b, 0x26),
    foreground: rgb(0xc0, 0xca, 0xf5),
    cursor: rgb(0xc0, 0xca, 0xf5),
    selection_bg: rgb(0x33, 0x46, 0x7c),
    selection_fg: None,
    match_bg: rgb(0x53, 0x44, 0x2a),
    match_fg: rgb(0xe0, 0xaf, 0x68),
    active_match_bg: rgb(0xe0, 0xaf, 0x68),
    active_match_fg: rgb(0x1a, 0x1b, 0x26),
    ui_surface: rgb(0x24, 0x28, 0x3b),
    ui_border: rgb(0x41, 0x48, 0x68),
    ui_accent: rgb(0x7a, 0xa2, 0xf7),
    ansi: [
        rgb(0x15, 0x16, 0x1e),
        rgb(0xf7, 0x76, 0x8e),
        rgb(0x9e, 0xce, 0x6a),
        rgb(0xe0, 0xaf, 0x68),
        rgb(0x7a, 0xa2, 0xf7),
        rgb(0xbb, 0x9a, 0xf7),
        rgb(0x7d, 0xcf, 0xff),
        rgb(0xa9, 0xb1, 0xd6),
        rgb(0x41, 0x48, 0x68),
        rgb(0xf7, 0x76, 0x8e),
        rgb(0x9e, 0xce, 0x6a),
        rgb(0xe0, 0xaf, 0x68),
        rgb(0x7a, 0xa2, 0xf7),
        rgb(0xbb, 0x9a, 0xf7),
        rgb(0x7d, 0xcf, 0xff),
        rgb(0xc0, 0xca, 0xf5),
    ],
};

const DRACULA: ThemePalette = ThemePalette {
    theme: BuiltinTheme::Dracula,
    is_light: false,
    background: rgb(0x28, 0x2a, 0x36),
    foreground: rgb(0xf8, 0xf8, 0xf2),
    cursor: rgb(0xf8, 0xf8, 0xf2),
    selection_bg: rgb(0x44, 0x47, 0x5a),
    selection_fg: None,
    match_bg: rgb(0x5b, 0x54, 0x32),
    match_fg: rgb(0xf1, 0xfa, 0x8c),
    active_match_bg: rgb(0xf1, 0xfa, 0x8c),
    active_match_fg: rgb(0x28, 0x2a, 0x36),
    ui_surface: rgb(0x34, 0x37, 0x46),
    ui_border: rgb(0x62, 0x72, 0xa4),
    ui_accent: rgb(0xbd, 0x93, 0xf9),
    ansi: [
        rgb(0x21, 0x22, 0x2c),
        rgb(0xff, 0x55, 0x55),
        rgb(0x50, 0xfa, 0x7b),
        rgb(0xf1, 0xfa, 0x8c),
        rgb(0xbd, 0x93, 0xf9),
        rgb(0xff, 0x79, 0xc6),
        rgb(0x8b, 0xe9, 0xfd),
        rgb(0xf8, 0xf8, 0xf2),
        rgb(0x62, 0x72, 0xa4),
        rgb(0xff, 0x6e, 0x6e),
        rgb(0x69, 0xff, 0x94),
        rgb(0xff, 0xff, 0xa5),
        rgb(0xd6, 0xac, 0xff),
        rgb(0xff, 0x92, 0xdf),
        rgb(0xa4, 0xff, 0xff),
        rgb(0xff, 0xff, 0xff),
    ],
};

const NORD: ThemePalette = ThemePalette {
    theme: BuiltinTheme::Nord,
    is_light: false,
    background: rgb(0x2e, 0x34, 0x40),
    foreground: rgb(0xec, 0xef, 0xf4),
    cursor: rgb(0xd8, 0xde, 0xe9),
    selection_bg: rgb(0x43, 0x4c, 0x5e),
    selection_fg: None,
    match_bg: rgb(0x5a, 0x4f, 0x37),
    match_fg: rgb(0xeb, 0xcb, 0x8b),
    active_match_bg: rgb(0xeb, 0xcb, 0x8b),
    active_match_fg: rgb(0x2e, 0x34, 0x40),
    ui_surface: rgb(0x3b, 0x42, 0x52),
    ui_border: rgb(0x4c, 0x56, 0x6a),
    ui_accent: rgb(0x88, 0xc0, 0xd0),
    ansi: [
        rgb(0x3b, 0x42, 0x52),
        rgb(0xbf, 0x61, 0x6a),
        rgb(0xa3, 0xbe, 0x8c),
        rgb(0xeb, 0xcb, 0x8b),
        rgb(0x81, 0xa1, 0xc1),
        rgb(0xb4, 0x8e, 0xad),
        rgb(0x88, 0xc0, 0xd0),
        rgb(0xe5, 0xe9, 0xf0),
        rgb(0x4c, 0x56, 0x6a),
        rgb(0xbf, 0x61, 0x6a),
        rgb(0xa3, 0xbe, 0x8c),
        rgb(0xeb, 0xcb, 0x8b),
        rgb(0x81, 0xa1, 0xc1),
        rgb(0xb4, 0x8e, 0xad),
        rgb(0x8f, 0xbc, 0xbb),
        rgb(0xec, 0xef, 0xf4),
    ],
};

const DARK: ThemePalette = ThemePalette {
    theme: BuiltinTheme::Dark,
    is_light: false,
    background: rgb(0x2b, 0x2d, 0x31),
    foreground: rgb(0xe5, 0xe5, 0xe5),
    cursor: rgb(0x58, 0x65, 0xf2),
    selection_bg: rgb(0x3e, 0x44, 0x52),
    selection_fg: None,
    match_bg: rgb(0x5a, 0x4b, 0x2c),
    match_fg: rgb(0xf2, 0xcc, 0x86),
    active_match_bg: rgb(0xf2, 0xcc, 0x86),
    active_match_fg: rgb(0x1e, 0x22, 0x2a),
    ui_surface: rgb(0x23, 0x25, 0x29),
    ui_border: rgb(0x4a, 0x4f, 0x5a),
    ui_accent: rgb(0x58, 0x65, 0xf2),
    ansi: [
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
    ],
};

const LIGHT: ThemePalette = ThemePalette {
    theme: BuiltinTheme::Light,
    is_light: true,
    background: rgb(0xff, 0xff, 0xff),
    foreground: rgb(0x1e, 0x22, 0x2a),
    cursor: rgb(0x58, 0x65, 0xf2),
    selection_bg: rgb(0xd0, 0xdc, 0xf5),
    selection_fg: None,
    match_bg: rgb(0xfe, 0xe8, 0xb0),
    match_fg: rgb(0x1e, 0x22, 0x2a),
    active_match_bg: rgb(0xf5, 0x9e, 0x0b),
    active_match_fg: rgb(0xff, 0xff, 0xff),
    ui_surface: rgb(0xf3, 0xf4, 0xf6),
    ui_border: rgb(0xd1, 0xd5, 0xdb),
    ui_accent: rgb(0x4f, 0x46, 0xe5),
    ansi: [
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
    ],
};
