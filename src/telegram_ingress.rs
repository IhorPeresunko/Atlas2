use crate::{
    domain::{ApprovalId, PlanFollowUpId, TelegramChatId, TelegramUserId, UserInputRequestId},
    error::{AppError, AppResult},
    services::{
        AppServices, FolderCallbackResult, ModelCallbackResult, PlanFollowUpCallbackResult,
        UserInputCallbackResult, UserInputTextResult,
    },
    telegram::{BotCommand, TelegramApi, Update},
};

pub(crate) async fn handle_update<Tg>(services: &AppServices<Tg>, update: Update) -> AppResult<()>
where
    Tg: TelegramApi + 'static,
{
    let update_id = update.update_id;
    if let Some(message) = update.message {
        let chat_id = TelegramChatId(message.chat.id);
        let chat_kind = message.chat.kind.clone();
        let chat_title = message.chat.title.clone();
        services
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
                if let Some(result) = services
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
                            services
                                .telegram
                                .send_message(chat_id, &text, None, Some(markup))
                                .await?;
                        }
                        UserInputTextResult::Replace(summary) => {
                            services
                                .telegram
                                .send_message(chat_id, &summary, None, None)
                                .await?;
                        }
                    }
                    return Ok(());
                }
                if let Some(prompt) = services
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
                    let services = services.clone();
                    tokio::spawn(async move {
                        if let Err(error) = services.turns.run_plan_prompt(chat_id, &prompt).await {
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
                    services
                        .telegram
                        .send_message(
                            chat_id,
                            "Atlas2 commands:\n/new - reuse a historic project or add a new project folder\n/resume - resume an existing thread in the active session's workspace\n/sessions - list known sessions\n/plan <prompt> - run a read-only planning turn\n/model - pick the model (or /model <name> to set it directly)\nAny other text - send a prompt to the active session\nUse the Stop button on a running turn to interrupt it.",
                            None,
                            None,
                        )
                        .await?;
                }
                IncomingMessage::NewSession => {
                    services.require_group_admin(chat_id, user_id).await?;
                    // Starting fresh supersedes any in-flight turn for this
                    // chat; cancel it so it does not keep holding the turn lock.
                    services.turns.cancel_active_turn(chat_id).await;
                    let (text, markup) = services.folder.begin_new_session(chat_id).await?;
                    services
                        .telegram
                        .send_message(chat_id, &text, None, Some(markup))
                        .await?;
                }
                IncomingMessage::Resume => {
                    services.require_group_admin(chat_id, user_id).await?;
                    let (text, markup) = services.resume.begin_resume(chat_id).await?;
                    services
                        .telegram
                        .send_message(chat_id, &text, None, markup)
                        .await?;
                }
                IncomingMessage::Sessions => {
                    let summary = services.render_sessions().await?;
                    services
                        .telegram
                        .send_message(chat_id, &summary, None, None)
                        .await?;
                }
                IncomingMessage::Plan(prompt) => {
                    let prompt = prompt.to_string();
                    let services = services.clone();
                    tokio::spawn(async move {
                        if let Err(error) = services.turns.run_plan_prompt(chat_id, &prompt).await {
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
                    services
                        .telegram
                        .send_message(chat_id, "Usage: /plan <prompt>", None, None)
                        .await?;
                }
                IncomingMessage::ModelMenu => {
                    let services = services.clone();
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
                    let text = services
                        .model
                        .set_chat_model_by_name(chat_id, model)
                        .await?;
                    services
                        .telegram
                        .send_message(chat_id, &text, None, None)
                        .await?;
                }
                IncomingMessage::Yolo(desired) => {
                    let text = services
                        .set_skip_permissions(chat_id, user_id, desired)
                        .await?;
                    services
                        .telegram
                        .send_message(chat_id, &text, None, None)
                        .await?;
                }
                IncomingMessage::UnknownCommand => {
                    services
                        .telegram
                        .send_message(chat_id, "Unknown command.", None, None)
                        .await?;
                }
                IncomingMessage::Prompt(prompt) => {
                    let prompt = prompt.to_string();
                    let services = services.clone();
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
            let services = services.clone();
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
            services
                .approvals
                .resolve_approval(approval_id, chat_id, user_id, true)
                .await
        } else if let Some(id) = data.strip_prefix("approval-reject:") {
            let approval_id = ApprovalId(uuid::Uuid::parse_str(id).map_err(|error| {
                AppError::Validation(format!("invalid approval ID in callback: {error}"))
            })?);
            services
                .approvals
                .resolve_approval(approval_id, chat_id, user_id, false)
                .await
        } else if let Some(id) = data.strip_prefix("turn-stop:") {
            let session_id =
                crate::domain::SessionId(uuid::Uuid::parse_str(id).map_err(|error| {
                    AppError::Validation(format!("invalid session ID in callback: {error}"))
                })?);
            services.turns.stop_turn(session_id, chat_id, user_id).await
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

            match services
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
                    services
                        .telegram
                        .edit_message_text(chat_id, message.message_id, &text, None, Some(markup))
                        .await?;
                    Ok("Choice sent.".into())
                }
                UserInputCallbackResult::Replace(text) => {
                    services
                        .telegram
                        .edit_message_text(chat_id, message.message_id, &text, None, None)
                        .await?;
                    Ok("Choice sent.".into())
                }
            }
        } else if let Some(id) = data.strip_prefix("plan-implement:") {
            let follow_up_id = PlanFollowUpId(uuid::Uuid::parse_str(id).map_err(|error| {
                AppError::Validation(format!("invalid plan follow-up ID in callback: {error}"))
            })?);
            match services
                .plans
                .resolve_plan_follow_up_implement(follow_up_id, chat_id, user_id)
                .await?
            {
                PlanFollowUpCallbackResult::Replace(text) => {
                    services
                        .telegram
                        .edit_message_text(chat_id, message.message_id, &text, None, None)
                        .await?;
                    Ok(text)
                }
                PlanFollowUpCallbackResult::Implement { text, prompt } => {
                    services
                        .telegram
                        .edit_message_text(chat_id, message.message_id, &text, None, None)
                        .await?;
                    let services = services.clone();
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
            match services
                .plans
                .resolve_plan_follow_up_refine(follow_up_id, chat_id, user_id)
                .await?
            {
                PlanFollowUpCallbackResult::Replace(text) => {
                    services
                        .telegram
                        .edit_message_text(chat_id, message.message_id, &text, None, None)
                        .await?;
                    Ok("Plan refinement enabled.".into())
                }
                PlanFollowUpCallbackResult::Implement { .. } => unreachable!(),
            }
        } else if let Some(model) = data.strip_prefix("model-set:") {
            match services.model.select_chat_model(chat_id, model).await? {
                ModelCallbackResult::Render(text, markup) => {
                    services
                        .telegram
                        .edit_message_text(chat_id, message.message_id, &text, None, Some(markup))
                        .await?;
                    Ok("Now pick a thinking level.".into())
                }
                ModelCallbackResult::Replace(text) => {
                    services
                        .telegram
                        .edit_message_text(chat_id, message.message_id, &text, None, None)
                        .await?;
                    Ok(text)
                }
            }
        } else if let Some(effort) = data.strip_prefix("model-effort:") {
            let text = services
                .model
                .select_chat_reasoning_effort(chat_id, effort)
                .await?;
            services
                .telegram
                .edit_message_text(chat_id, message.message_id, &text, None, None)
                .await?;
            Ok(text)
        } else if let Some(thread_id) = data.strip_prefix("resume-select:") {
            // Switching threads supersedes any in-flight turn for this chat.
            services.turns.cancel_active_turn(chat_id).await;
            let result = services
                .resume
                .handle_resume_callback(chat_id, user_id, thread_id)
                .await?;
            services
                .telegram
                .edit_message_text(
                    chat_id,
                    message.message_id,
                    &result.confirmation,
                    None,
                    None,
                )
                .await?;
            // send_message chunks long text, so the transcript is shown in full.
            services
                .telegram
                .send_message(chat_id, &result.transcript, None, None)
                .await?;
            Ok("Resumed thread.".into())
        } else {
            match services
                .folder
                .handle_folder_callback(chat_id, user_id, data)
                .await?
            {
                FolderCallbackResult::Render(text, markup) => {
                    services
                        .telegram
                        .edit_message_text(chat_id, message.message_id, &text, None, Some(markup))
                        .await?;
                    Ok("Updated folder browser.".into())
                }
                FolderCallbackResult::Replace(text) => {
                    services
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
        services
            .telegram
            .answer_callback_query(&callback.id, &callback_text, false)
            .await?;
    }

    Ok(())
}

fn incoming_message_name(message: &IncomingMessage<'_>) -> &'static str {
    match message {
        IncomingMessage::Help => "help",
        IncomingMessage::NewSession => "new_session",
        IncomingMessage::Resume => "resume",
        IncomingMessage::Sessions => "sessions",
        IncomingMessage::Plan(_) => "plan",
        IncomingMessage::PlanUsage => "plan_usage",
        IncomingMessage::ModelMenu => "model_menu",
        IncomingMessage::SetModel(_) => "set_model",
        IncomingMessage::Yolo(_) => "yolo",
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

pub(crate) fn bot_commands() -> Vec<BotCommand> {
    vec![
        BotCommand::new("start", "Show intro and help"),
        BotCommand::new("help", "Show available commands"),
        BotCommand::new("new", "Choose or add a project folder"),
        BotCommand::new("resume", "Resume an existing thread"),
        BotCommand::new("sessions", "List known sessions"),
        BotCommand::new("plan", "Run a read-only planning turn"),
        BotCommand::new("model", "Pick or set the model"),
        BotCommand::new("yolo", "Toggle skipping Claude permission prompts (dangerous)"),
    ]
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum IncomingMessage<'a> {
    Help,
    NewSession,
    Resume,
    Sessions,
    Plan(&'a str),
    PlanUsage,
    ModelMenu,
    SetModel(&'a str),
    /// Toggle skipping the agent's permission prompts. `None` flips the current
    /// state; `Some(_)` sets it explicitly (`/yolo on` / `/yolo off`).
    Yolo(Option<bool>),
    UnknownCommand,
    Prompt(&'a str),
}

fn parse_message_text(text: &str) -> IncomingMessage<'_> {
    if !text.starts_with('/') {
        return IncomingMessage::Prompt(text);
    }

    let (command, rest) = split_command(text);
    match command {
        "/start" | "/help" => IncomingMessage::Help,
        "/new" => IncomingMessage::NewSession,
        "/resume" => IncomingMessage::Resume,
        "/sessions" => IncomingMessage::Sessions,
        "/plan" => {
            let prompt = rest.trim();
            if prompt.is_empty() {
                IncomingMessage::PlanUsage
            } else {
                IncomingMessage::Plan(prompt)
            }
        }
        "/model" => {
            let model = rest.trim();
            if model.is_empty() {
                IncomingMessage::ModelMenu
            } else {
                IncomingMessage::SetModel(model)
            }
        }
        "/yolo" => match rest.trim().to_ascii_lowercase().as_str() {
            "on" | "true" | "1" => IncomingMessage::Yolo(Some(true)),
            "off" | "false" | "0" => IncomingMessage::Yolo(Some(false)),
            _ => IncomingMessage::Yolo(None),
        },
        _ => IncomingMessage::UnknownCommand,
    }
}

fn split_command(text: &str) -> (&str, &str) {
    let (command, rest) = match text.find(char::is_whitespace) {
        Some(index) => (&text[..index], &text[index..]),
        None => (text, ""),
    };
    let command = command
        .split_once('@')
        .map(|(command, _)| command)
        .unwrap_or(command);
    (command, rest)
}

#[cfg(test)]
mod tests {
    use super::{IncomingMessage, bot_commands, parse_message_text, preview_text};

    #[test]
    fn parses_plan_command_with_inline_prompt() {
        assert_eq!(
            parse_message_text("/plan inspect the session flow"),
            IncomingMessage::Plan("inspect the session flow")
        );
        assert_eq!(
            parse_message_text("/plan@atlas2codingbot inspect the session flow"),
            IncomingMessage::Plan("inspect the session flow")
        );
    }

    #[test]
    fn rejects_empty_plan_command() {
        assert_eq!(parse_message_text("/plan"), IncomingMessage::PlanUsage);
        assert_eq!(parse_message_text("/plan   "), IncomingMessage::PlanUsage);
        assert_eq!(
            parse_message_text("/plan@atlas2codingbot"),
            IncomingMessage::PlanUsage
        );
    }

    #[test]
    fn parses_yolo_command_variants() {
        assert_eq!(parse_message_text("/yolo"), IncomingMessage::Yolo(None));
        assert_eq!(
            parse_message_text("/yolo@atlas2codingbot"),
            IncomingMessage::Yolo(None)
        );
        assert_eq!(parse_message_text("/yolo on"), IncomingMessage::Yolo(Some(true)));
        assert_eq!(
            parse_message_text("/yolo OFF"),
            IncomingMessage::Yolo(Some(false))
        );
        // Unrecognized argument falls back to a plain toggle.
        assert_eq!(parse_message_text("/yolo maybe"), IncomingMessage::Yolo(None));
    }

    #[test]
    fn parses_model_command_variants() {
        assert_eq!(parse_message_text("/model"), IncomingMessage::ModelMenu);
        assert_eq!(parse_message_text("/model   "), IncomingMessage::ModelMenu);
        assert_eq!(
            parse_message_text("/model@atlas2codingbot"),
            IncomingMessage::ModelMenu
        );
        assert_eq!(
            parse_message_text("/model gpt-5.5"),
            IncomingMessage::SetModel("gpt-5.5")
        );
        assert_eq!(
            parse_message_text("/model@atlas2codingbot gpt-5.5"),
            IncomingMessage::SetModel("gpt-5.5")
        );
    }

    #[test]
    fn parses_mentioned_group_commands() {
        assert_eq!(
            parse_message_text("/help@atlas2codingbot"),
            IncomingMessage::Help
        );
        assert_eq!(
            parse_message_text("/new@atlas2codingbot"),
            IncomingMessage::NewSession
        );
        assert_eq!(
            parse_message_text("/resume@atlas2codingbot"),
            IncomingMessage::Resume
        );
        assert_eq!(
            parse_message_text("/sessions@atlas2codingbot"),
            IncomingMessage::Sessions
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

    #[test]
    fn bot_commands_cover_supported_slash_commands() {
        let commands: Vec<_> = bot_commands()
            .into_iter()
            .map(|command| command.command)
            .collect();

        assert_eq!(
            commands,
            vec![
                "start", "help", "new", "resume", "sessions", "plan", "model", "yolo"
            ]
        );
    }
}
