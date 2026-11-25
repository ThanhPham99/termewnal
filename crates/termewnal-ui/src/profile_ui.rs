//! Tabby-inspired GUI Profile Manager modal for creating, editing, and deleting Saved `Profile`s.

use std::path::PathBuf;

use iced::widget::{
    button, checkbox, column, container, row, rule, scrollable, space, text, text_input,
};
use iced::{Alignment, Element, Length, Theme};
use termewnal_core::{Profile, ProfileTarget, SshAuthMethod, ThemePalette};
use zeroize::Zeroizing;

use crate::app::Message;
use crate::ui_theme::{
    ChromePalette, PillTone, modal_card_style, modal_overlay, pill_button_style, styled_input_style,
};

const PROFILE_MODAL_WIDTH: f32 = 560.0;

/// Connection target kind in the GUI Profile editor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ProfileKindChoice {
    #[default]
    Ssh,
    LocalShell,
}

/// SSH authentication choice in the GUI Profile editor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ProfileAuthChoice {
    #[default]
    Agent,
    PublicKey,
    Password,
    KeyboardInteractive,
}

/// Result produced when saving a [`ProfileEditorState`] draft.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SavedProfileDraft {
    pub profile: Profile,
    pub secret_to_store: Option<(String, Zeroizing<String>)>,
}

/// Interactive state for creating/editing/deleting a Saved `Profile`.
#[derive(Debug, Clone, Default)]
pub struct ProfileEditorState {
    pub open: bool,
    pub editing_id: Option<String>,
    pub name: String,
    pub group: String,
    pub tags: String,
    pub kind: ProfileKindChoice,
    pub local_shell: String,
    pub local_working_dir: String,
    pub ssh_host: String,
    pub ssh_port: String,
    pub ssh_username: String,
    pub auth_choice: ProfileAuthChoice,
    pub key_path: String,
    pub existing_secret_ref: Option<String>,
    pub secret_draft: String,
    pub proxy_jump: String,
    pub auto_connect_on_startup: bool,
    pub auto_attach_mux: bool,
    pub mux_attach_command: String,
    pub validation_error: Option<String>,
}

impl ProfileEditorState {
    /// Returns `true` when the Profile editor modal is open.
    pub fn is_open(&self) -> bool {
        self.open
    }

    /// Opens the modal in "New Profile" mode with sensible defaults.
    pub fn open_new(&mut self, default_user: String) {
        *self = Self {
            open: true,
            editing_id: None,
            kind: ProfileKindChoice::Ssh,
            ssh_port: "22".to_string(),
            ssh_username: default_user,
            auth_choice: ProfileAuthChoice::Agent,
            ..Self::default()
        };
    }

    /// Opens the modal pre-populated from an existing [`Profile`].
    pub fn open_edit(&mut self, profile: &Profile) {
        let mut next = Self {
            open: true,
            editing_id: Some(profile.id.clone()),
            name: profile.name.clone(),
            group: profile.group.clone().unwrap_or_default(),
            tags: profile.tags.join(", "),
            ssh_port: "22".to_string(),
            ..Self::default()
        };

        match &profile.target {
            ProfileTarget::LocalShell {
                shell, working_dir, ..
            } => {
                next.kind = ProfileKindChoice::LocalShell;
                next.local_shell = shell.clone().unwrap_or_default();
                next.local_working_dir = working_dir
                    .as_ref()
                    .map(|p| p.display().to_string())
                    .unwrap_or_default();
            }
            ProfileTarget::Ssh {
                host,
                port,
                username,
                auth,
                proxy_jump,
                auto_connect_on_startup,
                tmux_reattach,
                auto_attach_mux,
                mux_attach_command,
                ..
            } => {
                next.kind = ProfileKindChoice::Ssh;
                next.ssh_host = host.clone();
                next.ssh_port = port.to_string();
                next.ssh_username = username.clone();
                next.proxy_jump = proxy_jump.join(", ");
                next.auto_connect_on_startup = *auto_connect_on_startup;
                next.auto_attach_mux = *auto_attach_mux || tmux_reattach.is_some();
                next.mux_attach_command = mux_attach_command
                    .clone()
                    .or_else(|| tmux_reattach.clone())
                    .unwrap_or_default();
                match auth {
                    SshAuthMethod::Agent => {
                        next.auth_choice = ProfileAuthChoice::Agent;
                    }
                    SshAuthMethod::PublicKey {
                        key_path,
                        vault_secret_ref,
                    } => {
                        next.auth_choice = ProfileAuthChoice::PublicKey;
                        next.key_path = key_path
                            .as_ref()
                            .map(|p| p.display().to_string())
                            .unwrap_or_default();
                        next.existing_secret_ref = vault_secret_ref.clone();
                    }
                    SshAuthMethod::Password { vault_secret_ref } => {
                        next.auth_choice = ProfileAuthChoice::Password;
                        next.existing_secret_ref = vault_secret_ref.clone();
                    }
                    SshAuthMethod::KeyboardInteractive => {
                        next.auth_choice = ProfileAuthChoice::KeyboardInteractive;
                    }
                }
            }
        }
        *self = next;
    }

    /// Closes the modal without saving.
    pub fn close(&mut self) {
        self.open = false;
        self.secret_draft.clear();
        self.validation_error = None;
    }

    /// Validates draft fields and returns a [`SavedProfileDraft`], preserving non-edited
    /// fields (such as existing attached `TunnelRule`s and per-profile keepalive overrides) from `existing`.
    pub fn build_profile(&mut self, existing: Option<&Profile>) -> Result<SavedProfileDraft, String> {
        let name = self.name.trim().to_string();
        if name.is_empty() {
            return Err("Profile name is required".to_string());
        }

        let group = {
            let trimmed = self.group.trim();
            (!trimmed.is_empty()).then(|| trimmed.to_string())
        };
        let tags: Vec<String> = self
            .tags
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(ToString::to_string)
            .collect();

        let mut profile = match self.kind {
            ProfileKindChoice::LocalShell => {
                let shell = {
                    let trimmed = self.local_shell.trim();
                    (!trimmed.is_empty()).then(|| trimmed.to_string())
                };
                let mut p = Profile::local_shell(name, shell);
                if let Some(prev) = existing {
                    p.id = prev.id.clone();
                    p.tunnels = prev.tunnels.clone();
                }
                if let ProfileTarget::LocalShell {
                    working_dir, args, ..
                } = &mut p.target
                {
                    let wd = self.local_working_dir.trim();
                    *working_dir = (!wd.is_empty()).then(|| PathBuf::from(wd));
                    if let Some(prev) = existing
                        && let ProfileTarget::LocalShell {
                            args: prev_args, ..
                        } = &prev.target
                    {
                        *args = prev_args.clone();
                    }
                }
                p.group = group;
                p.tags = tags;
                return Ok(SavedProfileDraft {
                    profile: p,
                    secret_to_store: None,
                });
            }
            ProfileKindChoice::Ssh => {
                let host = self.ssh_host.trim().to_string();
                if host.is_empty() {
                    return Err("SSH Host is required".to_string());
                }
                let port: u16 = self
                    .ssh_port
                    .trim()
                    .parse()
                    .ok()
                    .filter(|&p| p > 0)
                    .ok_or_else(|| "SSH Port must be between 1 and 65535".to_string())?;
                let username = self.ssh_username.trim().to_string();
                if username.is_empty() {
                    return Err("SSH Username is required".to_string());
                }

                let mut p = Profile::ssh(name, host, port, username, SshAuthMethod::Agent);
                if let Some(prev) = existing {
                    p.id = prev.id.clone();
                    p.tunnels = prev.tunnels.clone();
                }
                p
            }
        };

        profile.group = group;
        profile.tags = tags;

        let mut secret_to_store: Option<(String, Zeroizing<String>)> = None;
        let has_secret_draft = !self.secret_draft.is_empty();
        let default_secret_ref = || format!("termewnal/profile/{}", profile.id);

        let auth = match self.auth_choice {
            ProfileAuthChoice::Agent => SshAuthMethod::Agent,
            ProfileAuthChoice::KeyboardInteractive => SshAuthMethod::KeyboardInteractive,
            ProfileAuthChoice::PublicKey => {
                let key_path = {
                    let trimmed = self.key_path.trim();
                    (!trimmed.is_empty()).then(|| PathBuf::from(trimmed))
                };
                let secret_ref = if has_secret_draft {
                    let r = self
                        .existing_secret_ref
                        .clone()
                        .unwrap_or_else(default_secret_ref);
                    secret_to_store =
                        Some((r.clone(), Zeroizing::new(self.secret_draft.clone())));
                    Some(r)
                } else {
                    self.existing_secret_ref.clone()
                };
                SshAuthMethod::PublicKey {
                    key_path,
                    vault_secret_ref: secret_ref,
                }
            }
            ProfileAuthChoice::Password => {
                let secret_ref = if has_secret_draft {
                    let r = self
                        .existing_secret_ref
                        .clone()
                        .unwrap_or_else(default_secret_ref);
                    secret_to_store =
                        Some((r.clone(), Zeroizing::new(self.secret_draft.clone())));
                    Some(r)
                } else {
                    self.existing_secret_ref.clone()
                };
                SshAuthMethod::Password {
                    vault_secret_ref: secret_ref,
                }
            }
        };

        let hops: Vec<String> = self
            .proxy_jump
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(ToString::to_string)
            .collect();
        let mux_cmd = {
            let trimmed = self.mux_attach_command.trim();
            (!trimmed.is_empty()).then(|| trimmed.to_string())
        };

        if let ProfileTarget::Ssh {
            auth: target_auth,
            proxy_jump,
            auto_connect_on_startup,
            auto_attach_mux,
            mux_attach_command,
            keepalive_interval_secs,
            keepalive_max_missed,
            tmux_reattach,
            ..
        } = &mut profile.target
        {
            *target_auth = auth;
            *proxy_jump = hops;
            *auto_connect_on_startup = self.auto_connect_on_startup;
            *auto_attach_mux = self.auto_attach_mux;
            *mux_attach_command = mux_cmd;
            if let Some(prev) = existing
                && let ProfileTarget::Ssh {
                    keepalive_interval_secs: k_int,
                    keepalive_max_missed: k_max,
                    tmux_reattach: prev_tmux,
                    ..
                } = &prev.target
            {
                *keepalive_interval_secs = *k_int;
                *keepalive_max_missed = *k_max;
                *tmux_reattach = prev_tmux.clone();
            }
        }

        self.validation_error = None;
        Ok(SavedProfileDraft {
            profile,
            secret_to_store,
        })
    }

    /// Renders the Profile create/edit modal overlay when open.
    pub fn view<'a>(
        &'a self,
        palette: &ThemePalette,
    ) -> Option<Element<'a, Message, Theme, iced::Renderer>> {
        if !self.open {
            return None;
        }
        let chrome = ChromePalette::from_palette(palette);
        let is_editing = self.editing_id.is_some();

        let header = row![
            column![
                text(if is_editing {
                    "Edit Profile"
                } else {
                    "New Profile"
                })
                .size(16)
                .color(chrome.text_primary),
                text("Saved in profiles.toml; secrets stay isolated in Vault")
                    .size(11)
                    .color(chrome.text_muted),
            ]
            .spacing(2),
            space::horizontal(),
            button(text("Close").size(12))
                .padding([5, 10])
                .style(pill_button_style(chrome, PillTone::Ghost))
                .on_press(Message::CloseProfileEditor),
        ]
        .align_y(Alignment::Center);

        let kind_row = row![
            text("Type").size(12).width(Length::Fixed(105.0)).color(chrome.text_secondary),
            button(text("SSH Host").size(12))
                .padding([5, 12])
                .style(pill_button_style(
                    chrome,
                    if self.kind == ProfileKindChoice::Ssh {
                        PillTone::Primary
                    } else {
                        PillTone::Subtle
                    },
                ))
                .on_press(Message::ProfileFormKindChanged(ProfileKindChoice::Ssh)),
            button(text("Local Shell").size(12))
                .padding([5, 12])
                .style(pill_button_style(
                    chrome,
                    if self.kind == ProfileKindChoice::LocalShell {
                        PillTone::Primary
                    } else {
                        PillTone::Subtle
                    },
                ))
                .on_press(Message::ProfileFormKindChanged(ProfileKindChoice::LocalShell)),
        ]
        .spacing(8)
        .align_y(Alignment::Center);

        let mut form = column![
            kind_row,
            labeled_input("Name", "Prod Bastion", &self.name, Message::ProfileFormNameChanged, chrome),
            row![
                text("Group & Tags").size(12).width(Length::Fixed(105.0)).color(chrome.text_secondary),
                text_input("Group (e.g. Production)", &self.group)
                    .on_input(Message::ProfileFormGroupChanged)
                    .size(12)
                    .padding([6, 8])
                    .style(styled_input_style(chrome))
                    .width(Length::Fill),
                text_input("Tags (comma-separated)", &self.tags)
                    .on_input(Message::ProfileFormTagsChanged)
                    .size(12)
                    .padding([6, 8])
                    .style(styled_input_style(chrome))
                    .width(Length::Fill),
            ]
            .spacing(8)
            .align_y(Alignment::Center),
        ]
        .spacing(10);

        match self.kind {
            ProfileKindChoice::LocalShell => {
                form = form
                    .push(labeled_input(
                        "Shell",
                        "Optional shell path (default OS shell)",
                        &self.local_shell,
                        Message::ProfileFormLocalShellChanged,
                        chrome,
                    ))
                    .push(labeled_input(
                        "Working dir",
                        "Optional startup folder",
                        &self.local_working_dir,
                        Message::ProfileFormLocalDirChanged,
                        chrome,
                    ));
            }
            ProfileKindChoice::Ssh => {
                let auth_pills = row![
                    text("Auth").size(12).width(Length::Fixed(105.0)).color(chrome.text_secondary),
                    auth_btn("Agent", ProfileAuthChoice::Agent, self.auth_choice, chrome),
                    auth_btn("Key", ProfileAuthChoice::PublicKey, self.auth_choice, chrome),
                    auth_btn("Password", ProfileAuthChoice::Password, self.auth_choice, chrome),
                    auth_btn("2FA/KBI", ProfileAuthChoice::KeyboardInteractive, self.auth_choice, chrome),
                ]
                .spacing(6)
                .align_y(Alignment::Center);

                form = form
                    .push(
                        row![
                            text("Host & Port").size(12).width(Length::Fixed(105.0)).color(chrome.text_secondary),
                            text_input("10.0.0.10 or host.example.com", &self.ssh_host)
                                .on_input(Message::ProfileFormHostChanged)
                                .size(12)
                                .padding([6, 8])
                                .style(styled_input_style(chrome))
                                .width(Length::Fill),
                            text_input("22", &self.ssh_port)
                                .on_input(Message::ProfileFormPortChanged)
                                .size(12)
                                .padding([6, 8])
                                .style(styled_input_style(chrome))
                                .width(Length::Fixed(72.0)),
                        ]
                        .spacing(8)
                        .align_y(Alignment::Center),
                    )
                    .push(labeled_input(
                        "Username",
                        "deploy",
                        &self.ssh_username,
                        Message::ProfileFormUsernameChanged,
                        chrome,
                    ))
                    .push(auth_pills);

                if self.auth_choice == ProfileAuthChoice::PublicKey {
                    form = form.push(labeled_input(
                        "Key path",
                        "~/.ssh/id_ed25519",
                        &self.key_path,
                        Message::ProfileFormKeyPathChanged,
                        chrome,
                    ));
                }

                if matches!(
                    self.auth_choice,
                    ProfileAuthChoice::Password | ProfileAuthChoice::PublicKey
                ) {
                    let label = if self.auth_choice == ProfileAuthChoice::Password {
                        "Password"
                    } else {
                        "Passphrase"
                    };
                    let placeholder = if self.existing_secret_ref.is_some() {
                        "Leave empty to keep existing Vault secret"
                    } else {
                        "Optional secret stored in Vault"
                    };
                    form = form.push(
                        row![
                            text(label).size(12).width(Length::Fixed(105.0)).color(chrome.text_secondary),
                            text_input(placeholder, &self.secret_draft)
                                .secure(true)
                                .on_input(Message::ProfileFormSecretChanged)
                                .size(12)
                                .padding([6, 8])
                                .style(styled_input_style(chrome))
                                .width(Length::Fill),
                        ]
                        .spacing(8)
                        .align_y(Alignment::Center),
                    );
                }

                form = form
                    .push(labeled_input(
                        "ProxyJump",
                        "Optional hops: jump1, user@jump2:2222",
                        &self.proxy_jump,
                        Message::ProfileFormProxyJumpChanged,
                        chrome,
                    ))
                    .push(
                        checkbox(self.auto_connect_on_startup)
                            .label("Auto-connect on startup when restoring layout")
                            .on_toggle(Message::ProfileFormAutoConnectToggled)
                            .size(15),
                    )
                    .push(
                        checkbox(self.auto_attach_mux)
                            .label("Auto-attach tmux/screen on reconnect")
                            .on_toggle(Message::ProfileFormAutoMuxToggled)
                            .size(15),
                    );

                if self.auto_attach_mux {
                    form = form.push(labeled_input(
                        "Mux command",
                        "tmux new-session -A -s main",
                        &self.mux_attach_command,
                        Message::ProfileFormMuxCommandChanged,
                        chrome,
                    ));
                }
            }
        }

        let mut footer = row![].spacing(8).align_y(Alignment::Center);
        if let Some(pid) = &self.editing_id {
            footer = footer.push(
                button(text("Delete").size(12))
                    .padding([6, 12])
                    .style(pill_button_style(chrome, PillTone::Danger))
                    .on_press(Message::ProfileDelete(pid.clone())),
            );
        }
        footer = footer
            .push(space::horizontal())
            .push(
                button(text("Cancel").size(12))
                    .padding([6, 12])
                    .style(pill_button_style(chrome, PillTone::Subtle))
                    .on_press(Message::CloseProfileEditor),
            )
            .push(
                button(text("Save Profile").size(12))
                    .padding([6, 16])
                    .style(pill_button_style(chrome, PillTone::Primary))
                    .on_press(Message::ProfileSave),
            );

        let mut body = column![
            header,
            rule::horizontal(1),
            scrollable(form).height(Length::Fixed(360.0)),
        ]
        .spacing(12);

        if let Some(err) = &self.validation_error {
            body = body.push(text(err.as_str()).size(12).color(chrome.danger));
        }

        body = body.push(rule::horizontal(1)).push(footer);

        let card = container(body.padding(16))
            .width(Length::Fixed(PROFILE_MODAL_WIDTH))
            .style(move |_t| modal_card_style(chrome, true));

        Some(modal_overlay(card.into(), Some(Message::CloseProfileEditor), 24.0))
    }
}

fn labeled_input<'a>(
    label: &'static str,
    placeholder: &'static str,
    value: &str,
    on_input: impl Fn(String) -> Message + 'a,
    chrome: ChromePalette,
) -> Element<'a, Message, Theme, iced::Renderer> {
    row![
        text(label).size(12).width(Length::Fixed(105.0)).color(chrome.text_secondary),
        text_input(placeholder, value)
            .on_input(on_input)
            .size(12)
            .padding([6, 8])
            .style(styled_input_style(chrome))
            .width(Length::Fill),
    ]
    .spacing(8)
    .align_y(Alignment::Center)
    .into()
}

fn auth_btn<'a>(
    label: &'static str,
    choice: ProfileAuthChoice,
    current: ProfileAuthChoice,
    chrome: ChromePalette,
) -> Element<'a, Message, Theme, iced::Renderer> {
    button(text(label).size(11))
        .padding([4, 9])
        .style(pill_button_style(
            chrome,
            if choice == current {
                PillTone::Primary
            } else {
                PillTone::Subtle
            },
        ))
        .on_press(Message::ProfileFormAuthChanged(choice))
        .into()
}
