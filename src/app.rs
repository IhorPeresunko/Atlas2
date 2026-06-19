use crate::{
    codex::CodexClient,
    config::{Config, ServeArgs},
    domain::{ApprovalId, PlanFollowUpId, TelegramChatId, TelegramUserId, UserInputRequestId},
    error::{AppError, AppResult},
    filesystem::FilesystemService,
    services::{
        AppServices, FolderCallbackResult, ModelCallbackResult, PlanFollowUpCallbackResult,
        UserInputCallbackResult, UserInputTextResult,
    },
    storage::Storage,
    stt::SttClient,
    telegram::TelegramClient,
};

#[derive(Clone)]
pub struct App {
    services: AppServices,
}

impl App {
    pub async fn bootstrap(args: &ServeArgs) -> AppResult<Self> {
        let config = Config::load(args)?;
        ensure_database_parent_dir(&config.database_url)?;
        let storage = Storage::connect(&config.database_url).await?;
        storage
            .mark_interrupted_app_server_sessions_failed()
            .await?;
        let telegram = TelegramClient::new(&config.telegram_api_base, &config.telegram_bot_token);
        let filesystem = FilesystemService::default();
        let codex = CodexClient::new(
            config.codex_bin.clone(),
            config.workspace_additional_writable_dirs.clone(),
        );
        let stt = SttClient::from_config(&config)?;

        Ok(Self {
            services: AppServices::new(config, storage, telegram, filesystem, codex, stt),
        })
    }

    pub async fn run(self) -> AppResult<()> {
        tracing::info!("Atlas2 starting with Telegram long polling");
        let mut offset = None;

        loop {
            let updates = self
                .services
                .telegram
                .get_updates(offset, self.services.config.poll_timeout_seconds)
                .await?;

            for update in updates {
                offset = Some(update.update_id + 1);
                if let Err(error) = self.handle_update(update).await {
                    tracing::error!("failed to handle Telegram update: {error}");
                }
            }
        }
    }

    async fn handle_update(&self, update: crate::telegram::Update) -> AppResult<()> {
        let update_id = update.update_id;
        if let Some(message) = update.message {
            let chat_id = TelegramChatId(message.chat.id);
            let chat_kind = message.chat.kind.clone();
            let chat_title = message.chat.title.clone();
            self.services
                .register_chat(chat_id, &message.chat.kind, message.chat.title.as_deref())
                .await?;

            let user_id = message
                .from
                .as_ref()
                .map(|user| TelegramUserId(user.id))
                .ok_or_else(|| AppError::Validation("message missing sender".into()))?;

            if let Some(text) = message.text.clone() {
                tracing::info!(
                    update_id,
                    chat_id = chat_id.0,
                    user_id = user_id.0,
                    chat_kind,
                    chat_title = chat_title.as_deref().unwrap_or(""),
                    text_preview = preview_text(&text),
                    "received Telegram text message"
                );
                if !text.starts_with('/') {
                    if let Some(result) = self
                        .services
                        .user_input
                        .consume_user_input_text(chat_id, user_id, &text)
                        .await?
                    {
                        tracing::info!(
                            update_id,
                            chat_id = chat_id.0,
                            user_id = user_id.0,
                            "routing message to pending user input flow"
                        );
                        match result {
                            UserInputTextResult::Render(text, markup) => {
                                self.services
                                    .telegram
                                    .send_message(chat_id, &text, None, Some(markup))
                                    .await?;
                            }
                            UserInputTextResult::Replace(summary) => {
                                self.services
                                    .telegram
                                    .send_message(chat_id, &summary, None, None)
                                    .await?;
                            }
                        }
                        return Ok(());
                    }
                    if let Some(prompt) = self
                        .services
                        .plans
                        .consume_plan_refinement(chat_id, &text)
                        .await?
                    {
                        tracing::info!(
                            update_id,
                            chat_id = chat_id.0,
                            user_id = user_id.0,
                            prompt_preview = preview_text(&prompt),
                            "routing message to plan refinement flow"
                        );
                        let services = self.services.clone();
                        tokio::spawn(async move {
                            if let Err(error) =
                                services.turns.run_plan_prompt(chat_id, &prompt).await
                            {
                                tracing::error!(
                                    chat_id = chat_id.0,
                                    error = %error,
                                    "plan refinement prompt failed"
                                );
                                let _ = services
                                    .telegram
                                    .send_message(
                                        chat_id,
                                        &format!("Prompt failed: {error}"),
                                        None,
                                        None,
                                    )
                                    .await;
                            }
                        });
                        return Ok(());
                    }
                }

                let route = parse_message_text(&text);
                tracing::info!(
                    update_id,
                    chat_id = chat_id.0,
                    user_id = user_id.0,
                    route = incoming_message_name(&route),
                    "parsed Telegram text message"
                );

                match route {
                    IncomingMessage::Help => {
                        self.services
                            .telegram
                            .send_message(
                                chat_id,
                                "Atlas2 commands:\n/new - reuse a historic project or add a new project folder\n/sessions - list known sessions\n/plan <prompt> - run a read-only planning turn\n/model - pick the Codex model (or /model <name> to set it directly)\nAny other text - send a prompt to the active Codex session\nUse the Stop button on a running turn to interrupt it.",
                                None,
                                None,
                            )
                            .await?;
                    }
                    IncomingMessage::NewSession => {
                        self.services.require_group_admin(chat_id, user_id).await?;
                        let (text, markup) =
                            self.services.folder.begin_new_session(chat_id).await?;
                        self.services
                            .telegram
                            .send_message(chat_id, &text, None, Some(markup))
                            .await?;
                    }
                    IncomingMessage::Sessions => {
                        let summary = self.services.render_sessions().await?;
                        self.services
                            .telegram
                            .send_message(chat_id, &summary, None, None)
                            .await?;
                    }
                    IncomingMessage::Plan(prompt) => {
                        let prompt = prompt.to_string();
                        let services = self.services.clone();
                        tokio::spawn(async move {
                            if let Err(error) =
                                services.turns.run_plan_prompt(chat_id, &prompt).await
                            {
                                tracing::error!(
                                    chat_id = chat_id.0,
                                    error = %error,
                                    prompt_preview = preview_text(&prompt),
                                    "plan prompt failed"
                                );
                                let _ = services
                                    .telegram
                                    .send_message(
                                        chat_id,
                                        &format!("Prompt failed: {error}"),
                                        None,
                                        None,
                                    )
                                    .await;
                            }
                        });
                    }
                    IncomingMessage::PlanUsage => {
                        self.services
                            .telegram
                            .send_message(chat_id, "Usage: /plan <prompt>", None, None)
                            .await?;
                    }
                    IncomingMessage::ModelMenu => {
                        let services = self.services.clone();
                        tokio::spawn(async move {
                            match services.model.model_menu(chat_id).await {
                                Ok((text, markup)) => {
                                    let _ = services
                                        .telegram
                                        .send_message(chat_id, &text, None, markup)
                                        .await;
                                }
                                Err(error) => {
                                    tracing::error!(
                                        chat_id = chat_id.0,
                                        error = %error,
                                        "model menu failed"
                                    );
                                    let _ = services
                                        .telegram
                                        .send_message(
                                            chat_id,
                                            &format!("Could not list models: {error}"),
                                            None,
                                            None,
                                        )
                                        .await;
                                }
                            }
                        });
                    }
                    IncomingMessage::SetModel(model) => {
                        let text = self
                            .services
                            .model
                            .set_chat_model_by_name(chat_id, model)
                            .await?;
                        self.services
                            .telegram
                            .send_message(chat_id, &text, None, None)
                            .await?;
                    }
                    IncomingMessage::UnknownCommand => {
                        self.services
                            .telegram
                            .send_message(chat_id, "Unknown command.", None, None)
                            .await?;
                    }
                    IncomingMessage::Prompt(prompt) => {
                        let prompt = prompt.to_string();
                        let services = self.services.clone();
                        tokio::spawn(async move {
                            if let Err(error) = services.turns.run_prompt(chat_id, &prompt).await {
                                tracing::error!(
                                    chat_id = chat_id.0,
                                    error = %error,
                                    prompt_preview = preview_text(&prompt),
                                    "prompt failed"
                                );
                                let _ = services
                                    .telegram
                                    .send_message(
                                        chat_id,
                                        &format!("Prompt failed: {error}"),
                                        None,
                                        None,
                                    )
                                    .await;
                            }
                        });
                    }
                }
                return Ok(());
            }

            if let Some(voice) = message.voice {
                tracing::info!(
                    update_id,
                    chat_id = chat_id.0,
                    user_id = user_id.0,
                    file_id = voice.file_id,
                    "received Telegram voice message"
                );
                let services = self.services.clone();
                tokio::spawn(async move {
                    if let Err(error) = services
                        .turns
                        .run_voice_prompt(
                            chat_id,
                            &voice.file_id,
                            &voice.file_unique_id,
                            voice.mime_type.as_deref(),
                        )
                        .await
                    {
                        tracing::error!(
                            chat_id = chat_id.0,
                            error = %error,
                            "voice prompt failed"
                        );
                        let _ = services
                            .telegram
                            .send_message(chat_id, &format!("Prompt failed: {error}"), None, None)
                            .await;
                    }
                });
            }

            return Ok(());
        }

        if let Some(callback) = update.callback_query {
            let Some(message) = callback.message else {
                return Ok(());
            };
            let chat_id = TelegramChatId(message.chat.id);
            let user_id = TelegramUserId(callback.from.id);
            let Some(data) = callback.data.as_deref() else {
                return Ok(());
            };
            tracing::info!(
                update_id,
                chat_id = chat_id.0,
                user_id = user_id.0,
                callback_data = data,
                "received Telegram callback query"
            );

            let response = if let Some(id) = data.strip_prefix("approval-approve:") {
                let approval_id = ApprovalId(uuid::Uuid::parse_str(id).map_err(|error| {
                    AppError::Validation(format!("invalid approval ID in callback: {error}"))
                })?);
                self.services
                    .approvals
                    .resolve_approval(approval_id, chat_id, user_id, true)
                    .await
            } else if let Some(id) = data.strip_prefix("approval-reject:") {
                let approval_id = ApprovalId(uuid::Uuid::parse_str(id).map_err(|error| {
                    AppError::Validation(format!("invalid approval ID in callback: {error}"))
                })?);
                self.services
                    .approvals
                    .resolve_approval(approval_id, chat_id, user_id, false)
                    .await
            } else if let Some(id) = data.strip_prefix("turn-stop:") {
                let session_id =
                    crate::domain::SessionId(uuid::Uuid::parse_str(id).map_err(|error| {
                        AppError::Validation(format!("invalid session ID in callback: {error}"))
                    })?);
                self.services
                    .turns
                    .stop_turn(session_id, chat_id, user_id)
                    .await
            } else if let Some(rest) = data.strip_prefix("user-input-answer:") {
                let mut parts = rest.split(':');
                let request_id = parts
                    .next()
                    .ok_or_else(|| AppError::Validation("missing user input request ID".into()))
                    .and_then(|id| {
                        uuid::Uuid::parse_str(id)
                            .map(UserInputRequestId)
                            .map_err(|error| {
                                AppError::Validation(format!(
                                    "invalid user input request ID in callback: {error}"
                                ))
                            })
                    })?;
                let question_index = parts
                    .next()
                    .ok_or_else(|| AppError::Validation("missing question index".into()))?
                    .parse::<usize>()
                    .map_err(|error| {
                        AppError::Validation(format!(
                            "invalid user input question index in callback: {error}"
                        ))
                    })?;
                let option_index = parts
                    .next()
                    .ok_or_else(|| AppError::Validation("missing option index".into()))?
                    .parse::<usize>()
                    .map_err(|error| {
                        AppError::Validation(format!(
                            "invalid user input option index in callback: {error}"
                        ))
                    })?;

                match self
                    .services
                    .user_input
                    .resolve_user_input_choice(
                        request_id,
                        chat_id,
                        user_id,
                        question_index,
                        option_index,
                    )
                    .await?
                {
                    UserInputCallbackResult::Render(text, markup) => {
                        self.services
                            .telegram
                            .edit_message_text(
                                chat_id,
                                message.message_id,
                                &text,
                                None,
                                Some(markup),
                            )
                            .await?;
                        Ok("Choice sent.".into())
                    }
                    UserInputCallbackResult::Replace(text) => {
                        self.services
                            .telegram
                            .edit_message_text(chat_id, message.message_id, &text, None, None)
                            .await?;
                        Ok("Choice sent to Codex.".into())
                    }
                }
            } else if let Some(id) = data.strip_prefix("plan-implement:") {
                let follow_up_id = PlanFollowUpId(uuid::Uuid::parse_str(id).map_err(|error| {
                    AppError::Validation(format!("invalid plan follow-up ID in callback: {error}"))
                })?);
                match self
                    .services
                    .plans
                    .resolve_plan_follow_up_implement(follow_up_id, chat_id, user_id)
                    .await?
                {
                    PlanFollowUpCallbackResult::Replace(text) => {
                        self.services
                            .telegram
                            .edit_message_text(chat_id, message.message_id, &text, None, None)
                            .await?;
                        Ok(text)
                    }
                    PlanFollowUpCallbackResult::Implement { text, prompt } => {
                        self.services
                            .telegram
                            .edit_message_text(chat_id, message.message_id, &text, None, None)
                            .await?;
                        let services = self.services.clone();
                        tokio::spawn(async move {
                            if let Err(error) = services.turns.run_prompt(chat_id, &prompt).await {
                                let _ = services
                                    .telegram
                                    .send_message(
                                        chat_id,
                                        &format!("Prompt failed: {error}"),
                                        None,
                                        None,
                                    )
                                    .await;
                            }
                        });
                        Ok("Starting plan implementation.".into())
                    }
                }
            } else if let Some(id) = data.strip_prefix("plan-refine:") {
                let follow_up_id = PlanFollowUpId(uuid::Uuid::parse_str(id).map_err(|error| {
                    AppError::Validation(format!("invalid plan follow-up ID in callback: {error}"))
                })?);
                match self
                    .services
                    .plans
                    .resolve_plan_follow_up_refine(follow_up_id, chat_id, user_id)
                    .await?
                {
                    PlanFollowUpCallbackResult::Replace(text) => {
                        self.services
                            .telegram
                            .edit_message_text(chat_id, message.message_id, &text, None, None)
                            .await?;
                        Ok("Plan refinement enabled.".into())
                    }
                    PlanFollowUpCallbackResult::Implement { .. } => unreachable!(),
                }
            } else if let Some(model) = data.strip_prefix("model-set:") {
                match self
                    .services
                    .model
                    .select_chat_model(chat_id, model)
                    .await?
                {
                    ModelCallbackResult::Render(text, markup) => {
                        self.services
                            .telegram
                            .edit_message_text(
                                chat_id,
                                message.message_id,
                                &text,
                                None,
                                Some(markup),
                            )
                            .await?;
                        Ok("Now pick a thinking level.".into())
                    }
                    ModelCallbackResult::Replace(text) => {
                        self.services
                            .telegram
                            .edit_message_text(chat_id, message.message_id, &text, None, None)
                            .await?;
                        Ok(text)
                    }
                }
            } else if let Some(effort) = data.strip_prefix("model-effort:") {
                let text = self
                    .services
                    .model
                    .select_chat_reasoning_effort(chat_id, effort)
                    .await?;
                self.services
                    .telegram
                    .edit_message_text(chat_id, message.message_id, &text, None, None)
                    .await?;
                Ok(text)
            } else {
                match self
                    .services
                    .folder
                    .handle_folder_callback(chat_id, user_id, data)
                    .await?
                {
                    FolderCallbackResult::Render(text, markup) => {
                        self.services
                            .telegram
                            .edit_message_text(
                                chat_id,
                                message.message_id,
                                &text,
                                None,
                                Some(markup),
                            )
                            .await?;
                        Ok("Updated folder browser.".into())
                    }
                    FolderCallbackResult::Replace(text) => {
                        self.services
                            .telegram
                            .edit_message_text(chat_id, message.message_id, &text, None, None)
                            .await?;
                        Ok(text)
                    }
                }
            };

            let callback_text = match response {
                Ok(text) => text,
                Err(error) => {
                    tracing::error!(
                        update_id,
                        chat_id = chat_id.0,
                        user_id = user_id.0,
                        callback_data = data,
                        error = %error,
                        "Telegram callback handling failed"
                    );
                    error.to_string()
                }
            };
            self.services
                .telegram
                .answer_callback_query(&callback.id, &callback_text, false)
                .await?;
        }

        Ok(())
    }
}

fn incoming_message_name(message: &IncomingMessage<'_>) -> &'static str {
    match message {
        IncomingMessage::Help => "help",
        IncomingMessage::NewSession => "new_session",
        IncomingMessage::Sessions => "sessions",
        IncomingMessage::Plan(_) => "plan",
        IncomingMessage::PlanUsage => "plan_usage",
        IncomingMessage::ModelMenu => "model_menu",
        IncomingMessage::SetModel(_) => "set_model",
        IncomingMessage::UnknownCommand => "unknown_command",
        IncomingMessage::Prompt(_) => "prompt",
    }
}

fn preview_text(text: &str) -> String {
    const MAX_PREVIEW_CHARS: usize = 120;
    let compact = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let preview: String = compact.chars().take(MAX_PREVIEW_CHARS).collect();
    if compact.chars().count() > MAX_PREVIEW_CHARS {
        format!("{preview}...")
    } else {
        preview
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum IncomingMessage<'a> {
    Help,
    NewSession,
    Sessions,
    Plan(&'a str),
    PlanUsage,
    ModelMenu,
    SetModel(&'a str),
    UnknownCommand,
    Prompt(&'a str),
}

fn parse_message_text(text: &str) -> IncomingMessage<'_> {
    match text {
        "/start" | "/help" => IncomingMessage::Help,
        "/new" => IncomingMessage::NewSession,
        "/sessions" => IncomingMessage::Sessions,
        "/plan" => IncomingMessage::PlanUsage,
        "/model" => IncomingMessage::ModelMenu,
        _ => {
            if let Some(prompt) = text.strip_prefix("/plan ") {
                let prompt = prompt.trim();
                if prompt.is_empty() {
                    IncomingMessage::PlanUsage
                } else {
                    IncomingMessage::Plan(prompt)
                }
            } else if let Some(prompt) = text.strip_prefix("/plan\n") {
                let prompt = prompt.trim();
                if prompt.is_empty() {
                    IncomingMessage::PlanUsage
                } else {
                    IncomingMessage::Plan(prompt)
                }
            } else if let Some(model) = text.strip_prefix("/model ") {
                let model = model.trim();
                if model.is_empty() {
                    IncomingMessage::ModelMenu
                } else {
                    IncomingMessage::SetModel(model)
                }
            } else if text.starts_with('/') {
                IncomingMessage::UnknownCommand
            } else {
                IncomingMessage::Prompt(text)
            }
        }
    }
}

fn ensure_database_parent_dir(database_url: &str) -> AppResult<()> {
    let path = database_url
        .strip_prefix("sqlite://")
        .unwrap_or(database_url);
    let path = std::path::Path::new(path);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{IncomingMessage, parse_message_text, preview_text};

    #[test]
    fn parses_plan_command_with_inline_prompt() {
        assert_eq!(
            parse_message_text("/plan inspect the session flow"),
            IncomingMessage::Plan("inspect the session flow")
        );
    }

    #[test]
    fn rejects_empty_plan_command() {
        assert_eq!(parse_message_text("/plan"), IncomingMessage::PlanUsage);
        assert_eq!(parse_message_text("/plan   "), IncomingMessage::PlanUsage);
    }

    #[test]
    fn parses_model_command_variants() {
        assert_eq!(parse_message_text("/model"), IncomingMessage::ModelMenu);
        assert_eq!(parse_message_text("/model   "), IncomingMessage::ModelMenu);
        assert_eq!(
            parse_message_text("/model gpt-5.5"),
            IncomingMessage::SetModel("gpt-5.5")
        );
    }

    #[test]
    fn parses_plain_text_as_prompt() {
        assert_eq!(
            parse_message_text("hello world"),
            IncomingMessage::Prompt("hello world")
        );
    }

    #[test]
    fn preview_text_compacts_whitespace() {
        assert_eq!(preview_text("hello\n\nworld"), "hello world");
    }
}
