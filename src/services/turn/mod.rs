//! Turn orchestration: prompt/plan/voice turns, the live-turn registry and
//! per-chat serialization, and the stop control. The provider event stream is
//! handled by [`events::TurnEventHandler`].

use std::{collections::HashMap, sync::Arc};

use tokio::sync::{Mutex, OwnedMutexGuard, mpsc::unbounded_channel};

use crate::{
    domain::{PromptMode, SessionId, SessionRecord, SessionStatus, TelegramChatId},
    error::{AppError, AppResult},
    provider::ProviderRegistry,
    presentation::{
        TelegramTurnDeliveryState, TurnTerminalState, render_turn_terminal_text,
        render_voice_transcript_message, send_clear_status_update, send_status_update,
        send_telegram_update, turn_control_markup,
    },
    storage::Storage,
    stt::SttClient,
    telegram::TelegramApi,
};

use super::model::ModelService;

mod events;

use events::TurnEventHandler;

#[derive(Debug, Clone)]
struct LiveTurnControl {
    chat_id: TelegramChatId,
    control_message_id: i64,
    stop_requested: bool,
}

#[derive(Clone)]
pub struct TurnService<Tg: TelegramApi> {
    storage: Storage,
    telegram: Tg,
    providers: ProviderRegistry,
    stt: SttClient,
    model: ModelService,
    session_locks: Arc<Mutex<HashMap<i64, Arc<Mutex<()>>>>>,
    live_turns: Arc<Mutex<HashMap<SessionId, LiveTurnControl>>>,
}

impl<Tg: TelegramApi + 'static> TurnService<Tg> {
    pub fn new(
        storage: Storage,
        telegram: Tg,
        providers: ProviderRegistry,
        stt: SttClient,
        model: ModelService,
    ) -> Self {
        Self {
            storage,
            telegram,
            providers,
            stt,
            model,
            session_locks: Arc::new(Mutex::new(HashMap::new())),
            live_turns: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub async fn run_prompt(&self, chat_id: TelegramChatId, prompt: &str) -> AppResult<()> {
        self.run_prompt_with_mode(chat_id, prompt, PromptMode::Normal)
            .await
    }

    pub async fn run_plan_prompt(&self, chat_id: TelegramChatId, prompt: &str) -> AppResult<()> {
        self.run_prompt_with_mode(chat_id, prompt, PromptMode::Plan)
            .await
    }

    pub async fn stop_turn(
        &self,
        session_id: SessionId,
        chat_id: TelegramChatId,
    ) -> AppResult<String> {
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

        if let Err(error) = self
            .providers
            .get(session.provider)?
            .stop_turn(&session_id)
            .await
        {
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

        Ok(format!(
            "Stopping {} turn.",
            session.provider.display_name()
        ))
    }

    /// Best-effort cancellation of whatever turn is currently running in a chat.
    /// Used when `/new` supersedes the active session: a parked turn (e.g. one
    /// waiting on approvals) would otherwise keep holding the per-chat turn lock
    /// and wedge the chat. No-op when nothing is running.
    pub async fn cancel_active_turn(&self, chat_id: TelegramChatId) {
        let session_id = {
            let mut live_turns = self.live_turns.lock().await;
            match live_turns
                .iter_mut()
                .find(|(_, live_turn)| live_turn.chat_id == chat_id)
            {
                Some((session_id, live_turn)) => {
                    if live_turn.stop_requested {
                        return;
                    }
                    live_turn.stop_requested = true;
                    session_id.clone()
                }
                None => return,
            }
        };
        if let Ok(Some(session)) = self.storage.get_session(&session_id).await
            && let Ok(provider) = self.providers.get(session.provider)
        {
            let _ = provider.stop_turn(&session_id).await;
        }
        let _ = self
            .storage
            .expire_pending_approvals_for_session(&session_id)
            .await;
        let _ = self
            .storage
            .expire_pending_user_inputs_for_session(&session_id)
            .await;
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
        let (mut model, reasoning_effort, skip_permissions) = chat_binding
            .map(|chat| {
                (
                    chat.model,
                    chat.reasoning_effort,
                    chat.dangerously_skip_permissions,
                )
            })
            .unwrap_or((None, None, false));
        let session = self.require_active_session(chat_id).await?;
        // Some providers (e.g. Codex) reject a turn without a model. Resolve and
        // persist its default when the chat has none (e.g. a brand-new chat).
        if model.is_none() {
            model = self
                .model
                .ensure_chat_model(chat_id, session.provider, &session.workspace_path.0)
                .await?;
        }
        tracing::info!(
            chat_id = chat_id.0,
            session_id = %session.session_id.0,
            mode = ?mode,
            workspace_path = session.workspace_path.0,
            prompt_chars = prompt.chars().count(),
            "starting provider prompt"
        );

        let provider_name = session.provider.display_name();
        self.storage
            .update_session_status(&session.session_id, SessionStatus::Running, None)
            .await?;
        let control_message = self
            .telegram
            .send_message(
                chat_id,
                &format!("{provider_name} turn running."),
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
        let chat_id_copy = chat_id;
        let (telegram_updates_tx, mut telegram_updates_rx) = unbounded_channel();
        let telegram_sender = tokio::spawn(async move {
            let mut delivery_state = TelegramTurnDeliveryState::default();
            while let Some(update) = telegram_updates_rx.recv().await {
                let _ = send_telegram_update(&telegram, chat_id_copy, &mut delivery_state, update)
                    .await;
            }
        });

        send_status_update(
            &telegram_updates_tx,
            format!("Starting {provider_name} turn..."),
        );
        let handler = TurnEventHandler {
            storage: self.storage.clone(),
            session_id: session.session_id.clone(),
            chat_id,
            mode,
            provider_name,
            updates_tx: telegram_updates_tx.clone(),
        };

        let result = self
            .providers
            .get(session.provider)?
            .run_turn(
                &session,
                prompt,
                mode,
                model.as_deref(),
                reasoning_effort.as_deref(),
                skip_permissions,
                Box::new(move |event| handler.handle(event)),
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
                    "provider prompt finished"
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
                        .finish_turn_control(
                            provider_name,
                            live_turn,
                            terminal_state,
                            result.failure.as_deref(),
                        )
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
                    "provider prompt execution failed"
                );
                let live_turn = self.live_turns.lock().await.remove(&session.session_id);
                if let Some(live_turn) = live_turn {
                    let _ = self
                        .finish_turn_control(
                            provider_name,
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
        provider_name: &str,
        live_turn: LiveTurnControl,
        state: TurnTerminalState,
        detail: Option<&str>,
    ) -> AppResult<()> {
        self.telegram
            .edit_message_text(
                live_turn.chat_id,
                live_turn.control_message_id,
                &render_turn_terminal_text(provider_name, state, detail),
                None,
                None,
            )
            .await?;
        Ok(())
    }
}
