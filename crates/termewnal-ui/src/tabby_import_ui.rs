use std::path::PathBuf;

use iced::widget::{Id, button, column, container, row, rule, scrollable, space, text, text_input};
use iced::{Alignment, Background, Border, Element, Length, Theme};
use termewnal_core::{
    TabbyCredentialKind, TabbyImportOutcome, TabbyVaultStatus, ThemePalette,
    default_tabby_config_path,
};
use zeroize::{Zeroize, Zeroizing};

use crate::app::Message;
use crate::ui_theme::{
    ChromePalette, PillTone, card_container_style, modal_card_style, modal_overlay,
    pill_button_style, styled_input_style,
};

const MODAL_WIDTH: f32 = 580.0;

/// Returns the static widget [`Id`] for the Tabby `config.yaml` path input.
pub const fn tabby_import_path_input_id() -> Id {
    Id::new("tabby-import-path-input")
}

/// Per-profile credential row surfaced when Tabby's encrypted vault holds locked credentials.
#[derive(Debug, Clone)]
pub struct TabbyCredentialPromptRow {
    pub profile_id: String,
    pub profile_name: String,
    pub target_display: String,
    pub secret_ref: String,
    pub kind: TabbyCredentialKind,
    pub secret_draft: Zeroizing<String>,
    pub saved: bool,
    pub error: Option<String>,
}

/// Summary of a completed Tabby `config.yaml` import.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TabbyImportSummary {
    pub source_path: PathBuf,
    pub added_count: usize,
    pub skipped_duplicate_count: usize,
    pub imported_tunnel_count: usize,
    pub skipped_unsupported_count: usize,
    pub vault_status: TabbyVaultStatus,
    pub vault_notice: Option<String>,
}

/// Modal state backing 1-click Tabby `config.yaml` migration and locked-vault credential prompts.
#[derive(Debug, Default)]
pub struct TabbyImportUiState {
    open: bool,
    path_input: String,
    import_error: Option<String>,
    summary: Option<TabbyImportSummary>,
    credential_rows: Vec<TabbyCredentialPromptRow>,
}

impl TabbyImportUiState {
    /// Returns `true` when the Tabby importer modal is open.
    pub fn is_open(&self) -> bool {
        self.open
    }

    /// Returns the current Tabby `config.yaml` path text buffer.
    pub fn path_input(&self) -> &str {
        &self.path_input
    }

    /// Returns the latest import error displayed inside the modal, if any.
    pub fn import_error(&self) -> Option<&str> {
        self.import_error.as_deref()
    }

    /// Returns the most recent completed import summary, if any.
    pub fn summary(&self) -> Option<&TabbyImportSummary> {
        self.summary.as_ref()
    }

    /// Returns locked/missing credential rows awaiting user input into Termewnal's `Vault`.
    pub fn credential_rows(&self) -> &[TabbyCredentialPromptRow] {
        &self.credential_rows
    }

    /// Opens the Tabby importer modal seeded with the OS default `config.yaml` path if available.
    pub fn open(&mut self) {
        self.open = true;
        self.import_error = None;
        if self.path_input.trim().is_empty()
            && let Some(default_path) = default_tabby_config_path()
        {
            self.path_input = default_path.to_string_lossy().into_owned();
        }
    }

    /// Opens the modal explicitly targeting `path`.
    pub fn open_with_path(&mut self, path: impl Into<String>) {
        self.open = true;
        self.path_input = path.into();
        self.import_error = None;
    }

    /// Closes the modal and zeroizes any un-saved credential input drafts.
    pub fn close(&mut self) {
        self.clear_drafts();
        self.open = false;
        self.import_error = None;
    }

    /// Updates the Tabby `config.yaml` path input and clears stale errors.
    pub fn set_path_input(&mut self, path: impl Into<String>) {
        self.path_input = path.into();
        self.import_error = None;
    }

    /// Records an import failure message without altering existing saved `Profile`s.
    pub fn set_import_error(&mut self, err: impl Into<String>) {
        self.import_error = Some(err.into());
    }

    /// Records a completed [`TabbyImportOutcome`] and builds locked-vault credential prompts.
    pub fn record_outcome(&mut self, source_path: PathBuf, outcome: &TabbyImportOutcome) {
        self.clear_drafts();
        self.import_error = None;

        let vault_notice = match &outcome.vault_status {
            TabbyVaultStatus::Locked { reason } => Some(format!(
                "{reason} Supply credentials below to store them in Termewnal's Vault, or close to enter them when connecting."
            )),
            TabbyVaultStatus::MissingCredentials => Some(
                "One or more imported SSH profiles require credentials not stored in Tabby plaintext. Enter them below to save to Termewnal's Vault."
                    .to_string(),
            ),
            TabbyVaultStatus::None => None,
        };

        self.summary = Some(TabbyImportSummary {
            source_path,
            added_count: outcome.added_count,
            skipped_duplicate_count: outcome.skipped_duplicate_count,
            imported_tunnel_count: outcome.imported_tunnel_count,
            skipped_unsupported_count: outcome.skipped_unsupported_count,
            vault_status: outcome.vault_status.clone(),
            vault_notice,
        });

        self.credential_rows = outcome
            .missing_credentials
            .iter()
            .map(|req| TabbyCredentialPromptRow {
                profile_id: req.profile_id.clone(),
                profile_name: req.profile_name.clone(),
                target_display: req.target_display.clone(),
                secret_ref: req.secret_ref.clone(),
                kind: req.kind,
                secret_draft: Zeroizing::new(String::new()),
                saved: false,
                error: None,
            })
            .collect();
    }

    /// Updates the masked secret draft for `profile_id`.
    pub fn set_credential_draft(&mut self, profile_id: &str, value: String) {
        if let Some(row) = self
            .credential_rows
            .iter_mut()
            .find(|r| r.profile_id == profile_id)
        {
            row.secret_draft.zeroize();
            *row.secret_draft = value;
            row.error = None;
            row.saved = false;
        }
    }

    /// Looks up `(secret_ref, draft)` for `profile_id`.
    pub fn credential_entry(&self, profile_id: &str) -> Option<(&str, &str)> {
        self.credential_rows
            .iter()
            .find(|r| r.profile_id == profile_id)
            .map(|r| (r.secret_ref.as_str(), r.secret_draft.as_str()))
    }

    /// Marks `profile_id`'s credential saved and zeroizes its draft buffer.
    pub fn mark_credential_saved(&mut self, profile_id: &str) {
        if let Some(row) = self
            .credential_rows
            .iter_mut()
            .find(|r| r.profile_id == profile_id)
        {
            row.secret_draft.zeroize();
            row.saved = true;
            row.error = None;
        }
    }

    /// Records a validation or Vault error for `profile_id`.
    pub fn set_credential_error(&mut self, profile_id: &str, err: impl Into<String>) {
        if let Some(row) = self
            .credential_rows
            .iter_mut()
            .find(|r| r.profile_id == profile_id)
        {
            row.error = Some(err.into());
        }
    }

    fn clear_drafts(&mut self) {
        for row in &mut self.credential_rows {
            row.secret_draft.zeroize();
        }
        self.credential_rows.clear();
    }

    /// Renders the centered Tabby `config.yaml` importer modal and locked-vault prompt.
    pub fn view(
        &self,
        palette: &ThemePalette,
        vault_requires_unlock: bool,
    ) -> Element<'_, Message, Theme, iced::Renderer> {
        let chrome = ChromePalette::from_palette(palette);

        let header = row![
            text("Import Tabby config.yaml")
                .size(16)
                .color(chrome.text_primary),
            space::horizontal(),
            button(text("Close").size(12))
                .padding([4, 10])
                .style(pill_button_style(chrome, PillTone::Ghost))
                .on_press(Message::CloseTabbyImportModal),
        ]
        .align_y(Alignment::Center);

        let path_row = row![
            text_input("/path/to/tabby/config.yaml", &self.path_input)
                .id(tabby_import_path_input_id())
                .on_input(Message::TabbyImportPathChanged)
                .on_submit(Message::SubmitTabbyImport)
                .size(13)
                .padding([7, 10])
                .style(styled_input_style(chrome))
                .width(Length::Fill),
            button(text("Import").size(12))
                .padding([7, 14])
                .style(pill_button_style(chrome, PillTone::Primary))
                .on_press(Message::SubmitTabbyImport),
        ]
        .spacing(8)
        .align_y(Alignment::Center);

        let mut body = column![
            header,
            text("Migrates SSH & Local Profiles, groups/tags, and Tunnel rules into profiles.toml (ignores plugin settings).")
                .size(12)
                .color(chrome.text_muted),
            path_row,
        ]
        .spacing(10);

        if let Some(err) = &self.import_error {
            body = body.push(text(err.as_str()).size(12).color(chrome.danger));
        }

        if let Some(summary) = &self.summary {
            body = body.push(self.view_summary(summary, chrome, vault_requires_unlock));
        }

        let card = container(body)
            .width(Length::Fixed(MODAL_WIDTH))
            .padding(16)
            .style(move |_theme: &Theme| modal_card_style(chrome, true));

        modal_overlay(card.into(), Some(Message::CloseTabbyImportModal), 24.0)
    }

    fn view_summary<'a>(
        &'a self,
        summary: &'a TabbyImportSummary,
        chrome: ChromePalette,
        vault_requires_unlock: bool,
    ) -> Element<'a, Message, Theme, iced::Renderer> {
        let mut section = column![
            rule::horizontal(1),
            text(format!(
                "Imported {} profile(s), {} tunnel(s) (skipped {} duplicate(s), {} unsupported).",
                summary.added_count,
                summary.imported_tunnel_count,
                summary.skipped_duplicate_count,
                summary.skipped_unsupported_count
            ))
            .size(12)
            .color(chrome.success),
        ]
        .spacing(10);

        if let Some(notice) = &summary.vault_notice {
            section = section.push(
                container(text(notice.as_str()).size(12).color(chrome.warning))
                    .width(Length::Fill)
                    .padding([8, 10])
                    .style(move |_theme: &Theme| container::Style {
                        background: Some(Background::Color(chrome.warning_soft)),
                        border: Border {
                            color: chrome.warning,
                            width: 1.0,
                            radius: 6.0.into(),
                        },
                        ..Default::default()
                    }),
            );
        }

        if vault_requires_unlock && !self.credential_rows.is_empty() {
            section = section.push(
                text("Termewnal's local Vault is locked — unlock it in the top banner before saving secrets.")
                    .size(12)
                    .color(chrome.danger),
            );
        }

        if !self.credential_rows.is_empty() {
            let mut rows_col = column![].spacing(8);
            for row_item in &self.credential_rows {
                rows_col = rows_col.push(view_credential_row(row_item, chrome));
            }
            section = section.push(
                scrollable(rows_col)
                    .width(Length::Fill)
                    .height(Length::Fixed(190.0)),
            );
        }

        section.into()
    }
}

fn view_credential_row<'a>(
    row_item: &'a TabbyCredentialPromptRow,
    chrome: ChromePalette,
) -> Element<'a, Message, Theme, iced::Renderer> {
    let (kind_label, placeholder) = match row_item.kind {
        TabbyCredentialKind::Password => ("Password", "Enter SSH password..."),
        TabbyCredentialKind::KeyPassphrase => ("Key Passphrase", "Enter key passphrase..."),
    };
    let pid = row_item.profile_id.clone();
    let pid_save = pid.clone();

    let action_elem: Element<'_, Message, Theme, iced::Renderer> = if row_item.saved {
        text("Saved to Vault").size(12).color(chrome.success).into()
    } else {
        button(text("Save to Vault").size(12))
            .padding([5, 10])
            .style(pill_button_style(chrome, PillTone::Primary))
            .on_press(Message::SaveTabbyVaultCredential(pid_save))
            .into()
    };

    let input_row = row![
        text_input(placeholder, row_item.secret_draft.as_str())
            .secure(true)
            .size(12)
            .padding([6, 8])
            .style(styled_input_style(chrome))
            .width(Length::Fill)
            .on_input(move |value| Message::TabbyVaultCredentialChanged {
                profile_id: pid.clone(),
                value,
            })
            .on_submit(Message::SaveTabbyVaultCredential(row_item.profile_id.clone())),
        action_elem,
    ]
    .spacing(6)
    .align_y(Alignment::Center);

    let mut entry_col = column![
        text(format!(
            "{} ({}) — {kind_label}",
            row_item.profile_name, row_item.target_display
        ))
        .size(12)
        .color(chrome.text_primary),
        input_row,
    ]
    .spacing(6);

    if let Some(err) = &row_item.error {
        entry_col = entry_col.push(text(err.as_str()).size(11).color(chrome.danger));
    }

    container(entry_col)
        .padding([8, 10])
        .width(Length::Fill)
        .style(move |_| card_container_style(chrome, false))
        .into()
}
