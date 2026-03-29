use std::{collections::HashMap, sync::Arc};

use chrono::Utc;
use regex::Regex;
use tokio::sync::{
    Mutex, OwnedMutexGuard,
    mpsc::{UnboundedSender, unbounded_channel},
};

use crate::{
    codex::{CodexClient, CodexEvent},
    config::Config,
    domain::{
        ApprovalId, ApprovalStatus, FolderBrowseState, PendingApproval, PromptMode, SessionBackend,
        SessionId, SessionRecord, SessionStatus, TelegramChatId, TelegramUserId, WorkspacePath,
    },
    error::{AppError, AppResult},
    filesystem::FilesystemService,
    storage::Storage,
    stt::SttClient,
    telegram::{InlineKeyboardMarkup, ParseMode, TelegramClient, button},
};

const TELEGRAM_TEXT_LIMIT: usize = 3900;

#[derive(Debug, Clone)]
struct LiveTurnControl {
    chat_id: TelegramChatId,
    control_message_id: i64,
    stop_requested: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TurnTerminalState {
    Completed,
    Interrupted,
    Stopped,
    Failed,
}

#[derive(Clone)]
pub struct AppServices {
    pub config: Config,
    pub storage: Storage,
    pub telegram: TelegramClient,
    pub filesystem: FilesystemService,
    pub codex: CodexClient,
    pub stt: SttClient,
    session_locks: Arc<Mutex<HashMap<i64, Arc<Mutex<()>>>>>,
    live_turns: Arc<Mutex<HashMap<SessionId, LiveTurnControl>>>,
}

impl AppServices {
    pub fn new(
        config: Config,
        storage: Storage,
        telegram: TelegramClient,
        filesystem: FilesystemService,
        codex: CodexClient,
        stt: SttClient,
    ) -> Self {
        Self {
            config,
            storage,
            telegram,
            filesystem,
            codex,
            stt,
            session_locks: Arc::new(Mutex::new(HashMap::new())),
            live_turns: Arc::new(Mutex::new(HashMap::new())),
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
                .provider_thread_id
                .map(|id| id.0)
                .unwrap_or_else(|| "not started".into());
            lines.push(format!(
                "- {} | chat={} | workspace={} | backend={} | status={} | thread={}",
                session.session_id.0,
                title,
                session.workspace_path.0,
                session.backend.as_str(),
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
            let message = if approval.status == ApprovalStatus::Expired {
                "approval request is no longer active"
            } else {
                "approval request has already been resolved"
            };
            return Err(AppError::Validation(message.into()));
        }

        let new_status = if approved {
            ApprovalStatus::Approved
        } else {
            ApprovalStatus::Rejected
        };
        self.codex
            .resolve_approval(&approval.session_id, &approval_id, approved)
            .await?;
        self.storage
            .resolve_approval(&approval_id, new_status.clone(), user_id)
            .await?;
        self.storage
            .update_session_status(&approval.session_id, SessionStatus::Running, None)
            .await?;

        Ok(match new_status {
            ApprovalStatus::Approved => "Approval sent to Codex.".into(),
            ApprovalStatus::Rejected => "Rejection sent to Codex.".into(),
            ApprovalStatus::Pending => unreachable!(),
            ApprovalStatus::Expired => unreachable!(),
        })
    }

    pub async fn run_prompt(&self, chat_id: TelegramChatId, prompt: &str) -> AppResult<()> {
        self.run_prompt_with_mode(chat_id, prompt, PromptMode::Normal)
            .await
    }

    pub async fn stop_turn(
        &self,
        session_id: SessionId,
        chat_id: TelegramChatId,
        user_id: TelegramUserId,
    ) -> AppResult<String> {
        self.require_group_admin(chat_id, user_id).await?;

        let session = self.require_active_session(chat_id).await?;
        if session.session_id != session_id {
            return Err(AppError::Validation("turn is no longer active".into()));
        }

        {
            let mut live_turns = self.live_turns.lock().await;
            let live_turn = live_turns
                .get_mut(&session_id)
                .ok_or_else(|| AppError::Validation("turn is no longer running".into()))?;
            if live_turn.chat_id != chat_id {
                return Err(AppError::Validation(
                    "turn belongs to a different chat".into(),
                ));
            }
            if live_turn.stop_requested {
                return Err(AppError::Validation(
                    "turn stop is already in progress".into(),
                ));
            }
            live_turn.stop_requested = true;
        }

        if let Err(error) = self.codex.stop_turn(&session_id).await {
            let mut live_turns = self.live_turns.lock().await;
            if let Some(live_turn) = live_turns.get_mut(&session_id) {
                live_turn.stop_requested = false;
            }
            return Err(error);
        }

        self.storage
            .expire_pending_approvals_for_session(&session_id)
            .await?;

        Ok("Stopping Codex turn.".into())
    }

    pub async fn run_plan_prompt(&self, chat_id: TelegramChatId, prompt: &str) -> AppResult<()> {
        self.run_prompt_with_mode(chat_id, prompt, PromptMode::Plan)
            .await
    }

    pub async fn run_voice_prompt(
        &self,
        chat_id: TelegramChatId,
        file_id: &str,
        file_unique_id: &str,
        mime_type: Option<&str>,
    ) -> AppResult<()> {
        let _guard = self.acquire_session_lock(chat_id).await;
        self.require_active_session(chat_id).await?;

        let file = self.telegram.get_file(file_id).await?;
        let file_path = file.file_path.ok_or_else(|| {
            AppError::Telegram("telegram getFile returned no file_path for voice message".into())
        })?;
        let audio_bytes = self.telegram.download_file_bytes(&file_path).await?;
        let mime_type = mime_type.unwrap_or("audio/ogg");
        let transcript = self
            .stt
            .transcribe_voice(&format!("{file_unique_id}.oga"), mime_type, audio_bytes)
            .await?;

        self.telegram
            .send_message(
                chat_id,
                &render_voice_transcript_message(&transcript),
                None,
                None,
            )
            .await?;

        self.run_prompt_with_mode_locked(chat_id, &transcript, PromptMode::Normal)
            .await
    }

    async fn run_prompt_with_mode(
        &self,
        chat_id: TelegramChatId,
        prompt: &str,
        mode: PromptMode,
    ) -> AppResult<()> {
        let _guard = self.acquire_session_lock(chat_id).await;
        self.run_prompt_with_mode_locked(chat_id, prompt, mode)
            .await
    }

    async fn run_prompt_with_mode_locked(
        &self,
        chat_id: TelegramChatId,
        prompt: &str,
        mode: PromptMode,
    ) -> AppResult<()> {
        let _chat_binding = self.storage.get_chat(chat_id).await?;
        let session = self.require_active_session(chat_id).await?;
        if session.backend != SessionBackend::AppServer {
            return Err(AppError::Validation(
                "this session was created before the app-server migration and cannot continue; run /new to start a fresh session".into(),
            ));
        }

        self.storage
            .update_session_status(&session.session_id, SessionStatus::Running, None)
            .await?;
        let control_message = self
            .telegram
            .send_message(
                chat_id,
                "Codex turn running.",
                None,
                Some(turn_control_markup(&session.session_id)),
            )
            .await?;
        self.live_turns.lock().await.insert(
            session.session_id.clone(),
            LiveTurnControl {
                chat_id,
                control_message_id: control_message.message_id,
                stop_requested: false,
            },
        );
        let telegram = self.telegram.clone();
        let storage = self.storage.clone();
        let session_id = session.session_id.clone();
        let chat_id_copy = chat_id;
        let (telegram_updates_tx, mut telegram_updates_rx) = unbounded_channel();
        let telegram_sender = tokio::spawn(async move {
            let mut delivery_state = TelegramTurnDeliveryState::default();
            while let Some(update) = telegram_updates_rx.recv().await {
                let _ = send_telegram_update(&telegram, chat_id_copy, &mut delivery_state, update)
                    .await;
            }
        });

        send_status_update(&telegram_updates_tx, "Starting Codex turn...");
        let event_updates_tx = telegram_updates_tx.clone();

        let result = self
            .codex
            .run_turn(&session, prompt, mode, move |event| {
                match event {
                    CodexEvent::ThreadStarted {
                        thread_id,
                        resume_cursor_json,
                    } => {
                        let storage = storage.clone();
                        let session_id = session_id.clone();
                        tokio::spawn(async move {
                            let _ = storage
                                .update_session_provider_state(
                                    &session_id,
                                    Some(&thread_id),
                                    resume_cursor_json.as_deref(),
                                )
                                .await;
                        });
                    }
                    CodexEvent::Status { text } => {
                        send_status_update(&event_updates_tx, format!("Status: {text}"));
                    }
                    CodexEvent::Output { text } => {
                        send_text_update(&event_updates_tx, text);
                    }
                    CodexEvent::CommandStarted { command } => {
                        send_text_update(
                            &event_updates_tx,
                            format!("Running command:\n`{command}`"),
                        );
                    }
                    CodexEvent::CommandFinished {
                        command,
                        exit_code,
                        output,
                    } => {
                        send_command_finished_update(
                            &event_updates_tx,
                            render_command_finished_message(&command, exit_code, &output),
                        );
                    }
                    CodexEvent::ApprovalRequested { approval } => {
                        let storage = storage.clone();
                        let session_id = session_id.clone();
                        let telegram_updates_tx = event_updates_tx.clone();
                        tokio::spawn(async move {
                            let _ = storage
                                .update_session_status(
                                    &session_id,
                                    SessionStatus::WaitingForApproval,
                                    None,
                                )
                                .await;
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
                                    button(
                                        "Approve",
                                        format!("approval-approve:{}", pending.approval_id.0),
                                    ),
                                    button(
                                        "Reject",
                                        format!("approval-reject:{}", pending.approval_id.0),
                                    ),
                                ]],
                            };
                            let _ = telegram_updates_tx.send(TelegramTurnUpdate::Approval {
                                summary: pending.summary,
                                markup,
                            });
                        });
                    }
                    CodexEvent::TurnCompleted => {}
                    CodexEvent::TurnInterrupted { message } => {
                        let _ = message;
                    }
                    CodexEvent::TurnFailed { message } => {
                        send_text_update(
                            &event_updates_tx,
                            format!("Codex turn failed: {message}"),
                        );
                    }
                }
                Ok(())
            })
            .await;

        send_clear_status_update(&telegram_updates_tx);
        drop(telegram_updates_tx);
        let _ = telegram_sender.await;

        match result {
            Ok(result) => {
                let live_turn = self.live_turns.lock().await.remove(&session.session_id);
                let terminal_state = match &live_turn {
                    Some(live_turn) if live_turn.stop_requested => TurnTerminalState::Stopped,
                    Some(_) if result.interrupted => TurnTerminalState::Interrupted,
                    Some(_) if result.failure.is_some() => TurnTerminalState::Failed,
                    Some(_) => TurnTerminalState::Completed,
                    None if result.interrupted => TurnTerminalState::Interrupted,
                    None if result.failure.is_some() => TurnTerminalState::Failed,
                    None => TurnTerminalState::Completed,
                };
                if let Some(live_turn) = live_turn {
                    self.finish_turn_control(live_turn, terminal_state, result.failure.as_deref())
                        .await?;
                }
                if let Some(thread_id) = result.thread_id.as_ref() {
                    self.storage
                        .update_session_provider_state(
                            &session.session_id,
                            Some(&thread_id),
                            result.resume_cursor_json.as_deref(),
                        )
                        .await?;
                }
                if terminal_state == TurnTerminalState::Stopped
                    || terminal_state == TurnTerminalState::Interrupted
                {
                    self.storage
                        .update_session_status(&session.session_id, SessionStatus::Ready, None)
                        .await?;
                } else if let Some(message) = result.failure {
                    self.storage
                        .update_session_status(
                            &session.session_id,
                            SessionStatus::Failed,
                            Some(&message),
                        )
                        .await?;
                } else {
                    self.storage
                        .update_session_status(&session.session_id, SessionStatus::Ready, None)
                        .await?;
                }
                Ok(())
            }
            Err(error) => {
                let live_turn = self.live_turns.lock().await.remove(&session.session_id);
                if let Some(live_turn) = live_turn {
                    let _ = self
                        .finish_turn_control(
                            live_turn,
                            TurnTerminalState::Failed,
                            Some(&error.to_string()),
                        )
                        .await;
                }
                self.storage
                    .update_session_status(
                        &session.session_id,
                        SessionStatus::Failed,
                        Some(&error.to_string()),
                    )
                    .await?;
                let message = format!("Codex execution failed: {error}");
                self.telegram
                    .send_message(chat_id, &message, None, None)
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

    async fn require_active_session(&self, chat_id: TelegramChatId) -> AppResult<SessionRecord> {
        self.storage
            .get_active_session_for_chat(chat_id)
            .await?
            .ok_or_else(|| {
                AppError::Validation(
                    "this group does not have an active session; run /new first".into(),
                )
            })
    }

    async fn finish_turn_control(
        &self,
        live_turn: LiveTurnControl,
        state: TurnTerminalState,
        detail: Option<&str>,
    ) -> AppResult<()> {
        self.telegram
            .edit_message_text(
                live_turn.chat_id,
                live_turn.control_message_id,
                &render_turn_terminal_text(state, detail),
                None,
                None,
            )
            .await?;
        Ok(())
    }
}

pub enum FolderCallbackResult {
    Render(String, InlineKeyboardMarkup),
    Replace(String),
}

fn trim_for_telegram(text: &str) -> String {
    let trimmed: String = text.chars().take(TELEGRAM_TEXT_LIMIT).collect();
    if trimmed.is_empty() {
        "Working...".into()
    } else {
        trimmed
    }
}

fn turn_control_markup(session_id: &SessionId) -> InlineKeyboardMarkup {
    InlineKeyboardMarkup::single_column(vec![button("Stop", format!("turn-stop:{}", session_id.0))])
}

fn render_turn_terminal_text(state: TurnTerminalState, detail: Option<&str>) -> String {
    match state {
        TurnTerminalState::Completed => "Codex turn completed.".into(),
        TurnTerminalState::Interrupted => "Codex turn interrupted.".into(),
        TurnTerminalState::Stopped => "Codex turn stopped.".into(),
        TurnTerminalState::Failed => match detail {
            Some(detail) if !detail.is_empty() => {
                trim_for_telegram(&format!("Codex turn failed.\n{detail}"))
            }
            _ => "Codex turn failed.".into(),
        },
    }
}

#[derive(Debug)]
enum TelegramTurnUpdate {
    Status(TelegramMessage),
    Message(TelegramMessage),
    ClearStatus,
    Approval {
        summary: String,
        markup: InlineKeyboardMarkup,
    },
}

#[derive(Debug, Clone)]
struct TelegramMessage {
    text: String,
    parse_mode: Option<ParseMode>,
}

#[derive(Debug, Default)]
struct TelegramTurnDeliveryState {
    transient_status_message_id: Option<i64>,
    transient_status_text: Option<String>,
}

#[cfg(test)]
fn build_codex_prompt(prompt: &str, mode: PromptMode) -> String {
    match mode {
        PromptMode::Normal => prompt.to_string(),
        PromptMode::Plan => format!(
            concat!(
                "You are in Atlas2 plan mode.\n",
                "Analyze the request and return a concrete implementation plan only.\n",
                "Do not modify files, do not apply patches, and do not run write operations.\n",
                "You may inspect the codebase as needed.\n\n",
                "User request:\n{}"
            ),
            prompt
        ),
    }
}

fn render_voice_transcript_message(transcript: &str) -> String {
    format!(
        "Transcribed voice message:\n{}",
        compact_text_for_telegram(transcript)
    )
}

fn send_text_update(
    telegram_updates_tx: &UnboundedSender<TelegramTurnUpdate>,
    text: impl Into<String>,
) {
    send_plain_update(telegram_updates_tx, text);
}

fn send_status_update(
    telegram_updates_tx: &UnboundedSender<TelegramTurnUpdate>,
    text: impl Into<String>,
) {
    let compact = compact_text_for_telegram(&text.into());
    let _ = telegram_updates_tx.send(TelegramTurnUpdate::Status(TelegramMessage {
        text: trim_for_telegram(&compact),
        parse_mode: None,
    }));
}

fn send_clear_status_update(telegram_updates_tx: &UnboundedSender<TelegramTurnUpdate>) {
    let _ = telegram_updates_tx.send(TelegramTurnUpdate::ClearStatus);
}

fn send_plain_update(
    telegram_updates_tx: &UnboundedSender<TelegramTurnUpdate>,
    text: impl Into<String>,
) {
    let compact = compact_text_for_telegram(&text.into());
    let _ = telegram_updates_tx.send(TelegramTurnUpdate::Message(TelegramMessage {
        text: if compact.is_empty() {
            "Working...".into()
        } else {
            compact
        },
        parse_mode: None,
    }));
}

fn send_command_finished_update(
    telegram_updates_tx: &UnboundedSender<TelegramTurnUpdate>,
    message: TelegramMessage,
) {
    let _ = telegram_updates_tx.send(TelegramTurnUpdate::Message(message));
}

async fn send_telegram_update(
    telegram: &TelegramClient,
    chat_id: TelegramChatId,
    delivery_state: &mut TelegramTurnDeliveryState,
    update: TelegramTurnUpdate,
) -> AppResult<()> {
    match update {
        TelegramTurnUpdate::Status(message) => {
            upsert_status_message(telegram, chat_id, delivery_state, message).await?;
        }
        TelegramTurnUpdate::Message(message) => {
            clear_status_message(telegram, chat_id, delivery_state).await?;
            telegram
                .send_message(chat_id, &message.text, message.parse_mode, None)
                .await?;
        }
        TelegramTurnUpdate::ClearStatus => {
            clear_status_message(telegram, chat_id, delivery_state).await?;
        }
        TelegramTurnUpdate::Approval { summary, markup } => {
            clear_status_message(telegram, chat_id, delivery_state).await?;
            telegram
                .send_message(chat_id, &summary, None, Some(markup))
                .await?;
        }
    }
    Ok(())
}

async fn upsert_status_message(
    telegram: &TelegramClient,
    chat_id: TelegramChatId,
    delivery_state: &mut TelegramTurnDeliveryState,
    message: TelegramMessage,
) -> AppResult<()> {
    if let Some(message_id) = delivery_state.transient_status_message_id {
        if delivery_state.transient_status_text.as_deref() == Some(message.text.as_str()) {
            return Ok(());
        }
        telegram
            .edit_message_text(chat_id, message_id, &message.text, message.parse_mode, None)
            .await?;
    } else {
        let sent = telegram
            .send_message(chat_id, &message.text, message.parse_mode, None)
            .await?;
        delivery_state.transient_status_message_id = Some(sent.message_id);
    }

    delivery_state.transient_status_text = Some(message.text);
    Ok(())
}

async fn clear_status_message(
    telegram: &TelegramClient,
    chat_id: TelegramChatId,
    delivery_state: &mut TelegramTurnDeliveryState,
) -> AppResult<()> {
    if let Some(message_id) = delivery_state.transient_status_message_id.take() {
        let _ = telegram.delete_message(chat_id, message_id).await?;
    }
    delivery_state.transient_status_text = None;
    Ok(())
}

fn render_command_finished_message(command: &str, exit_code: i64, output: &str) -> TelegramMessage {
    let summary = format!(
        "<b>Command finished ({exit_code})</b>\n<code>{}</code>\n<blockquote expandable>",
        escape_html(command)
    );
    let suffix = "</blockquote>";
    let available = TELEGRAM_TEXT_LIMIT.saturating_sub(summary.len() + suffix.len());
    let escaped_output = escape_html(output);
    let output_body = if escaped_output.is_empty() {
        "(no output)".to_string()
    } else {
        trim_html_body(&escaped_output, available)
    };

    TelegramMessage {
        text: format!("{summary}{output_body}{suffix}"),
        parse_mode: Some(ParseMode::Html),
    }
}

fn trim_html_body(text: &str, max_len: usize) -> String {
    if max_len == 0 {
        return String::new();
    }

    let mut trimmed = String::new();
    for ch in text.chars() {
        if trimmed.len() + ch.len_utf8() > max_len {
            break;
        }
        trimmed.push(ch);
    }

    if trimmed.is_empty() {
        return trimmed;
    }

    if trimmed.len() == text.len() {
        return trimmed;
    }

    let ellipsis = "...";
    while trimmed.len() + ellipsis.len() > max_len {
        if trimmed.pop().is_none() {
            return String::new();
        }
    }
    trimmed.push_str(ellipsis);
    trimmed
}

fn escape_html(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            _ => escaped.push(ch),
        }
    }
    escaped
}

fn compact_text_for_telegram(text: &str) -> String {
    let mut compacted = replace_markdown_file_links(text);
    compacted = shorten_bare_absolute_paths(&compacted);
    compacted
}

fn replace_markdown_file_links(text: &str) -> String {
    let re = Regex::new(r"\[([^\]]+)\]\((/[^)\s]+)\)").expect("valid markdown file link regex");
    re.replace_all(text, |captures: &regex::Captures<'_>| {
        compact_path_label(&captures[1])
    })
    .into_owned()
}

fn shorten_bare_absolute_paths(text: &str) -> String {
    let re = Regex::new(r"(/home/[^\s)\]]+)").expect("valid absolute path regex");
    re.replace_all(text, |captures: &regex::Captures<'_>| {
        compact_absolute_path(&captures[1])
    })
    .into_owned()
}

fn compact_path_label(label: &str) -> String {
    if label.contains('/') {
        compact_relative_path(label)
    } else {
        label.to_string()
    }
}

fn compact_relative_path(label: &str) -> String {
    let (path, suffix) = split_path_suffix(label);
    let segments: Vec<&str> = path
        .split('/')
        .filter(|segment| !segment.is_empty())
        .collect();
    if segments.len() <= 3 {
        return label.to_string();
    }

    format!(
        ".../{}/{}{}",
        segments[segments.len() - 2],
        segments[segments.len() - 1],
        suffix
    )
}

fn compact_absolute_path(path: &str) -> String {
    let (path, suffix) = split_path_suffix(path);
    let segments: Vec<&str> = path
        .split('/')
        .filter(|segment| !segment.is_empty())
        .collect();
    if segments.len() <= 3 {
        return format!("{path}{suffix}");
    }

    format!(
        ".../{}/{}{}",
        segments[segments.len() - 2],
        segments[segments.len() - 1],
        suffix
    )
}

fn split_path_suffix(path: &str) -> (&str, &str) {
    for marker in ["#L", ":", "?"] {
        if let Some(index) = path.find(marker) {
            return (&path[..index], &path[index..]);
        }
    }
    (path, "")
}

#[cfg(test)]
mod tests {
    use crate::{
        domain::{PromptMode, SessionId},
        services::{
            TELEGRAM_TEXT_LIMIT, TelegramTurnUpdate, TurnTerminalState, build_codex_prompt,
            compact_text_for_telegram, render_command_finished_message, render_turn_terminal_text,
            render_voice_transcript_message, send_clear_status_update, send_status_update,
            send_text_update, trim_for_telegram, turn_control_markup,
        },
        telegram::ParseMode,
    };
    use tokio::sync::mpsc::unbounded_channel;

    #[test]
    fn trims_large_messages() {
        let input = "a".repeat(5000);
        let output = trim_for_telegram(&input);
        assert_eq!(output.len(), 3900);
    }

    #[test]
    fn queued_text_updates_preserve_full_text_before_delivery() {
        let (tx, mut rx) = unbounded_channel();

        send_text_update(&tx, "a".repeat(5000));

        let update = rx.try_recv().expect("queued update");
        let TelegramTurnUpdate::Message(message) = update else {
            panic!("expected message update");
        };
        assert_eq!(message.text.len(), 5000);
        assert_eq!(message.parse_mode, None);
    }

    #[test]
    fn queued_text_updates_compact_markdown_file_links() {
        let (tx, mut rx) = unbounded_channel();
        send_text_update(
            &tx,
            "- See [api/app/modules/telephony/routes.py](/home/ihor/code/clients/aicalls/api/app/modules/telephony/routes.py#L1039)",
        );

        let update = rx.try_recv().expect("queued update");
        let TelegramTurnUpdate::Message(message) = update else {
            panic!("expected message update");
        };
        assert_eq!(message.text, "- See .../telephony/routes.py");
    }

    #[test]
    fn queued_empty_text_updates_render_as_working() {
        let (tx, mut rx) = unbounded_channel();

        send_text_update(&tx, "");

        let update = rx.try_recv().expect("queued update");
        let TelegramTurnUpdate::Message(message) = update else {
            panic!("expected message update");
        };
        assert_eq!(message.text, "Working...");
        assert_eq!(message.parse_mode, None);
    }

    #[test]
    fn queued_status_updates_use_status_variant() {
        let (tx, mut rx) = unbounded_channel();

        send_status_update(&tx, "Codex turn started");

        let update = rx.try_recv().expect("queued update");
        let TelegramTurnUpdate::Status(message) = update else {
            panic!("expected status update");
        };
        assert_eq!(message.text, "Codex turn started");
        assert_eq!(message.parse_mode, None);
    }

    #[test]
    fn queued_clear_status_updates_use_clear_variant() {
        let (tx, mut rx) = unbounded_channel();

        send_clear_status_update(&tx);

        let update = rx.try_recv().expect("queued update");
        let TelegramTurnUpdate::ClearStatus = update else {
            panic!("expected clear status update");
        };
    }

    #[test]
    fn turn_control_markup_uses_stop_callback() {
        let session_id = SessionId::new();
        let markup = turn_control_markup(&session_id);

        assert_eq!(markup.inline_keyboard.len(), 1);
        assert_eq!(markup.inline_keyboard[0][0].text, "Stop");
        assert_eq!(
            markup.inline_keyboard[0][0].callback_data,
            format!("turn-stop:{}", session_id.0)
        );
    }

    #[test]
    fn stopped_turn_terminal_text_is_stable() {
        let text = render_turn_terminal_text(TurnTerminalState::Stopped, None);

        assert_eq!(text, "Codex turn stopped.");
    }

    #[test]
    fn leaves_normal_prompt_unchanged() {
        assert_eq!(
            build_codex_prompt("fix the bug", PromptMode::Normal),
            "fix the bug"
        );
    }

    #[test]
    fn wraps_plan_prompt_with_plan_contract() {
        let prompt = build_codex_prompt("trace the approval flow", PromptMode::Plan);

        assert!(prompt.contains("Atlas2 plan mode"));
        assert!(prompt.contains("return a concrete implementation plan only"));
        assert!(prompt.contains("trace the approval flow"));
    }

    #[test]
    fn command_finished_messages_use_expandable_html() {
        let message = render_command_finished_message("/bin/echo hello", 0, "line 1\nline 2");

        assert_eq!(message.parse_mode, Some(ParseMode::Html));
        assert!(message.text.contains("<blockquote expandable>"));
        assert!(message.text.contains("<code>/bin/echo hello</code>"));
        assert!(message.text.contains("line 1\nline 2"));
    }

    #[test]
    fn command_finished_messages_escape_html_sensitive_text() {
        let message =
            render_command_finished_message("echo \"<tag>\" && true", 1, "<ok> & \"quoted\"");

        assert!(message.text.contains("&lt;tag&gt;"));
        assert!(message.text.contains("&amp;"));
        assert!(message.text.contains("&quot;quoted&quot;"));
    }

    #[test]
    fn command_finished_messages_trim_to_telegram_limit() {
        let message = render_command_finished_message("cmd", 0, &"<".repeat(6000));

        assert!(message.text.len() <= TELEGRAM_TEXT_LIMIT);
        assert!(message.text.ends_with("...</blockquote>"));
    }

    #[test]
    fn command_finished_messages_render_placeholder_for_empty_output() {
        let message = render_command_finished_message("cmd", 0, "");

        assert!(message.text.contains("(no output)"));
    }

    #[test]
    fn compacts_bare_absolute_paths() {
        let compacted = compact_text_for_telegram(
            "Check /home/ihor/code/clients/aicalls/web/src/routes/_authenticated/call-agents.tsx#L1 for details.",
        );

        assert_eq!(
            compacted,
            "Check .../_authenticated/call-agents.tsx#L1 for details."
        );
    }

    #[test]
    fn leaves_short_non_path_text_unchanged() {
        let compacted = compact_text_for_telegram("Status: turn started");

        assert_eq!(compacted, "Status: turn started");
    }

    #[test]
    fn renders_voice_transcript_message() {
        let message = render_voice_transcript_message("inspect /home/ihor/code/atlas2/src/app.rs");

        assert!(message.starts_with("Transcribed voice message:\n"));
        assert!(message.contains(".../src/app.rs"));
    }
}
