//! Bridges `iced::keyboard` events to `termewnal_core::KeyChord` / `KeybindingResolver`
//! and translates matched [`KeyAction`]s into workspace [`Message`]s.

use iced::keyboard::{Key, Modifiers, key::Named};
use termewnal_core::{ChordMatch, KeyAction, KeyChord, KeybindingResolver};

use crate::app::{Message, SessionId};
use crate::workspace::{CycleDirection, FocusDirection, SplitAxis, TabId};

/// Converts an `iced` key press into a canonical [`KeyChord`], ignoring bare modifier keys.
pub fn iced_key_to_chord(
    key: &Key,
    modified_key: Option<&Key>,
    modifiers: Modifiers,
) -> Option<KeyChord> {
    let token = iced_key_token(key).or_else(|| modified_key.and_then(iced_key_token))?;
    Some(KeyChord {
        ctrl: modifiers.control(),
        shift: modifiers.shift(),
        alt: modifiers.alt(),
        super_key: modifiers.logo(),
        key: token,
    })
}

fn iced_key_token(key: &Key) -> Option<String> {
    match key {
        Key::Named(named) => named_key_token(*named).map(ToString::to_string),
        Key::Character(ch) => char_key_token(ch.as_str()),
        Key::Unidentified => None,
    }
}

fn named_key_token(named: Named) -> Option<&'static str> {
    Some(match named {
        Named::Tab => "Tab",
        Named::Enter => "Enter",
        Named::Escape => "Escape",
        Named::Space => "Space",
        Named::Backspace => "Backspace",
        Named::Delete => "Delete",
        Named::Insert => "Insert",
        Named::Home => "Home",
        Named::End => "End",
        Named::PageUp => "PageUp",
        Named::PageDown => "PageDown",
        Named::ArrowLeft => "Left",
        Named::ArrowRight => "Right",
        Named::ArrowUp => "Up",
        Named::ArrowDown => "Down",
        Named::F1 => "F1",
        Named::F2 => "F2",
        Named::F3 => "F3",
        Named::F4 => "F4",
        Named::F5 => "F5",
        Named::F6 => "F6",
        Named::F7 => "F7",
        Named::F8 => "F8",
        Named::F9 => "F9",
        Named::F10 => "F10",
        Named::F11 => "F11",
        Named::F12 => "F12",
        _ => return None,
    })
}

fn char_key_token(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    let normalized = match trimmed {
        "," => "Comma".to_string(),
        "." => "Period".to_string(),
        "=" | "+" => "Equal".to_string(),
        "-" | "_" => "Minus".to_string(),
        "\\" | "|" => "Backslash".to_string(),
        "/" | "?" => "Slash".to_string(),
        "[" | "{" => "BracketLeft".to_string(),
        "]" | "}" => "BracketRight".to_string(),
        s if s.chars().count() == 1 => {
            let ch = s.chars().next()?;
            if ch.is_control() {
                return None;
            }
            if ch.is_ascii_alphabetic() {
                ch.to_ascii_uppercase().to_string()
            } else {
                ch.to_string()
            }
        }
        _ => return None,
    };
    Some(normalized)
}

/// Evaluates `chord` against `resolver` + `pending_prefix` and converts matched action into [`Message`].
pub fn resolve_keybinding_message(
    resolver: &KeybindingResolver,
    pending_prefix: &[KeyChord],
    chord: &KeyChord,
    active_tab: Option<TabId>,
    active_session: Option<SessionId>,
) -> KeybindingStepOutcome {
    match resolver.resolve_step(pending_prefix, chord) {
        ChordMatch::PendingPrefix => KeybindingStepOutcome::PendingPrefix(chord.clone()),
        ChordMatch::Matched(action) => {
            KeybindingStepOutcome::Matched(action_to_message(action, active_tab, active_session))
        }
        ChordMatch::NoMatch => KeybindingStepOutcome::NoMatch,
    }
}

#[derive(Debug, Clone)]
pub enum KeybindingStepOutcome {
    Matched(Option<Message>),
    PendingPrefix(KeyChord),
    NoMatch,
}

/// Converts a resolved [`KeyAction`] into the corresponding Elm [`Message`].
pub fn action_to_message(
    action: KeyAction,
    active_tab: Option<TabId>,
    active_session: Option<SessionId>,
) -> Option<Message> {
    match action {
        KeyAction::NewTab => Some(Message::NewTab),
        KeyAction::DuplicateTab => active_tab.map(Message::DuplicateTab),
        KeyAction::RenameTab => active_tab.map(Message::BeginTabRename),
        KeyAction::CloseFocusedPane => Some(Message::CloseFocusedPane),
        KeyAction::SplitVertical => Some(Message::SplitFocusedPane(SplitAxis::Vertical)),
        KeyAction::SplitHorizontal => Some(Message::SplitFocusedPane(SplitAxis::Horizontal)),
        KeyAction::TogglePaneZoom => Some(Message::TogglePaneZoom),
        KeyAction::ToggleBroadcastInput => Some(Message::ToggleBroadcastInput),
        KeyAction::ToggleSidebar => Some(Message::ToggleSidebar),
        KeyAction::ToggleSftpDrawer => Some(Message::ToggleSftpDrawer),
        KeyAction::ToggleCommandPalette => Some(Message::ToggleCommandPalette),
        KeyAction::OpenQuickConnect => Some(Message::OpenQuickConnectPalette),
        KeyAction::OpenSettings => Some(Message::OpenSettingsModal),
        KeyAction::OpenNewProfile => Some(Message::OpenNewProfileModal),
        KeyAction::OpenQuickCommands => Some(Message::OpenQuickCommandsModal),
        KeyAction::OpenWorkspaces => Some(Message::OpenWorkspacesModal),
        KeyAction::OpenSearch => Some(Message::OpenSearch),
        KeyAction::Copy => active_session.map(|session_id| Message::CopySelection { session_id }),
        KeyAction::Paste => {
            active_session.map(|session_id| Message::PasteFromClipboard { session_id })
        }
        KeyAction::ClearTerminal => active_session.map(Message::ClearTerminalBuffer),
        KeyAction::SaveScrollback => active_session.map(Message::SaveScrollback),
        KeyAction::ToggleSessionLogging => active_session.map(Message::ToggleSessionLogging),
        KeyAction::FontZoomIn => Some(Message::AdjustFontSize(1.0)),
        KeyAction::FontZoomOut => Some(Message::AdjustFontSize(-1.0)),
        KeyAction::FontZoomReset => Some(Message::ResetFontSize),
        KeyAction::CycleTabNext => Some(Message::CycleTab(CycleDirection::Next)),
        KeyAction::CycleTabPrev => Some(Message::CycleTab(CycleDirection::Previous)),
        KeyAction::MoveTabLeft => active_tab.map(|tab_id| Message::MoveTabBy {
            tab_id,
            delta: -1,
        }),
        KeyAction::MoveTabRight => active_tab.map(|tab_id| Message::MoveTabBy { tab_id, delta: 1 }),
        KeyAction::FocusPaneNext => Some(Message::CycleFocus(CycleDirection::Next)),
        KeyAction::FocusPanePrev => Some(Message::CycleFocus(CycleDirection::Previous)),
        KeyAction::FocusPaneLeft => Some(Message::MoveFocus(FocusDirection::Left)),
        KeyAction::FocusPaneRight => Some(Message::MoveFocus(FocusDirection::Right)),
        KeyAction::FocusPaneUp => Some(Message::MoveFocus(FocusDirection::Up)),
        KeyAction::FocusPaneDown => Some(Message::MoveFocus(FocusDirection::Down)),
        KeyAction::SwitchTab1 => Some(Message::SelectTabByIndex(0)),
        KeyAction::SwitchTab2 => Some(Message::SelectTabByIndex(1)),
        KeyAction::SwitchTab3 => Some(Message::SelectTabByIndex(2)),
        KeyAction::SwitchTab4 => Some(Message::SelectTabByIndex(3)),
        KeyAction::SwitchTab5 => Some(Message::SelectTabByIndex(4)),
        KeyAction::SwitchTab6 => Some(Message::SelectTabByIndex(5)),
        KeyAction::SwitchTab7 => Some(Message::SelectTabByIndex(6)),
        KeyAction::SwitchTab8 => Some(Message::SelectTabByIndex(7)),
        KeyAction::SwitchTab9 => Some(Message::SelectTabByIndex(8)),
        KeyAction::NewWindow => Some(Message::NewWindow),
        KeyAction::ToggleQuake => Some(Message::ToggleQuakeWindow),
        KeyAction::ZmodemSend => active_session.map(Message::TriggerZmodemSend),
    }
}
