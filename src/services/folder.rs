//! New-session creation and workspace folder browsing.

use chrono::Utc;

use crate::{
    codex::CodexApi,
    config::Config,
    domain::{
        FolderBrowseState, HistoricProject, SessionBackend, SessionId, SessionRecord,
        SessionStatus, TelegramChatId, TelegramUserId, WorkspacePath,
    },
    error::{AppError, AppResult},
    filesystem::FilesystemService,
    presentation::{historic_projects_markup, render_historic_projects_prompt},
    storage::Storage,
    telegram::{InlineKeyboardMarkup, TelegramApi, button},
};

use super::{model::ModelService, require_group_admin};

const HISTORIC_PROJECT_LIMIT: usize = 8;

pub enum FolderCallbackResult {
    Render(String, InlineKeyboardMarkup),
    Replace(String),
}

#[derive(Clone)]
pub struct FolderService<Cx: CodexApi, Tg: TelegramApi> {
    storage: Storage,
    telegram: Tg,
    filesystem: FilesystemService,
    config: Config,
    model: ModelService<Cx>,
}

impl<Cx: CodexApi, Tg: TelegramApi> FolderService<Cx, Tg> {
    pub fn new(
        storage: Storage,
        telegram: Tg,
        filesystem: FilesystemService,
        config: Config,
        model: ModelService<Cx>,
    ) -> Self {
        Self {
            storage,
            telegram,
            filesystem,
            config,
            model,
        }
    }

    pub async fn begin_new_session(
        &self,
        chat_id: TelegramChatId,
    ) -> AppResult<(String, InlineKeyboardMarkup)> {
        let historic = self
            .storage
            .list_historic_projects_for_chat(chat_id, HISTORIC_PROJECT_LIMIT)
            .await?;
        let historic = self.filter_existing_historic_projects(historic).await;

        if historic.is_empty() {
            let text = self.begin_folder_selection(chat_id).await?;
            let markup = self.folder_markup("/").await?;
            return Ok((text, markup));
        }

        Ok((
            render_historic_projects_prompt(),
            historic_projects_markup(&historic),
        ))
    }

    async fn begin_folder_selection(&self, chat_id: TelegramChatId) -> AppResult<String> {
        let state = FolderBrowseState {
            chat_id,
            current_path: WorkspacePath("/".into()),
        };
        self.storage.set_folder_browse_state(&state).await?;
        self.render_folder_prompt(chat_id, &state.current_path.0)
            .await
    }

    pub async fn handle_folder_callback(
        &self,
        chat_id: TelegramChatId,
        user_id: TelegramUserId,
        payload: &str,
    ) -> AppResult<FolderCallbackResult> {
        require_group_admin(&self.telegram, chat_id, user_id).await?;
        self.handle_folder_callback_authorized(chat_id, payload)
            .await
    }

    async fn handle_folder_callback_authorized(
        &self,
        chat_id: TelegramChatId,
        payload: &str,
    ) -> AppResult<FolderCallbackResult> {
        let mut parts = payload.splitn(3, ':');
        let action = parts.next().unwrap_or_default();
        let raw_value = parts.next().unwrap_or_default();

        match action {
            "project-history-select" => {
                let source_session_id =
                    SessionId(uuid::Uuid::parse_str(raw_value).map_err(|error| {
                        AppError::Validation(format!(
                            "invalid historic project session ID in callback: {error}"
                        ))
                    })?);
                let source_workspace = self
                    .storage
                    .get_session_workspace_for_chat(chat_id, &source_session_id)
                    .await?
                    .ok_or_else(|| {
                        AppError::Validation("historic project no longer exists".into())
                    })?;
                let workspace = self
                    .filesystem
                    .normalize_directory(&source_workspace.0)
                    .await?;
                let text = self.start_new_session(chat_id, workspace).await?;
                Ok(FolderCallbackResult::Replace(text))
            }
            "project-add-new" => {
                let text = self.begin_folder_selection(chat_id).await?;
                Ok(FolderCallbackResult::Render(
                    text,
                    self.folder_markup("/").await?,
                ))
            }
            "folder-open" => {
                let state = self
                    .storage
                    .get_folder_browse_state(chat_id)
                    .await?
                    .ok_or_else(|| {
                        AppError::Validation("no active folder selection for this group".into())
                    })?;
                let target = self
                    .resolve_folder_entry(&state.current_path.0, raw_value)
                    .await?;
                let normalized = self.filesystem.normalize_directory(&target.0).await?;
                let new_state = FolderBrowseState {
                    chat_id,
                    current_path: normalized.clone(),
                };
                self.storage.set_folder_browse_state(&new_state).await?;
                let text = self.render_folder_prompt(chat_id, &normalized.0).await?;
                Ok(FolderCallbackResult::Render(
                    text,
                    self.folder_markup(&normalized.0).await?,
                ))
            }
            "folder-up" => {
                let state = self
                    .storage
                    .get_folder_browse_state(chat_id)
                    .await?
                    .ok_or_else(|| {
                        AppError::Validation("no active folder selection for this group".into())
                    })?;
                let parent = self
                    .filesystem
                    .parent_directory(&state.current_path.0)
                    .unwrap_or(WorkspacePath("/".into()));
                let normalized = self.filesystem.normalize_directory(&parent.0).await?;
                let new_state = FolderBrowseState {
                    chat_id,
                    current_path: normalized.clone(),
                };
                self.storage.set_folder_browse_state(&new_state).await?;
                let text = self.render_folder_prompt(chat_id, &normalized.0).await?;
                Ok(FolderCallbackResult::Render(
                    text,
                    self.folder_markup(&normalized.0).await?,
                ))
            }
            "folder-select" => {
                let state = self
                    .storage
                    .get_folder_browse_state(chat_id)
                    .await?
                    .ok_or_else(|| {
                        AppError::Validation("no active folder selection for this group".into())
                    })?;
                let workspace = self
                    .filesystem
                    .normalize_directory(&state.current_path.0)
                    .await?;
                self.storage.clear_folder_browse_state(chat_id).await?;
                let text = self.start_new_session(chat_id, workspace).await?;
                Ok(FolderCallbackResult::Replace(text))
            }
            "folder-cancel" => {
                self.storage.clear_folder_browse_state(chat_id).await?;
                Ok(FolderCallbackResult::Replace(
                    "Cancelled folder selection.".into(),
                ))
            }
            _ => Err(AppError::Validation("unknown folder action".into())),
        }
    }

    async fn filter_existing_historic_projects(
        &self,
        projects: Vec<HistoricProject>,
    ) -> Vec<HistoricProject> {
        let mut existing = Vec::new();
        for project in projects {
            if self
                .filesystem
                .normalize_directory(&project.workspace_path.0)
                .await
                .is_ok()
            {
                existing.push(project);
            }
        }
        existing
    }

    async fn start_new_session(
        &self,
        chat_id: TelegramChatId,
        workspace: WorkspacePath,
    ) -> AppResult<String> {
        let now = Utc::now();
        let session = SessionRecord {
            session_id: SessionId::new(),
            chat_id,
            workspace_path: workspace.clone(),
            backend: SessionBackend::AppServer,
            provider_thread_id: None,
            resume_cursor_json: None,
            status: SessionStatus::Ready,
            last_error: None,
            created_at: now,
            updated_at: now,
        };
        self.storage.insert_session(&session).await?;
        self.storage
            .set_active_session(chat_id, Some(&session.session_id))
            .await?;
        let model_note = match self.model.ensure_chat_model(chat_id, &workspace.0).await {
            Ok(Some(model)) => format!("\nModel: {model}."),
            Ok(None) => String::new(),
            Err(error) => {
                tracing::warn!(chat_id = chat_id.0, error = %error, "could not resolve default model for new session");
                String::new()
            }
        };
        Ok(format!(
            "Started new session in `{}`.{model_note}\nSend a prompt to start working.",
            workspace.0
        ))
    }

    async fn render_folder_prompt(
        &self,
        _chat_id: TelegramChatId,
        path: &str,
    ) -> AppResult<String> {
        let entries = self
            .filesystem
            .list_directory(path, self.config.max_directory_entries)
            .await?;
        let mut body = format!("Select a workspace folder.\nCurrent path: `{path}`");
        if entries.is_empty() {
            body.push_str("\n\nNo entries found here.");
        }
        Ok(body)
    }

    pub async fn folder_markup(&self, path: &str) -> AppResult<InlineKeyboardMarkup> {
        let entries = self
            .filesystem
            .list_directory(path, self.config.max_directory_entries)
            .await?;

        let mut buttons = Vec::new();
        buttons.push(button("Select this folder", "folder-select:current"));
        if path != "/" {
            buttons.push(button("Up", "folder-up:current"));
        }
        for (index, entry) in entries.into_iter().filter(|entry| entry.is_dir).enumerate() {
            buttons.push(button(
                format!("Open {}", entry.name),
                format!("folder-open:{index}"),
            ));
        }
        buttons.push(button("Cancel", "folder-cancel:current"));
        Ok(InlineKeyboardMarkup::single_column(buttons))
    }

    async fn resolve_folder_entry(
        &self,
        current_path: &str,
        raw_index: &str,
    ) -> AppResult<WorkspacePath> {
        let index = raw_index
            .parse::<usize>()
            .map_err(|_| AppError::Validation("invalid folder entry selection".into()))?;
        let entries = self
            .filesystem
            .list_directory(current_path, self.config.max_directory_entries)
            .await?;
        let entry = entries
            .into_iter()
            .filter(|entry| entry.is_dir)
            .nth(index)
            .ok_or_else(|| AppError::Validation("folder entry no longer exists".into()))?;
        Ok(entry.path)
    }
}
