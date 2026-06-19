//! Plan-mode follow-up resolution: Implement / Add-details buttons and the
//! refinement text that follows an "Add details" tap.

use crate::{
    domain::{PlanFollowUpId, PlanFollowUpStatus, TelegramChatId, TelegramUserId},
    error::{AppError, AppResult},
    storage::Storage,
    telegram::TelegramApi,
};

use super::require_group_admin;

pub enum PlanFollowUpCallbackResult {
    Replace(String),
    Implement { text: String, prompt: String },
}

#[derive(Clone)]
pub struct PlanService<Tg: TelegramApi> {
    storage: Storage,
    telegram: Tg,
}

impl<Tg: TelegramApi> PlanService<Tg> {
    pub fn new(storage: Storage, telegram: Tg) -> Self {
        Self { storage, telegram }
    }

    pub async fn resolve_plan_follow_up_implement(
        &self,
        follow_up_id: PlanFollowUpId,
        chat_id: TelegramChatId,
        user_id: TelegramUserId,
    ) -> AppResult<PlanFollowUpCallbackResult> {
        require_group_admin(&self.telegram, chat_id, user_id).await?;

        let follow_up = self
            .storage
            .get_pending_plan_follow_up(&follow_up_id)
            .await?
            .ok_or_else(|| AppError::Validation("plan follow-up not found".into()))?;

        if follow_up.chat_id != chat_id {
            return Err(AppError::Validation(
                "plan follow-up belongs to a different chat".into(),
            ));
        }
        if follow_up.status != PlanFollowUpStatus::Pending {
            return Err(AppError::Validation(
                "plan follow-up is no longer active".into(),
            ));
        }

        self.storage
            .resolve_pending_plan_follow_up(
                &follow_up_id,
                PlanFollowUpStatus::Implemented,
                Some(user_id),
            )
            .await?;

        Ok(PlanFollowUpCallbackResult::Implement {
            text: "Starting plan implementation.".into(),
            prompt: build_plan_implementation_prompt(&follow_up.plan_markdown),
        })
    }

    pub async fn resolve_plan_follow_up_refine(
        &self,
        follow_up_id: PlanFollowUpId,
        chat_id: TelegramChatId,
        user_id: TelegramUserId,
    ) -> AppResult<PlanFollowUpCallbackResult> {
        require_group_admin(&self.telegram, chat_id, user_id).await?;

        let follow_up = self
            .storage
            .get_pending_plan_follow_up(&follow_up_id)
            .await?
            .ok_or_else(|| AppError::Validation("plan follow-up not found".into()))?;

        if follow_up.chat_id != chat_id {
            return Err(AppError::Validation(
                "plan follow-up belongs to a different chat".into(),
            ));
        }
        if follow_up.status != PlanFollowUpStatus::Pending {
            return Err(AppError::Validation(
                "plan follow-up is no longer active".into(),
            ));
        }

        self.storage
            .resolve_pending_plan_follow_up(
                &follow_up_id,
                PlanFollowUpStatus::AwaitingRefinement,
                Some(user_id),
            )
            .await?;

        Ok(PlanFollowUpCallbackResult::Replace(
            "Send your next message with feedback to refine the plan.".into(),
        ))
    }

    pub async fn consume_plan_refinement(
        &self,
        chat_id: TelegramChatId,
        text: &str,
    ) -> AppResult<Option<String>> {
        let Some(follow_up) = self
            .storage
            .get_awaiting_plan_follow_up_for_chat(chat_id)
            .await?
        else {
            return Ok(None);
        };

        self.storage
            .resolve_pending_plan_follow_up(
                &follow_up.follow_up_id,
                PlanFollowUpStatus::Refined,
                None,
            )
            .await?;
        Ok(Some(text.to_string()))
    }
}

pub(crate) fn build_plan_implementation_prompt(plan_markdown: &str) -> String {
    format!("PLEASE IMPLEMENT THIS PLAN:\n{}", plan_markdown.trim())
}
