//! Per-turn provider event handling. One named method per `ProviderEvent` variant,
//! each performing the storage mutation and/or Telegram update for that event.
//! The handler owns the values the turn loop previously captured in a closure.

use std::collections::HashMap;

use chrono::Utc;
use tokio::sync::mpsc::UnboundedSender;

use crate::{
    provider::{ProviderEvent, ProviderApprovalRequest, ProviderUserInputRequest},
    domain::{
        ApprovalStatus, ThreadId, PendingApproval, PendingPlanFollowUp, PendingUserInput,
        PlanFollowUpId, PlanFollowUpStatus, PromptMode, SessionId, SessionStatus, TelegramChatId,
        UserInputStatus,
    },
    error::AppResult,
    presentation::{
        TelegramMessage, TelegramTurnUpdate, plan_follow_up_markup,
        render_command_finished_message, render_user_input_prompt, send_command_finished_update,
        send_status_update, send_text_update, user_input_markup,
    },
    storage::Storage,
    telegram::{InlineKeyboardMarkup, button},
};

/// Owns the per-turn context a provider event handler needs. Storage mutations and
/// Telegram updates are dispatched on background tasks, matching the original
/// fire-and-forget behavior of the inline turn closure.
pub(super) struct TurnEventHandler {
    pub storage: Storage,
    pub session_id: SessionId,
    pub chat_id: TelegramChatId,
    pub mode: PromptMode,
    pub provider_name: &'static str,
    pub updates_tx: UnboundedSender<TelegramTurnUpdate>,
}

impl TurnEventHandler {
    pub fn handle(&self, event: ProviderEvent) -> AppResult<()> {
        match event {
            ProviderEvent::ThreadStarted {
                thread_id,
                resume_cursor_json,
            } => self.on_thread_started(thread_id, resume_cursor_json),
            ProviderEvent::Status { text } => self.on_status(text),
            ProviderEvent::Output { text } => self.on_output(text),
            ProviderEvent::CommandStarted { command } => self.on_command_started(command),
            ProviderEvent::CommandFinished {
                command,
                exit_code,
                output,
            } => self.on_command_finished(command, exit_code, output),
            ProviderEvent::ApprovalRequested { approval } => self.on_approval_requested(approval),
            ProviderEvent::UserInputRequested { request } => self.on_user_input_requested(request),
            ProviderEvent::PlanCompleted { markdown } => self.on_plan_completed(markdown),
            ProviderEvent::TurnCompleted => Ok(()),
            ProviderEvent::TurnInterrupted { message } => {
                let _ = message;
                Ok(())
            }
            ProviderEvent::TurnFailed { message } => self.on_turn_failed(message),
        }
    }

    fn on_thread_started(
        &self,
        thread_id: ThreadId,
        resume_cursor_json: Option<String>,
    ) -> AppResult<()> {
        let storage = self.storage.clone();
        let session_id = self.session_id.clone();
        tokio::spawn(async move {
            let _ = storage
                .update_session_provider_state(
                    &session_id,
                    Some(&thread_id),
                    resume_cursor_json.as_deref(),
                )
                .await;
        });
        Ok(())
    }

    fn on_status(&self, text: String) -> AppResult<()> {
        send_status_update(&self.updates_tx, format!("Status: {text}"));
        Ok(())
    }

    fn on_output(&self, text: String) -> AppResult<()> {
        send_text_update(&self.updates_tx, text);
        Ok(())
    }

    fn on_command_started(&self, command: String) -> AppResult<()> {
        send_text_update(&self.updates_tx, format!("Running command:\n`{command}`"));
        Ok(())
    }

    fn on_command_finished(
        &self,
        command: String,
        exit_code: i64,
        output: String,
    ) -> AppResult<()> {
        send_command_finished_update(
            &self.updates_tx,
            render_command_finished_message(&command, exit_code, &output),
        );
        Ok(())
    }

    fn on_approval_requested(&self, approval: ProviderApprovalRequest) -> AppResult<()> {
        let storage = self.storage.clone();
        let session_id = self.session_id.clone();
        let chat_id = self.chat_id;
        let updates_tx = self.updates_tx.clone();
        tokio::spawn(async move {
            let _ = storage
                .update_session_status(&session_id, SessionStatus::WaitingForApproval, None)
                .await;
            let pending = PendingApproval {
                approval_id: approval.approval_id.clone(),
                session_id,
                chat_id,
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
            let _ = updates_tx.send(TelegramTurnUpdate::Approval {
                summary: pending.summary,
                markup,
            });
        });
        Ok(())
    }

    fn on_user_input_requested(&self, request: ProviderUserInputRequest) -> AppResult<()> {
        let storage = self.storage.clone();
        let session_id = self.session_id.clone();
        let chat_id = self.chat_id;
        let provider_name = self.provider_name;
        let updates_tx = self.updates_tx.clone();
        tokio::spawn(async move {
            let _ = storage
                .update_session_status(&session_id, SessionStatus::WaitingForInput, None)
                .await;
            let pending = PendingUserInput {
                request_id: request.request_id.clone(),
                session_id,
                chat_id,
                questions: request.questions,
                answers: HashMap::new(),
                status: UserInputStatus::Pending,
                created_at: Utc::now(),
                resolved_by: None,
            };
            let _ = storage.insert_pending_user_input(&pending).await;
            if let Ok(markup) = user_input_markup(&pending) {
                let _ = updates_tx.send(TelegramTurnUpdate::UserInput {
                    text: render_user_input_prompt(provider_name, &pending),
                    markup,
                });
            }
        });
        Ok(())
    }

    fn on_plan_completed(&self, markdown: String) -> AppResult<()> {
        if self.mode != PromptMode::Plan {
            send_text_update(&self.updates_tx, markdown);
            return Ok(());
        }
        let storage = self.storage.clone();
        let session_id = self.session_id.clone();
        let chat_id = self.chat_id;
        let updates_tx = self.updates_tx.clone();
        tokio::spawn(async move {
            let _ = storage
                .expire_pending_plan_follow_ups_for_session(&session_id)
                .await;
            let follow_up = PendingPlanFollowUp {
                follow_up_id: PlanFollowUpId::new(),
                session_id,
                chat_id,
                plan_markdown: markdown.clone(),
                status: PlanFollowUpStatus::Pending,
                created_at: Utc::now(),
                resolved_by: None,
            };
            let _ = storage.insert_pending_plan_follow_up(&follow_up).await;
            let _ = updates_tx.send(TelegramTurnUpdate::Message(TelegramMessage {
                text: markdown,
                parse_mode: None,
            }));
            let _ = updates_tx.send(TelegramTurnUpdate::PlanFollowUp {
                text: "Plan ready. Implement it now or send more details to refine it.".into(),
                markup: plan_follow_up_markup(&follow_up),
            });
        });
        Ok(())
    }

    fn on_turn_failed(&self, message: String) -> AppResult<()> {
        send_text_update(
            &self.updates_tx,
            format!("{} turn failed: {message}", self.provider_name),
        );
        Ok(())
    }
}
