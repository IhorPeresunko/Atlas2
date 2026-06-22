//! Interactive `request_user_input` flow: button taps and free-text answers,
//! advancing question-by-question until the full response goes back to the provider.

use crate::{
    domain::{
        PendingUserInput, SessionStatus, TelegramChatId, TelegramUserId, UserInputAnswer,
        UserInputRequestId, UserInputStatus,
    },
    error::{AppError, AppResult},
    presentation::{render_user_input_prompt, render_user_input_summary, user_input_markup},
    provider::ProviderRegistry,
    storage::Storage,
    telegram::InlineKeyboardMarkup,
};

pub enum UserInputCallbackResult {
    Render(String, InlineKeyboardMarkup),
    Replace(String),
}

pub enum UserInputTextResult {
    Render(String, InlineKeyboardMarkup),
    Replace(String),
}

enum UserInputAdvance {
    NextQuestion {
        text: String,
        markup: InlineKeyboardMarkup,
    },
    Completed {
        summary: String,
    },
}

#[derive(Clone)]
pub struct UserInputService {
    storage: Storage,
    providers: ProviderRegistry,
}

impl UserInputService {
    pub fn new(storage: Storage, providers: ProviderRegistry) -> Self {
        Self { storage, providers }
    }

    pub async fn resolve_user_input_choice(
        &self,
        request_id: UserInputRequestId,
        chat_id: TelegramChatId,
        user_id: TelegramUserId,
        question_index: usize,
        option_index: usize,
    ) -> AppResult<UserInputCallbackResult> {
        let request = self
            .storage
            .get_pending_user_input(&request_id)
            .await?
            .ok_or_else(|| AppError::Validation("user input request not found".into()))?;

        if request.chat_id != chat_id {
            return Err(AppError::Validation(
                "user input request belongs to a different chat".into(),
            ));
        }
        if request.status != UserInputStatus::Pending {
            let message = if request.status == UserInputStatus::Expired {
                "user input request is no longer active"
            } else {
                "user input request has already been answered"
            };
            return Err(AppError::Validation(message.into()));
        }

        let answered_count = request.answers.len();
        if question_index != answered_count {
            return Err(AppError::Validation(
                "this question is no longer awaiting an answer".into(),
            ));
        }

        let question = request
            .questions
            .get(question_index)
            .ok_or_else(|| AppError::Validation("invalid question index".into()))?;
        let option = question
            .options
            .as_ref()
            .and_then(|options| options.get(option_index))
            .ok_or_else(|| AppError::Validation("invalid option index".into()))?;
        let answer = option.label.clone();

        match self
            .apply_user_input_answer(request, user_id, answer)
            .await?
        {
            UserInputAdvance::NextQuestion { text, markup } => {
                Ok(UserInputCallbackResult::Render(text, markup))
            }
            UserInputAdvance::Completed { summary } => {
                Ok(UserInputCallbackResult::Replace(summary))
            }
        }
    }

    pub async fn consume_user_input_text(
        &self,
        chat_id: TelegramChatId,
        user_id: TelegramUserId,
        text: &str,
    ) -> AppResult<Option<UserInputTextResult>> {
        let request = match self
            .storage
            .get_pending_user_input_for_chat(chat_id)
            .await?
        {
            Some(request) => request,
            None => return Ok(None),
        };

        if request.status != UserInputStatus::Pending {
            return Ok(None);
        }

        let answer = text.trim();
        if answer.is_empty() {
            return Ok(None);
        }

        let result = self
            .apply_user_input_answer(request, user_id, answer.to_string())
            .await?;
        Ok(Some(match result {
            UserInputAdvance::NextQuestion { text, markup } => {
                UserInputTextResult::Render(text, markup)
            }
            UserInputAdvance::Completed { summary } => UserInputTextResult::Replace(summary),
        }))
    }

    async fn apply_user_input_answer(
        &self,
        mut request: PendingUserInput,
        user_id: TelegramUserId,
        answer: String,
    ) -> AppResult<UserInputAdvance> {
        let question_index = request.answers.len();
        let question = request
            .questions
            .get(question_index)
            .ok_or_else(|| AppError::Validation("no pending question remains".into()))?;

        request.answers.insert(
            question.id.clone(),
            UserInputAnswer {
                answers: vec![answer],
            },
        );
        let answers_json = serde_json::to_string(&request.answers)?;

        let session = self
            .storage
            .get_session(&request.session_id)
            .await?
            .ok_or_else(|| AppError::Validation("session no longer exists".into()))?;
        let provider_name = session.provider.display_name();

        if request.answers.len() < request.questions.len() {
            self.storage
                .update_pending_user_input_answers(&request.request_id, &answers_json)
                .await?;
            return Ok(UserInputAdvance::NextQuestion {
                text: render_user_input_prompt(provider_name, &request),
                markup: user_input_markup(&request)?,
            });
        }

        self.providers
            .get(session.provider)?
            .resolve_user_input(
                &request.session_id,
                &request.request_id,
                request.answers.clone(),
            )
            .await?;
        self.storage
            .resolve_pending_user_input(
                &request.request_id,
                UserInputStatus::Answered,
                user_id,
                &answers_json,
            )
            .await?;
        self.storage
            .update_session_status(&request.session_id, SessionStatus::Running, None)
            .await?;

        Ok(UserInputAdvance::Completed {
            summary: render_user_input_summary(provider_name, &request),
        })
    }
}
