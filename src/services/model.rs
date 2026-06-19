//! Model and reasoning-effort selection for a chat's Codex sessions.
//!
//! Depends only on storage and the Codex catalog — no Telegram transport.

use crate::{
    codex::CodexApi,
    domain::TelegramChatId,
    error::{AppError, AppResult},
    storage::Storage,
    telegram::{InlineKeyboardMarkup, button},
};

pub enum ModelCallbackResult {
    /// Advance to the reasoning-level step (step 2) with new buttons.
    Render(String, InlineKeyboardMarkup),
    /// Finalize the selection, replacing the message with a confirmation.
    Replace(String),
}

#[derive(Clone)]
pub struct ModelService<Cx: CodexApi> {
    storage: Storage,
    codex: Cx,
}

impl<Cx: CodexApi> ModelService<Cx> {
    pub fn new(storage: Storage, codex: Cx) -> Self {
        Self { storage, codex }
    }

    /// Resolve the workspace whose Codex catalog should back the model picker:
    /// the active session's directory, or the current directory when none.
    async fn chat_workspace(&self, chat_id: TelegramChatId) -> AppResult<String> {
        Ok(self
            .storage
            .get_active_session_for_chat(chat_id)
            .await?
            .map(|session| session.workspace_path.0)
            .unwrap_or_else(|| ".".to_string()))
    }

    /// Build the `/model` picker (step 1): the current selection plus the model
    /// catalog reported by Codex core, rendered as tap-to-select buttons.
    pub async fn model_menu(
        &self,
        chat_id: TelegramChatId,
    ) -> AppResult<(String, Option<InlineKeyboardMarkup>)> {
        let chat = self.storage.get_chat(chat_id).await?;
        let current = chat.as_ref().and_then(|chat| chat.model.clone());
        let current_effort = chat.as_ref().and_then(|chat| chat.reasoning_effort.clone());

        let workspace = self.chat_workspace(chat_id).await?;
        let models = self.codex.list_models(&workspace).await?;

        let model_label = current.as_deref().unwrap_or("Codex default");
        let effort_label = current_effort.as_deref().unwrap_or("model default");
        let mut text = format!("Current model: {model_label}\nThinking level: {effort_label}\n");
        if models.is_empty() {
            text.push_str("\nCodex reported no models. Set one with /model <name>.");
            return Ok((text, None));
        }
        text.push_str("\nPick a model:");

        let mut buttons = Vec::new();
        for model in &models {
            let selected = current.as_deref() == Some(model.model.as_str());
            let mut label = model.display_name.clone();
            if model.is_default {
                label.push_str(" (default)");
            }
            if selected {
                label = format!("✓ {label}");
            }
            buttons.push(button(label, format!("model-set:{}", model.model)));
        }
        Ok((text, Some(InlineKeyboardMarkup::single_column(buttons))))
    }

    /// Persist a model picked from the step-1 buttons, then either advance to
    /// the reasoning-level step (step 2) when the model supports it, or finalize.
    pub async fn select_chat_model(
        &self,
        chat_id: TelegramChatId,
        model: &str,
    ) -> AppResult<ModelCallbackResult> {
        // Persisting the model also clears any prior effort (it is model-specific).
        self.storage.set_chat_model(chat_id, Some(model)).await?;

        let workspace = self.chat_workspace(chat_id).await?;
        let chosen = self
            .codex
            .list_models(&workspace)
            .await?
            .into_iter()
            .find(|entry| entry.model == model);

        let Some(chosen) = chosen else {
            // Model not in the catalog (e.g. stale list); keep it as-is.
            return Ok(ModelCallbackResult::Replace(format!(
                "Model set to {model}. It applies to the next turn."
            )));
        };

        if chosen.supported_reasoning_efforts.is_empty() {
            return Ok(ModelCallbackResult::Replace(format!(
                "Model set to {model}. It applies to the next turn."
            )));
        }

        // Seed the model's default effort so a sensible value is stored even if
        // the user dismisses step 2 without choosing.
        if let Some(default_effort) = chosen.default_reasoning_effort.as_deref() {
            self.storage
                .set_chat_reasoning_effort(chat_id, Some(default_effort))
                .await?;
        }

        let text = format!("Model set to {model}.\nNow pick a thinking level:");
        let mut buttons = Vec::new();
        for effort in &chosen.supported_reasoning_efforts {
            let is_default =
                chosen.default_reasoning_effort.as_deref() == Some(effort.effort.as_str());
            let mut label = if effort.description.is_empty() {
                effort.effort.clone()
            } else {
                format!("{} — {}", effort.effort, effort.description)
            };
            if is_default {
                label.push_str(" (default)");
            }
            buttons.push(button(label, format!("model-effort:{}", effort.effort)));
        }
        Ok(ModelCallbackResult::Render(
            text,
            InlineKeyboardMarkup::single_column(buttons),
        ))
    }

    /// Persist a reasoning level picked from the step-2 buttons and finalize.
    pub async fn select_chat_reasoning_effort(
        &self,
        chat_id: TelegramChatId,
        effort: &str,
    ) -> AppResult<String> {
        self.storage
            .set_chat_reasoning_effort(chat_id, Some(effort))
            .await?;
        let model = self
            .storage
            .get_chat(chat_id)
            .await?
            .and_then(|chat| chat.model)
            .unwrap_or_else(|| "Codex default".to_string());
        Ok(format!(
            "Model {model}, thinking level {effort}. It applies to the next turn."
        ))
    }

    /// Persist a model set directly via `/model <name>`. The reasoning level is
    /// left at the model's default (cleared); use /model buttons to tune it.
    pub async fn set_chat_model_by_name(
        &self,
        chat_id: TelegramChatId,
        model: &str,
    ) -> AppResult<String> {
        let model = model.trim();
        if model.is_empty() {
            return Err(AppError::Validation("model name cannot be empty".into()));
        }
        self.storage.set_chat_model(chat_id, Some(model)).await?;
        Ok(format!(
            "Model set to {model}. Use /model to also pick a thinking level. It applies to the next turn."
        ))
    }

    /// Returns Codex's default model for a workspace (the entry it flags as
    /// default, otherwise the first one), or None if Codex reports no models.
    async fn resolve_default_model(&self, workspace: &str) -> AppResult<Option<String>> {
        let models = self.codex.list_models(workspace).await?;
        Ok(models
            .iter()
            .find(|entry| entry.is_default)
            .or_else(|| models.first())
            .map(|entry| entry.model.clone()))
    }

    /// Ensures the chat has a model selected, defaulting to Codex's default for
    /// the workspace when none is set. The Codex app-server requires a model on
    /// every turn, so a chat must never reach a turn without one. Returns the
    /// effective model name when known.
    pub async fn ensure_chat_model(
        &self,
        chat_id: TelegramChatId,
        workspace: &str,
    ) -> AppResult<Option<String>> {
        if let Some(model) = self
            .storage
            .get_chat(chat_id)
            .await?
            .and_then(|chat| chat.model)
        {
            return Ok(Some(model));
        }
        let Some(default_model) = self.resolve_default_model(workspace).await? else {
            return Ok(None);
        };
        self.storage
            .set_chat_model(chat_id, Some(&default_model))
            .await?;
        Ok(Some(default_model))
    }
}
