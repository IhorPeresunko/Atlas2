//! Resolution of Codex approval requests (approve / reject) raised during a turn.

use crate::{
    codex::CodexApi,
    domain::{ApprovalId, ApprovalStatus, SessionStatus, TelegramChatId, TelegramUserId},
    error::{AppError, AppResult},
    storage::Storage,
    telegram::TelegramApi,
};

use super::require_group_admin;

#[derive(Clone)]
pub struct ApprovalService<Cx: CodexApi, Tg: TelegramApi> {
    storage: Storage,
    telegram: Tg,
    codex: Cx,
}

impl<Cx: CodexApi, Tg: TelegramApi> ApprovalService<Cx, Tg> {
    pub fn new(storage: Storage, telegram: Tg, codex: Cx) -> Self {
        Self {
            storage,
            telegram,
            codex,
        }
    }

    pub async fn resolve_approval(
        &self,
        approval_id: ApprovalId,
        chat_id: TelegramChatId,
        user_id: TelegramUserId,
        approved: bool,
    ) -> AppResult<String> {
        require_group_admin(&self.telegram, chat_id, user_id).await?;

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
}
