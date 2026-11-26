//! Configurable single-key and multi-chord keyboard shortcuts.

use std::collections::BTreeMap;
use std::fmt;

use serde::{Deserialize, Serialize};

/// Bindable application, workspace, and terminal actions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KeyAction {
    NewTab,
    DuplicateTab,
    RenameTab,
    CloseFocusedPane,
    SplitVertical,
    SplitHorizontal,
    TogglePaneZoom,
    ToggleBroadcastInput,
    ToggleSidebar,
    ToggleSftpDrawer,
    ToggleCommandPalette,
    OpenQuickConnect,
    OpenSettings,
    OpenNewProfile,
    OpenQuickCommands,
    OpenWorkspaces,
    OpenSearch,
    Copy,
    Paste,
    ClearTerminal,
    SaveScrollback,
    ToggleSessionLogging,
    FontZoomIn,
    FontZoomOut,
    FontZoomReset,
    CycleTabNext,
    CycleTabPrev,
    MoveTabLeft,
    MoveTabRight,
    FocusPaneNext,
    FocusPanePrev,
    FocusPaneLeft,
    FocusPaneRight,
    FocusPaneUp,
    FocusPaneDown,
    SwitchTab1,
    SwitchTab2,
    SwitchTab3,
    SwitchTab4,
    SwitchTab5,
    SwitchTab6,
    SwitchTab7,
    SwitchTab8,
    SwitchTab9,
    NewWindow,
    ToggleQuake,
    ZmodemSend,
}

impl KeyAction {
    /// Every bindable action in display order.
    pub const ALL: &'static [Self] = &[
        Self::NewTab,
        Self::DuplicateTab,
        Self::RenameTab,
        Self::CloseFocusedPane,
        Self::SplitVertical,
        Self::SplitHorizontal,
        Self::TogglePaneZoom,
        Self::ToggleBroadcastInput,
        Self::ToggleSftpDrawer,
        Self::ToggleCommandPalette,
        Self::OpenQuickConnect,
        Self::OpenSettings,
        Self::OpenNewProfile,
        Self::OpenQuickCommands,
        Self::OpenWorkspaces,
        Self::OpenSearch,
        Self::Copy,
        Self::Paste,
        Self::ClearTerminal,
        Self::SaveScrollback,
        Self::ToggleSessionLogging,
        Self::FontZoomIn,
        Self::FontZoomOut,
        Self::FontZoomReset,
        Self::CycleTabNext,
        Self::CycleTabPrev,
        Self::MoveTabLeft,
        Self::MoveTabRight,
        Self::FocusPaneNext,
        Self::FocusPanePrev,
        Self::FocusPaneLeft,
        Self::FocusPaneRight,
        Self::FocusPaneUp,
        Self::FocusPaneDown,
        Self::SwitchTab1,
        Self::SwitchTab2,
        Self::SwitchTab3,
        Self::SwitchTab4,
        Self::SwitchTab5,
        Self::SwitchTab6,
        Self::SwitchTab7,
        Self::SwitchTab8,
        Self::SwitchTab9,
        Self::NewWindow,
        Self::ToggleQuake,
        Self::ZmodemSend,
    ];

    /// Human-readable action title shown in Command Palette and Keybindings Settings.
    pub const fn label(self) -> &'static str {
        match self {
            Self::NewTab => "New Tab",
            Self::DuplicateTab => "Duplicate Active Tab",
            Self::RenameTab => "Rename Active Tab",
            Self::CloseFocusedPane => "Close Focused Pane",
            Self::SplitVertical => "Split Pane Vertically",
            Self::SplitHorizontal => "Split Pane Horizontally",
            Self::TogglePaneZoom => "Toggle Pane Zoom",
            Self::ToggleBroadcastInput => "Toggle Broadcast Input",
            Self::ToggleSidebar => "Toggle Profiles Sidebar",
            Self::ToggleSftpDrawer => "Toggle SFTP Drawer",
            Self::ToggleCommandPalette => "Toggle Command Palette",
            Self::OpenQuickConnect => "Quick Connect SSH",
            Self::OpenSettings => "Open Settings",
            Self::OpenNewProfile => "Create New Profile",
            Self::OpenQuickCommands => "Open Quick Commands",
            Self::OpenWorkspaces => "Open Named Workspaces",
            Self::OpenSearch => "Search Terminal Scrollback",
            Self::Copy => "Copy Selection",
            Self::Paste => "Paste Clipboard",
            Self::ClearTerminal => "Clear Terminal & Scrollback",
            Self::SaveScrollback => "Save Terminal Output",
            Self::ToggleSessionLogging => "Toggle Session Logging",
            Self::FontZoomIn => "Zoom In Font",
            Self::FontZoomOut => "Zoom Out Font",
            Self::FontZoomReset => "Reset Font Size",
            Self::CycleTabNext => "Next Tab",
            Self::CycleTabPrev => "Previous Tab",
            Self::MoveTabLeft => "Move Tab Left",
            Self::MoveTabRight => "Move Tab Right",
            Self::FocusPaneNext => "Next Split Pane",
            Self::FocusPanePrev => "Previous Split Pane",
            Self::FocusPaneLeft => "Focus Pane Left",
            Self::FocusPaneRight => "Focus Pane Right",
            Self::FocusPaneUp => "Focus Pane Up",
            Self::FocusPaneDown => "Focus Pane Down",
            Self::SwitchTab1 => "Switch to Tab 1",
            Self::SwitchTab2 => "Switch to Tab 2",
            Self::SwitchTab3 => "Switch to Tab 3",
            Self::SwitchTab4 => "Switch to Tab 4",
            Self::SwitchTab5 => "Switch to Tab 5",
            Self::SwitchTab6 => "Switch to Tab 6",
            Self::SwitchTab7 => "Switch to Tab 7",
            Self::SwitchTab8 => "Switch to Tab 8",
            Self::SwitchTab9 => "Switch to Tab 9",
            Self::NewWindow => "New Window",
            Self::ToggleQuake => "Toggle Quake Window",
            Self::ZmodemSend => "Zmodem Send File",
        }
    }

    /// Grouping category shown in the Keybindings Settings tab.
    pub const fn category(self) -> &'static str {
        match self {
            Self::NewTab
            | Self::DuplicateTab
            | Self::RenameTab
            | Self::CycleTabNext
            | Self::CycleTabPrev
            | Self::MoveTabLeft
            | Self::MoveTabRight
            | Self::SwitchTab1
            | Self::SwitchTab2
            | Self::SwitchTab3
            | Self::SwitchTab4
            | Self::SwitchTab5
            | Self::SwitchTab6
            | Self::SwitchTab7
            | Self::SwitchTab8
            | Self::SwitchTab9 => "Tabs",
            Self::CloseFocusedPane
            | Self::SplitVertical
            | Self::SplitHorizontal
            | Self::TogglePaneZoom
            | Self::ToggleBroadcastInput
            | Self::FocusPaneNext
            | Self::FocusPanePrev
            | Self::FocusPaneLeft
            | Self::FocusPaneRight
            | Self::FocusPaneUp
            | Self::FocusPaneDown => "Panes",
            Self::OpenSearch
            | Self::Copy
            | Self::Paste
            | Self::ClearTerminal
            | Self::SaveScrollback
            | Self::ToggleSessionLogging
            | Self::FontZoomIn
            | Self::FontZoomOut
            | Self::FontZoomReset => "Terminal",
            Self::ToggleSidebar
            | Self::ToggleSftpDrawer
            | Self::ToggleCommandPalette
            | Self::OpenQuickConnect
            | Self::OpenSettings
            | Self::OpenNewProfile
            | Self::OpenQuickCommands
            | Self::OpenWorkspaces
            | Self::NewWindow
            | Self::ToggleQuake
            | Self::ZmodemSend => "Workspace",
        }
    }

    /// Default shortcut sequences bound to this action.
    pub fn default_bindings(self) -> Vec<&'static str> {
        match self {
            Self::NewTab => vec!["Ctrl+Shift+T"],
            Self::DuplicateTab => vec!["Ctrl+Shift+N"],
            Self::RenameTab => vec!["Ctrl+Shift+R"],
            Self::CloseFocusedPane => vec!["Ctrl+Shift+W"],
            Self::SplitVertical => vec!["Ctrl+Shift+D", "Ctrl+Shift+Backslash"],
            Self::SplitHorizontal => vec!["Ctrl+Shift+O", "Ctrl+Shift+Minus"],
            Self::TogglePaneZoom => vec!["Ctrl+Shift+Z"],
            Self::ToggleBroadcastInput => vec!["Ctrl+Shift+B"],
            Self::ToggleSidebar => vec![],
            Self::ToggleSftpDrawer => vec!["Ctrl+Shift+E"],
            Self::ToggleCommandPalette => vec!["Ctrl+K", "Ctrl+Shift+P"],
            Self::OpenQuickConnect => vec!["Ctrl+Shift+G"],
            Self::OpenSettings => vec!["Ctrl+Comma"],
            Self::OpenNewProfile => vec![],
            Self::OpenQuickCommands => vec!["Ctrl+Shift+Q"],
            Self::OpenWorkspaces => vec!["Ctrl+Shift+M"],
            Self::OpenSearch => vec!["Ctrl+F"],
            Self::Copy => vec!["Ctrl+Shift+C"],
            Self::Paste => vec!["Ctrl+Shift+V"],
            Self::ClearTerminal => vec!["Ctrl+Shift+L"],
            Self::SaveScrollback => vec![],
            Self::ToggleSessionLogging => vec![],
            Self::FontZoomIn => vec!["Ctrl+Equal", "Ctrl+Shift+Equal"],
            Self::FontZoomOut => vec!["Ctrl+Minus"],
            Self::FontZoomReset => vec!["Ctrl+0"],
            Self::CycleTabNext => vec!["Ctrl+Tab"],
            Self::CycleTabPrev => vec!["Ctrl+Shift+Tab"],
            Self::MoveTabLeft => vec!["Ctrl+Shift+PageUp"],
            Self::MoveTabRight => vec!["Ctrl+Shift+PageDown"],
            Self::FocusPaneNext => vec!["Ctrl+Shift+BracketRight"],
            Self::FocusPanePrev => vec!["Ctrl+Shift+BracketLeft"],
            Self::FocusPaneLeft => vec!["Ctrl+Alt+Left"],
            Self::FocusPaneRight => vec!["Ctrl+Alt+Right"],
            Self::FocusPaneUp => vec!["Ctrl+Alt+Up"],
            Self::FocusPaneDown => vec!["Ctrl+Alt+Down"],
            Self::SwitchTab1 => vec!["Ctrl+1"],
            Self::SwitchTab2 => vec!["Ctrl+2"],
            Self::SwitchTab3 => vec!["Ctrl+3"],
            Self::SwitchTab4 => vec!["Ctrl+4"],
            Self::SwitchTab5 => vec!["Ctrl+5"],
            Self::SwitchTab6 => vec!["Ctrl+6"],
            Self::SwitchTab7 => vec!["Ctrl+7"],
            Self::SwitchTab8 => vec!["Ctrl+8"],
            Self::SwitchTab9 => vec!["Ctrl+9"],
            Self::NewWindow => vec![],
            Self::ToggleQuake => vec!["Ctrl+Alt+Space"],
            Self::ZmodemSend => vec![],
        }
    }
}

/// Normalized single key press chord (modifiers + normalized key token).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct KeyChord {
    pub ctrl: bool,
    pub shift: bool,
    pub alt: bool,
    pub super_key: bool,
    pub key: String,
}

impl KeyChord {
    /// Parses a chord string such as `"Ctrl+Shift+T"` or `"Ctrl+Alt+Left"`.
    pub fn parse(raw: &str) -> Option<Self> {
        let mut ctrl = false;
        let mut shift = false;
        let mut alt = false;
        let mut super_key = false;
        let mut key: Option<String> = None;

        for token in raw.split('+').map(str::trim).filter(|t| !t.is_empty()) {
            match token.to_ascii_lowercase().as_str() {
                "ctrl" | "control" => ctrl = true,
                "shift" => shift = true,
                "alt" | "option" => alt = true,
                "super" | "meta" | "cmd" | "win" | "logo" => super_key = true,
                other => {
                    if key.is_some() {
                        return None;
                    }
                    key = Some(normalize_key_token(other)?);
                }
            }
        }

        Some(Self {
            ctrl,
            shift,
            alt,
            super_key,
            key: key?,
        })
    }

    /// Formats this chord into canonical display syntax (e.g. `"Ctrl+Shift+T"`).
    pub fn canonical(&self) -> String {
        let mut parts = Vec::with_capacity(5);
        if self.ctrl {
            parts.push("Ctrl");
        }
        if self.shift {
            parts.push("Shift");
        }
        if self.alt {
            parts.push("Alt");
        }
        if self.super_key {
            parts.push("Super");
        }
        parts.push(&self.key);
        parts.join("+")
    }
}

impl fmt::Display for KeyChord {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.canonical())
    }
}

/// Single-stroke or multi-chord shortcut sequence (e.g. `"Ctrl+K Ctrl+S"`).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct KeySequence(pub Vec<KeyChord>);

impl KeySequence {
    /// Parses whitespace-separated chords into a [`KeySequence`].
    pub fn parse(raw: &str) -> Option<Self> {
        let chords: Option<Vec<KeyChord>> = raw
            .split_whitespace()
            .map(KeyChord::parse)
            .collect();
        let chords = chords?;
        (!chords.is_empty()).then_some(Self(chords))
    }

    /// Formats this sequence into canonical string form.
    pub fn canonical(&self) -> String {
        self.0
            .iter()
            .map(KeyChord::canonical)
            .collect::<Vec<_>>()
            .join(" ")
    }
}

/// Persisted keybinding overrides stored in `config.toml`.
///
/// Missing entries inherit [`KeyAction::default_bindings`]; an explicitly empty list unbinds the action.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct KeybindingConfig {
    pub bindings: BTreeMap<KeyAction, Vec<String>>,
}

impl KeybindingConfig {
    /// Returns the effective normalized binding strings for `action`.
    pub fn bindings_for(&self, action: KeyAction) -> Vec<String> {
        if let Some(custom) = self.bindings.get(&action) {
            custom
                .iter()
                .filter_map(|s| KeySequence::parse(s).map(|seq| seq.canonical()))
                .collect()
        } else {
            action
                .default_bindings()
                .into_iter()
                .map(ToString::to_string)
                .collect()
        }
    }

    /// Sets custom bindings for `action` (validating each non-empty chord sequence).
    pub fn set_bindings(&mut self, action: KeyAction, raw_bindings: Vec<String>) -> Result<(), String> {
        let mut normalized = Vec::with_capacity(raw_bindings.len());
        for raw in raw_bindings {
            let trimmed = raw.trim();
            if trimmed.is_empty() {
                continue;
            }
            let seq = KeySequence::parse(trimmed)
                .ok_or_else(|| format!("Invalid keybinding sequence '{trimmed}'"))?;
            normalized.push(seq.canonical());
        }
        self.bindings.insert(action, normalized);
        Ok(())
    }

    /// Resets `action` back to its built-in default bindings.
    pub fn reset_action(&mut self, action: KeyAction) {
        self.bindings.remove(&action);
    }

    /// Builds a [`KeybindingResolver`] merging built-in defaults and overrides.
    pub fn resolver(&self) -> KeybindingResolver {
        let mut exact: BTreeMap<Vec<KeyChord>, KeyAction> = BTreeMap::new();
        for &action in KeyAction::ALL {
            for binding in self.bindings_for(action) {
                if let Some(seq) = KeySequence::parse(&binding) {
                    exact.entry(seq.0).or_insert(action);
                }
            }
        }
        KeybindingResolver { exact }
    }
}

/// Result of advancing a chord state machine with a single [`KeyChord`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChordMatch {
    /// Complete shortcut sequence matched `action`.
    Matched(KeyAction),
    /// Chord is a valid prefix of a multi-chord shortcut; wait for next chord.
    PendingPrefix,
    /// No shortcut matches this chord.
    NoMatch,
}

/// Fast lookup table supporting single-key and multi-chord shortcuts.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct KeybindingResolver {
    exact: BTreeMap<Vec<KeyChord>, KeyAction>,
}

impl KeybindingResolver {
    /// Resolves `pending_prefix + next` against configured bindings.
    pub fn resolve_step(&self, pending_prefix: &[KeyChord], next: &KeyChord) -> ChordMatch {
        let mut seq = Vec::with_capacity(pending_prefix.len() + 1);
        seq.extend_from_slice(pending_prefix);
        seq.push(next.clone());

        if self
            .exact
            .keys()
            .any(|k| k.len() > seq.len() && k.starts_with(&seq))
        {
            return ChordMatch::PendingPrefix;
        }
        if let Some(&action) = self.exact.get(&seq) {
            return ChordMatch::Matched(action);
        }
        if !pending_prefix.is_empty() {
            let single = [next.clone()];
            if self
                .exact
                .keys()
                .any(|k| k.len() > 1 && k.starts_with(&single[..]))
            {
                return ChordMatch::PendingPrefix;
            }
            if let Some(&action) = self.exact.get(&single[..]) {
                return ChordMatch::Matched(action);
            }
        }
        ChordMatch::NoMatch
    }

    /// Returns the first display shortcut string bound to `action`, if any.
    pub fn primary_shortcut(&self, action: KeyAction) -> Option<String> {
        self.exact
            .iter()
            .find_map(|(seq, &act)| (act == action).then(|| KeySequence(seq.clone()).canonical()))
    }
}

fn normalize_key_token(token: &str) -> Option<String> {
    let lower = token.trim().to_ascii_lowercase();
    let canonical = match lower.as_str() {
        "" => return None,
        "tab" => "Tab".to_string(),
        "enter" | "return" => "Enter".to_string(),
        "esc" | "escape" => "Escape".to_string(),
        "space" | " " => "Space".to_string(),
        "backspace" | "bksp" => "Backspace".to_string(),
        "delete" | "del" => "Delete".to_string(),
        "insert" | "ins" => "Insert".to_string(),
        "home" => "Home".to_string(),
        "end" => "End".to_string(),
        "pageup" | "pgup" => "PageUp".to_string(),
        "pagedown" | "pgdn" => "PageDown".to_string(),
        "left" | "arrowleft" => "Left".to_string(),
        "right" | "arrowright" => "Right".to_string(),
        "up" | "arrowup" => "Up".to_string(),
        "down" | "arrowdown" => "Down".to_string(),
        "," | "comma" => "Comma".to_string(),
        "." | "period" => "Period".to_string(),
        "=" | "+" | "equal" | "plus" => "Equal".to_string(),
        "-" | "_" | "minus" => "Minus".to_string(),
        "\\" | "|" | "backslash" => "Backslash".to_string(),
        "/" | "slash" => "Slash".to_string(),
        "[" | "{" | "bracketleft" => "BracketLeft".to_string(),
        "]" | "}" | "bracketright" => "BracketRight".to_string(),
        "f1" | "f2" | "f3" | "f4" | "f5" | "f6" | "f7" | "f8" | "f9" | "f10" | "f11" | "f12" => {
            lower.to_ascii_uppercase()
        }
        other if other.chars().count() == 1 => {
            let ch = other.chars().next()?;
            if ch.is_ascii_alphabetic() {
                ch.to_ascii_uppercase().to_string()
            } else {
                ch.to_string()
            }
        }
        _ => return None,
    };
    Some(canonical)
}
