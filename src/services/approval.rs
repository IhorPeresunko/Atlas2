//! Resolution of provider approval requests (approve / reject) raised during a turn.

use crate::{
    domain::{ApprovalId, ApprovalStatus, SessionStatus, TelegramChatId, TelegramUserId},
    error::{AppError, AppResult},
    provider::ProviderRegistry,
    storage::Storage,
};

#[derive(Clone)]
pub struct ApprovalService {
    storage: Storage,
    providers: ProviderRegistry,
}

impl ApprovalService {
    pub fn new(storage: Storage, providers: ProviderRegistry) -> Self {
        Self { storage, providers }
    }

    pub async fn resolve_approval(
        &self,
        approval_id: ApprovalId,
        chat_id: TelegramChatId,
        user_id: TelegramUserId,
        approved: bool,
    ) -> AppResult<String> {
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

        let session = self
            .storage
            .get_session(&approval.session_id)
            .await?
            .ok_or_else(|| AppError::Validation("session no longer exists".into()))?;

        let new_status = if approved {
            ApprovalStatus::Approved
        } else {
            ApprovalStatus::Rejected
        };
        self.providers
            .get(session.provider)?
            .resolve_approval(&approval.session_id, &approval_id, approved)
            .await?;
        self.storage
            .resolve_approval(&approval_id, new_status.clone(), user_id)
            .await?;
        self.storage
            .update_session_status(&approval.session_id, SessionStatus::Running, None)
            .await?;

        let provider_name = session.provider.display_name();
        Ok(match new_status {
            ApprovalStatus::Approved => format!("Approval sent to {provider_name}."),
            ApprovalStatus::Rejected => format!("Rejection sent to {provider_name}."),
            ApprovalStatus::Pending => unreachable!(),
            ApprovalStatus::Expired => unreachable!(),
        })
    }
}
