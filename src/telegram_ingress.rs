use crate::{
    domain::{ApprovalId, PlanFollowUpId, TelegramChatId, TelegramUserId, UserInputRequestId},
    error::{AppError, AppResult},
    services::{
        AppServices, FolderCallbackResult, ModelCallbackResult, PlanFollowUpCallbackResult,
        UserInputCallbackResult, UserInputTextResult,
    },
    telegram::{BotCommand, ChatMemberUpdated, TelegramApi, Update},
};

pub(crate) async fn handle_update<Tg>(services: &AppServices<Tg>, update: Update) -> AppResult<()>
where
    Tg: TelegramApi + 'static,
{
    let update_id = update.update_id;

    if let Some(membership) = update.my_chat_member {
        return handle_my_chat_member(services, update_id, membership).await;
    }

    if let Some(message) = update.message {
        let chat_id = TelegramChatId(message.chat.id);
        let chat_kind = message.chat.kind.clone();
        let chat_title = message.chat.title.clone();

        let user_id = message
            .from
            .as_ref()
            .map(|user| TelegramUserId(user.id))
            .ok_or_else(|| AppError::Validation("message missing sender".into()))?;

        // Trust-on-first-use: while the bot is unclaimed, the first person to DM
        // it becomes the owner. This is what removes the need to look up your
        // numeric ID — you just message your own bot once, right after creating it.
        if chat_kind == "private" && services.try_claim_owner(user_id).await? {
            tracing::info!(
                update_id,
                user_id = user_id.0,
                "owner claimed via first direct message"
            );
            services
                .telegram
                .send_message(
                    chat_id,
                    "✅ You are now the owner of this bot. Only you can use it and add it to groups. Send /help to get started.",
                    None,
                    None,
                )
                .await?;
            return Ok(());
        }

        // Owner-only activation commands must work before a chat is authorized,
        // so they are handled ahead of the authorization gate.
        if let Some(activation) = message.text.as_deref().and_then(parse_activation) {
            return handle_activation(
                services,
                update_id,
                chat_id,
                &chat_kind,
                chat_title.as_deref(),
                user_id,
                activation,
            )
            .await;
        }

        // Authorization gate: silently ignore anything from a chat the owner has
        // not authorized, and any private chat that is not the owner's. This is
        // the single chokepoint that keeps strangers from driving the agent. The
        // owner's own interaction in a group auto-activates it (see helper).
        if !ensure_chat_authorized(
            services,
            update_id,
            chat_id,
            &chat_kind,
            chat_title.as_deref(),
            user_id,
        )
        .await?
        {
            tracing::warn!(
                update_id,
                chat_id = chat_id.0,
                user_id = user_id.0,
                chat_kind,
                "ignoring message from unauthorized chat"
            );
            return Ok(());
        }

        services
            .register_chat(chat_id, &message.chat.kind, message.chat.title.as_deref())
            .await?;

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
                    let text = services.set_skip_permissions(chat_id, desired).await?;
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
        let chat_kind = message.chat.kind.clone();
        let chat_title = message.chat.title.clone();
        let user_id = TelegramUserId(callback.from.id);

        // Same authorization gate as messages: a callback from an unauthorized
        // chat (or a non-owner DM) must never reach a handler. The owner's own
        // interaction in a group auto-activates it (see helper).
        if !ensure_chat_authorized(
            services,
            update_id,
            chat_id,
            &chat_kind,
            chat_title.as_deref(),
            user_id,
        )
        .await?
        {
            tracing::warn!(
                update_id,
                chat_id = chat_id.0,
                user_id = user_id.0,
                chat_kind,
                "ignoring callback from unauthorized chat"
            );
            services
                .telegram
                .answer_callback_query(&callback.id, "Not authorized.", true)
                .await?;
            return Ok(());
        }

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
            services.turns.stop_turn(session_id, chat_id).await
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
                .handle_resume_callback(chat_id, thread_id)
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
                .handle_folder_callback(chat_id, data)
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

/// Authorization gate shared by the message and callback paths. Returns whether
/// the bot may act on this chat.
///
/// Beyond the stored authorization flag, this auto-activates a group the moment
/// the **owner** interacts in it: only the owner can have added the bot to a
/// group (a non-owner add gets the bot kicked), so the owner showing up is
/// authorization in itself. This removes the need to run `/activate` and
/// self-heals groups whose add-event was missed (e.g. the bot was added while
/// the daemon was down).
async fn ensure_chat_authorized<Tg>(
    services: &AppServices<Tg>,
    update_id: i64,
    chat_id: TelegramChatId,
    chat_kind: &str,
    chat_title: Option<&str>,
    user_id: TelegramUserId,
) -> AppResult<bool>
where
    Tg: TelegramApi + 'static,
{
    if services.is_authorized(chat_id, chat_kind, user_id).await? {
        return Ok(true);
    }
    if chat_kind != "private" && services.is_owner(user_id).await? {
        services
            .authorize_chat(chat_id, chat_kind, chat_title)
            .await?;
        tracing::info!(
            update_id,
            chat_id = chat_id.0,
            "owner interacted in an unauthorized group; chat authorized"
        );
        return Ok(true);
    }
    Ok(false)
}

/// Confirmation sent when a group is activated, tailored to whether the bot has
/// the admin rights it needs. A non-admin bot is subject to Telegram group
/// privacy mode (on by default), which withholds ordinary messages — only
/// commands and replies reach it — so the owner is told to promote it before the
/// group is usable for normal chatting.
fn group_activated_message(bot_is_admin: bool) -> &'static str {
    if bot_is_admin {
        "✅ Atlas2 is active here and I can read messages. Send /new to start."
    } else {
        "⚠️ Atlas2 is active here, but I'm not a group admin — Telegram only lets me see \
         /commands and replies, not normal messages. Make me an admin (or disable privacy \
         mode in @BotFather), then everyone can just chat with me. Send /new to start."
    }
}

/// Reacts to a change in the bot's own membership in a chat. This is how the
/// "only the owner can add the bot to a group" rule is enforced: the bot leaves
/// any group it was added to by someone other than the owner, and auto-authorizes
/// groups the owner adds it to.
async fn handle_my_chat_member<Tg>(
    services: &AppServices<Tg>,
    update_id: i64,
    membership: ChatMemberUpdated,
) -> AppResult<()>
where
    Tg: TelegramApi + 'static,
{
    let chat_id = TelegramChatId(membership.chat.id);
    let chat_kind = membership.chat.kind.clone();
    let chat_title = membership.chat.title.clone();
    let actor = TelegramUserId(membership.from.id);
    let bot_present = membership.new_chat_member.is_present();
    let bot_is_admin = membership.new_chat_member.is_admin();

    // Private chats are authorized per-message by owner identity, not by a stored
    // flag, so membership changes there need no action.
    if chat_kind == "private" {
        return Ok(());
    }

    // Bot left or was removed: drop authorization so any re-add must be
    // re-authorized by the owner.
    if !bot_present {
        services.deauthorize_chat(chat_id).await?;
        return Ok(());
    }

    let owner = match services.owner_id().await? {
        Some(owner) => owner,
        None => {
            // Unclaimed: the first person to add the bot anywhere becomes the
            // owner, and the group they added it to is authorized (trust-on-
            // first-use), mirroring claim-via-DM.
            services.try_claim_owner(actor).await?;
            services
                .authorize_chat(chat_id, &chat_kind, chat_title.as_deref())
                .await?;
            tracing::info!(
                update_id,
                chat_id = chat_id.0,
                actor = actor.0,
                "owner claimed via group add; chat authorized"
            );
            let _ = services
                .telegram
                .send_message(
                    chat_id,
                    &format!(
                        "✅ You are now the owner of this bot.\n\n{}",
                        group_activated_message(bot_is_admin)
                    ),
                    None,
                    None,
                )
                .await;
            return Ok(());
        }
    };

    if actor.0 == owner {
        services
            .authorize_chat(chat_id, &chat_kind, chat_title.as_deref())
            .await?;
        tracing::info!(
            update_id,
            chat_id = chat_id.0,
            "owner added bot to group; chat authorized"
        );
        let _ = services
            .telegram
            .send_message(chat_id, group_activated_message(bot_is_admin), None, None)
            .await;
    } else {
        tracing::warn!(
            update_id,
            chat_id = chat_id.0,
            actor = actor.0,
            "non-owner added bot to a group; leaving"
        );
        let _ = services
            .telegram
            .send_message(
                chat_id,
                "This bot is private. Only its owner can add it to groups. Leaving.",
                None,
                None,
            )
            .await;
        services.deauthorize_chat(chat_id).await?;
        services.telegram.leave_chat(chat_id).await?;
    }

    Ok(())
}

/// Owner-only `/activate` and `/deactivate` for the current chat. Used to enable
/// the bot in groups it is already a member of (e.g. after upgrading), and to
/// revoke a group later.
async fn handle_activation<Tg>(
    services: &AppServices<Tg>,
    update_id: i64,
    chat_id: TelegramChatId,
    chat_kind: &str,
    chat_title: Option<&str>,
    user_id: TelegramUserId,
    activation: Activation,
) -> AppResult<()>
where
    Tg: TelegramApi + 'static,
{
    if !services.is_owner(user_id).await? {
        tracing::warn!(
            update_id,
            chat_id = chat_id.0,
            user_id = user_id.0,
            "non-owner attempted an activation command"
        );
        // Only acknowledge in chats that are already authorized, so the bot stays
        // silent to strangers in chats it has not been activated for.
        if services.is_authorized(chat_id, chat_kind, user_id).await? {
            services
                .telegram
                .send_message(
                    chat_id,
                    "Only the bot owner can activate or deactivate this chat.",
                    None,
                    None,
                )
                .await?;
        }
        return Ok(());
    }

    match activation {
        Activation::Activate => {
            services
                .authorize_chat(chat_id, chat_kind, chat_title)
                .await?;
            services
                .telegram
                .send_message(
                    chat_id,
                    "Atlas2 is now active in this chat. Everyone here can use it — send /new to start.",
                    None,
                    None,
                )
                .await?;
        }
        Activation::Deactivate => {
            services.deauthorize_chat(chat_id).await?;
            services
                .telegram
                .send_message(
                    chat_id,
                    "Atlas2 is now deactivated in this chat. Send /activate to re-enable it.",
                    None,
                    None,
                )
                .await?;
        }
    }

    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Activation {
    Activate,
    Deactivate,
}

/// Recognizes the owner-only activation commands, which are routed before the
/// authorization gate. Returns `None` for everything else.
fn parse_activation(text: &str) -> Option<Activation> {
    let (command, _) = split_command(text);
    match command {
        "/activate" => Some(Activation::Activate),
        "/deactivate" => Some(Activation::Deactivate),
        _ => None,
    }
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
        BotCommand::new("activate", "Owner only: enable Atlas2 in this chat"),
        BotCommand::new("deactivate", "Owner only: disable Atlas2 in this chat"),
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
    fn parses_activation_commands() {
        use super::{Activation, parse_activation};
        assert_eq!(parse_activation("/activate"), Some(Activation::Activate));
        assert_eq!(
            parse_activation("/activate@atlas2codingbot"),
            Some(Activation::Activate)
        );
        assert_eq!(
            parse_activation("/deactivate"),
            Some(Activation::Deactivate)
        );
        assert_eq!(parse_activation("/new"), None);
        assert_eq!(parse_activation("hello"), None);
    }

    #[test]
    fn group_activation_message_warns_only_without_admin() {
        use super::group_activated_message;
        // Admin: clean confirmation, no privacy-mode warning.
        let admin = group_activated_message(true);
        assert!(!admin.contains("admin"));
        assert!(admin.contains("/new"));
        // Non-admin: must tell the owner why normal messages won't work.
        let non_admin = group_activated_message(false);
        assert!(non_admin.contains("admin"));
        assert!(non_admin.to_lowercase().contains("privacy"));
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
                "start", "help", "new", "resume", "sessions", "plan", "model", "yolo", "activate",
                "deactivate"
            ]
        );
    }
}
