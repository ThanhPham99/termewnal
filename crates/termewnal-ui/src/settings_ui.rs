//! Tabby-inspired Settings modal (`Ctrl+,`) for live Appearance, Terminal, Keybindings,
//! Output Highlights, SSH/SFTP, and Vault config.

use std::collections::BTreeMap;
use std::path::PathBuf;

use iced::widget::{button, column, container, row, scrollable, space, text, text_input};
use iced::{Alignment, Background, Border, Color, Element, Length, Padding, Theme};
use termewnal_core::{
    AppConfig, BellBehavior, BuiltinTheme, CursorStyle, HighlightRule, KeyAction, KeySequence,
    RgbColor, RightClickAction, SyncProviderKind, TabBarPosition, ThemeOverrides, ThemePalette,
    TransferConflictPolicy, VaultBackend, VaultSession,
};
use termewnal_pty::compile_highlight_rules;

use crate::app::Message;
use crate::ui_theme::{
    ChromePalette, FONT_BODY, FONT_META, FONT_SUBHEAD, PillTone, apple_checkbox, form_line,
    hairline_h, hairline_v, modal_body_height, modal_card_style, modal_overlay, modal_width,
    mono_font, pill_button_style, settings_nav_pill_style, status_dot, styled_input_style,
    tab_bar_button_style, tabby_scrollable_style, ui_font, ui_font_medium,
};

const SETTINGS_MODAL_WIDTH: f32 = 940.0;
const SETTINGS_BODY_HEIGHT: f32 = 600.0;

/// Settings modal category tabs modeled after Tabby's Preferences sidebar.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SettingsTab {
    #[default]
    Appearance,
    Terminal,
    Keybindings,
    Highlights,
    NetworkAndSftp,
    Vault,
    Sync,
}

impl SettingsTab {
    const ALL: &'static [Self] = &[
        Self::Appearance,
        Self::Terminal,
        Self::Keybindings,
        Self::Highlights,
        Self::NetworkAndSftp,
        Self::Vault,
        Self::Sync,
    ];

    const fn label(self) -> &'static str {
        match self {
            Self::Appearance => "Appearance",
            Self::Terminal => "Terminal",
            Self::Keybindings => "Hotkeys",
            Self::Highlights => "Highlights",
            Self::NetworkAndSftp => "SSH & SFTP",
            Self::Vault => "Vault",
            Self::Sync => "Config sync",
        }
    }
}

/// Customizable color role inside [`ThemeOverrides`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThemeOverrideField {
    Background,
    Foreground,
    Cursor,
    SelectionBg,
    UiAccent,
}

/// Interactive draft state for the GUI Settings modal (`Ctrl+,`).
#[derive(Debug, Clone, Default)]
pub struct SettingsUiState {
    open: bool,
    active_tab: SettingsTab,
    draft: AppConfig,
    font_size_text: String,
    opacity_text: String,
    scrollback_text: String,
    log_dir_text: String,
    sftp_download_dir_text: String,
    keepalive_interval_text: String,
    keepalive_missed_text: String,
    auto_lock_text: String,
    override_bg: String,
    override_fg: String,
    override_cursor: String,
    override_selection: String,
    override_accent: String,
    keybinding_filter: String,
    keybinding_drafts: BTreeMap<KeyAction, String>,
    capture_keybinding: Option<KeyAction>,
    highlight_new_name: String,
    highlight_new_pattern: String,
    highlight_new_fg: String,
    highlight_new_bg: String,
    highlight_new_bold: bool,
    highlight_new_case_insensitive: bool,
    sync_local_dir: String,
    sync_webdav_url: String,
    sync_webdav_username: String,
    sync_gist_id: String,
    sync_token_draft: String,
    sync_passphrase_draft: String,
    sync_status_notice: Option<String>,
    error: Option<String>,
    saved_notice: Option<String>,
}

impl SettingsUiState {
    /// Returns `true` when the Settings modal is open.
    pub fn is_open(&self) -> bool {
        self.open
    }

    /// Returns the active category tab.
    pub fn active_tab(&self) -> SettingsTab {
        self.active_tab
    }

    /// Returns the draft [`AppConfig`].
    pub fn draft(&self) -> &AppConfig {
        &self.draft
    }

    /// Returns the [`KeyAction`] currently waiting for interactive keystroke capture, if any.
    pub fn capturing_action(&self) -> Option<KeyAction> {
        self.capture_keybinding
    }

    /// Opens the modal initialized from `config`.
    pub fn open_with(&mut self, config: &AppConfig) {
        self.open = true;
        self.capture_keybinding = None;
        self.error = None;
        self.saved_notice = None;
        self.sync_from_config(config);
    }

    /// Closes the modal without discarding external config changes.
    pub fn close(&mut self) {
        self.open = false;
        self.capture_keybinding = None;
        self.error = None;
        self.saved_notice = None;
    }

    /// Refreshes draft fields from `config`.
    pub fn sync_from_config(&mut self, config: &AppConfig) {
        self.draft = config.clone();
        self.font_size_text = format!("{:.0}", config.terminal.font_size);
        self.opacity_text = format!("{:.2}", config.terminal.window_opacity);
        self.scrollback_text = config.terminal.scrollback_lines.to_string();
        self.log_dir_text = config
            .terminal
            .log_directory
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_default();
        self.sftp_download_dir_text = config
            .terminal
            .sftp_download_dir
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_default();
        self.keepalive_interval_text = config.keepalive_interval_secs.to_string();
        self.keepalive_missed_text = config.keepalive_max_missed.to_string();
        self.auto_lock_text = config.vault.auto_lock_minutes.to_string();
        self.override_bg = config
            .terminal
            .theme_overrides
            .background
            .clone()
            .unwrap_or_default();
        self.override_fg = config
            .terminal
            .theme_overrides
            .foreground
            .clone()
            .unwrap_or_default();
        self.override_cursor = config
            .terminal
            .theme_overrides
            .cursor
            .clone()
            .unwrap_or_default();
        self.override_selection = config
            .terminal
            .theme_overrides
            .selection_bg
            .clone()
            .unwrap_or_default();
        self.override_accent = config
            .terminal
            .theme_overrides
            .ui_accent
            .clone()
            .unwrap_or_default();
        self.sync_local_dir = config
            .sync
            .local_dir
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_default();
        self.sync_webdav_url = config.sync.webdav_url.clone().unwrap_or_default();
        self.sync_webdav_username = config.sync.webdav_username.clone().unwrap_or_default();
        self.sync_gist_id = config.sync.gist_id.clone().unwrap_or_default();
        self.sync_keybinding_drafts();
    }

    fn sync_keybinding_drafts(&mut self) {
        self.keybinding_drafts.clear();
        for &action in KeyAction::ALL {
            let joined = self.draft.keybindings.bindings_for(action).join(", ");
            self.keybinding_drafts.insert(action, joined);
        }
    }

    pub fn set_tab(&mut self, tab: SettingsTab) {
        self.active_tab = tab;
        self.capture_keybinding = None;
        self.error = None;
    }

    pub fn set_theme(&mut self, theme: BuiltinTheme) {
        self.draft.terminal.theme = theme.name().to_string();
        self.saved_notice = None;
    }

    pub fn set_font_family(&mut self, family: String) {
        self.draft.terminal.font_family = family;
        self.saved_notice = None;
    }

    pub fn set_font_size_text(&mut self, value: String) {
        if let Ok(parsed) = value.trim().parse::<f32>()
            && parsed.is_finite()
            && (8.0..=36.0).contains(&parsed)
        {
            self.draft.terminal.font_size = parsed;
        }
        self.font_size_text = value;
        self.error = None;
        self.saved_notice = None;
    }

    pub fn step_font_size(&mut self, delta: f32) -> f32 {
        let current = self
            .font_size_text
            .trim()
            .parse::<f32>()
            .unwrap_or(self.draft.terminal.font_size);
        let next = (current + delta).clamp(8.0, 36.0);
        self.draft.terminal.font_size = next;
        self.font_size_text = format!("{next:.0}");
        self.saved_notice = None;
        next
    }

    pub fn set_opacity_text(&mut self, value: String) {
        if let Ok(parsed) = value.trim().parse::<f32>()
            && parsed.is_finite()
            && (0.3..=1.0).contains(&parsed)
        {
            self.draft.terminal.window_opacity = parsed;
        }
        self.opacity_text = value;
        self.error = None;
        self.saved_notice = None;
    }

    pub fn step_opacity(&mut self, delta: f32) -> f32 {
        let current = self
            .opacity_text
            .trim()
            .parse::<f32>()
            .unwrap_or(self.draft.terminal.window_opacity);
        let next = ((current + delta) * 100.0).round() / 100.0;
        let clamped = next.clamp(0.3, 1.0);
        self.draft.terminal.window_opacity = clamped;
        self.opacity_text = format!("{clamped:.2}");
        self.saved_notice = None;
        clamped
    }

    pub fn set_font_ligatures(&mut self, enabled: bool) {
        self.draft.terminal.font_ligatures = enabled;
        self.saved_notice = None;
    }

    pub fn set_cursor_style(&mut self, style: CursorStyle) {
        self.draft.terminal.cursor_style = style;
        self.saved_notice = None;
    }

    pub fn set_cursor_blink(&mut self, enabled: bool) {
        self.draft.terminal.cursor_blink = enabled;
        self.saved_notice = None;
    }

    pub fn set_tab_bar_position(&mut self, pos: TabBarPosition) {
        self.draft.terminal.tab_bar_position = pos;
        self.saved_notice = None;
    }

    pub fn set_show_profile_sidebar(&mut self, enabled: bool) {
        self.draft.terminal.show_profile_sidebar = enabled;
        self.saved_notice = None;
    }

    pub fn set_show_status_bar(&mut self, enabled: bool) {
        self.draft.terminal.show_status_bar = enabled;
        self.saved_notice = None;
    }

    pub fn set_show_server_stats(&mut self, enabled: bool) {
        self.draft.terminal.show_server_stats = enabled;
        self.saved_notice = None;
    }

    pub fn set_hide_tab_index(&mut self, enabled: bool) {
        self.draft.terminal.hide_tab_index = enabled;
        self.saved_notice = None;
    }

    pub fn set_flex_tabs(&mut self, enabled: bool) {
        self.draft.terminal.flex_tabs = enabled;
        self.saved_notice = None;
    }

    pub fn set_tray_enabled(&mut self, enabled: bool) {
        self.draft.tray.enabled = enabled;
        self.saved_notice = None;
    }

    pub fn set_minimize_to_tray(&mut self, enabled: bool) {
        self.draft.tray.minimize_to_tray = enabled;
        self.saved_notice = None;
    }

    pub fn set_theme_override(&mut self, field: ThemeOverrideField, value: String) {
        let opt = (!value.trim().is_empty())
            .then(|| value.trim().to_string())
            .filter(|s| RgbColor::from_hex(s).is_some());
        match field {
            ThemeOverrideField::Background => {
                self.override_bg = value;
                if self.override_bg.trim().is_empty() || opt.is_some() {
                    self.draft.terminal.theme_overrides.background = opt;
                }
            }
            ThemeOverrideField::Foreground => {
                self.override_fg = value;
                if self.override_fg.trim().is_empty() || opt.is_some() {
                    self.draft.terminal.theme_overrides.foreground = opt;
                }
            }
            ThemeOverrideField::Cursor => {
                self.override_cursor = value;
                if self.override_cursor.trim().is_empty() || opt.is_some() {
                    self.draft.terminal.theme_overrides.cursor = opt;
                }
            }
            ThemeOverrideField::SelectionBg => {
                self.override_selection = value;
                if self.override_selection.trim().is_empty() || opt.is_some() {
                    self.draft.terminal.theme_overrides.selection_bg = opt;
                }
            }
            ThemeOverrideField::UiAccent => {
                self.override_accent = value;
                if self.override_accent.trim().is_empty() || opt.is_some() {
                    self.draft.terminal.theme_overrides.ui_accent = opt;
                }
            }
        }
        self.error = None;
        self.saved_notice = None;
    }

    pub fn set_scrollback_text(&mut self, value: String) {
        if let Ok(parsed) = value.trim().parse::<usize>()
            && parsed >= 100
        {
            self.draft.terminal.scrollback_lines = parsed;
        }
        self.scrollback_text = value;
        self.error = None;
        self.saved_notice = None;
    }

    pub fn set_copy_on_select(&mut self, enabled: bool) {
        self.draft.terminal.copy_on_select = enabled;
        self.saved_notice = None;
    }

    pub fn set_confirm_multiline_paste(&mut self, enabled: bool) {
        self.draft.terminal.confirm_multiline_paste = enabled;
        self.saved_notice = None;
    }

    pub fn set_right_click_action(&mut self, action: RightClickAction) {
        self.draft.terminal.right_click_action = action;
        self.saved_notice = None;
    }

    pub fn set_middle_click_paste(&mut self, enabled: bool) {
        self.draft.terminal.middle_click_paste = enabled;
        self.saved_notice = None;
    }

    pub fn set_bell_behavior(&mut self, behavior: BellBehavior) {
        self.draft.terminal.bell_behavior = behavior;
        self.saved_notice = None;
    }

    pub fn set_alt_is_meta(&mut self, enabled: bool) {
        self.draft.terminal.alt_is_meta = enabled;
        self.saved_notice = None;
    }

    pub fn set_word_separators(&mut self, value: String) {
        self.draft.terminal.word_separators = value;
        self.saved_notice = None;
    }

    pub fn set_auto_log_sessions(&mut self, enabled: bool) {
        self.draft.terminal.auto_log_sessions = enabled;
        self.saved_notice = None;
    }

    pub fn set_log_directory_text(&mut self, value: String) {
        let trimmed = value.trim().to_string();
        self.draft.terminal.log_directory = (!trimmed.is_empty()).then(|| PathBuf::from(trimmed));
        self.log_dir_text = value;
        self.saved_notice = None;
    }

    pub fn set_shell_text(&mut self, value: String) {
        let trimmed = value.trim().to_string();
        self.draft.terminal.shell = (!trimmed.is_empty()).then_some(trimmed);
        self.saved_notice = None;
    }

    pub fn set_keybinding_filter(&mut self, filter: String) {
        self.keybinding_filter = filter;
    }

    pub fn set_keybinding_text(&mut self, action: KeyAction, value: String) {
        let entries: Vec<String> = value
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(ToString::to_string)
            .collect();
        self.keybinding_drafts.insert(action, value);
        if self.draft.keybindings.set_bindings(action, entries).is_ok() {
            self.error = None;
        }
        self.saved_notice = None;
    }

    pub fn begin_shortcut_capture(&mut self, action: KeyAction) {
        self.capture_keybinding = Some(action);
        self.error = None;
    }

    pub fn apply_captured_shortcut(&mut self, chord: String) -> bool {
        let Some(action) = self.capture_keybinding.take() else {
            return false;
        };
        self.keybinding_drafts.insert(action, chord.clone());
        let _ = self.draft.keybindings.set_bindings(action, vec![chord]);
        self.error = None;
        self.saved_notice = None;
        true
    }

    pub fn cancel_shortcut_capture(&mut self) {
        self.capture_keybinding = None;
    }

    pub fn reset_keybinding(&mut self, action: KeyAction) {
        self.capture_keybinding = None;
        self.draft.keybindings.reset_action(action);
        let joined = self.draft.keybindings.bindings_for(action).join(", ");
        self.keybinding_drafts.insert(action, joined);
        self.error = None;
        self.saved_notice = None;
    }

    pub fn reset_all_keybindings(&mut self) {
        self.capture_keybinding = None;
        self.draft.keybindings.bindings.clear();
        self.sync_keybinding_drafts();
        self.error = None;
        self.saved_notice = None;
    }

    pub fn set_highlight_name(&mut self, name: String) {
        self.highlight_new_name = name;
        self.error = None;
    }

    pub fn set_highlight_pattern(&mut self, pattern: String) {
        self.highlight_new_pattern = pattern;
        self.error = None;
    }

    pub fn set_highlight_fg(&mut self, fg: String) {
        self.highlight_new_fg = fg;
        self.error = None;
    }

    pub fn set_highlight_bg(&mut self, bg: String) {
        self.highlight_new_bg = bg;
        self.error = None;
    }

    pub fn set_highlight_bold(&mut self, bold: bool) {
        self.highlight_new_bold = bold;
    }

    pub fn set_highlight_case(&mut self, case_insensitive: bool) {
        self.highlight_new_case_insensitive = case_insensitive;
    }

    pub fn add_highlight_rule(&mut self) -> Result<(), String> {
        let pattern = self.highlight_new_pattern.trim().to_string();
        if pattern.is_empty() {
            let msg = "Highlight regex pattern cannot be empty".to_string();
            self.error = Some(msg.clone());
            return Err(msg);
        }
        let fg_raw = self.highlight_new_fg.trim();
        let foreground = if fg_raw.is_empty() {
            None
        } else {
            let parsed = RgbColor::from_hex(fg_raw)
                .ok_or_else(|| format!("Invalid foreground hex color '{fg_raw}'"))?;
            Some(parsed.to_hex())
        };
        let bg_raw = self.highlight_new_bg.trim();
        let background = if bg_raw.is_empty() {
            None
        } else {
            let parsed = RgbColor::from_hex(bg_raw)
                .ok_or_else(|| format!("Invalid background hex color '{bg_raw}'"))?;
            Some(parsed.to_hex())
        };
        if foreground.is_none() && background.is_none() && !self.highlight_new_bold {
            let msg = "Specify at least a foreground color, background color, or bold style"
                .to_string();
            self.error = Some(msg.clone());
            return Err(msg);
        }

        let name = {
            let trimmed = self.highlight_new_name.trim();
            if trimmed.is_empty() {
                pattern.clone()
            } else {
                trimmed.to_string()
            }
        };
        let idx = self.draft.terminal.highlight_rules.len() + 1;
        let candidate = HighlightRule {
            id: format!("hl-{idx}-{}", pattern.len()),
            name,
            pattern,
            foreground,
            background,
            bold: self.highlight_new_bold,
            enabled: true,
            case_insensitive: self.highlight_new_case_insensitive,
        };
        if compile_highlight_rules(std::slice::from_ref(&candidate)).is_empty() {
            let msg = format!("Invalid regex pattern '{}'", candidate.pattern);
            self.error = Some(msg.clone());
            return Err(msg);
        }

        self.draft.terminal.highlight_rules.push(candidate);
        self.highlight_new_name.clear();
        self.highlight_new_pattern.clear();
        self.highlight_new_fg.clear();
        self.highlight_new_bg.clear();
        self.highlight_new_bold = false;
        self.highlight_new_case_insensitive = false;
        self.error = None;
        self.saved_notice = None;
        Ok(())
    }

    pub fn toggle_highlight_rule(&mut self, id: &str) {
        if let Some(rule) = self
            .draft
            .terminal
            .highlight_rules
            .iter_mut()
            .find(|r| r.id == id)
        {
            rule.enabled = !rule.enabled;
            self.saved_notice = None;
        }
    }

    pub fn delete_highlight_rule(&mut self, id: &str) {
        self.draft.terminal.highlight_rules.retain(|r| r.id != id);
        self.saved_notice = None;
    }

    pub fn set_keepalive_interval_text(&mut self, value: String) {
        self.keepalive_interval_text = value;
        self.error = None;
        self.saved_notice = None;
    }

    pub fn set_keepalive_missed_text(&mut self, value: String) {
        self.keepalive_missed_text = value;
        self.error = None;
        self.saved_notice = None;
    }

    pub fn set_conflict_policy(&mut self, policy: TransferConflictPolicy) {
        self.draft.transfer.default_conflict_policy = policy;
        self.saved_notice = None;
    }

    pub fn set_follow_symlinks(&mut self, enabled: bool) {
        self.draft.transfer.follow_symlinks = enabled;
        self.saved_notice = None;
    }

    pub fn set_auto_open_sftp(&mut self, enabled: bool) {
        self.draft.terminal.auto_open_sftp = enabled;
        self.saved_notice = None;
    }

    pub fn set_sftp_download_dir_text(&mut self, value: String) {
        let trimmed = value.trim().to_string();
        self.draft.terminal.sftp_download_dir = (!trimmed.is_empty()).then(|| PathBuf::from(trimmed));
        self.sftp_download_dir_text = value;
        self.error = None;
        self.saved_notice = None;
    }

    pub fn set_vault_backend(&mut self, backend: VaultBackend) {
        self.draft.vault.backend = backend;
        self.saved_notice = None;
    }

    pub fn set_auto_lock_text(&mut self, value: String) {
        self.auto_lock_text = value;
        self.error = None;
        self.saved_notice = None;
    }

    pub fn set_sync_provider(&mut self, provider: SyncProviderKind) {
        self.draft.sync.provider = provider;
        self.sync_status_notice = None;
        self.error = None;
    }

    pub fn set_sync_local_dir(&mut self, value: String) {
        let trimmed = value.trim().to_string();
        self.draft.sync.local_dir = (!trimmed.is_empty()).then(|| PathBuf::from(trimmed));
        self.sync_local_dir = value;
        self.error = None;
    }

    pub fn set_sync_webdav_url(&mut self, value: String) {
        let trimmed = value.trim().to_string();
        self.draft.sync.webdav_url = (!trimmed.is_empty()).then_some(trimmed);
        self.sync_webdav_url = value;
        self.error = None;
    }

    pub fn set_sync_webdav_username(&mut self, value: String) {
        let trimmed = value.trim().to_string();
        self.draft.sync.webdav_username = (!trimmed.is_empty()).then_some(trimmed);
        self.sync_webdav_username = value;
        self.error = None;
    }

    pub fn set_sync_gist_id(&mut self, value: String) {
        let trimmed = value.trim().to_string();
        self.draft.sync.gist_id = (!trimmed.is_empty()).then_some(trimmed);
        self.sync_gist_id = value;
        self.error = None;
    }

    pub fn set_sync_token_draft(&mut self, value: String) {
        self.sync_token_draft = value;
        self.error = None;
    }

    pub fn set_sync_passphrase_draft(&mut self, value: String) {
        self.sync_passphrase_draft = value;
        self.error = None;
    }

    pub fn set_sync_encrypt(&mut self, enabled: bool) {
        self.draft.sync.encrypt_bundle = enabled;
        self.error = None;
    }

    pub fn set_sync_auto_save(&mut self, enabled: bool) {
        self.draft.sync.auto_sync_on_save = enabled;
        self.error = None;
    }

    pub fn sync_token_draft(&self) -> &str {
        &self.sync_token_draft
    }

    pub fn sync_passphrase_draft(&self) -> &str {
        &self.sync_passphrase_draft
    }

    pub fn set_sync_status_notice(&mut self, notice: Option<String>) {
        self.sync_status_notice = notice;
    }

    pub fn sync_status_notice(&self) -> Option<&str> {
        self.sync_status_notice.as_deref()
    }

    pub fn set_sync_vault_token_ref(&mut self, value: Option<String>) {
        self.draft.sync.vault_token_ref = value;
    }

    pub fn clear_sync_token_draft(&mut self) {
        self.sync_token_draft.clear();
    }

    /// Returns the active validation or persistence error message, if any.
    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    pub fn set_error(&mut self, err: Option<String>) {
        self.error = err;
    }

    pub fn set_saved_notice(&mut self, notice: Option<String>) {
        self.saved_notice = notice;
    }

    /// Validates numeric, color, and keybinding fields and builds a ready-to-persist [`AppConfig`].
    pub fn build_validated_config(&mut self) -> Result<AppConfig, String> {
        let font_size: f32 = self
            .font_size_text
            .trim()
            .parse()
            .map_err(|_| "Font size must be a number between 8 and 36".to_string())?;
        if !(8.0..=36.0).contains(&font_size) {
            return Err("Font size must be between 8 and 36".to_string());
        }

        let opacity: f32 = self
            .opacity_text
            .trim()
            .parse()
            .map_err(|_| "Window opacity must be a number between 0.30 and 1.00".to_string())?;
        if !(0.3..=1.0).contains(&opacity) {
            return Err("Window opacity must be between 0.30 and 1.00".to_string());
        }

        let scrollback_lines: usize = self
            .scrollback_text
            .trim()
            .parse()
            .map_err(|_| "Scrollback lines must be a positive integer".to_string())?;
        if !(100..=500_000).contains(&scrollback_lines) {
            return Err("Scrollback lines must be between 100 and 500,000".to_string());
        }

        let keepalive_interval_secs: u64 = self
            .keepalive_interval_text
            .trim()
            .parse()
            .map_err(|_| "Keepalive interval must be a non-negative integer (seconds)".to_string())?;

        let keepalive_max_missed: u32 = self
            .keepalive_missed_text
            .trim()
            .parse()
            .map_err(|_| "Max missed probes must be a positive integer".to_string())?;
        if keepalive_max_missed == 0 {
            return Err("Max missed probes must be at least 1".to_string());
        }

        let auto_lock_minutes: u64 = self
            .auto_lock_text
            .trim()
            .parse()
            .map_err(|_| "Vault auto-lock timeout must be a non-negative integer (minutes)".to_string())?;

        let validate_hex = |label: &str, raw: &str| -> Result<Option<String>, String> {
            let trimmed = raw.trim();
            if trimmed.is_empty() {
                return Ok(None);
            }
            let parsed = RgbColor::from_hex(trimmed)
                .ok_or_else(|| format!("Invalid {label} override hex color '{trimmed}'"))?;
            Ok(Some(parsed.to_hex()))
        };
        let theme_overrides = ThemeOverrides {
            background: validate_hex("background", &self.override_bg)?,
            foreground: validate_hex("foreground", &self.override_fg)?,
            cursor: validate_hex("cursor", &self.override_cursor)?,
            selection_bg: validate_hex("selection", &self.override_selection)?,
            ui_accent: validate_hex("accent", &self.override_accent)?,
        };

        let mut keybindings = self.draft.keybindings.clone();
        for (&action, raw) in &self.keybinding_drafts {
            let defaults = action.default_bindings().join(", ");
            let list: Vec<String> = raw
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(ToString::to_string)
                .collect();
            for item in &list {
                if KeySequence::parse(item).is_none() {
                    return Err(format!(
                        "Invalid shortcut '{item}' for {} (use e.g. Ctrl+Shift+T or Ctrl+K Ctrl+S)",
                        action.label()
                    ));
                }
            }
            if raw.trim() == defaults {
                keybindings.reset_action(action);
            } else {
                keybindings.set_bindings(action, list)?;
            }
        }

        let mut cfg = self.draft.clone();
        cfg.terminal.font_size = font_size;
        cfg.terminal.window_opacity = opacity;
        cfg.terminal.theme_overrides = theme_overrides;
        if cfg.terminal.font_family.trim().is_empty() {
            cfg.terminal.font_family = "monospace".to_string();
        }
        cfg.terminal.scrollback_lines = scrollback_lines;
        cfg.keepalive_interval_secs = keepalive_interval_secs;
        cfg.keepalive_max_missed = keepalive_max_missed;
        cfg.vault.auto_lock_minutes = auto_lock_minutes;
        cfg.keybindings = keybindings;
        self.draft = cfg.clone();
        self.error = None;
        Ok(cfg)
    }

    /// Renders the Tabby-styled Settings modal adapted to `window_width` and `window_height`.
    pub fn view_responsive<'a>(
        &'a self,
        palette: &ThemePalette,
        vault: &VaultSession,
        window_width: f32,
        window_height: f32,
    ) -> Option<Element<'a, Message, Theme, iced::Renderer>> {
        if !self.open {
            return None;
        }
        let chrome = ChromePalette::from_palette(palette);
        let card_w = modal_width(SETTINGS_MODAL_WIDTH, window_width);
        let compact_nav = card_w < 680.0;
        let rail_w = 212.0;
        let pane_w = if compact_nav { card_w } else { card_w - rail_w - 1.0 };
        let compact_form = pane_w < 500.0;
        let swatch_cols = if pane_w >= 540.0 {
            3
        } else if pane_w >= 360.0 {
            2
        } else {
            1
        };
        let body_h = modal_body_height(
            SETTINGS_BODY_HEIGHT,
            window_height,
            if compact_nav { 230.0 } else { 140.0 },
        );

        let pane_content = match self.active_tab {
            SettingsTab::Appearance => self.view_appearance(chrome, swatch_cols, compact_form),
            SettingsTab::Terminal => self.view_terminal(chrome, compact_form),
            SettingsTab::Keybindings => self.view_keybindings(chrome, compact_form),
            SettingsTab::Highlights => self.view_highlights(chrome, compact_form),
            SettingsTab::NetworkAndSftp => self.view_network_and_sftp(chrome, compact_form),
            SettingsTab::Vault => self.view_vault(chrome, vault, compact_form),
            SettingsTab::Sync => self.view_sync(chrome, compact_form),
        };

        let scrolled_pane = scrollable(
            container(pane_content)
                .padding(Padding {
                    top: 18.0,
                    right: 20.0,
                    bottom: 20.0,
                    left: 24.0,
                })
                .width(Length::Fill),
        )
        .style(tabby_scrollable_style(chrome))
        .width(Length::Fill)
        .height(if compact_nav {
            Length::Fixed(body_h)
        } else {
            Length::Fill
        });

        let body_section: Element<'a, Message, Theme, iced::Renderer> = if compact_nav {
            let mut hnav = row![].spacing(4).align_y(Alignment::Center);
            for &tab in SettingsTab::ALL {
                let active = self.active_tab == tab;
                hnav = hnav.push(
                    button(
                        text(tab.label())
                            .size(FONT_BODY)
                            .font(if active { ui_font_medium() } else { ui_font() }),
                    )
                    .padding([6, 11])
                    .style(settings_nav_pill_style(chrome, active))
                    .on_press(Message::SettingsSelectTab(tab)),
                );
            }
            let nav_bar = container(
                row![
                    scrollable(hnav)
                        .direction(scrollable::Direction::Horizontal(
                            scrollable::Scrollbar::new().width(2).scroller_width(2),
                        ))
                        .width(Length::Fill),
                    button(text("×").size(15.0))
                        .padding([2, 8])
                        .style(tab_bar_button_style(chrome, false, false))
                        .on_press(Message::CloseSettingsModal),
                ]
                .spacing(6)
                .align_y(Alignment::Center),
            )
            .padding([8, 10])
            .width(Length::Fill)
            .style(move |_| container::Style {
                background: Some(Background::Color(chrome.bg_more)),
                border: Border {
                    radius: iced::border::Radius {
                        top_left: 5.0,
                        top_right: 5.0,
                        bottom_right: 0.0,
                        bottom_left: 0.0,
                    },
                    ..Default::default()
                },
                ..Default::default()
            });

            column![nav_bar, hairline_h(chrome), scrolled_pane]
                .width(Length::Fill)
                .into()
        } else {
            let mut nav_items = column![].spacing(3).width(Length::Fill);
            for &tab in SettingsTab::ALL {
                let active = self.active_tab == tab;
                nav_items = nav_items.push(
                    button(
                        text(tab.label())
                            .size(FONT_BODY)
                            .font(if active { ui_font_medium() } else { ui_font() }),
                    )
                    .padding([7, 12])
                    .width(Length::Fill)
                    .style(settings_nav_pill_style(chrome, active))
                    .on_press(Message::SettingsSelectTab(tab)),
                );
            }

            let rail = container(
                column![
                    row![
                        text("Settings")
                            .size(17.0)
                            .font(ui_font_medium())
                            .color(chrome.text_emphasis),
                        space::horizontal(),
                        button(text("×").size(15.0).font(ui_font()))
                            .padding([2, 7])
                            .style(tab_bar_button_style(chrome, false, false))
                            .on_press(Message::CloseSettingsModal),
                    ]
                    .padding(Padding {
                        top: 4.0,
                        right: 4.0,
                        bottom: 12.0,
                        left: 6.0,
                    })
                    .align_y(Alignment::Center),
                    nav_items,
                    space::vertical(),
                    hairline_h(chrome),
                    row![
                        status_dot(chrome.success),
                        text("config.toml")
                            .size(FONT_META)
                            .font(mono_font())
                            .color(chrome.text_muted),
                        space::horizontal(),
                        text("Live").size(FONT_META).color(chrome.text_secondary),
                    ]
                    .spacing(6)
                    .padding([6, 4])
                    .align_y(Alignment::Center),
                ]
                .spacing(4)
                .height(Length::Fill),
            )
            .padding(Padding {
                top: 14.0,
                right: 12.0,
                bottom: 10.0,
                left: 12.0,
            })
            .width(Length::Fixed(rail_w))
            .height(Length::Fill)
            .style(move |_| container::Style {
                background: Some(Background::Color(chrome.bg_more)),
                border: Border {
                    radius: iced::border::Radius {
                        top_left: 5.0,
                        top_right: 0.0,
                        bottom_right: 0.0,
                        bottom_left: 0.0,
                    },
                    ..Default::default()
                },
                ..Default::default()
            });

            container(
                row![rail, hairline_v(chrome), scrolled_pane]
                    .width(Length::Fill)
                    .height(Length::Fill),
            )
            .width(Length::Fill)
            .height(Length::Fixed(body_h))
            .into()
        };

        let footer_msg: Element<'_, Message, Theme, iced::Renderer> = if let Some(err) = &self.error {
            text(err.as_str())
                .size(FONT_BODY)
                .color(chrome.danger)
                .wrapping(text::Wrapping::None)
                .into()
        } else if let Some(notice) = &self.saved_notice {
            text(notice.as_str())
                .size(FONT_BODY)
                .color(chrome.success)
                .wrapping(text::Wrapping::None)
                .into()
        } else {
            text("~/.config/termewnal/config.toml (Ctrl+,)")
                .size(FONT_META)
                .font(mono_font())
                .color(chrome.text_muted)
                .wrapping(text::Wrapping::None)
                .into()
        };
        let footer_actions = row![
            button(text("Cancel").size(FONT_BODY))
                .padding([6, 14])
                .style(pill_button_style(chrome, PillTone::Subtle))
                .on_press(Message::CloseSettingsModal),
            button(text("Save Settings").size(FONT_BODY).font(ui_font_medium()))
                .padding([6, 16])
                .style(pill_button_style(chrome, PillTone::Primary))
                .on_press(Message::SettingsSave),
        ]
        .spacing(8)
        .align_y(Alignment::Center);

        let footer_row: Element<'_, Message, Theme, iced::Renderer> = if card_w < 540.0 {
            column![
                container(footer_msg).width(Length::Fill).clip(true),
                row![space::horizontal(), footer_actions].align_y(Alignment::Center),
            ]
            .spacing(8)
            .width(Length::Fill)
            .into()
        } else {
            row![
                container(footer_msg).width(Length::Fill).clip(true),
                footer_actions,
            ]
            .spacing(12)
            .width(Length::Fill)
            .align_y(Alignment::Center)
            .into()
        };

        let footer_bar = container(footer_row)
            .width(Length::Fill)
            .padding([12, 16])
            .style(move |_| container::Style {
                background: Some(Background::Color(chrome.bg_more)),
                border: Border {
                    radius: iced::border::Radius {
                        top_left: 0.0,
                        top_right: 0.0,
                        bottom_right: 5.0,
                        bottom_left: 5.0,
                    },
                    ..Default::default()
                },
                ..Default::default()
            });

        let card = container(
            column![body_section, hairline_h(chrome), footer_bar].width(Length::Fill),
        )
        .padding(1)
        .width(Length::Fixed(card_w))
        .style(move |_t| modal_card_style(chrome, true));

        Some(modal_overlay(card.into(), Some(Message::CloseSettingsModal), 24.0))
    }

    fn view_appearance(
        &self,
        chrome: ChromePalette,
        swatch_cols: usize,
        compact_form: bool,
    ) -> Element<'_, Message, Theme, iced::Renderer> {
        let current = BuiltinTheme::resolve(&self.draft.terminal.theme);
        let preview_palette = ThemePalette::for_name_with_overrides(
            &self.draft.terminal.theme,
            &self.draft.terminal.theme_overrides,
        );
        let p_bg = Color::from_rgb8(
            preview_palette.background.r,
            preview_palette.background.g,
            preview_palette.background.b,
        );
        let p_fg = Color::from_rgb8(
            preview_palette.foreground.r,
            preview_palette.foreground.g,
            preview_palette.foreground.b,
        );
        let p_cursor = Color::from_rgb8(
            preview_palette.cursor.r,
            preview_palette.cursor.g,
            preview_palette.cursor.b,
        );
        let p_green = Color::from_rgb8(
            preview_palette.ansi[2].r,
            preview_palette.ansi[2].g,
            preview_palette.ansi[2].b,
        );
        let p_yellow = Color::from_rgb8(
            preview_palette.ansi[3].r,
            preview_palette.ansi[3].g,
            preview_palette.ansi[3].b,
        );
        let p_blue = Color::from_rgb8(
            preview_palette.ansi[4].r,
            preview_palette.ansi[4].g,
            preview_palette.ansi[4].b,
        );
        let p_magenta = Color::from_rgb8(
            preview_palette.ansi[5].r,
            preview_palette.ansi[5].g,
            preview_palette.ansi[5].b,
        );
        let p_cyan = Color::from_rgb8(
            preview_palette.ansi[6].r,
            preview_palette.ansi[6].g,
            preview_palette.ansi[6].b,
        );

        let scheme_preview = container(
            column![
                row![
                    text("john@doe-pc")
                        .size(13.0)
                        .font(mono_font())
                        .color(p_green),
                    text("$ ls")
                        .size(13.0)
                        .font(mono_font())
                        .color(p_fg),
                ],
                row![
                    text("Documents").size(13.0).font(mono_font()).color(p_blue),
                    text("Downloads").size(13.0).font(mono_font()).color(p_cyan),
                    text("Pictures").size(13.0).font(mono_font()).color(p_magenta),
                    text("Music").size(13.0).font(mono_font()).color(p_yellow),
                ]
                .spacing(16)
                .wrap()
                .vertical_spacing(4),
                row![
                    text("john@doe-pc")
                        .size(13.0)
                        .font(mono_font())
                        .color(p_green),
                    text("$ ").size(13.0).font(mono_font()).color(p_fg),
                    text("█").size(13.0).font(mono_font()).color(p_cursor),
                ],
            ]
            .spacing(3),
        )
        .width(Length::Fill)
        .padding([12, 16])
        .style(move |_| container::Style {
            background: Some(Background::Color(p_bg)),
            border: Border {
                color: chrome.secondary,
                width: 1.0,
                radius: 5.0.into(),
            },
            text_color: Some(p_fg),
            ..Default::default()
        });

        let mut swatches_col = column![].spacing(6);
        for chunk in BuiltinTheme::ALL.chunks(swatch_cols.max(1)) {
            let mut r = row![].spacing(6).width(Length::Fill);
            for &theme_opt in chunk {
                let pal = theme_opt.palette();
                let selected = theme_opt == current;
                let bg = Color::from_rgb8(pal.background.r, pal.background.g, pal.background.b);
                let fg = Color::from_rgb8(pal.foreground.r, pal.foreground.g, pal.foreground.b);
                let acc = Color::from_rgb8(pal.ui_accent.r, pal.ui_accent.g, pal.ui_accent.b);
                let g = Color::from_rgb8(pal.ansi[2].r, pal.ansi[2].g, pal.ansi[2].b);

                let card = button(
                    row![
                        container(
                            row![status_dot(bg), status_dot(fg), status_dot(acc), status_dot(g)]
                                .spacing(3)
                                .align_y(Alignment::Center),
                        )
                        .padding([4, 6])
                        .style(move |_t| container::Style {
                            background: Some(Background::Color(bg)),
                            border: Border {
                                color: chrome.border_subtle,
                                width: 1.0,
                                radius: 4.0.into(),
                            },
                            ..Default::default()
                        }),
                        container(
                            text(theme_opt.name())
                                .size(12.0)
                                .font(if selected { ui_font_medium() } else { ui_font() })
                                .wrapping(text::Wrapping::None),
                        )
                        .width(Length::Fill)
                        .clip(true),
                    ]
                    .spacing(8)
                    .align_y(Alignment::Center),
                )
                .padding([6, 8])
                .width(Length::Fill)
                .style(settings_nav_pill_style(chrome, selected))
                .on_press(Message::SettingsThemeSelected(theme_opt));

                r = r.push(card);
            }
            for _ in chunk.len()..swatch_cols.max(1) {
                r = r.push(space::horizontal().width(Length::Fill));
            }
            swatches_col = swatches_col.push(r);
        }

        let overrides_row: Element<'_, Message, Theme, iced::Renderer> = if compact_form {
            row![
                override_input_fixed("BG #hex", &self.override_bg, ThemeOverrideField::Background, chrome, 96.0),
                override_input_fixed("FG #hex", &self.override_fg, ThemeOverrideField::Foreground, chrome, 96.0),
                override_input_fixed("Cursor #hex", &self.override_cursor, ThemeOverrideField::Cursor, chrome, 96.0),
                override_input_fixed("Select #hex", &self.override_selection, ThemeOverrideField::SelectionBg, chrome, 96.0),
                override_input_fixed("Accent #hex", &self.override_accent, ThemeOverrideField::UiAccent, chrome, 96.0),
            ]
            .spacing(6)
            .wrap()
            .vertical_spacing(6)
            .into()
        } else {
            row![
                override_input("BG #hex", &self.override_bg, ThemeOverrideField::Background, chrome),
                override_input("FG #hex", &self.override_fg, ThemeOverrideField::Foreground, chrome),
                override_input("Cursor #hex", &self.override_cursor, ThemeOverrideField::Cursor, chrome),
                override_input("Select #hex", &self.override_selection, ThemeOverrideField::SelectionBg, chrome),
                override_input("Accent #hex", &self.override_accent, ThemeOverrideField::UiAccent, chrome),
            ]
            .spacing(6)
            .into()
        };

        let cursor_controls = row![
            cursor_btn("█ Block", CursorStyle::Block, self.draft.terminal.cursor_style, chrome),
            cursor_btn("| Beam", CursorStyle::Beam, self.draft.terminal.cursor_style, chrome),
            cursor_btn("▁ Underline", CursorStyle::Underline, self.draft.terminal.cursor_style, chrome),
        ]
        .spacing(4)
        .wrap()
        .vertical_spacing(4);

        let mut tab_pos_row = row![].spacing(4);
        for (label, pos) in [
            ("Top", TabBarPosition::Top),
            ("Bottom", TabBarPosition::Bottom),
            ("Left", TabBarPosition::Left),
            ("Right", TabBarPosition::Right),
        ] {
            let active = self.draft.terminal.tab_bar_position == pos;
            tab_pos_row = tab_pos_row.push(
                button(
                    text(label)
                        .size(12.0)
                        .font(if active { ui_font_medium() } else { ui_font() }),
                )
                .padding([5, 11])
                .style(settings_nav_pill_style(chrome, active))
                .on_press(Message::SettingsTabBarPositionSelected(pos)),
            );
        }

        let tab_width_toggle = row![
            button(text("Fixed").size(12.0))
                .padding([5, 11])
                .style(settings_nav_pill_style(chrome, !self.draft.terminal.flex_tabs))
                .on_press(Message::SettingsFlexTabsToggled(false)),
            button(text("Dynamic").size(12.0))
                .padding([5, 11])
                .style(settings_nav_pill_style(chrome, self.draft.terminal.flex_tabs))
                .on_press(Message::SettingsFlexTabsToggled(true)),
        ]
        .spacing(4);

        column![
            column![
                section_heading("Appearance", "Color scheme, fonts, and Tabby workspace chrome", chrome),
                scheme_preview,
                swatches_col,
            ]
            .spacing(12)
            .padding(Padding {
                top: 0.0,
                right: 0.0,
                bottom: 8.0,
                left: 0.0,
            }),
            hairline_h(chrome),
            form_line(
                "Palette overrides",
                Some("Optional #RRGGBB overrides for background, foreground, cursor, selection, accent"),
                overrides_row,
                true,
                chrome,
            ),
            hairline_h(chrome),
            form_line(
                "Font",
                Some("Monospace family used by terminal grids"),
                text_input("monospace", &self.draft.terminal.font_family)
                    .on_input(Message::SettingsFontFamilyChanged)
                    .font(mono_font())
                    .size(FONT_BODY)
                    .padding([6, 10])
                    .style(styled_input_style(chrome))
                    .width(Length::Fill)
                    .into(),
                compact_form,
                chrome,
            ),
            hairline_h(chrome),
            form_line(
                "Font size",
                Some("Rendered grid size in pixels"),
                row![
                    button(text("-").size(13.0).font(ui_font_medium()))
                        .padding([4, 10])
                        .style(pill_button_style(chrome, PillTone::Subtle))
                        .on_press(Message::SettingsStepFontSize(-1.0)),
                    text_input("14", &self.font_size_text)
                        .on_input(Message::SettingsFontSizeChanged)
                        .font(mono_font())
                        .width(Length::Fixed(64.0))
                        .size(FONT_BODY)
                        .padding([5, 8])
                        .style(styled_input_style(chrome)),
                    button(text("+").size(13.0).font(ui_font_medium()))
                        .padding([4, 10])
                        .style(pill_button_style(chrome, PillTone::Subtle))
                        .on_press(Message::SettingsStepFontSize(1.0)),
                    text("px").size(FONT_META).font(mono_font()).color(chrome.text_muted),
                ]
                .spacing(6)
                .align_y(Alignment::Center)
                .into(),
                compact_form,
                chrome,
            ),
            hairline_h(chrome),
            form_line(
                "Font ligatures",
                None,
                apple_checkbox(
                    self.draft.terminal.font_ligatures,
                    "Enable programming ligatures",
                    Message::SettingsLigaturesToggled,
                    chrome,
                ),
                compact_form,
                chrome,
            ),
            hairline_h(chrome),
            form_line(
                "Cursor shape",
                None,
                cursor_controls.into(),
                compact_form,
                chrome,
            ),
            hairline_h(chrome),
            form_line(
                "Blink cursor",
                None,
                apple_checkbox(
                    self.draft.terminal.cursor_blink,
                    "Blink terminal cursor",
                    Message::SettingsCursorBlinkToggled,
                    chrome,
                ),
                compact_form,
                chrome,
            ),
            hairline_h(chrome),
            form_line(
                "Window opacity",
                Some("0.30 to 1.00"),
                row![
                    button(text("-").size(13.0).font(ui_font_medium()))
                        .padding([4, 10])
                        .style(pill_button_style(chrome, PillTone::Subtle))
                        .on_press(Message::SettingsStepOpacity(-0.05)),
                    text_input("1.00", &self.opacity_text)
                        .on_input(Message::SettingsOpacityChanged)
                        .font(mono_font())
                        .width(Length::Fixed(64.0))
                        .size(FONT_BODY)
                        .padding([5, 8])
                        .style(styled_input_style(chrome)),
                    button(text("+").size(13.0).font(ui_font_medium()))
                        .padding([4, 10])
                        .style(pill_button_style(chrome, PillTone::Subtle))
                        .on_press(Message::SettingsStepOpacity(0.05)),
                ]
                .spacing(6)
                .align_y(Alignment::Center)
                .into(),
                compact_form,
                chrome,
            ),
            hairline_h(chrome),
            form_line(
                "Tabs location",
                Some("Dock tab bar on top, bottom, left, or right edge"),
                tab_pos_row.wrap().vertical_spacing(4).into(),
                compact_form,
                chrome,
            ),
            hairline_h(chrome),
            form_line(
                "Tabs width",
                None,
                tab_width_toggle.into(),
                compact_form,
                chrome,
            ),
            hairline_h(chrome),
            form_line(
                "Hide tab index",
                Some("Hide 1-based numbering on tab headers"),
                apple_checkbox(
                    self.draft.terminal.hide_tab_index,
                    "Hide tab numbers",
                    Message::SettingsHideTabIndexToggled,
                    chrome,
                ),
                compact_form,
                chrome,
            ),
            hairline_h(chrome),
            form_line(
                "Show Profiles sidebar by default",
                Some("Expand the navigation sidebar automatically on desktop windows (Ctrl+Shift+S)"),
                apple_checkbox(
                    self.draft.terminal.show_profile_sidebar,
                    "Show Profiles sidebar by default",
                    Message::SettingsShowProfileSidebarToggled,
                    chrome,
                ),
                compact_form,
                chrome,
            ),
            hairline_h(chrome),
            form_line(
                "Status bar",
                Some("Show diagnostic bottom telemetry strip"),
                apple_checkbox(
                    self.draft.terminal.show_status_bar,
                    "Show bottom status bar",
                    Message::SettingsShowStatusBarToggled,
                    chrome,
                ),
                compact_form,
                chrome,
            ),
            hairline_h(chrome),
            form_line(
                "Server stats",
                Some("Show live remote CPU, RAM, disk, and network stats below each SSH session"),
                apple_checkbox(
                    self.draft.terminal.show_server_stats,
                    "Show server stats bar",
                    Message::SettingsShowServerStatsToggled,
                    chrome,
                ),
                compact_form,
                chrome,
            ),
            hairline_h(chrome),
            form_line(
                "System tray",
                Some("Show Termewnal icon in the desktop notification area"),
                apple_checkbox(
                    self.draft.tray.enabled,
                    "Enable system tray icon",
                    Message::SettingsTrayEnabledToggled,
                    chrome,
                ),
                compact_form,
                chrome,
            ),
            hairline_h(chrome),
            form_line(
                "Close to tray",
                Some("Minimize to tray instead of quitting when closing the last window"),
                apple_checkbox(
                    self.draft.tray.minimize_to_tray,
                    "Minimize to tray on close",
                    Message::SettingsMinimizeToTrayToggled,
                    chrome,
                ),
                compact_form,
                chrome,
            ),
        ]
        .spacing(4)
        .into()
    }

    fn view_terminal(
        &self,
        chrome: ChromePalette,
        compact_form: bool,
    ) -> Element<'_, Message, Theme, iced::Renderer> {
        let shell_val = self.draft.terminal.shell.as_deref().unwrap_or("");
        let mut right_click_track = row![].spacing(4);
        for (label, act) in [
            ("Context Menu", RightClickAction::Menu),
            ("Paste", RightClickAction::Paste),
            ("Disabled", RightClickAction::Off),
        ] {
            let active = self.draft.terminal.right_click_action == act;
            right_click_track = right_click_track.push(
                button(
                    text(label)
                        .size(12.0)
                        .font(if active { ui_font_medium() } else { ui_font() }),
                )
                .padding([5, 10])
                .style(settings_nav_pill_style(chrome, active))
                .on_press(Message::SettingsRightClickActionSelected(act)),
            );
        }

        let mut bell_track = row![].spacing(4);
        for (label, bel) in [
            ("Visual", BellBehavior::Visual),
            ("Notification", BellBehavior::Notification),
            ("Visual + Notify", BellBehavior::VisualAndNotification),
            ("Off", BellBehavior::Off),
        ] {
            let active = self.draft.terminal.bell_behavior == bel;
            bell_track = bell_track.push(
                button(
                    text(label)
                        .size(12.0)
                        .font(if active { ui_font_medium() } else { ui_font() }),
                )
                .padding([5, 10])
                .style(settings_nav_pill_style(chrome, active))
                .on_press(Message::SettingsBellBehaviorSelected(bel)),
            );
        }

        column![
            container(section_heading(
                "Terminal",
                "Shell, scrollback, clipboard, bell, and logging",
                chrome,
            ))
            .padding(Padding {
                top: 0.0,
                right: 0.0,
                bottom: 6.0,
                left: 0.0,
            }),
            form_line(
                "Default shell",
                Some("Leave blank to auto-detect the OS shell"),
                text_input("Auto-detect OS shell", shell_val)
                    .on_input(Message::SettingsShellChanged)
                    .font(mono_font())
                    .size(FONT_BODY)
                    .padding([6, 10])
                    .style(styled_input_style(chrome))
                    .width(Length::Fill)
                    .into(),
                compact_form,
                chrome,
            ),
            hairline_h(chrome),
            form_line(
                "Scrollback lines",
                Some("Default: 10,000 lines"),
                text_input("10000", &self.scrollback_text)
                    .on_input(Message::SettingsScrollbackChanged)
                    .font(mono_font())
                    .width(Length::Fixed(120.0))
                    .size(FONT_BODY)
                    .padding([6, 10])
                    .style(styled_input_style(chrome))
                    .into(),
                compact_form,
                chrome,
            ),
            hairline_h(chrome),
            form_line(
                "Copy on select",
                None,
                apple_checkbox(
                    self.draft.terminal.copy_on_select,
                    "Automatically copy terminal selection to clipboard",
                    Message::SettingsCopyOnSelectToggled,
                    chrome,
                ),
                compact_form,
                chrome,
            ),
            hairline_h(chrome),
            form_line(
                "Middle-click paste",
                Some("Shift+click bypasses PTY mouse tracking"),
                apple_checkbox(
                    self.draft.terminal.middle_click_paste,
                    "Paste clipboard on middle-click",
                    Message::SettingsMiddleClickPasteToggled,
                    chrome,
                ),
                compact_form,
                chrome,
            ),
            hairline_h(chrome),
            form_line(
                "Paste on right-click / menu",
                None,
                right_click_track.wrap().vertical_spacing(4).into(),
                compact_form,
                chrome,
            ),
            hairline_h(chrome),
            form_line(
                "Confirm multi-line paste",
                None,
                apple_checkbox(
                    self.draft.terminal.confirm_multiline_paste,
                    "Warn before pasting multi-line text",
                    Message::SettingsConfirmMultilinePasteToggled,
                    chrome,
                ),
                compact_form,
                chrome,
            ),
            hairline_h(chrome),
            form_line(
                "Alt as Meta",
                None,
                apple_checkbox(
                    self.draft.terminal.alt_is_meta,
                    "Treat Alt+key as ESC meta prefix",
                    Message::SettingsAltIsMetaToggled,
                    chrome,
                ),
                compact_form,
                chrome,
            ),
            hairline_h(chrome),
            form_line(
                "Terminal bell",
                None,
                bell_track.wrap().vertical_spacing(4).into(),
                compact_form,
                chrome,
            ),
            hairline_h(chrome),
            form_line(
                "Word separators",
                Some("Characters delimiting double-click word selection"),
                text_input(" ()[]{}'\"`,;:", &self.draft.terminal.word_separators)
                    .on_input(Message::SettingsWordSeparatorsChanged)
                    .font(mono_font())
                    .size(FONT_BODY)
                    .padding([6, 10])
                    .style(styled_input_style(chrome))
                    .width(Length::Fill)
                    .into(),
                compact_form,
                chrome,
            ),
            hairline_h(chrome),
            form_line(
                "Auto-log sessions",
                Some("Continuously save terminal output to disk"),
                apple_checkbox(
                    self.draft.terminal.auto_log_sessions,
                    "Enable automatic session logging",
                    Message::SettingsAutoLogToggled,
                    chrome,
                ),
                compact_form,
                chrome,
            ),
            hairline_h(chrome),
            form_line(
                "Log directory",
                Some("Blank uses XDG data_local/logs"),
                text_input("Default XDG data_local/logs", &self.log_dir_text)
                    .on_input(Message::SettingsLogDirectoryChanged)
                    .font(mono_font())
                    .size(FONT_BODY)
                    .padding([6, 10])
                    .style(styled_input_style(chrome))
                    .width(Length::Fill)
                    .into(),
                compact_form,
                chrome,
            ),
        ]
        .spacing(4)
        .into()
    }

    fn view_keybindings(
        &self,
        chrome: ChromePalette,
        compact_form: bool,
    ) -> Element<'_, Message, Theme, iced::Renderer> {
        let filter_lower = self.keybinding_filter.trim().to_ascii_lowercase();
        let top_controls = row![
            text_input("Filter hotkeys...", &self.keybinding_filter)
                .on_input(Message::SettingsKeybindingFilterChanged)
                .size(FONT_BODY)
                .padding([6, 10])
                .style(styled_input_style(chrome))
                .width(Length::Fill),
            button(text("Reset All").size(FONT_META))
                .padding([6, 12])
                .style(pill_button_style(chrome, PillTone::Subtle))
                .on_press(Message::SettingsResetAllKeybindings),
        ]
        .spacing(8)
        .align_y(Alignment::Center);

        let mut list = column![].spacing(4);
        let mut shown = 0usize;
        for &action in KeyAction::ALL {
            let draft_val = self
                .keybinding_drafts
                .get(&action)
                .map(String::as_str)
                .unwrap_or("");
            if !filter_lower.is_empty()
                && !action.label().to_ascii_lowercase().contains(&filter_lower)
                && !action.category().to_ascii_lowercase().contains(&filter_lower)
                && !draft_val.to_ascii_lowercase().contains(&filter_lower)
            {
                continue;
            }
            if shown > 0 {
                list = list.push(hairline_h(chrome));
            }
            shown += 1;
            let capturing = self.capture_keybinding == Some(action);
            let record_btn = if capturing {
                button(text("Press key... (Esc)").size(FONT_META).font(ui_font_medium()))
                    .padding([4, 9])
                    .style(pill_button_style(chrome, PillTone::Primary))
                    .on_press(Message::SettingsCancelShortcutCapture)
            } else {
                button(text("Record").size(FONT_META))
                    .padding([4, 9])
                    .style(tab_bar_button_style(chrome, false, false))
                    .on_press(Message::SettingsBeginShortcutCapture(action))
            };
            let input = text_input("Unbound (e.g. Ctrl+Shift+T)", draft_val)
                .on_input(move |v| Message::SettingsKeybindingChanged {
                    action,
                    value: v,
                })
                .font(mono_font())
                .size(FONT_META)
                .padding([5, 9])
                .style(move |_theme, status| {
                    let mut st = styled_input_style(chrome)(_theme, status);
                    if !draft_val.trim().is_empty() {
                        st.border.color = chrome.accent;
                        st.value = chrome.accent;
                    }
                    st
                });
            let reset_btn = button(text("Reset").size(FONT_META))
                .padding([4, 8])
                .style(tab_bar_button_style(chrome, false, false))
                .on_press(Message::SettingsResetKeybinding(action));

            let row_el: Element<'_, Message, Theme, iced::Renderer> = if compact_form {
                column![
                    row![
                        text(action.category())
                            .size(FONT_META)
                            .color(chrome.text_muted),
                        text("/").size(FONT_META).color(chrome.text_muted),
                        text(action.label())
                            .size(FONT_BODY)
                            .color(chrome.text_primary),
                    ]
                    .spacing(6)
                    .align_y(Alignment::Center)
                    .wrap()
                    .vertical_spacing(2),
                    row![
                        input.width(Length::Fill),
                        record_btn,
                        reset_btn,
                    ]
                    .spacing(6)
                    .align_y(Alignment::Center),
                ]
                .spacing(6)
                .into()
            } else {
                row![
                    column![
                        text(action.label())
                            .size(FONT_BODY)
                            .color(chrome.text_primary),
                        text(action.category())
                            .size(FONT_META)
                            .color(chrome.text_muted),
                    ]
                    .spacing(1)
                    .width(Length::FillPortion(3)),
                    input.width(Length::FillPortion(3)),
                    record_btn,
                    reset_btn,
                ]
                .spacing(6)
                .align_y(Alignment::Center)
                .into()
            };

            list = list.push(
                container(row_el)
                    .padding([6, 4])
                    .width(Length::Fill)
                    .style(move |_| container::Style {
                        background: if capturing {
                            Some(Background::Color(chrome.accent_soft))
                        } else {
                            None
                        },
                        border: Border {
                            radius: 5.0.into(),
                            ..Default::default()
                        },
                        ..Default::default()
                    }),
            );
        }
        if shown == 0 {
            list = list.push(
                container(
                    text("No matching shortcuts.")
                        .size(FONT_BODY)
                        .color(chrome.text_muted),
                )
                .padding([10, 4]),
            );
        }

        column![
            section_heading(
                "Hotkeys",
                "Comma-separated sequences; multi-chord sequences use spaces (e.g. Ctrl+K Ctrl+S)",
                chrome
            ),
            top_controls,
            hairline_h(chrome),
            list,
        ]
        .spacing(10)
        .into()
    }

    fn view_highlights(
        &self,
        chrome: ChromePalette,
        compact_form: bool,
    ) -> Element<'_, Message, Theme, iced::Renderer> {
        let name_pattern_row: Element<'_, Message, Theme, iced::Renderer> = if compact_form {
            column![
                text_input("Rule name", &self.highlight_new_name)
                    .on_input(Message::SettingsHighlightNameChanged)
                    .size(FONT_BODY)
                    .padding([6, 10])
                    .style(styled_input_style(chrome))
                    .width(Length::Fill),
                text_input("Regex pattern (e.g. ERROR|FATAL)", &self.highlight_new_pattern)
                    .on_input(Message::SettingsHighlightPatternChanged)
                    .font(mono_font())
                    .size(FONT_BODY)
                    .padding([6, 10])
                    .style(styled_input_style(chrome))
                    .width(Length::Fill),
            ]
            .spacing(8)
            .into()
        } else {
            row![
                text_input("Rule name", &self.highlight_new_name)
                    .on_input(Message::SettingsHighlightNameChanged)
                    .size(FONT_BODY)
                    .padding([6, 10])
                    .style(styled_input_style(chrome))
                    .width(Length::FillPortion(2)),
                text_input("Regex pattern (e.g. ERROR|FATAL)", &self.highlight_new_pattern)
                    .on_input(Message::SettingsHighlightPatternChanged)
                    .font(mono_font())
                    .size(FONT_BODY)
                    .padding([6, 10])
                    .style(styled_input_style(chrome))
                    .width(Length::FillPortion(3)),
            ]
            .spacing(8)
            .into()
        };

        let add_form = column![
            name_pattern_row,
            row![
                text_input("FG #ff5555", &self.highlight_new_fg)
                    .on_input(Message::SettingsHighlightFgChanged)
                    .font(mono_font())
                    .size(FONT_META)
                    .padding([5, 9])
                    .style(styled_input_style(chrome))
                    .width(Length::Fixed(108.0)),
                text_input("BG #331111", &self.highlight_new_bg)
                    .on_input(Message::SettingsHighlightBgChanged)
                    .font(mono_font())
                    .size(FONT_META)
                    .padding([5, 9])
                    .style(styled_input_style(chrome))
                    .width(Length::Fixed(108.0)),
                apple_checkbox(
                    self.highlight_new_bold,
                    "Bold",
                    Message::SettingsHighlightBoldToggled,
                    chrome,
                ),
                apple_checkbox(
                    self.highlight_new_case_insensitive,
                    "Ignore case",
                    Message::SettingsHighlightCaseToggled,
                    chrome,
                ),
                space::horizontal(),
                button(text("Add Rule").size(FONT_BODY).font(ui_font_medium()))
                    .padding([6, 14])
                    .style(pill_button_style(chrome, PillTone::Primary))
                    .on_press(Message::SettingsAddHighlightRule),
            ]
            .spacing(10)
            .align_y(Alignment::Center)
            .wrap()
            .vertical_spacing(6),
        ]
        .spacing(8);

        let mut rules_list = column![].spacing(4);
        if self.draft.terminal.highlight_rules.is_empty() {
            rules_list = rules_list.push(
                container(
                    text("No output highlight rules configured yet.")
                        .size(FONT_BODY)
                        .color(chrome.text_muted),
                )
                .padding([8, 4]),
            );
        } else {
            let total = self.draft.terminal.highlight_rules.len();
            for (idx, r) in self.draft.terminal.highlight_rules.iter().enumerate() {
                let id_toggle = r.id.clone();
                let id_delete = r.id.clone();
                let preview = format!(
                    "/{}/ fg:{} bg:{}{}",
                    r.pattern,
                    r.foreground.as_deref().unwrap_or("-"),
                    r.background.as_deref().unwrap_or("-"),
                    if r.bold { " bold" } else { "" }
                );
                rules_list = rules_list.push(
                    container(
                        row![
                            apple_checkbox(
                                r.enabled,
                                "",
                                move |_| Message::SettingsToggleHighlightRule(id_toggle.clone()),
                                chrome,
                            ),
                            column![
                                text(r.name.as_str()).size(FONT_BODY).color(chrome.text_primary),
                                text(preview).size(FONT_META).font(mono_font()).color(chrome.text_muted),
                            ]
                            .spacing(2)
                            .width(Length::Fill),
                            button(text("Delete").size(FONT_META))
                                .padding([4, 9])
                                .style(tab_bar_button_style(chrome, false, true))
                                .on_press(Message::SettingsDeleteHighlightRule(id_delete)),
                        ]
                        .spacing(8)
                        .align_y(Alignment::Center),
                    )
                    .padding([6, 4])
                    .width(Length::Fill),
                );
                if idx + 1 < total {
                    rules_list = rules_list.push(hairline_h(chrome));
                }
            }
        }

        column![
            section_heading("Output Highlighting", "Colorize matching regex patterns across terminal rows", chrome),
            add_form,
            hairline_h(chrome),
            rules_list,
        ]
        .spacing(10)
        .into()
    }

    fn view_network_and_sftp(
        &self,
        chrome: ChromePalette,
        compact_form: bool,
    ) -> Element<'_, Message, Theme, iced::Renderer> {
        let policies = [
            ("Ask", TransferConflictPolicy::Ask),
            ("Overwrite", TransferConflictPolicy::Overwrite),
            ("Resume", TransferConflictPolicy::Resume),
            ("Skip", TransferConflictPolicy::Skip),
            ("Rename", TransferConflictPolicy::Rename),
        ];
        let mut policy_track = row![].spacing(4);
        for (label, policy) in policies {
            let active = self.draft.transfer.default_conflict_policy == policy;
            policy_track = policy_track.push(
                button(
                    text(label)
                        .size(12.0)
                        .font(if active { ui_font_medium() } else { ui_font() }),
                )
                .padding([5, 10])
                .style(settings_nav_pill_style(chrome, active))
                .on_press(Message::SettingsConflictPolicySelected(policy)),
            );
        }

        column![
            container(section_heading(
                "SSH & SFTP",
                "Connection supervision and SFTP transfer policies",
                chrome,
            ))
            .padding(Padding {
                top: 0.0,
                right: 0.0,
                bottom: 6.0,
                left: 0.0,
            }),
            form_line(
                "Keepalive interval (s)",
                Some("0 disables keepalive probes"),
                text_input("15", &self.keepalive_interval_text)
                    .on_input(Message::SettingsKeepaliveIntervalChanged)
                    .font(mono_font())
                    .width(Length::Fixed(100.0))
                    .size(FONT_BODY)
                    .padding([6, 10])
                    .style(styled_input_style(chrome))
                    .into(),
                compact_form,
                chrome,
            ),
            hairline_h(chrome),
            form_line(
                "Max missed probes",
                Some("Triggers auto-reconnect after ~45s default"),
                text_input("3", &self.keepalive_missed_text)
                    .on_input(Message::SettingsKeepaliveMissedChanged)
                    .font(mono_font())
                    .width(Length::Fixed(100.0))
                    .size(FONT_BODY)
                    .padding([6, 10])
                    .style(styled_input_style(chrome))
                    .into(),
                compact_form,
                chrome,
            ),
            hairline_h(chrome),
            form_line(
                "File conflict policy",
                Some("Default behavior when remote/local file already exists"),
                policy_track.wrap().vertical_spacing(4).into(),
                compact_form,
                chrome,
            ),
            hairline_h(chrome),
            form_line(
                "Follow symlinks",
                Some("Loop-guarded symlink traversal during recursive SFTP transfers"),
                apple_checkbox(
                    self.draft.transfer.follow_symlinks,
                    "Follow symlinks during SFTP transfers",
                    Message::SettingsFollowSymlinksToggled,
                    chrome,
                ),
                compact_form,
                chrome,
            ),
            hairline_h(chrome),
            form_line(
                "Auto-open SFTP+",
                Some("Automatically open and connect SFTP+ on new SSH connections"),
                apple_checkbox(
                    self.draft.terminal.auto_open_sftp,
                    "Open SFTP+ panel automatically when SSH connects",
                    Message::SettingsAutoOpenSftpToggled,
                    chrome,
                ),
                compact_form,
                chrome,
            ),
            hairline_h(chrome),
            form_line(
                "SFTP download folder",
                Some("Leave empty to use system Downloads folder"),
                row![
                    text_input("System Downloads...", &self.sftp_download_dir_text)
                        .on_input(Message::SettingsSftpDownloadDirChanged)
                        .font(mono_font())
                        .size(FONT_BODY)
                        .padding([6, 10])
                        .style(styled_input_style(chrome))
                        .width(Length::Fill),
                    button(text("Browse...").size(FONT_META).font(ui_font_medium()))
                        .padding([6, 10])
                        .style(pill_button_style(chrome, PillTone::Subtle))
                        .on_press(Message::SettingsBrowseSftpDownloadDir),
                    button(text("Reset").size(FONT_META))
                        .padding([6, 10])
                        .style(pill_button_style(chrome, PillTone::Ghost))
                        .on_press(Message::SettingsResetSftpDownloadDir),
                ]
                .spacing(6)
                .align_y(Alignment::Center)
                .into(),
                compact_form,
                chrome,
            ),
        ]
        .spacing(4)
        .into()
    }

    fn view_vault<'a>(
        &'a self,
        chrome: ChromePalette,
        vault: &VaultSession,
        compact_form: bool,
    ) -> Element<'a, Message, Theme, iced::Renderer> {
        let keyring_active = self.draft.vault.backend == VaultBackend::Keyring;
        let encrypted_active = self.draft.vault.backend == VaultBackend::EncryptedFile;

        let status_line = match (vault.backend(), vault.is_unlocked()) {
            (VaultBackend::Keyring, _) => "Active backend: OS Keyring (unlocked by OS session)".to_string(),
            (VaultBackend::EncryptedFile, true) => "Active backend: Encrypted Local Vault (Unlocked)".to_string(),
            (VaultBackend::EncryptedFile, false) => "Active backend: Encrypted Local Vault (Locked)".to_string(),
        };

        let mut status_row = row![
            status_dot(if vault.is_unlocked() {
                chrome.success
            } else {
                chrome.warning
            }),
            text(status_line).size(FONT_BODY).color(chrome.text_primary),
            space::horizontal(),
        ]
        .spacing(8)
        .align_y(Alignment::Center);

        if vault.backend() == VaultBackend::EncryptedFile && vault.is_unlocked() {
            status_row = status_row.push(
                button(text("Lock Vault Now").size(FONT_META).font(ui_font_medium()))
                    .padding([5, 11])
                    .style(pill_button_style(chrome, PillTone::Danger))
                    .on_press(Message::VaultLock),
            );
        }

        let keyring_card = button(
            column![
                text("OS Keyring").size(FONT_SUBHEAD).font(ui_font_medium()),
                text("Secret Service / Keychain / Credential Manager")
                    .size(FONT_META)
                    .color(chrome.text_muted),
            ]
            .spacing(3),
        )
        .padding([10, 12])
        .width(Length::Fill)
        .style(pill_button_style(
            chrome,
            if keyring_active {
                PillTone::ActiveAccent
            } else {
                PillTone::Subtle
            },
        ))
        .on_press(Message::SettingsVaultBackendSelected(VaultBackend::Keyring));

        let encrypted_card = button(
            column![
                text("Encrypted Local Vault").size(FONT_SUBHEAD).font(ui_font_medium()),
                text("Argon2id + AES-256-GCM Master Password")
                    .size(FONT_META)
                    .color(chrome.text_muted),
            ]
            .spacing(3),
        )
        .padding([10, 12])
        .width(Length::Fill)
        .style(pill_button_style(
            chrome,
            if encrypted_active {
                PillTone::ActiveAccent
            } else {
                PillTone::Subtle
            },
        ))
        .on_press(Message::SettingsVaultBackendSelected(VaultBackend::EncryptedFile));

        let backend_choices: Element<'a, Message, Theme, iced::Renderer> = if compact_form {
            column![keyring_card, encrypted_card].spacing(8).into()
        } else {
            row![keyring_card, encrypted_card].spacing(8).into()
        };

        column![
            section_heading("Vault", "Passwords and passphrases are never stored in TOML", chrome),
            status_row.wrap().vertical_spacing(6),
            backend_choices,
            hairline_h(chrome),
            form_line(
                "Auto-lock idle (min)",
                Some("0 disables idle auto-lock"),
                text_input("30", &self.auto_lock_text)
                    .on_input(Message::SettingsAutoLockChanged)
                    .font(mono_font())
                    .width(Length::Fixed(100.0))
                    .size(FONT_BODY)
                    .padding([6, 10])
                    .style(styled_input_style(chrome))
                    .into(),
                compact_form,
                chrome,
            ),
        ]
        .spacing(10)
        .into()
    }

    fn view_sync(
        &self,
        chrome: ChromePalette,
        compact_form: bool,
    ) -> Element<'_, Message, Theme, iced::Renderer> {
        let mut providers_track = row![].spacing(4);
        for (label, pkind) in [
            ("Disabled", SyncProviderKind::None),
            ("Local Folder", SyncProviderKind::LocalDirectory),
            ("WebDAV", SyncProviderKind::WebDav),
            ("GitHub Gist", SyncProviderKind::GitHubGist),
        ] {
            let active = self.draft.sync.provider == pkind;
            providers_track = providers_track.push(
                button(
                    text(label)
                        .size(12.0)
                        .font(if active { ui_font_medium() } else { ui_font() }),
                )
                .padding([5, 11])
                .style(settings_nav_pill_style(chrome, active))
                .on_press(Message::SettingsSyncProviderSelected(pkind)),
            );
        }

        let mut col = column![
            section_heading(
                "Config sync",
                "Sync config.toml, profiles, quick commands, and workspaces (secrets excluded)",
                chrome
            ),
            form_line(
                "Provider",
                None,
                providers_track.wrap().vertical_spacing(4).into(),
                compact_form,
                chrome,
            ),
        ]
        .spacing(10);

        match self.draft.sync.provider {
            SyncProviderKind::None => {}
            SyncProviderKind::LocalDirectory => {
                col = col.push(hairline_h(chrome)).push(form_line(
                    "Sync directory",
                    None,
                    text_input("/path/to/sync-folder", &self.sync_local_dir)
                        .on_input(Message::SettingsSyncLocalDirChanged)
                        .font(mono_font())
                        .size(FONT_BODY)
                        .padding([6, 10])
                        .style(styled_input_style(chrome))
                        .width(Length::Fill)
                        .into(),
                    compact_form,
                    chrome,
                ));
            }
            SyncProviderKind::WebDav => {
                col = col
                    .push(hairline_h(chrome))
                    .push(form_line(
                        "WebDAV URL",
                        None,
                        text_input(
                            "https://dav.example.com/remote.php/dav/files/user/termewnal",
                            &self.sync_webdav_url,
                        )
                        .on_input(Message::SettingsSyncWebDavUrlChanged)
                        .font(mono_font())
                        .size(FONT_BODY)
                        .padding([6, 10])
                        .style(styled_input_style(chrome))
                        .width(Length::Fill)
                        .into(),
                        compact_form,
                        chrome,
                    ))
                    .push(hairline_h(chrome))
                    .push(form_line(
                        "Username",
                        None,
                        text_input("WebDAV username", &self.sync_webdav_username)
                            .on_input(Message::SettingsSyncWebDavUserChanged)
                            .size(FONT_BODY)
                            .padding([6, 10])
                            .style(styled_input_style(chrome))
                            .width(Length::Fill)
                            .into(),
                        compact_form,
                        chrome,
                    ));
            }
            SyncProviderKind::GitHubGist => {
                col = col.push(hairline_h(chrome)).push(form_line(
                    "Gist ID",
                    Some("Blank creates a new secret Gist"),
                    text_input("Optional existing Gist ID", &self.sync_gist_id)
                        .on_input(Message::SettingsSyncGistIdChanged)
                        .font(mono_font())
                        .size(FONT_BODY)
                        .padding([6, 10])
                        .style(styled_input_style(chrome))
                        .width(Length::Fill)
                        .into(),
                    compact_form,
                    chrome,
                ));
            }
        }

        if matches!(
            self.draft.sync.provider,
            SyncProviderKind::WebDav | SyncProviderKind::GitHubGist
        ) {
            let token_ph = if self.draft.sync.vault_token_ref.is_some() {
                "Leave blank to keep stored Vault token"
            } else {
                "Access token / password (stored in Vault)"
            };
            col = col.push(hairline_h(chrome)).push(form_line(
                "Vault token",
                None,
                text_input(token_ph, &self.sync_token_draft)
                    .secure(true)
                    .on_input(Message::SettingsSyncTokenChanged)
                    .size(FONT_BODY)
                    .padding([6, 10])
                    .style(styled_input_style(chrome))
                    .width(Length::Fill)
                    .into(),
                compact_form,
                chrome,
            ));
        }

        col = col
            .push(hairline_h(chrome))
            .push(form_line(
                "Encrypt bundle",
                Some("Argon2id + AES-256-GCM passphrase encryption"),
                apple_checkbox(
                    self.draft.sync.encrypt_bundle,
                    "Encrypt sync bundle",
                    Message::SettingsSyncEncryptToggled,
                    chrome,
                ),
                compact_form,
                chrome,
            ))
            .push(hairline_h(chrome))
            .push(form_line(
                "Auto sync",
                Some("Push bundle automatically when saving Settings or Profiles"),
                apple_checkbox(
                    self.draft.sync.auto_sync_on_save,
                    "Auto-push on save",
                    Message::SettingsSyncAutoToggled,
                    chrome,
                ),
                compact_form,
                chrome,
            ));

        if self.draft.sync.encrypt_bundle {
            col = col.push(hairline_h(chrome)).push(form_line(
                "Sync passphrase",
                Some("Required for encrypted push & pull"),
                text_input("Required for encrypted push & pull", &self.sync_passphrase_draft)
                    .secure(true)
                    .on_input(Message::SettingsSyncPassphraseChanged)
                    .size(FONT_BODY)
                    .padding([6, 10])
                    .style(styled_input_style(chrome))
                    .width(Length::Fill)
                    .into(),
                compact_form,
                chrome,
            ));
        }

        let last_sync = self
            .draft
            .sync
            .last_sync_at
            .as_deref()
            .unwrap_or("Never synced");

        col = col.push(hairline_h(chrome)).push(
            row![
                button(text("Push Now").size(FONT_BODY).font(ui_font_medium()))
                    .padding([6, 14])
                    .style(pill_button_style(chrome, PillTone::Primary))
                    .on_press(Message::SettingsSyncPushNow),
                button(text("Pull & Apply").size(FONT_BODY))
                    .padding([6, 14])
                    .style(pill_button_style(chrome, PillTone::Subtle))
                    .on_press(Message::SettingsSyncPullNow),
                text(format!("Last sync: {last_sync}"))
                    .size(FONT_META)
                    .font(mono_font())
                    .color(chrome.text_muted),
            ]
            .spacing(8)
            .align_y(Alignment::Center)
            .wrap()
            .vertical_spacing(6),
        );

        if let Some(status) = &self.sync_status_notice {
            col = col.push(text(status.as_str()).size(FONT_BODY).color(chrome.success));
        }

        col.into()
    }
}

fn section_heading<'a>(
    title: &'static str,
    subtitle: &'static str,
    chrome: ChromePalette,
) -> Element<'a, Message, Theme, iced::Renderer> {
    column![
        text(title)
            .size(19.0)
            .font(ui_font())
            .color(chrome.text_primary),
        text(subtitle)
            .size(FONT_META)
            .font(ui_font())
            .color(chrome.text_muted),
    ]
    .spacing(2)
    .into()
}

fn cursor_btn<'a>(
    label: &'static str,
    style: CursorStyle,
    current: CursorStyle,
    chrome: ChromePalette,
) -> Element<'a, Message, Theme, iced::Renderer> {
    let active = style == current;
    button(
        text(label)
            .size(12.0)
            .font(if active { ui_font_medium() } else { ui_font() }),
    )
    .padding([5, 11])
    .style(settings_nav_pill_style(chrome, active))
    .on_press(Message::SettingsCursorStyleSelected(style))
    .into()
}

fn override_input<'a>(
    placeholder: &'static str,
    value: &'a str,
    field: ThemeOverrideField,
    chrome: ChromePalette,
) -> Element<'a, Message, Theme, iced::Renderer> {
    text_input(placeholder, value)
        .on_input(move |v| Message::SettingsThemeOverrideChanged { field, value: v })
        .font(mono_font())
        .size(FONT_META)
        .padding([6, 8])
        .style(styled_input_style(chrome))
        .width(Length::Fill)
        .into()
}

fn override_input_fixed<'a>(
    placeholder: &'static str,
    value: &'a str,
    field: ThemeOverrideField,
    chrome: ChromePalette,
    width: f32,
) -> Element<'a, Message, Theme, iced::Renderer> {
    text_input(placeholder, value)
        .on_input(move |v| Message::SettingsThemeOverrideChanged { field, value: v })
        .font(mono_font())
        .size(FONT_META)
        .padding([6, 8])
        .style(styled_input_style(chrome))
        .width(Length::Fixed(width))
        .into()
}
