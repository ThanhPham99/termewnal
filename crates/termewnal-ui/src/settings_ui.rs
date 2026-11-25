//! Tabby-inspired Settings modal (`Ctrl+,`) for live Appearance, Terminal, SSH/SFTP, and Vault config.

use iced::widget::{
    button, checkbox, column, container, row, rule, scrollable, space, text, text_input,
};
use iced::{Alignment, Background, Border, Color, Element, Length, Theme};
use termewnal_core::{
    AppConfig, BuiltinTheme, CursorStyle, ThemePalette, TransferConflictPolicy, VaultBackend,
    VaultSession,
};

use crate::app::Message;
use crate::ui_theme::{
    ChromePalette, PillTone, card_container_style, keycap_pill, modal_card_style, modal_overlay,
    pill_button_style, status_dot, styled_input_style,
};

const SETTINGS_MODAL_WIDTH: f32 = 720.0;
const SETTINGS_BODY_HEIGHT: f32 = 420.0;

/// Settings modal category tabs modeled after Tabby's Preferences sidebar.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SettingsTab {
    #[default]
    Appearance,
    Terminal,
    NetworkAndSftp,
    Vault,
}

impl SettingsTab {
    const ALL: &'static [Self] = &[
        Self::Appearance,
        Self::Terminal,
        Self::NetworkAndSftp,
        Self::Vault,
    ];

    const fn label(self) -> &'static str {
        match self {
            Self::Appearance => "Appearance",
            Self::Terminal => "Terminal",
            Self::NetworkAndSftp => "Network & SFTP",
            Self::Vault => "Secrets Vault",
        }
    }
}

/// Interactive draft state for the GUI Settings modal (`Ctrl+,`).
#[derive(Debug, Clone, Default)]
pub struct SettingsUiState {
    open: bool,
    active_tab: SettingsTab,
    draft: AppConfig,
    font_size_text: String,
    scrollback_text: String,
    keepalive_interval_text: String,
    keepalive_missed_text: String,
    auto_lock_text: String,
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

    /// Opens the modal initialized from `config`.
    pub fn open_with(&mut self, config: &AppConfig) {
        self.open = true;
        self.error = None;
        self.saved_notice = None;
        self.sync_from_config(config);
    }

    /// Closes the modal without discarding external config changes.
    pub fn close(&mut self) {
        self.open = false;
        self.error = None;
        self.saved_notice = None;
    }

    /// Refreshes draft fields from `config`.
    pub fn sync_from_config(&mut self, config: &AppConfig) {
        self.draft = config.clone();
        self.font_size_text = format!("{:.0}", config.terminal.font_size);
        self.scrollback_text = config.terminal.scrollback_lines.to_string();
        self.keepalive_interval_text = config.keepalive_interval_secs.to_string();
        self.keepalive_missed_text = config.keepalive_max_missed.to_string();
        self.auto_lock_text = config.vault.auto_lock_minutes.to_string();
    }

    pub fn set_tab(&mut self, tab: SettingsTab) {
        self.active_tab = tab;
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

    pub fn set_font_ligatures(&mut self, enabled: bool) {
        self.draft.terminal.font_ligatures = enabled;
        self.saved_notice = None;
    }

    pub fn set_cursor_style(&mut self, style: CursorStyle) {
        self.draft.terminal.cursor_style = style;
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

    pub fn set_shell_text(&mut self, value: String) {
        let trimmed = value.trim().to_string();
        self.draft.terminal.shell = (!trimmed.is_empty()).then_some(trimmed);
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

    pub fn set_vault_backend(&mut self, backend: VaultBackend) {
        self.draft.vault.backend = backend;
        self.saved_notice = None;
    }

    pub fn set_auto_lock_text(&mut self, value: String) {
        self.auto_lock_text = value;
        self.error = None;
        self.saved_notice = None;
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

    /// Validates numeric fields and builds a ready-to-persist [`AppConfig`].
    pub fn build_validated_config(&mut self) -> Result<AppConfig, String> {
        let font_size: f32 = self
            .font_size_text
            .trim()
            .parse()
            .map_err(|_| "Font size must be a number between 8 and 36".to_string())?;
        if !(8.0..=36.0).contains(&font_size) {
            return Err("Font size must be between 8 and 36".to_string());
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

        let mut cfg = self.draft.clone();
        cfg.terminal.font_size = font_size;
        if cfg.terminal.font_family.trim().is_empty() {
            cfg.terminal.font_family = "monospace".to_string();
        }
        cfg.terminal.scrollback_lines = scrollback_lines;
        cfg.keepalive_interval_secs = keepalive_interval_secs;
        cfg.keepalive_max_missed = keepalive_max_missed;
        cfg.vault.auto_lock_minutes = auto_lock_minutes;
        self.draft = cfg.clone();
        self.error = None;
        Ok(cfg)
    }

    /// Renders the Tabby-inspired Settings modal when open.
    pub fn view<'a>(
        &'a self,
        palette: &ThemePalette,
        vault: &VaultSession,
    ) -> Option<Element<'a, Message, Theme, iced::Renderer>> {
        if !self.open {
            return None;
        }
        let chrome = ChromePalette::from_palette(palette);

        let header = row![
            column![
                text("Settings").size(16).color(chrome.text_primary),
                text("Changes apply live and save atomically to config.toml")
                    .size(11)
                    .color(chrome.text_muted),
            ]
            .spacing(2),
            space::horizontal(),
            keycap_pill("Ctrl+,", chrome),
            button(text("Close").size(12))
                .padding([5, 10])
                .style(pill_button_style(chrome, PillTone::Ghost))
                .on_press(Message::CloseSettingsModal),
        ]
        .spacing(8)
        .align_y(Alignment::Center);

        let mut nav = column![].spacing(4).width(Length::Fixed(165.0));
        for &tab in SettingsTab::ALL {
            let active = self.active_tab == tab;
            nav = nav.push(
                button(
                    row![
                        container(iced::widget::space::horizontal().width(3).height(14)).style(
                            move |_t| container::Style {
                                background: Some(Background::Color(if active {
                                    chrome.accent
                                } else {
                                    Color::TRANSPARENT
                                })),
                                border: Border {
                                    radius: 2.0.into(),
                                    ..Default::default()
                                },
                                ..Default::default()
                            },
                        ),
                        text(tab.label()).size(12),
                    ]
                    .spacing(8)
                    .align_y(Alignment::Center),
                )
                .padding([8, 10])
                .width(Length::Fill)
                .style(pill_button_style(
                    chrome,
                    if active {
                        PillTone::ActiveAccent
                    } else {
                        PillTone::Ghost
                    },
                ))
                .on_press(Message::SettingsSelectTab(tab)),
            );
        }

        let pane_content = match self.active_tab {
            SettingsTab::Appearance => self.view_appearance(chrome),
            SettingsTab::Terminal => self.view_terminal(chrome),
            SettingsTab::NetworkAndSftp => self.view_network_and_sftp(chrome),
            SettingsTab::Vault => self.view_vault(chrome, vault),
        };

        let body_row = row![
            container(nav)
                .padding(8)
                .height(Length::Fixed(SETTINGS_BODY_HEIGHT))
                .style(move |_t| container::Style {
                    background: Some(Background::Color(chrome.chrome_bg)),
                    border: Border {
                        color: chrome.border_subtle,
                        width: 1.0,
                        radius: 8.0.into(),
                    },
                    ..Default::default()
                }),
            scrollable(container(pane_content).padding([4, 8]).width(Length::Fill))
                .width(Length::Fill)
                .height(Length::Fixed(SETTINGS_BODY_HEIGHT)),
        ]
        .spacing(12);

        let mut footer_row = row![].spacing(8).align_y(Alignment::Center);
        if let Some(err) = &self.error {
            footer_row = footer_row.push(text(err.as_str()).size(12).color(chrome.danger));
        } else if let Some(notice) = &self.saved_notice {
            footer_row = footer_row.push(text(notice.as_str()).size(12).color(chrome.success));
        }
        footer_row = footer_row
            .push(space::horizontal())
            .push(
                button(text("Cancel").size(12))
                    .padding([6, 12])
                    .style(pill_button_style(chrome, PillTone::Subtle))
                    .on_press(Message::CloseSettingsModal),
            )
            .push(
                button(text("Save Settings").size(12))
                    .padding([6, 16])
                    .style(pill_button_style(chrome, PillTone::Primary))
                    .on_press(Message::SettingsSave),
            );

        let card = container(
            column![
                header,
                rule::horizontal(1),
                body_row,
                rule::horizontal(1),
                footer_row,
            ]
            .spacing(12)
            .padding(16),
        )
        .width(Length::Fixed(SETTINGS_MODAL_WIDTH))
        .style(move |_t| modal_card_style(chrome, true));

        Some(modal_overlay(card.into(), Some(Message::CloseSettingsModal), 24.0))
    }

    fn view_appearance(&self, chrome: ChromePalette) -> Element<'_, Message, Theme, iced::Renderer> {
        let current = BuiltinTheme::resolve(&self.draft.terminal.theme);

        let mut swatches_col = column![].spacing(8);
        for chunk in BuiltinTheme::ALL.chunks(2) {
            let mut r = row![].spacing(8).width(Length::Fill);
            for &theme_opt in chunk {
                let pal = theme_opt.palette();
                let selected = theme_opt == current;
                let bg = Color::from_rgb8(pal.background.r, pal.background.g, pal.background.b);
                let fg = Color::from_rgb8(pal.foreground.r, pal.foreground.g, pal.foreground.b);
                let acc = Color::from_rgb8(pal.ui_accent.r, pal.ui_accent.g, pal.ui_accent.b);
                let green = Color::from_rgb8(pal.ansi[2].r, pal.ansi[2].g, pal.ansi[2].b);
                let amber = Color::from_rgb8(pal.ansi[3].r, pal.ansi[3].g, pal.ansi[3].b);

                let card = button(
                    row![
                        container(
                            row![status_dot(bg), status_dot(fg), status_dot(acc), status_dot(green), status_dot(amber)]
                                .spacing(4)
                                .align_y(Alignment::Center),
                        )
                        .padding([6, 8])
                        .style(move |_t| container::Style {
                            background: Some(Background::Color(bg)),
                            border: Border {
                                color: chrome.border_subtle,
                                width: 1.0,
                                radius: 5.0.into(),
                            },
                            ..Default::default()
                        }),
                        column![
                            text(theme_opt.name()).size(13),
                            text(if pal.is_light { "Light theme" } else { "Dark theme" })
                                .size(10)
                                .color(chrome.text_muted),
                        ]
                        .spacing(1),
                        space::horizontal(),
                    ]
                    .spacing(10)
                    .align_y(Alignment::Center),
                )
                .padding([8, 10])
                .width(Length::Fill)
                .style(pill_button_style(
                    chrome,
                    if selected {
                        PillTone::ActiveAccent
                    } else {
                        PillTone::Subtle
                    },
                ))
                .on_press(Message::SettingsThemeSelected(theme_opt));

                r = r.push(card);
            }
            swatches_col = swatches_col.push(r);
        }

        let font_row = row![
            text("Font family").size(12).width(Length::Fixed(120.0)).color(chrome.text_secondary),
            text_input("monospace", &self.draft.terminal.font_family)
                .on_input(Message::SettingsFontFamilyChanged)
                .size(12)
                .padding([6, 10])
                .style(styled_input_style(chrome))
                .width(Length::Fill),
        ]
        .spacing(8)
        .align_y(Alignment::Center);

        let font_size_row = row![
            text("Font size").size(12).width(Length::Fixed(120.0)).color(chrome.text_secondary),
            button(text("-").size(13))
                .padding([4, 10])
                .style(pill_button_style(chrome, PillTone::Subtle))
                .on_press(Message::SettingsStepFontSize(-1.0)),
            text_input("14", &self.font_size_text)
                .on_input(Message::SettingsFontSizeChanged)
                .width(Length::Fixed(64.0))
                .size(12)
                .padding([6, 8])
                .style(styled_input_style(chrome)),
            button(text("+").size(13))
                .padding([4, 10])
                .style(pill_button_style(chrome, PillTone::Subtle))
                .on_press(Message::SettingsStepFontSize(1.0)),
            text("px").size(11).color(chrome.text_muted),
            space::horizontal(),
            checkbox(self.draft.terminal.font_ligatures)
                .label("Font ligatures")
                .on_toggle(Message::SettingsLigaturesToggled)
                .size(15),
        ]
        .spacing(8)
        .align_y(Alignment::Center);

        let cursor_row = row![
            text("Cursor style").size(12).width(Length::Fixed(120.0)).color(chrome.text_secondary),
            cursor_btn("Block", CursorStyle::Block, self.draft.terminal.cursor_style, chrome),
            cursor_btn("Underline", CursorStyle::Underline, self.draft.terminal.cursor_style, chrome),
            cursor_btn("Beam", CursorStyle::Beam, self.draft.terminal.cursor_style, chrome),
        ]
        .spacing(8)
        .align_y(Alignment::Center);

        column![
            section_heading("Color Theme", "Applies live across terminal grids and chrome", chrome),
            swatches_col,
            rule::horizontal(1),
            section_heading("Typography & Cursor", "Monospace terminal rendering", chrome),
            font_row,
            font_size_row,
            cursor_row,
        ]
        .spacing(12)
        .into()
    }

    fn view_terminal(&self, chrome: ChromePalette) -> Element<'_, Message, Theme, iced::Renderer> {
        let shell_val = self.draft.terminal.shell.as_deref().unwrap_or("");
        column![
            section_heading("Shell & Scrollback", "Local PTY behavior", chrome),
            row![
                text("Default shell").size(12).width(Length::Fixed(130.0)).color(chrome.text_secondary),
                text_input("Auto-detect OS shell", shell_val)
                    .on_input(Message::SettingsShellChanged)
                    .size(12)
                    .padding([6, 10])
                    .style(styled_input_style(chrome))
                    .width(Length::Fill),
            ]
            .spacing(8)
            .align_y(Alignment::Center),
            row![
                text("Scrollback lines").size(12).width(Length::Fixed(130.0)).color(chrome.text_secondary),
                text_input("10000", &self.scrollback_text)
                    .on_input(Message::SettingsScrollbackChanged)
                    .width(Length::Fixed(120.0))
                    .size(12)
                    .padding([6, 10])
                    .style(styled_input_style(chrome)),
                text("Default: 10,000 lines")
                    .size(11)
                    .color(chrome.text_muted),
            ]
            .spacing(8)
            .align_y(Alignment::Center),
            rule::horizontal(1),
            section_heading("Clipboard & Safety", "Selection and paste guards", chrome),
            checkbox(self.draft.terminal.copy_on_select)
                .label("Copy terminal selection to clipboard automatically")
                .on_toggle(Message::SettingsCopyOnSelectToggled)
                .size(15),
            checkbox(self.draft.terminal.confirm_multiline_paste)
                .label("Prompt before pasting multi-line text into terminal")
                .on_toggle(Message::SettingsConfirmMultilinePasteToggled)
                .size(15),
        ]
        .spacing(12)
        .into()
    }

    fn view_network_and_sftp(
        &self,
        chrome: ChromePalette,
    ) -> Element<'_, Message, Theme, iced::Renderer> {
        let policies = [
            ("Ask", TransferConflictPolicy::Ask),
            ("Overwrite", TransferConflictPolicy::Overwrite),
            ("Resume", TransferConflictPolicy::Resume),
            ("Skip", TransferConflictPolicy::Skip),
            ("Rename", TransferConflictPolicy::Rename),
        ];
        let mut policy_row = row![].spacing(6).align_y(Alignment::Center);
        for (label, policy) in policies {
            let active = self.draft.transfer.default_conflict_policy == policy;
            policy_row = policy_row.push(
                button(text(label).size(11))
                    .padding([4, 9])
                    .style(pill_button_style(
                        chrome,
                        if active {
                            PillTone::Primary
                        } else {
                            PillTone::Subtle
                        },
                    ))
                    .on_press(Message::SettingsConflictPolicySelected(policy)),
            );
        }

        column![
            section_heading("SSH Connection Supervision", "Global keepalive probe defaults", chrome),
            row![
                text("Keepalive interval (s)").size(12).width(Length::Fixed(155.0)).color(chrome.text_secondary),
                text_input("15", &self.keepalive_interval_text)
                    .on_input(Message::SettingsKeepaliveIntervalChanged)
                    .width(Length::Fixed(90.0))
                    .size(12)
                    .padding([6, 10])
                    .style(styled_input_style(chrome)),
                text("0 disables keepalives").size(11).color(chrome.text_muted),
            ]
            .spacing(8)
            .align_y(Alignment::Center),
            row![
                text("Max missed probes").size(12).width(Length::Fixed(155.0)).color(chrome.text_secondary),
                text_input("3", &self.keepalive_missed_text)
                    .on_input(Message::SettingsKeepaliveMissedChanged)
                    .width(Length::Fixed(90.0))
                    .size(12)
                    .padding([6, 10])
                    .style(styled_input_style(chrome)),
                text("Triggers reconnect after ~45s default")
                    .size(11)
                    .color(chrome.text_muted),
            ]
            .spacing(8)
            .align_y(Alignment::Center),
            rule::horizontal(1),
            section_heading("SFTP Transfers", "Collision handling and directory traversal", chrome),
            row![
                text("File conflict default").size(12).width(Length::Fixed(155.0)).color(chrome.text_secondary),
                policy_row,
            ]
            .spacing(8)
            .align_y(Alignment::Center),
            checkbox(self.draft.transfer.follow_symlinks)
                .label("Follow symlinks during recursive SFTP transfers (loop-guarded)")
                .on_toggle(Message::SettingsFollowSymlinksToggled)
                .size(15),
        ]
        .spacing(12)
        .into()
    }

    fn view_vault<'a>(
        &'a self,
        chrome: ChromePalette,
        vault: &VaultSession,
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
            text(status_line).size(12).color(chrome.text_primary),
            space::horizontal(),
        ]
        .spacing(8)
        .align_y(Alignment::Center);

        if vault.backend() == VaultBackend::EncryptedFile && vault.is_unlocked() {
            status_row = status_row.push(
                button(text("Lock Vault Now").size(11))
                    .padding([4, 10])
                    .style(pill_button_style(chrome, PillTone::Danger))
                    .on_press(Message::VaultLock),
            );
        }

        column![
            section_heading("Secrets Storage", "Passwords and passphrases are never stored in TOML", chrome),
            container(status_row)
                .padding([8, 12])
                .width(Length::Fill)
                .style(move |_t| card_container_style(chrome, false)),
            row![
                button(
                    column![
                        text("OS Keyring").size(13),
                        text("Secret Service / Keychain / Credential Manager")
                            .size(11)
                            .color(chrome.text_muted),
                    ]
                    .spacing(2),
                )
                .padding([8, 12])
                .width(Length::Fill)
                .style(pill_button_style(
                    chrome,
                    if keyring_active {
                        PillTone::ActiveAccent
                    } else {
                        PillTone::Subtle
                    },
                ))
                .on_press(Message::SettingsVaultBackendSelected(VaultBackend::Keyring)),
                button(
                    column![
                        text("Encrypted Local Vault").size(13),
                        text("Argon2id + AES-256-GCM Master Password")
                            .size(11)
                            .color(chrome.text_muted),
                    ]
                    .spacing(2),
                )
                .padding([8, 12])
                .width(Length::Fill)
                .style(pill_button_style(
                    chrome,
                    if encrypted_active {
                        PillTone::ActiveAccent
                    } else {
                        PillTone::Subtle
                    },
                ))
                .on_press(Message::SettingsVaultBackendSelected(VaultBackend::EncryptedFile)),
            ]
            .spacing(8),
            row![
                text("Auto-lock idle (min)").size(12).width(Length::Fixed(150.0)).color(chrome.text_secondary),
                text_input("30", &self.auto_lock_text)
                    .on_input(Message::SettingsAutoLockChanged)
                    .width(Length::Fixed(90.0))
                    .size(12)
                    .padding([6, 10])
                    .style(styled_input_style(chrome)),
                text("0 disables idle auto-lock").size(11).color(chrome.text_muted),
            ]
            .spacing(8)
            .align_y(Alignment::Center),
        ]
        .spacing(12)
        .into()
    }
}

fn section_heading<'a>(
    title: &'static str,
    subtitle: &'static str,
    chrome: ChromePalette,
) -> Element<'a, Message, Theme, iced::Renderer> {
    column![
        text(title).size(13).color(chrome.text_primary),
        text(subtitle).size(11).color(chrome.text_muted),
    ]
    .spacing(1)
    .into()
}

fn cursor_btn<'a>(
    label: &'static str,
    style: CursorStyle,
    current: CursorStyle,
    chrome: ChromePalette,
) -> Element<'a, Message, Theme, iced::Renderer> {
    let active = style == current;
    button(text(label).size(12))
        .padding([5, 12])
        .style(pill_button_style(
            chrome,
            if active {
                PillTone::Primary
            } else {
                PillTone::Subtle
            },
        ))
        .on_press(Message::SettingsCursorStyleSelected(style))
        .into()
}
