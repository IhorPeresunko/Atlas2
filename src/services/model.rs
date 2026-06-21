//! Model and reasoning-effort selection for a chat's sessions.
//!
//! Depends only on storage and the provider catalog — no Telegram transport. The
//! catalog is resolved for the active session's provider, so a Codex chat sees
//! Codex models and a Claude chat sees Claude models.

use crate::{
    domain::{ProviderKind, TelegramChatId},
    error::{AppError, AppResult},
    provider::ProviderRegistry,
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
pub struct ModelService {
    storage: Storage,
    providers: ProviderRegistry,
}

impl ModelService {
    pub fn new(storage: Storage, providers: ProviderRegistry) -> Self {
        Self { storage, providers }
    }

    /// Resolve the active session's provider kind and workspace, which together
    /// back the model picker. Returns `None` when the chat has no session yet.
    async fn active_provider(
        &self,
        chat_id: TelegramChatId,
    ) -> AppResult<Option<(ProviderKind, String)>> {
        Ok(self
            .storage
            .get_active_session_for_chat(chat_id)
            .await?
            .map(|session| (session.provider, session.workspace_path.0)))
    }

    /// Build the `/model` picker (step 1): the current selection plus the model
    /// catalog reported by the active session's provider, as tap-to-select
    /// buttons.
    pub async fn model_menu(
        &self,
        chat_id: TelegramChatId,
    ) -> AppResult<(String, Option<InlineKeyboardMarkup>)> {
        let Some((kind, workspace)) = self.active_provider(chat_id).await? else {
            return Ok((
                "No active session yet. Run /new to start one, then pick a model.".into(),
                None,
            ));
        };

        let chat = self.storage.get_chat(chat_id).await?;
        let current = chat.as_ref().and_then(|chat| chat.model.clone());
        let current_effort = chat.as_ref().and_then(|chat| chat.reasoning_effort.clone());

        let models = self.providers.get(kind)?.list_models(&workspace).await?;

        let model_label = current.as_deref().unwrap_or("provider default");
        let effort_label = current_effort.as_deref().unwrap_or("model default");
        let mut text = format!("Current model: {model_label}\nThinking level: {effort_label}\n");
        if models.is_empty() {
            text.push_str("\nNo models reported. Set one with /model <name>.");
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

        let chosen = match self.active_provider(chat_id).await? {
            Some((kind, workspace)) => self
                .providers
                .get(kind)?
                .list_models(&workspace)
                .await?
                .into_iter()
                .find(|entry| entry.model == model),
            None => None,
        };

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
            .unwrap_or_else(|| "provider default".to_string());
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

    /// Returns the provider's default model for a workspace (the entry it flags
    /// as default, otherwise the first one), or None if it reports no models.
    async fn resolve_default_model(
        &self,
        kind: ProviderKind,
        workspace: &str,
    ) -> AppResult<Option<String>> {
        let models = self.providers.get(kind)?.list_models(workspace).await?;
        Ok(models
            .iter()
            .find(|entry| entry.is_default)
            .or_else(|| models.first())
            .map(|entry| entry.model.clone()))
    }

    /// Ensures the chat has a model selected, defaulting to the provider's
    /// default for the workspace when none is set. Some providers (e.g. Codex)
    /// require a model on every turn, so a chat must never reach a turn without
    /// one. Returns the effective model name when known.
    pub async fn ensure_chat_model(
        &self,
        chat_id: TelegramChatId,
        kind: ProviderKind,
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
        let Some(default_model) = self.resolve_default_model(kind, workspace).await? else {
            return Ok(None);
        };
        self.storage
            .set_chat_model(chat_id, Some(&default_model))
            .await?;
        Ok(Some(default_model))
    }

    /// Reconciles the chat's stored model with a newly created session's
    /// provider. The model preference is per-chat but models are
    /// provider-specific, so a model carried over from a previous session (e.g.
    /// a Codex `gpt-5.5` when the new session is Claude) would be rejected at
    /// turn time. When the stored model is not in the new provider's catalog,
    /// replace it with the provider's default (which also resets the
    /// model-specific reasoning effort). Returns the effective model name.
    pub async fn reconcile_model_for_provider(
        &self,
        chat_id: TelegramChatId,
        kind: ProviderKind,
        workspace: &str,
    ) -> AppResult<Option<String>> {
        let models = self.providers.get(kind)?.list_models(workspace).await?;
        let stored = self
            .storage
            .get_chat(chat_id)
            .await?
            .and_then(|chat| chat.model);

        if let Some(model) = stored
            && models.iter().any(|entry| entry.model == model)
        {
            // Stored model is valid for this provider; keep it.
            return Ok(Some(model));
        }

        let default_model = models
            .iter()
            .find(|entry| entry.is_default)
            .or_else(|| models.first())
            .map(|entry| entry.model.clone());
        // Clears the now-invalid model (and its reasoning effort); seeds the
        // provider default when one is available.
        self.storage
            .set_chat_model(chat_id, default_model.as_deref())
            .await?;
        Ok(default_model)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{ApprovalId, PromptMode, SessionId, SessionRecord, UserInputRequestId};
    use crate::provider::{ModelOption, Provider, ProviderEvent, TurnResult};
    use std::collections::HashMap;
    use std::sync::Arc;

    /// Minimal provider that only reports a fixed model catalog; turn methods are
    /// unused by these tests.
    #[derive(Clone)]
    struct CatalogProvider {
        models: Vec<ModelOption>,
    }

    #[async_trait::async_trait]
    impl Provider for CatalogProvider {
        async fn list_models(&self, _workspace_path: &str) -> AppResult<Vec<ModelOption>> {
            Ok(self.models.clone())
        }
        async fn run_turn(
            &self,
            _session: &SessionRecord,
            _prompt: &str,
            _mode: PromptMode,
            _model: Option<&str>,
            _reasoning_effort: Option<&str>,
            _on_event: Box<dyn FnMut(ProviderEvent) -> AppResult<()> + Send>,
        ) -> AppResult<TurnResult> {
            Ok(TurnResult::default())
        }
        async fn resolve_approval(
            &self,
            _session_id: &SessionId,
            _approval_id: &ApprovalId,
            _approved: bool,
        ) -> AppResult<()> {
            Ok(())
        }
        async fn resolve_user_input(
            &self,
            _session_id: &SessionId,
            _request_id: &UserInputRequestId,
            _answers: HashMap<String, UserInputAnswer>,
        ) -> AppResult<()> {
            Ok(())
        }
        async fn stop_turn(&self, _session_id: &SessionId) -> AppResult<()> {
            Ok(())
        }
    }

    use crate::domain::UserInputAnswer;

    fn model(slug: &str, is_default: bool) -> ModelOption {
        ModelOption {
            model: slug.into(),
            display_name: slug.into(),
            is_default,
            default_reasoning_effort: None,
            supported_reasoning_efforts: Vec::new(),
        }
    }

    fn service_with(models: Vec<ModelOption>, storage: Storage) -> ModelService {
        let mut providers: HashMap<ProviderKind, Arc<dyn Provider>> = HashMap::new();
        providers.insert(ProviderKind::Claude, Arc::new(CatalogProvider { models }));
        ModelService::new(storage, ProviderRegistry::new(providers))
    }

    #[tokio::test]
    async fn reconcile_replaces_model_not_in_provider_catalog() {
        let storage = Storage::connect("sqlite::memory:").await.unwrap();
        let chat_id = TelegramChatId(1);
        // A Codex model carried over from a previous session.
        storage.set_chat_model(chat_id, Some("gpt-5.5")).await.unwrap();

        let service = service_with(vec![model("sonnet", true), model("opus", false)], storage.clone());
        let effective = service
            .reconcile_model_for_provider(chat_id, ProviderKind::Claude, ".")
            .await
            .unwrap();

        assert_eq!(effective.as_deref(), Some("sonnet"));
        let stored = storage.get_chat(chat_id).await.unwrap().unwrap().model;
        assert_eq!(stored.as_deref(), Some("sonnet"));
    }

    #[tokio::test]
    async fn reconcile_keeps_model_that_is_in_catalog() {
        let storage = Storage::connect("sqlite::memory:").await.unwrap();
        let chat_id = TelegramChatId(2);
        storage.set_chat_model(chat_id, Some("opus")).await.unwrap();

        let service = service_with(vec![model("sonnet", true), model("opus", false)], storage.clone());
        let effective = service
            .reconcile_model_for_provider(chat_id, ProviderKind::Claude, ".")
            .await
            .unwrap();

        assert_eq!(effective.as_deref(), Some("opus"));
    }

    #[tokio::test]
    async fn reconcile_seeds_default_when_no_model_stored() {
        let storage = Storage::connect("sqlite::memory:").await.unwrap();
        let chat_id = TelegramChatId(3);
        storage.upsert_chat(chat_id, "group", None).await.unwrap();

        let service = service_with(vec![model("sonnet", true), model("opus", false)], storage.clone());
        let effective = service
            .reconcile_model_for_provider(chat_id, ProviderKind::Claude, ".")
            .await
            .unwrap();

        assert_eq!(effective.as_deref(), Some("sonnet"));
    }
}
