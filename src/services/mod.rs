use std::{collections::HashMap, sync::Arc};

use chrono::Utc;
use tokio::sync::{Mutex, OwnedMutexGuard, mpsc::unbounded_channel};

use crate::{
    codex::{CodexApi, CodexClient, CodexEvent},
    config::Config,
    domain::{
        ApprovalStatus, PendingApproval, PendingPlanFollowUp, PendingUserInput, PlanFollowUpId,
        PlanFollowUpStatus, PromptMode, SessionBackend, SessionId, SessionRecord, SessionStatus,
        TelegramChatId, TelegramUserId, UserInputStatus,
    },
    error::{AppError, AppResult},
    filesystem::FilesystemService,
    presentation::{
        TelegramMessage, TelegramTurnDeliveryState, TelegramTurnUpdate, TurnTerminalState,
        plan_follow_up_markup, render_command_finished_message, render_turn_terminal_text,
        render_user_input_prompt, render_voice_transcript_message, send_clear_status_update,
        send_command_finished_update, send_status_update, send_telegram_update, send_text_update,
        turn_control_markup, user_input_markup,
    },
    storage::Storage,
    stt::SttClient,
    telegram::{InlineKeyboardMarkup, TelegramApi, TelegramClient, button},
};

mod approval;
mod folder;
mod model;
mod plan;
mod user_input;

pub use approval::ApprovalService;
pub use folder::{FolderCallbackResult, FolderService};
pub use model::{ModelCallbackResult, ModelService};
pub(crate) use plan::build_plan_implementation_prompt;
pub use plan::{PlanFollowUpCallbackResult, PlanService};
pub use user_input::{UserInputCallbackResult, UserInputService, UserInputTextResult};

/// Shared authorization helper: a chat action is allowed only for Telegram group
/// admins. Used by `AppServices` and the extracted sub-services.
pub(crate) async fn require_group_admin<Tg: TelegramApi>(
    telegram: &Tg,
    chat_id: TelegramChatId,
    user_id: TelegramUserId,
) -> AppResult<()> {
    let member = telegram.get_chat_member(chat_id, user_id).await?;
    if !member.is_admin() {
        return Err(AppError::Validation(
            "only Telegram group admins can perform this action".into(),
        ));
    }
    Ok(())
}

#[derive(Debug, Clone)]
struct LiveTurnControl {
    chat_id: TelegramChatId,
    control_message_id: i64,
    stop_requested: bool,
}

#[derive(Clone)]
pub struct AppServices<Cx: CodexApi = CodexClient, Tg: TelegramApi = TelegramClient> {
    pub config: Config,
    pub storage: Storage,
    pub telegram: Tg,
    pub codex: Cx,
    pub stt: SttClient,
    session_locks: Arc<Mutex<HashMap<i64, Arc<Mutex<()>>>>>,
    live_turns: Arc<Mutex<HashMap<SessionId, LiveTurnControl>>>,
    pub folder: FolderService<Cx, Tg>,
    pub model: ModelService<Cx>,
    pub approvals: ApprovalService<Cx, Tg>,
    pub user_input: UserInputService<Cx, Tg>,
    pub plans: PlanService<Tg>,
}

impl<Cx: CodexApi + 'static, Tg: TelegramApi + 'static> AppServices<Cx, Tg> {
    pub fn new(
        config: Config,
        storage: Storage,
        telegram: Tg,
        filesystem: FilesystemService,
        codex: Cx,
        stt: SttClient,
    ) -> Self {
        let model = ModelService::new(storage.clone(), codex.clone());
        let folder = FolderService::new(
            storage.clone(),
            telegram.clone(),
            filesystem.clone(),
            config.clone(),
            model.clone(),
        );
        let approvals = ApprovalService::new(storage.clone(), telegram.clone(), codex.clone());
        let user_input = UserInputService::new(storage.clone(), telegram.clone(), codex.clone());
        let plans = PlanService::new(storage.clone(), telegram.clone());
        Self {
            config,
            storage,
            telegram,
            codex,
            stt,
            session_locks: Arc::new(Mutex::new(HashMap::new())),
            live_turns: Arc::new(Mutex::new(HashMap::new())),
            folder,
            model,
            approvals,
            user_input,
            plans,
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
        require_group_admin(&self.telegram, chat_id, user_id).await
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
        self.storage
            .expire_pending_user_inputs_for_session(&session_id)
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
        let chat_binding = self.storage.get_chat(chat_id).await?;
        let (mut model, reasoning_effort) = chat_binding
            .map(|chat| (chat.model, chat.reasoning_effort))
            .unwrap_or((None, None));
        let session = self.require_active_session(chat_id).await?;
        // The Codex app-server rejects a turn without a model. Resolve and
        // persist its default when the chat has none (e.g. a brand-new chat).
        if model.is_none() {
            model = self
                .model
                .ensure_chat_model(chat_id, &session.workspace_path.0)
                .await?;
        }
        tracing::info!(
            chat_id = chat_id.0,
            session_id = %session.session_id.0,
            mode = ?mode,
            workspace_path = session.workspace_path.0,
            prompt_chars = prompt.chars().count(),
            "starting Codex prompt"
        );
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
            .run_turn(
                &session,
                prompt,
                mode,
                model.as_deref(),
                reasoning_effort.as_deref(),
                Box::new(move |event| {
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
                    CodexEvent::UserInputRequested { request } => {
                        let storage = storage.clone();
                        let session_id = session_id.clone();
                        let telegram_updates_tx = event_updates_tx.clone();
                        tokio::spawn(async move {
                            let _ = storage
                                .update_session_status(
                                    &session_id,
                                    SessionStatus::WaitingForInput,
                                    None,
                                )
                                .await;
                            let pending = PendingUserInput {
                                request_id: request.request_id.clone(),
                                session_id,
                                chat_id: chat_id_copy,
                                questions: request.questions,
                                answers: HashMap::new(),
                                status: UserInputStatus::Pending,
                                created_at: Utc::now(),
                                resolved_by: None,
                            };
                            let _ = storage.insert_pending_user_input(&pending).await;
                            if let Ok(markup) = user_input_markup(&pending) {
                                let _ = telegram_updates_tx.send(TelegramTurnUpdate::UserInput {
                                    text: render_user_input_prompt(&pending),
                                    markup,
                                });
                            }
                        });
                    }
                    CodexEvent::PlanCompleted { markdown } => {
                        if mode != PromptMode::Plan {
                            send_text_update(&event_updates_tx, markdown);
                            return Ok(());
                        }
                        let storage = storage.clone();
                        let session_id = session_id.clone();
                        let telegram_updates_tx = event_updates_tx.clone();
                        tokio::spawn(async move {
                            let _ = storage
                                .expire_pending_plan_follow_ups_for_session(&session_id)
                                .await;
                            let follow_up = PendingPlanFollowUp {
                                follow_up_id: PlanFollowUpId::new(),
                                session_id,
                                chat_id: chat_id_copy,
                                plan_markdown: markdown.clone(),
                                status: PlanFollowUpStatus::Pending,
                                created_at: Utc::now(),
                                resolved_by: None,
                            };
                            let _ = storage.insert_pending_plan_follow_up(&follow_up).await;
                            let _ = telegram_updates_tx.send(TelegramTurnUpdate::Message(
                                TelegramMessage {
                                    text: markdown,
                                    parse_mode: None,
                                },
                            ));
                            let _ = telegram_updates_tx.send(TelegramTurnUpdate::PlanFollowUp {
                                text: "Plan ready. Implement it now or send more details to refine it."
                                    .into(),
                                markup: plan_follow_up_markup(&follow_up),
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
            }),
            )
            .await;

        send_clear_status_update(&telegram_updates_tx);
        drop(telegram_updates_tx);
        let _ = telegram_sender.await;

        match result {
            Ok(result) => {
                tracing::info!(
                    chat_id = chat_id.0,
                    session_id = %session.session_id.0,
                    completed = result.completed,
                    interrupted = result.interrupted,
                    has_failure = result.failure.is_some(),
                    thread_id = result.thread_id.as_ref().map(|id| id.0.as_str()).unwrap_or(""),
                    "Codex prompt finished"
                );
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
                    if let Err(error) = self
                        .finish_turn_control(live_turn, terminal_state, result.failure.as_deref())
                        .await
                    {
                        tracing::warn!(
                            chat_id = chat_id.0,
                            session_id = %session.session_id.0,
                            error = %error,
                            "failed to update Telegram turn control message"
                        );
                    }
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
                tracing::error!(
                    chat_id = chat_id.0,
                    session_id = %session.session_id.0,
                    error = %error,
                    "Codex prompt execution failed"
                );
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

#[cfg(test)]
mod tests {
    use crate::codex::CodexClient;
    use crate::config::{Config, SttProvider};
    use crate::domain::{
        HistoricProject, PendingPlanFollowUp, PendingUserInput, PlanFollowUpId, PlanFollowUpStatus,
        SessionBackend, SessionId, SessionRecord, SessionStatus, TelegramChatId, UserInputOption,
        UserInputQuestion, UserInputRequestId, UserInputStatus, WorkspacePath,
    };
    use crate::{
        codex::build_codex_prompt,
        domain::PromptMode,
        presentation::{
            TELEGRAM_TEXT_LIMIT, TelegramTurnUpdate, TurnTerminalState, compact_text_for_telegram,
            historic_projects_markup, plan_follow_up_markup, render_command_finished_message,
            render_historic_projects_prompt, render_turn_terminal_text, render_user_input_prompt,
            render_voice_transcript_message, send_clear_status_update, send_status_update,
            send_text_update, trim_for_telegram, turn_control_markup, user_input_markup,
        },
        services::{AppServices, build_plan_implementation_prompt},
        storage::Storage,
        stt::SttClient,
        telegram::ParseMode,
        telegram::TelegramClient,
    };
    use chrono::Utc;
    use std::collections::HashMap;
    use tempfile::tempdir;
    use tokio::sync::mpsc::unbounded_channel;

    fn test_config() -> Config {
        Config {
            telegram_bot_token: "test-token".into(),
            telegram_api_base: "http://127.0.0.1:9".into(),
            database_url: "sqlite::memory:".into(),
            codex_bin: "codex".into(),
            poll_timeout_seconds: 30,
            max_directory_entries: 20,
            workspace_additional_writable_dirs: Vec::new(),
            stt_provider: SttProvider::None,
            stt_api_key: None,
        }
    }

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

    #[test]
    fn renders_user_input_prompt_and_markup() {
        let request = PendingUserInput {
            request_id: UserInputRequestId::new(),
            session_id: SessionId::new(),
            chat_id: TelegramChatId(1),
            questions: vec![
                UserInputQuestion {
                    id: "scope".into(),
                    header: "Scope".into(),
                    question: "Which path should Atlas2 take?".into(),
                    is_other: false,
                    is_secret: false,
                    options: Some(vec![
                        UserInputOption {
                            label: "Implement".into(),
                            description: "Start the code changes now.".into(),
                        },
                        UserInputOption {
                            label: "More details".into(),
                            description: "Ask a follow-up question first.".into(),
                        },
                    ]),
                },
                UserInputQuestion {
                    id: "risk".into(),
                    header: "Risk".into(),
                    question: "How cautious should the rollout be?".into(),
                    is_other: false,
                    is_secret: false,
                    options: Some(vec![UserInputOption {
                        label: "Conservative".into(),
                        description: "Keep the first pass narrow.".into(),
                    }]),
                },
            ],
            answers: HashMap::new(),
            status: UserInputStatus::Pending,
            created_at: Utc::now(),
            resolved_by: None,
        };

        let text = render_user_input_prompt(&request);
        let markup = user_input_markup(&request).unwrap();

        assert!(text.contains("Codex needs your input (1/2)"));
        assert!(text.contains("Reply with a button tap or send a text answer."));
        assert!(text.contains("Implement: Start the code changes now."));
        assert_eq!(markup.inline_keyboard.len(), 2);
        assert!(
            markup.inline_keyboard[0][0]
                .callback_data
                .starts_with("user-input-answer:")
        );
    }

    #[test]
    fn renders_plan_follow_up_markup_and_prompt() {
        let follow_up = PendingPlanFollowUp {
            follow_up_id: PlanFollowUpId::new(),
            session_id: SessionId::new(),
            chat_id: TelegramChatId(1),
            plan_markdown: "# Ship it\n\n- one".into(),
            status: PlanFollowUpStatus::Pending,
            created_at: Utc::now(),
            resolved_by: None,
        };

        let markup = plan_follow_up_markup(&follow_up);
        let prompt = build_plan_implementation_prompt(&follow_up.plan_markdown);

        assert_eq!(markup.inline_keyboard[0][0].text, "Implement");
        assert_eq!(markup.inline_keyboard[0][1].text, "Add details");
        assert!(
            markup.inline_keyboard[0][0]
                .callback_data
                .starts_with("plan-implement:")
        );
        assert_eq!(prompt, "PLEASE IMPLEMENT THIS PLAN:\n# Ship it\n\n- one");
    }

    #[test]
    fn renders_historic_projects_prompt_and_markup() {
        let projects = vec![HistoricProject {
            source_session_id: SessionId::new(),
            workspace_path: WorkspacePath("/home/ihor/code/atlas2".into()),
        }];

        let text = render_historic_projects_prompt();
        let markup = historic_projects_markup(&projects);

        assert_eq!(text, "Select a project or add a new one.");
        assert_eq!(markup.inline_keyboard.len(), 2);
        assert_eq!(markup.inline_keyboard[0][0].text, "Reuse .../code/atlas2");
        assert!(
            markup.inline_keyboard[0][0]
                .callback_data
                .starts_with("project-history-select:")
        );
        assert_eq!(markup.inline_keyboard[1][0].text, "Add new project");
        assert_eq!(
            markup.inline_keyboard[1][0].callback_data,
            "project-add-new:current"
        );
    }

    #[tokio::test]
    async fn begin_new_session_shows_historic_projects_for_chat() {
        let storage = Storage::connect("sqlite::memory:").await.unwrap();
        let temp = tempdir().unwrap();
        let workspace = WorkspacePath(temp.path().to_string_lossy().into_owned());
        let chat_id = TelegramChatId(99);
        storage
            .upsert_chat(chat_id, "supergroup", Some("Atlas"))
            .await
            .unwrap();
        storage
            .insert_session(&SessionRecord {
                session_id: SessionId::new(),
                chat_id,
                workspace_path: workspace.clone(),
                backend: SessionBackend::AppServer,
                provider_thread_id: None,
                resume_cursor_json: None,
                status: SessionStatus::Ready,
                last_error: None,
                created_at: Utc::now(),
                updated_at: Utc::now(),
            })
            .await
            .unwrap();

        let services = AppServices::new(
            test_config(),
            storage,
            TelegramClient::new("http://127.0.0.1:9", "token"),
            crate::filesystem::FilesystemService::default(),
            CodexClient::new("codex".into(), Vec::new()),
            SttClient::Disabled,
        );

        let (text, markup) = services.folder.begin_new_session(chat_id).await.unwrap();

        assert_eq!(text, "Select a project or add a new one.");
        assert_eq!(
            markup.inline_keyboard.last().unwrap()[0].text,
            "Add new project"
        );
        assert!(
            markup.inline_keyboard[0][0]
                .callback_data
                .starts_with("project-history-select:")
        );
    }

    use super::UserInputCallbackResult;
    use crate::codex::{CodexApi, CodexEvent, CodexTurnResult, ModelOption};
    use crate::domain::{
        ApprovalId, ApprovalStatus, PendingApproval, TelegramUserId, UserInputAnswer,
    };
    use crate::error::{AppError, AppResult};
    use crate::filesystem::FilesystemService;
    use crate::telegram::{
        Chat, ChatMember, InlineKeyboardMarkup, Message, TelegramApi, TelegramFile,
    };
    use std::sync::{Arc, Mutex as StdMutex};

    #[derive(Clone, Default)]
    struct FakeCodex {
        resolved_approvals: Arc<StdMutex<Vec<(SessionId, ApprovalId, bool)>>>,
        resolved_inputs: Arc<StdMutex<Vec<(SessionId, UserInputRequestId)>>>,
    }

    #[async_trait::async_trait]
    impl CodexApi for FakeCodex {
        async fn list_models(&self, _workspace_path: &str) -> AppResult<Vec<ModelOption>> {
            Ok(Vec::new())
        }
        async fn run_turn(
            &self,
            _session: &SessionRecord,
            _prompt: &str,
            _mode: PromptMode,
            _model: Option<&str>,
            _reasoning_effort: Option<&str>,
            _on_event: Box<dyn FnMut(CodexEvent) -> AppResult<()> + Send>,
        ) -> AppResult<CodexTurnResult> {
            Ok(CodexTurnResult::default())
        }
        async fn resolve_approval(
            &self,
            session_id: &SessionId,
            approval_id: &ApprovalId,
            approved: bool,
        ) -> AppResult<()> {
            self.resolved_approvals.lock().unwrap().push((
                session_id.clone(),
                approval_id.clone(),
                approved,
            ));
            Ok(())
        }
        async fn resolve_user_input(
            &self,
            session_id: &SessionId,
            request_id: &UserInputRequestId,
            _answers: HashMap<String, UserInputAnswer>,
        ) -> AppResult<()> {
            self.resolved_inputs
                .lock()
                .unwrap()
                .push((session_id.clone(), request_id.clone()));
            Ok(())
        }
        async fn stop_turn(&self, _session_id: &SessionId) -> AppResult<()> {
            Ok(())
        }
    }

    #[derive(Clone)]
    struct FakeTelegram {
        admin: bool,
    }

    fn fake_message() -> Message {
        Message {
            message_id: 1,
            chat: Chat {
                id: 0,
                kind: "supergroup".into(),
                title: None,
            },
            from: None,
            text: None,
            voice: None,
        }
    }

    #[async_trait::async_trait]
    impl TelegramApi for FakeTelegram {
        async fn send_message(
            &self,
            _chat_id: TelegramChatId,
            _text: &str,
            _parse_mode: Option<ParseMode>,
            _reply_markup: Option<InlineKeyboardMarkup>,
        ) -> AppResult<Message> {
            Ok(fake_message())
        }
        async fn edit_message_text(
            &self,
            _chat_id: TelegramChatId,
            _message_id: i64,
            _text: &str,
            _parse_mode: Option<ParseMode>,
            _reply_markup: Option<InlineKeyboardMarkup>,
        ) -> AppResult<Message> {
            Ok(fake_message())
        }
        async fn delete_message(
            &self,
            _chat_id: TelegramChatId,
            _message_id: i64,
        ) -> AppResult<bool> {
            Ok(true)
        }
        async fn get_chat_member(
            &self,
            _chat_id: TelegramChatId,
            _user_id: TelegramUserId,
        ) -> AppResult<ChatMember> {
            Ok(ChatMember {
                status: if self.admin {
                    "administrator".into()
                } else {
                    "member".into()
                },
            })
        }
        async fn get_file(&self, _file_id: &str) -> AppResult<TelegramFile> {
            Ok(TelegramFile { file_path: None })
        }
        async fn download_file_bytes(&self, _file_path: &str) -> AppResult<Vec<u8>> {
            Ok(Vec::new())
        }
    }

    async fn services_with_fakes(admin: bool) -> (AppServices<FakeCodex, FakeTelegram>, FakeCodex) {
        let storage = Storage::connect("sqlite::memory:").await.unwrap();
        let codex = FakeCodex::default();
        let services = AppServices::new(
            test_config(),
            storage,
            FakeTelegram { admin },
            FilesystemService::default(),
            codex.clone(),
            SttClient::Disabled,
        );
        (services, codex)
    }

    fn seed_session(chat_id: TelegramChatId) -> SessionRecord {
        SessionRecord {
            session_id: SessionId::new(),
            chat_id,
            workspace_path: WorkspacePath("/tmp".into()),
            backend: SessionBackend::AppServer,
            provider_thread_id: None,
            resume_cursor_json: None,
            status: SessionStatus::WaitingForApproval,
            last_error: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }

    fn pending_approval(
        session_id: SessionId,
        chat_id: TelegramChatId,
        status: ApprovalStatus,
    ) -> PendingApproval {
        PendingApproval {
            approval_id: ApprovalId::new(),
            session_id,
            chat_id,
            payload: "{}".into(),
            summary: "run a command".into(),
            status,
            created_at: Utc::now(),
            resolved_by: None,
        }
    }

    fn pending_user_input(
        session_id: SessionId,
        chat_id: TelegramChatId,
        question_count: usize,
    ) -> PendingUserInput {
        let questions = (0..question_count)
            .map(|index| UserInputQuestion {
                id: format!("q{index}"),
                header: format!("Q{index}"),
                question: format!("Question {index}?"),
                is_other: false,
                is_secret: false,
                options: Some(vec![
                    UserInputOption {
                        label: "Yes".into(),
                        description: "Affirmative.".into(),
                    },
                    UserInputOption {
                        label: "No".into(),
                        description: "Negative.".into(),
                    },
                ]),
            })
            .collect();
        PendingUserInput {
            request_id: UserInputRequestId::new(),
            session_id,
            chat_id,
            questions,
            answers: HashMap::new(),
            status: UserInputStatus::Pending,
            created_at: Utc::now(),
            resolved_by: None,
        }
    }

    #[tokio::test]
    async fn resolve_approval_approves_and_forwards_to_codex() {
        let chat_id = TelegramChatId(7);
        let (services, codex) = services_with_fakes(true).await;
        let session = seed_session(chat_id);
        services.storage.insert_session(&session).await.unwrap();
        let approval =
            pending_approval(session.session_id.clone(), chat_id, ApprovalStatus::Pending);
        services
            .storage
            .insert_pending_approval(&approval)
            .await
            .unwrap();

        let message = services
            .approvals
            .resolve_approval(
                approval.approval_id.clone(),
                chat_id,
                TelegramUserId(1),
                true,
            )
            .await
            .unwrap();

        assert_eq!(message, "Approval sent to Codex.");
        let forwarded = codex.resolved_approvals.lock().unwrap().clone();
        assert_eq!(forwarded.len(), 1);
        assert_eq!(forwarded[0].1, approval.approval_id);
        assert!(forwarded[0].2);
        let stored = services
            .storage
            .get_pending_approval(&approval.approval_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(stored.status, ApprovalStatus::Approved);
    }

    #[tokio::test]
    async fn resolve_approval_rejection_forwards_rejection_to_codex() {
        let chat_id = TelegramChatId(7);
        let (services, codex) = services_with_fakes(true).await;
        let session = seed_session(chat_id);
        services.storage.insert_session(&session).await.unwrap();
        let approval =
            pending_approval(session.session_id.clone(), chat_id, ApprovalStatus::Pending);
        services
            .storage
            .insert_pending_approval(&approval)
            .await
            .unwrap();

        let message = services
            .approvals
            .resolve_approval(
                approval.approval_id.clone(),
                chat_id,
                TelegramUserId(1),
                false,
            )
            .await
            .unwrap();

        assert_eq!(message, "Rejection sent to Codex.");
        assert!(!codex.resolved_approvals.lock().unwrap()[0].2);
        let stored = services
            .storage
            .get_pending_approval(&approval.approval_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(stored.status, ApprovalStatus::Rejected);
    }

    #[tokio::test]
    async fn resolve_approval_requires_group_admin() {
        let chat_id = TelegramChatId(7);
        let (services, codex) = services_with_fakes(false).await;
        let session = seed_session(chat_id);
        services.storage.insert_session(&session).await.unwrap();
        let approval =
            pending_approval(session.session_id.clone(), chat_id, ApprovalStatus::Pending);
        services
            .storage
            .insert_pending_approval(&approval)
            .await
            .unwrap();

        let result = services
            .approvals
            .resolve_approval(
                approval.approval_id.clone(),
                chat_id,
                TelegramUserId(1),
                true,
            )
            .await;

        assert!(matches!(result, Err(AppError::Validation(_))));
        // A non-admin click must never reach Codex.
        assert!(codex.resolved_approvals.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn resolve_approval_rejects_foreign_chat() {
        let chat_id = TelegramChatId(7);
        let (services, codex) = services_with_fakes(true).await;
        let session = seed_session(chat_id);
        services.storage.insert_session(&session).await.unwrap();
        let approval =
            pending_approval(session.session_id.clone(), chat_id, ApprovalStatus::Pending);
        services
            .storage
            .insert_pending_approval(&approval)
            .await
            .unwrap();

        let result = services
            .approvals
            .resolve_approval(
                approval.approval_id.clone(),
                TelegramChatId(999),
                TelegramUserId(1),
                true,
            )
            .await;

        assert!(matches!(result, Err(AppError::Validation(_))));
        assert!(codex.resolved_approvals.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn resolve_approval_rejects_already_resolved() {
        let chat_id = TelegramChatId(7);
        let (services, codex) = services_with_fakes(true).await;
        let session = seed_session(chat_id);
        services.storage.insert_session(&session).await.unwrap();
        let approval = pending_approval(
            session.session_id.clone(),
            chat_id,
            ApprovalStatus::Approved,
        );
        services
            .storage
            .insert_pending_approval(&approval)
            .await
            .unwrap();

        let result = services
            .approvals
            .resolve_approval(
                approval.approval_id.clone(),
                chat_id,
                TelegramUserId(1),
                true,
            )
            .await;

        assert!(matches!(result, Err(AppError::Validation(_))));
        assert!(codex.resolved_approvals.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn resolve_user_input_advances_to_next_question() {
        let chat_id = TelegramChatId(7);
        let (services, codex) = services_with_fakes(true).await;
        let session = seed_session(chat_id);
        services.storage.insert_session(&session).await.unwrap();
        let request = pending_user_input(session.session_id.clone(), chat_id, 2);
        services
            .storage
            .insert_pending_user_input(&request)
            .await
            .unwrap();

        let result = services
            .user_input
            .resolve_user_input_choice(request.request_id.clone(), chat_id, TelegramUserId(1), 0, 0)
            .await
            .unwrap();

        assert!(matches!(result, UserInputCallbackResult::Render(_, _)));
        // Codex is only answered once every question is resolved.
        assert!(codex.resolved_inputs.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn resolve_user_input_completes_and_forwards_to_codex() {
        let chat_id = TelegramChatId(7);
        let (services, codex) = services_with_fakes(true).await;
        let session = seed_session(chat_id);
        services.storage.insert_session(&session).await.unwrap();
        let request = pending_user_input(session.session_id.clone(), chat_id, 1);
        services
            .storage
            .insert_pending_user_input(&request)
            .await
            .unwrap();

        let result = services
            .user_input
            .resolve_user_input_choice(request.request_id.clone(), chat_id, TelegramUserId(1), 0, 0)
            .await
            .unwrap();

        assert!(matches!(result, UserInputCallbackResult::Replace(_)));
        let forwarded = codex.resolved_inputs.lock().unwrap().clone();
        assert_eq!(forwarded.len(), 1);
        assert_eq!(forwarded[0].1, request.request_id);
    }

    #[tokio::test]
    async fn resolve_user_input_rejects_out_of_order_answer() {
        let chat_id = TelegramChatId(7);
        let (services, codex) = services_with_fakes(true).await;
        let session = seed_session(chat_id);
        services.storage.insert_session(&session).await.unwrap();
        let request = pending_user_input(session.session_id.clone(), chat_id, 2);
        services
            .storage
            .insert_pending_user_input(&request)
            .await
            .unwrap();

        // Answering question index 1 while index 0 is still pending must be rejected.
        let result = services
            .user_input
            .resolve_user_input_choice(request.request_id.clone(), chat_id, TelegramUserId(1), 1, 0)
            .await;

        assert!(matches!(result, Err(AppError::Validation(_))));
        assert!(codex.resolved_inputs.lock().unwrap().is_empty());
    }
}
