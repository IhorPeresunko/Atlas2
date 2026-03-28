use std::{collections::HashMap, sync::Arc};

use chrono::Utc;
use tokio::sync::{Mutex, OwnedMutexGuard};

use crate::{
    codex::{CodexClient, CodexEvent},
    config::Config,
    domain::{
        ApprovalId, ApprovalStatus, FolderBrowseState, PendingApproval, SessionId, SessionRecord,
        SessionStatus, TelegramChatId, TelegramUserId, WorkspacePath,
    },
    error::{AppError, AppResult},
    filesystem::FilesystemService,
    storage::Storage,
    telegram::{InlineKeyboardMarkup, TelegramClient, button},
};

#[derive(Clone)]
pub struct AppServices {
    pub config: Config,
    pub storage: Storage,
    pub telegram: TelegramClient,
    pub filesystem: FilesystemService,
    pub codex: CodexClient,
    session_locks: Arc<Mutex<HashMap<i64, Arc<Mutex<()>>>>>,
}

impl AppServices {
    pub fn new(
        config: Config,
        storage: Storage,
        telegram: TelegramClient,
        filesystem: FilesystemService,
        codex: CodexClient,
    ) -> Self {
        Self {
            config,
            storage,
            telegram,
            filesystem,
            codex,
            session_locks: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub async fn register_chat(
        &self,
        chat_id: TelegramChatId,
        chat_kind: &str,
        title: Option<&str>,
    ) -> AppResult<()> {
        self.storage.upsert_chat(chat_id, chat_kind, title).await
    }

    pub async fn require_group_admin(
        &self,
        chat_id: TelegramChatId,
        user_id: TelegramUserId,
    ) -> AppResult<()> {
        let member = self.telegram.get_chat_member(chat_id, user_id).await?;
        if !member.is_admin() {
            return Err(AppError::Validation(
                "only Telegram group admins can perform this action".into(),
            ));
        }
        Ok(())
    }

    pub async fn begin_folder_selection(&self, chat_id: TelegramChatId) -> AppResult<String> {
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
        self.require_group_admin(chat_id, user_id).await?;

        let state = self
            .storage
            .get_folder_browse_state(chat_id)
            .await?
            .ok_or_else(|| {
                AppError::Validation("no active folder selection for this group".into())
            })?;

        let mut parts = payload.splitn(3, ':');
        let action = parts.next().unwrap_or_default();
        let raw_value = parts.next().unwrap_or_default();

        match action {
            "folder-open" => {
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
                let workspace = self
                    .filesystem
                    .normalize_directory(&state.current_path.0)
                    .await?;
                self.storage.clear_folder_browse_state(chat_id).await?;

                let now = Utc::now();
                let session = SessionRecord {
                    session_id: SessionId::new(),
                    chat_id,
                    workspace_path: workspace.clone(),
                    codex_thread_id: None,
                    status: SessionStatus::Ready,
                    created_at: now,
                    updated_at: now,
                };
                self.storage.insert_session(&session).await?;
                self.storage
                    .set_active_session(chat_id, Some(&session.session_id))
                    .await?;
                Ok(FolderCallbackResult::Replace(format!(
                    "Started new session in `{}`.\nSend a prompt to start working.",
                    workspace.0
                )))
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

    pub async fn render_sessions(&self) -> AppResult<String> {
        let sessions = self.storage.list_sessions().await?;
        if sessions.is_empty() {
            return Ok("No sessions exist yet.".into());
        }

        let mut lines = vec!["Known sessions:".to_string()];
        for session in sessions {
            let title = session
                .chat_title
                .unwrap_or_else(|| session.chat_id.0.to_string());
            let thread = session
                .codex_thread_id
                .map(|id| id.0)
                .unwrap_or_else(|| "not started".into());
            lines.push(format!(
                "- {} | chat={} | workspace={} | status={} | thread={}",
                session.session_id.0,
                title,
                session.workspace_path.0,
                session.status.as_str(),
                thread
            ));
        }
        Ok(lines.join("\n"))
    }

    pub async fn resolve_approval(
        &self,
        approval_id: ApprovalId,
        chat_id: TelegramChatId,
        user_id: TelegramUserId,
        approved: bool,
    ) -> AppResult<String> {
        self.require_group_admin(chat_id, user_id).await?;

        let approval = self
            .storage
            .get_pending_approval(&approval_id)
            .await?
            .ok_or_else(|| AppError::Validation("approval request not found".into()))?;

        if approval.chat_id != chat_id {
            return Err(AppError::Validation(
                "approval request belongs to a different chat".into(),
            ));
        }
        if approval.status != ApprovalStatus::Pending {
            return Err(AppError::Validation(
                "approval request has already been resolved".into(),
            ));
        }

        let new_status = if approved {
            ApprovalStatus::Approved
        } else {
            ApprovalStatus::Rejected
        };
        self.storage
            .resolve_approval(&approval_id, new_status.clone(), user_id)
            .await?;
        self.storage
            .update_session_runtime(&approval.session_id, SessionStatus::Ready, None)
            .await?;

        Ok(match new_status {
            ApprovalStatus::Approved => {
                "Approval recorded. Automatic continuation is not yet supported by exec-mode Codex; send the next prompt to continue.".into()
            }
            ApprovalStatus::Rejected => {
                "Rejection recorded. Send the next prompt to continue with different instructions.".into()
            }
            ApprovalStatus::Pending => unreachable!(),
        })
    }

    pub async fn run_prompt(&self, chat_id: TelegramChatId, prompt: &str) -> AppResult<()> {
        let _guard = self.acquire_session_lock(chat_id).await;
        let chat_binding = self.storage.get_chat(chat_id).await?;
        let session = self
            .storage
            .get_active_session_for_chat(chat_id)
            .await?
            .ok_or_else(|| {
                AppError::Validation(
                    "this group does not have an active session; run /new first".into(),
                )
            })?;
        let use_draft_streaming = matches!(
            chat_binding
                .as_ref()
                .map(|binding| binding.chat_kind.as_str()),
            Some("private")
        );

        self.storage
            .update_session_runtime(&session.session_id, SessionStatus::Running, None)
            .await?;

        let progress = self
            .telegram
            .send_message(chat_id, "Starting Codex turn...", None)
            .await?;

        let mut live_text = String::new();
        let telegram = self.telegram.clone();
        let storage = self.storage.clone();
        let session_id = session.session_id.clone();
        let chat_id_copy = chat_id;

        let result = self
            .codex
            .run_turn(
                &session.workspace_path.0,
                session.codex_thread_id.as_ref(),
                prompt,
                move |event| {
                    match event {
                        CodexEvent::ThreadStarted { thread_id } => {
                            let storage = storage.clone();
                            let session_id = session_id.clone();
                            tokio::spawn(async move {
                                let _ = storage
                                    .update_session_runtime(&session_id, SessionStatus::Running, Some(&thread_id))
                                    .await;
                            });
                        }
                        CodexEvent::Status { text } => {
                            live_text = format!("Status: {text}");
                            let text = live_text.clone();
                            let telegram = telegram.clone();
                            tokio::spawn(async move {
                                let _ = stream_telegram_text(
                                    &telegram,
                                    chat_id_copy,
                                    progress.message_id,
                                    use_draft_streaming,
                                    &text,
                                )
                                .await;
                            });
                        }
                        CodexEvent::Output { text } => {
                            if !live_text.is_empty() {
                                live_text.push_str("\n\n");
                            }
                            live_text.push_str(&text);
                            let text = trim_for_telegram(&live_text);
                            let telegram = telegram.clone();
                            tokio::spawn(async move {
                                let _ = stream_telegram_text(
                                    &telegram,
                                    chat_id_copy,
                                    progress.message_id,
                                    use_draft_streaming,
                                    &text,
                                )
                                .await;
                            });
                        }
                        CodexEvent::CommandStarted { command } => {
                            live_text = trim_for_telegram(&format!("{live_text}\n\nRunning command:\n`{command}`"));
                            let text = live_text.clone();
                            let telegram = telegram.clone();
                            tokio::spawn(async move {
                                let _ = stream_telegram_text(
                                    &telegram,
                                    chat_id_copy,
                                    progress.message_id,
                                    use_draft_streaming,
                                    &text,
                                )
                                .await;
                            });
                        }
                        CodexEvent::CommandFinished { command, exit_code, output } => {
                            let snippet = trim_for_telegram(&format!("{live_text}\n\nCommand finished ({exit_code}):\n`{command}`\n{output}"));
                            live_text = snippet.clone();
                            let telegram = telegram.clone();
                            tokio::spawn(async move {
                                let _ = stream_telegram_text(
                                    &telegram,
                                    chat_id_copy,
                                    progress.message_id,
                                    use_draft_streaming,
                                    &snippet,
                                )
                                .await;
                            });
                        }
                        CodexEvent::ApprovalRequested { approval } => {
                            let storage = storage.clone();
                            let telegram = telegram.clone();
                            let session_id = session_id.clone();
                            tokio::spawn(async move {
                                let pending = PendingApproval {
                                    approval_id: approval.approval_id.clone(),
                                    session_id,
                                    chat_id: chat_id_copy,
                                    payload: approval.payload,
                                    summary: approval.summary,
                                    status: ApprovalStatus::Pending,
                                    created_at: Utc::now(),
                                    resolved_by: None,
                                };
                                let _ = storage.insert_pending_approval(&pending).await;
                                let markup = InlineKeyboardMarkup {
                                    inline_keyboard: vec![vec![
                                        button("Approve", format!("approval-approve:{}", pending.approval_id.0)),
                                        button("Reject", format!("approval-reject:{}", pending.approval_id.0)),
                                    ]],
                                };
                                let _ = telegram
                                    .send_message(chat_id_copy, &pending.summary, Some(markup))
                                    .await;
                            });
                        }
                        CodexEvent::TurnCompleted => {}
                        CodexEvent::TurnFailed { message } => {
                            let text = format!("Codex turn failed: {message}");
                            let telegram = telegram.clone();
                            tokio::spawn(async move {
                                let _ = stream_telegram_text(
                                    &telegram,
                                    chat_id_copy,
                                    progress.message_id,
                                    use_draft_streaming,
                                    &text,
                                )
                                .await;
                            });
                        }
                    }
                    Ok(())
                },
            )
            .await;

        match result {
            Ok(result) => {
                if let Some(thread_id) = result.thread_id {
                    self.storage
                        .update_session_runtime(
                            &session.session_id,
                            SessionStatus::Ready,
                            Some(&thread_id),
                        )
                        .await?;
                } else {
                    self.storage
                        .update_session_runtime(&session.session_id, SessionStatus::Ready, None)
                        .await?;
                }
                Ok(())
            }
            Err(error) => {
                self.storage
                    .update_session_runtime(&session.session_id, SessionStatus::Failed, None)
                    .await?;
                let message = format!("Codex execution failed: {error}");
                self.telegram
                    .edit_message_text(chat_id, progress.message_id, &message, None)
                    .await?;
                Err(error)
            }
        }
    }

    async fn acquire_session_lock(&self, chat_id: TelegramChatId) -> OwnedMutexGuard<()> {
        let arc = {
            let mut locks = self.session_locks.lock().await;
            locks
                .entry(chat_id.0)
                .or_insert_with(|| Arc::new(Mutex::new(())))
                .clone()
        };
        arc.lock_owned().await
    }
}

pub enum FolderCallbackResult {
    Render(String, InlineKeyboardMarkup),
    Replace(String),
}

fn trim_for_telegram(text: &str) -> String {
    let trimmed: String = text.chars().take(3900).collect();
    if trimmed.is_empty() {
        "Working...".into()
    } else {
        trimmed
    }
}

async fn stream_telegram_text(
    telegram: &TelegramClient,
    chat_id: TelegramChatId,
    message_id: i64,
    use_draft_streaming: bool,
    text: &str,
) -> AppResult<()> {
    if use_draft_streaming {
        telegram
            .send_message_draft(chat_id, message_id, text)
            .await?;
    } else {
        telegram
            .edit_message_text(chat_id, message_id, text, None)
            .await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::services::trim_for_telegram;

    #[test]
    fn trims_large_messages() {
        let input = "a".repeat(5000);
        let output = trim_for_telegram(&input);
        assert_eq!(output.len(), 3900);
    }
}
