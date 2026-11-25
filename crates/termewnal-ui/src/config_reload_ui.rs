use std::collections::BTreeMap;

use iced::widget::{button, column, container, row, space, text, text_input};
use iced::{Alignment, Border, Color, Element, Length, Theme};
use termewnal_core::{
    AppConfig, ConfigFileKind, ConfigPaths, ProfilesConfig, RestoredSnapshot, TomlReloadError,
    VaultSession, reload_config_snapshot, reload_profiles_snapshot, restore_from_backup,
};
use zeroize::{Zeroize, Zeroizing};

use crate::app::Message;

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
    pub fn view_reload_toasts(&self) -> Option<Element<'_, Message, Theme, iced::Renderer>> {
        if self.errors.is_empty() {
            return None;
        }

        let mut list = column![].spacing(6).width(Length::Fill);
        for err in self.errors.values() {
            let mut controls = row![].spacing(8).align_y(Alignment::Center);
            if err.backup_available {
                controls = controls.push(
                    button(text("Restore .bak.1").size(13))
                        .on_press(Message::RestoreConfigBackup(err.kind)),
                );
            }
            controls = controls.push(
                button(text("Dismiss").size(13))
                    .on_press(Message::DismissReloadToast(err.kind)),
            );

            let banner = container(
                row![
                    text(err.to_string()).size(13),
                    space::horizontal(),
                    controls,
                ]
                .spacing(12)
                .align_y(Alignment::Center),
            )
            .width(Length::Fill)
            .padding([8, 12])
            .style(error_toast_style);

            list = list.push(banner);
        }

        Some(list.into())
    }

    /// Renders a non-modal top banner to unlock or initialize the local encrypted `Vault` when
    /// `EncryptedFile` backend is active and locked.
    pub fn view_vault_banner(
        &self,
        vault: &VaultSession,
    ) -> Option<Element<'_, Message, Theme, iced::Renderer>> {
        if !vault.requires_unlock() {
            return None;
        }

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
            text(if exists {
                "Local Vault locked:"
            } else {
                "Create Master Password for local Vault:"
            })
            .size(13),
            text_input("Master password...", self.vault_password_draft.as_str())
                .secure(true)
                .width(Length::Fixed(220.0))
                .on_input(Message::VaultUnlockInputChanged)
                .on_submit(submit_msg.clone()),
            button(text(action_label).size(13)).on_press(submit_msg),
        ]
        .spacing(8)
        .align_y(Alignment::Center);

        if let Some(err) = &self.vault_error {
            items = items.push(text(err.as_str()).size(12));
        }

        Some(
            container(items)
                .width(Length::Fill)
                .padding([6, 12])
                .style(vault_banner_style)
                .into(),
        )
    }
}

fn error_toast_style(_theme: &Theme) -> container::Style {
    container::Style::default()
        .background(Color::from_rgb8(0x5b, 0x1e, 0x24))
        .color(Color::from_rgb8(0xfd, 0xed, 0xee))
        .border(Border {
            color: Color::from_rgb8(0xd9, 0x53, 0x4f),
            width: 1.0,
            radius: 4.0.into(),
        })
}

fn vault_banner_style(_theme: &Theme) -> container::Style {
    container::Style::default()
        .background(Color::from_rgb8(0x28, 0x30, 0x3d))
        .color(Color::from_rgb8(0xe5, 0xe9, 0xf0))
        .border(Border {
            color: Color::from_rgb8(0x4c, 0x56, 0x6a),
            width: 1.0,
            radius: 0.0.into(),
        })
}
