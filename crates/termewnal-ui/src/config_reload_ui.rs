use std::collections::BTreeMap;

use iced::widget::{button, column, container, row, space, text, text_input};
use iced::{Alignment, Border, Element, Length, Theme};
use termewnal_core::{
    AppConfig, ConfigFileKind, ConfigPaths, ProfilesConfig, RestoredSnapshot, ThemePalette,
    TomlReloadError, VaultSession, reload_config_snapshot, reload_profiles_snapshot,
    restore_from_backup,
};
use zeroize::{Zeroize, Zeroizing};

use crate::app::Message;
use crate::ui_theme::{ChromePalette, PillTone, pill_button_style, status_dot, styled_input_style};

/// Tracks safe TOML hot-reload parse error toasts and local `Vault` unlock bar state for the UI.
#[derive(Debug, Default)]
pub struct ConfigReloadUiState {
    errors: BTreeMap<ConfigFileKind, TomlReloadError>,
    vault_password_draft: Zeroizing<String>,
    vault_error: Option<String>,
}

impl ConfigReloadUiState {
    /// Returns active hot-reload parse errors keyed by config file kind.
    pub fn errors(&self) -> &BTreeMap<ConfigFileKind, TomlReloadError> {
        &self.errors
    }

    /// Clears the parse error toast for `kind`.
    pub fn dismiss_error(&mut self, kind: ConfigFileKind) {
        self.errors.remove(&kind);
    }

    /// Updates the masked master-password input buffer.
    pub fn set_vault_password_draft(&mut self, value: String) {
        self.vault_password_draft.zeroize();
        *self.vault_password_draft = value;
    }

    /// Clears and zeroizes the masked master-password input buffer and any unlock error.
    pub fn clear_vault_prompt(&mut self) {
        self.vault_password_draft.zeroize();
        self.vault_error = None;
    }

    /// Records a Vault unlock/initialization error message for banner display.
    pub fn set_vault_error(&mut self, err: Option<String>) {
        self.vault_error = err;
    }

    /// Reloads `config.toml` into `active_config` and updates `vault` settings without disturbing
    /// running `Session`s. Retains `active_config` and records a toast error on invalid TOML.
    pub fn reload_config(
        &mut self,
        paths: &ConfigPaths,
        active_config: &mut AppConfig,
        vault: &VaultSession,
    ) -> Result<bool, TomlReloadError> {
        match reload_config_snapshot(paths) {
            Ok(next) => {
                let changed = next != *active_config;
                *active_config = next;
                vault.apply_config(&active_config.vault);
                self.errors.remove(&ConfigFileKind::AppConfig);
                Ok(changed)
            }
            Err(err) => {
                self.errors.insert(ConfigFileKind::AppConfig, err.clone());
                Err(err)
            }
        }
    }

    /// Reloads `profiles.toml` into `active_profiles` without disturbing running `Session`s.
    /// Retains `active_profiles` and records a toast error on invalid TOML.
    pub fn reload_profiles(
        &mut self,
        paths: &ConfigPaths,
        active_profiles: &mut ProfilesConfig,
    ) -> Result<bool, TomlReloadError> {
        match reload_profiles_snapshot(paths) {
            Ok(next) => {
                let changed = next != *active_profiles;
                *active_profiles = next;
                self.errors.remove(&ConfigFileKind::Profiles);
                Ok(changed)
            }
            Err(err) => {
                self.errors.insert(ConfigFileKind::Profiles, err.clone());
                Err(err)
            }
        }
    }

    /// Restores `kind` from `.bak.1`, activates the validated snapshot, and dismisses the toast.
    pub fn restore_backup(
        &mut self,
        paths: &ConfigPaths,
        kind: ConfigFileKind,
        active_config: &mut AppConfig,
        active_profiles: &mut ProfilesConfig,
        vault: &VaultSession,
    ) -> Result<(), String> {
        match restore_from_backup(paths, kind) {
            Ok(RestoredSnapshot::AppConfig(cfg)) => {
                *active_config = cfg;
                vault.apply_config(&active_config.vault);
                self.errors.remove(&ConfigFileKind::AppConfig);
                Ok(())
            }
            Ok(RestoredSnapshot::Profiles(profiles)) => {
                *active_profiles = profiles;
                self.errors.remove(&ConfigFileKind::Profiles);
                Ok(())
            }
            Err(err) => Err(err.to_string()),
        }
    }

    /// Renders any active TOML reload parse-error toasts (with 1-click `.bak.1` restore).
    pub fn view_reload_toasts(
        &self,
        palette: &ThemePalette,
    ) -> Option<Element<'_, Message, Theme, iced::Renderer>> {
        if self.errors.is_empty() {
            return None;
        }
        let chrome = ChromePalette::from_palette(palette);

        let mut list = column![].spacing(6).width(Length::Fill);
        for err in self.errors.values() {
            let mut controls = row![].spacing(8).align_y(Alignment::Center);
            if err.backup_available {
                controls = controls.push(
                    button(text("Restore .bak.1").size(12))
                        .padding([4, 10])
                        .style(pill_button_style(chrome, PillTone::Primary))
                        .on_press(Message::RestoreConfigBackup(err.kind)),
                );
            }
            controls = controls.push(
                button(text("Dismiss").size(12))
                    .padding([4, 10])
                    .style(pill_button_style(chrome, PillTone::Subtle))
                    .on_press(Message::DismissReloadToast(err.kind)),
            );

            let banner = container(
                row![
                    status_dot(chrome.danger),
                    text(err.to_string()).size(12).color(chrome.text_primary),
                    space::horizontal(),
                    controls,
                ]
                .spacing(10)
                .align_y(Alignment::Center),
            )
            .width(Length::Fill)
            .padding([8, 12])
            .style(move |_theme: &Theme| container::Style {
                background: Some(iced::Background::Color(chrome.danger_soft)),
                border: Border {
                    color: chrome.danger,
                    width: 1.0,
                    radius: 0.0.into(),
                },
                text_color: Some(chrome.text_primary),
                ..Default::default()
            });

            list = list.push(banner);
        }

        Some(list.into())
    }

    /// Renders a non-modal top banner to unlock or initialize the local encrypted `Vault` when
    /// `EncryptedFile` backend is active and locked.
    pub fn view_vault_banner(
        &self,
        vault: &VaultSession,
        palette: &ThemePalette,
    ) -> Option<Element<'_, Message, Theme, iced::Renderer>> {
        if !vault.requires_unlock() {
            return None;
        }
        let chrome = ChromePalette::from_palette(palette);

        let exists = vault.has_encrypted_vault();
        let action_label = if exists {
            "Unlock Vault"
        } else {
            "Initialize Vault"
        };
        let submit_msg = if exists {
            Message::VaultUnlockSubmitted(self.vault_password_draft.as_str().to_string())
        } else {
            Message::VaultInitializeSubmitted(self.vault_password_draft.as_str().to_string())
        };

        let mut items = row![
            status_dot(chrome.warning),
            text(if exists {
                "Local Vault locked:"
            } else {
                "Create Master Password for local Vault:"
            })
            .size(12)
            .color(chrome.text_primary),
            text_input("Master password...", self.vault_password_draft.as_str())
                .secure(true)
                .width(Length::Fixed(220.0))
                .size(12)
                .padding([5, 8])
                .style(styled_input_style(chrome))
                .on_input(Message::VaultUnlockInputChanged)
                .on_submit(submit_msg.clone()),
            button(text(action_label).size(12))
                .padding([5, 12])
                .style(pill_button_style(chrome, PillTone::Primary))
                .on_press(submit_msg),
        ]
        .spacing(8)
        .align_y(Alignment::Center);

        if let Some(err) = &self.vault_error {
            items = items.push(text(err.as_str()).size(12).color(chrome.danger));
        }

        Some(
            container(items)
                .width(Length::Fill)
                .padding([7, 12])
                .style(move |_theme: &Theme| container::Style {
                    background: Some(iced::Background::Color(chrome.panel_bg)),
                    border: Border {
                        color: chrome.border_subtle,
                        width: 1.0,
                        radius: 0.0.into(),
                    },
                    text_color: Some(chrome.text_primary),
                    ..Default::default()
                })
                .into(),
        )
    }
}
