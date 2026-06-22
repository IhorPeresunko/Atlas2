//! New-session creation and workspace folder browsing.

use chrono::Utc;

use crate::{
    config::Config,
    domain::{
        FolderBrowseState, HistoricProject, ProviderKind, SessionId, SessionRecord, SessionStatus,
        TelegramChatId, WorkspacePath,
    },
    error::{AppError, AppResult},
    filesystem::FilesystemService,
    presentation::{
        historic_projects_markup, provider_picker_markup, render_historic_projects_prompt,
        render_provider_picker_prompt,
    },
    storage::Storage,
    telegram::{InlineKeyboardMarkup, button},
};

use super::model::ModelService;

const HISTORIC_PROJECT_LIMIT: usize = 8;

pub enum FolderCallbackResult {
    Render(String, InlineKeyboardMarkup),
    Replace(String),
}

#[derive(Clone)]
pub struct FolderService {
    storage: Storage,
    filesystem: FilesystemService,
    config: Config,
    model: ModelService,
    /// Providers Atlas2 was built with, offered in the `/new` picker. When only
    /// one is available the session is created with it directly.
    available_providers: Vec<ProviderKind>,
}

impl FolderService {
    pub fn new(
        storage: Storage,
        filesystem: FilesystemService,
        config: Config,
        model: ModelService,
        available_providers: Vec<ProviderKind>,
    ) -> Self {
        Self {
            storage,
            filesystem,
            config,
            model,
            available_providers,
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
        payload: &str,
    ) -> AppResult<FolderCallbackResult> {
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
                self.start_new_session(chat_id, workspace).await
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
                self.start_new_session(chat_id, workspace).await
            }
            "newprovider-select" => {
                let kind = ProviderKind::parse(raw_value).ok_or_else(|| {
                    AppError::Validation("unknown provider in callback".into())
                })?;
                self.select_new_session_provider(chat_id, kind).await
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

    /// A workspace has been chosen for a new session. With a single available
    /// provider, create the session immediately; otherwise record the pending
    /// workspace and ask the user which provider to use.
    async fn start_new_session(
        &self,
        chat_id: TelegramChatId,
        workspace: WorkspacePath,
    ) -> AppResult<FolderCallbackResult> {
        match self.available_providers.as_slice() {
            [] => Err(AppError::Validation(
                "no coding-agent provider is available".into(),
            )),
            [only] => {
                let text = self.create_session(chat_id, workspace, *only).await?;
                Ok(FolderCallbackResult::Replace(text))
            }
            kinds => {
                self.storage
                    .set_pending_new_session(chat_id, &workspace)
                    .await?;
                Ok(FolderCallbackResult::Render(
                    render_provider_picker_prompt(&workspace.0),
                    provider_picker_markup(kinds),
                ))
            }
        }
    }

    /// The user picked a provider for the pending new session; create it.
    async fn select_new_session_provider(
        &self,
        chat_id: TelegramChatId,
        kind: ProviderKind,
    ) -> AppResult<FolderCallbackResult> {
        let workspace = self
            .storage
            .take_pending_new_session(chat_id)
            .await?
            .ok_or_else(|| {
                AppError::Validation("no pending new session; run /new again".into())
            })?;
        let text = self.create_session(chat_id, workspace, kind).await?;
        Ok(FolderCallbackResult::Replace(text))
    }

    async fn create_session(
        &self,
        chat_id: TelegramChatId,
        workspace: WorkspacePath,
        provider: ProviderKind,
    ) -> AppResult<String> {
        let now = Utc::now();
        let session = SessionRecord {
            session_id: SessionId::new(),
            chat_id,
            workspace_path: workspace.clone(),
            provider,
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
        let model_note = match self
            .model
            .reconcile_model_for_provider(chat_id, provider, &workspace.0)
            .await
        {
            Ok(Some(model)) => format!("\nModel: {model}."),
            Ok(None) => String::new(),
            Err(error) => {
                tracing::warn!(chat_id = chat_id.0, error = %error, "could not resolve default model for new session");
                String::new()
            }
        };
        Ok(format!(
            "Started new {} session in `{}`.{model_note}\nSend a prompt to start working.",
            provider.display_name(),
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
