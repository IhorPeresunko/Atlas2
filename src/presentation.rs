//! Telegram-facing presentation layer.
//!
//! Owns rendering of user-visible text and inline-button markup, the streamed
//! turn-update channel protocol, and the edit-in-place delivery state machine.
//! This module deliberately holds no business rules: services decide *what*
//! happens, this module decides *how it looks* in Telegram.

use regex::Regex;
use tokio::sync::mpsc::UnboundedSender;

use crate::{
    domain::{
        HistoricProject, PendingPlanFollowUp, PendingUserInput, SessionId, TelegramChatId,
        UserInputOption,
    },
    error::{AppError, AppResult},
    telegram::{InlineKeyboardMarkup, ParseMode, TelegramApi, button},
};

pub(crate) const TELEGRAM_TEXT_LIMIT: usize = 3900;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TurnTerminalState {
    Completed,
    Interrupted,
    Stopped,
    Failed,
}

#[derive(Debug)]
pub(crate) enum TelegramTurnUpdate {
    Status(TelegramMessage),
    Message(TelegramMessage),
    ClearStatus,
    Approval {
        summary: String,
        markup: InlineKeyboardMarkup,
    },
    PlanFollowUp {
        text: String,
        markup: InlineKeyboardMarkup,
    },
    UserInput {
        text: String,
        markup: InlineKeyboardMarkup,
    },
}

#[derive(Debug, Clone)]
pub(crate) struct TelegramMessage {
    pub(crate) text: String,
    pub(crate) parse_mode: Option<ParseMode>,
}

#[derive(Debug, Default)]
pub(crate) struct TelegramTurnDeliveryState {
    transient_status_message_id: Option<i64>,
    transient_status_text: Option<String>,
}

pub(crate) fn render_historic_projects_prompt() -> String {
    "Select a project or add a new one.".into()
}

pub(crate) fn historic_projects_markup(projects: &[HistoricProject]) -> InlineKeyboardMarkup {
    let mut buttons = Vec::new();
    for project in projects {
        let label = format!("Reuse {}", compact_absolute_path(&project.workspace_path.0));
        buttons.push(button(
            label,
            format!("project-history-select:{}", project.source_session_id.0),
        ));
    }
    buttons.push(button("Add new project", "project-add-new:current"));
    InlineKeyboardMarkup::single_column(buttons)
}

pub(crate) fn trim_for_telegram(text: &str) -> String {
    let trimmed: String = text.chars().take(TELEGRAM_TEXT_LIMIT).collect();
    if trimmed.is_empty() {
        "Working...".into()
    } else {
        trimmed
    }
}

pub(crate) fn user_input_markup(request: &PendingUserInput) -> AppResult<InlineKeyboardMarkup> {
    let question_index = request.answers.len();
    let question = request
        .questions
        .get(question_index)
        .ok_or_else(|| AppError::Validation("no pending question remains".into()))?;
    let options = question
        .options
        .as_ref()
        .ok_or_else(|| AppError::Validation("question has no selectable options".into()))?;

    let buttons = options
        .iter()
        .enumerate()
        .map(|(option_index, option)| {
            vec![button(
                &option.label,
                format!(
                    "user-input-answer:{}:{}:{}",
                    request.request_id.0, question_index, option_index
                ),
            )]
        })
        .collect();
    Ok(InlineKeyboardMarkup {
        inline_keyboard: buttons,
    })
}

pub(crate) fn render_user_input_prompt(request: &PendingUserInput) -> String {
    let question_index = request.answers.len();
    let question = &request.questions[question_index];

    let mut lines = vec![format!(
        "Codex needs your input ({}/{})",
        question_index + 1,
        request.questions.len()
    )];
    if !question.header.is_empty() {
        lines.push(question.header.clone());
    }
    lines.push(question.question.clone());
    lines.push("Reply with a button tap or send a text answer.".into());

    if !request.answers.is_empty() {
        lines.push(String::new());
        lines.push("Answered so far:".into());
        for answered_question in request.questions.iter().take(question_index) {
            if let Some(answer) = request.answers.get(&answered_question.id) {
                let value = answer.answers.join(", ");
                lines.push(format!("- {}: {}", answered_question.header, value));
            }
        }
    }

    if let Some(options) = question.options.as_ref() {
        lines.push(String::new());
        lines.push("Options:".into());
        for UserInputOption { label, description } in options {
            lines.push(format!("- {}: {}", label, description));
        }
    }

    trim_for_telegram(&lines.join("\n"))
}

pub(crate) fn render_user_input_summary(request: &PendingUserInput) -> String {
    let mut lines = vec!["Sent your response to Codex.".into()];
    for question in &request.questions {
        if let Some(answer) = request.answers.get(&question.id) {
            lines.push(format!(
                "- {}: {}",
                question.header,
                answer.answers.join(", ")
            ));
        }
    }
    trim_for_telegram(&lines.join("\n"))
}

pub(crate) fn plan_follow_up_markup(follow_up: &PendingPlanFollowUp) -> InlineKeyboardMarkup {
    InlineKeyboardMarkup {
        inline_keyboard: vec![vec![
            button(
                "Implement",
                format!("plan-implement:{}", follow_up.follow_up_id.0),
            ),
            button(
                "Add details",
                format!("plan-refine:{}", follow_up.follow_up_id.0),
            ),
        ]],
    }
}

pub(crate) fn turn_control_markup(session_id: &SessionId) -> InlineKeyboardMarkup {
    InlineKeyboardMarkup::single_column(vec![button("Stop", format!("turn-stop:{}", session_id.0))])
}

pub(crate) fn render_turn_terminal_text(state: TurnTerminalState, detail: Option<&str>) -> String {
    match state {
        TurnTerminalState::Completed => "Codex turn completed.".into(),
        TurnTerminalState::Interrupted => "Codex turn interrupted.".into(),
        TurnTerminalState::Stopped => "Codex turn stopped.".into(),
        TurnTerminalState::Failed => match detail {
            Some(detail) if !detail.is_empty() => {
                trim_for_telegram(&format!("Codex turn failed.\n{detail}"))
            }
            _ => "Codex turn failed.".into(),
        },
    }
}

pub(crate) fn render_voice_transcript_message(transcript: &str) -> String {
    format!(
        "Transcribed voice message:\n{}",
        compact_text_for_telegram(transcript)
    )
}

pub(crate) fn send_text_update(
    telegram_updates_tx: &UnboundedSender<TelegramTurnUpdate>,
    text: impl Into<String>,
) {
    send_plain_update(telegram_updates_tx, text);
}

pub(crate) fn send_status_update(
    telegram_updates_tx: &UnboundedSender<TelegramTurnUpdate>,
    text: impl Into<String>,
) {
    let compact = compact_text_for_telegram(&text.into());
    let _ = telegram_updates_tx.send(TelegramTurnUpdate::Status(TelegramMessage {
        text: trim_for_telegram(&compact),
        parse_mode: None,
    }));
}

pub(crate) fn send_clear_status_update(telegram_updates_tx: &UnboundedSender<TelegramTurnUpdate>) {
    let _ = telegram_updates_tx.send(TelegramTurnUpdate::ClearStatus);
}

fn send_plain_update(
    telegram_updates_tx: &UnboundedSender<TelegramTurnUpdate>,
    text: impl Into<String>,
) {
    let compact = compact_text_for_telegram(&text.into());
    let _ = telegram_updates_tx.send(TelegramTurnUpdate::Message(TelegramMessage {
        text: if compact.is_empty() {
            "Working...".into()
        } else {
            compact
        },
        parse_mode: None,
    }));
}

pub(crate) fn send_command_finished_update(
    telegram_updates_tx: &UnboundedSender<TelegramTurnUpdate>,
    message: TelegramMessage,
) {
    let _ = telegram_updates_tx.send(TelegramTurnUpdate::Message(message));
}

pub(crate) async fn send_telegram_update<T: TelegramApi>(
    telegram: &T,
    chat_id: TelegramChatId,
    delivery_state: &mut TelegramTurnDeliveryState,
    update: TelegramTurnUpdate,
) -> AppResult<()> {
    match update {
        TelegramTurnUpdate::Status(message) => {
            upsert_status_message(telegram, chat_id, delivery_state, message).await?;
        }
        TelegramTurnUpdate::Message(message) => {
            clear_status_message(telegram, chat_id, delivery_state).await?;
            telegram
                .send_message(chat_id, &message.text, message.parse_mode, None)
                .await?;
        }
        TelegramTurnUpdate::ClearStatus => {
            clear_status_message(telegram, chat_id, delivery_state).await?;
        }
        TelegramTurnUpdate::Approval { summary, markup } => {
            clear_status_message(telegram, chat_id, delivery_state).await?;
            telegram
                .send_message(chat_id, &summary, None, Some(markup))
                .await?;
        }
        TelegramTurnUpdate::PlanFollowUp { text, markup } => {
            clear_status_message(telegram, chat_id, delivery_state).await?;
            telegram
                .send_message(chat_id, &text, None, Some(markup))
                .await?;
        }
        TelegramTurnUpdate::UserInput { text, markup } => {
            clear_status_message(telegram, chat_id, delivery_state).await?;
            telegram
                .send_message(chat_id, &text, None, Some(markup))
                .await?;
        }
    }
    Ok(())
}

async fn upsert_status_message<T: TelegramApi>(
    telegram: &T,
    chat_id: TelegramChatId,
    delivery_state: &mut TelegramTurnDeliveryState,
    message: TelegramMessage,
) -> AppResult<()> {
    if let Some(message_id) = delivery_state.transient_status_message_id {
        if delivery_state.transient_status_text.as_deref() == Some(message.text.as_str()) {
            return Ok(());
        }
        telegram
            .edit_message_text(chat_id, message_id, &message.text, message.parse_mode, None)
            .await?;
    } else {
        let sent = telegram
            .send_message(chat_id, &message.text, message.parse_mode, None)
            .await?;
        delivery_state.transient_status_message_id = Some(sent.message_id);
    }

    delivery_state.transient_status_text = Some(message.text);
    Ok(())
}

async fn clear_status_message<T: TelegramApi>(
    telegram: &T,
    chat_id: TelegramChatId,
    delivery_state: &mut TelegramTurnDeliveryState,
) -> AppResult<()> {
    if let Some(message_id) = delivery_state.transient_status_message_id.take() {
        let _ = telegram.delete_message(chat_id, message_id).await?;
    }
    delivery_state.transient_status_text = None;
    Ok(())
}

pub(crate) fn render_command_finished_message(
    command: &str,
    exit_code: i64,
    output: &str,
) -> TelegramMessage {
    let summary = format!(
        "<b>Command finished ({exit_code})</b>\n<code>{}</code>\n<blockquote expandable>",
        escape_html(command)
    );
    let suffix = "</blockquote>";
    let available = TELEGRAM_TEXT_LIMIT.saturating_sub(summary.len() + suffix.len());
    let escaped_output = escape_html(output);
    let output_body = if escaped_output.is_empty() {
        "(no output)".to_string()
    } else {
        trim_html_body(&escaped_output, available)
    };

    TelegramMessage {
        text: format!("{summary}{output_body}{suffix}"),
        parse_mode: Some(ParseMode::Html),
    }
}

fn trim_html_body(text: &str, max_len: usize) -> String {
    if max_len == 0 {
        return String::new();
    }

    let mut trimmed = String::new();
    for ch in text.chars() {
        if trimmed.len() + ch.len_utf8() > max_len {
            break;
        }
        trimmed.push(ch);
    }

    if trimmed.is_empty() {
        return trimmed;
    }

    if trimmed.len() == text.len() {
        return trimmed;
    }

    let ellipsis = "...";
    while trimmed.len() + ellipsis.len() > max_len {
        if trimmed.pop().is_none() {
            return String::new();
        }
    }
    trimmed.push_str(ellipsis);
    trimmed
}

fn escape_html(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            _ => escaped.push(ch),
        }
    }
    escaped
}

pub(crate) fn compact_text_for_telegram(text: &str) -> String {
    let mut compacted = replace_markdown_file_links(text);
    compacted = shorten_bare_absolute_paths(&compacted);
    compacted
}

fn replace_markdown_file_links(text: &str) -> String {
    let re = Regex::new(r"\[([^\]]+)\]\((/[^)\s]+)\)").expect("valid markdown file link regex");
    re.replace_all(text, |captures: &regex::Captures<'_>| {
        compact_path_label(&captures[1])
    })
    .into_owned()
}

fn shorten_bare_absolute_paths(text: &str) -> String {
    let re = Regex::new(r"(/home/[^\s)\]]+)").expect("valid absolute path regex");
    re.replace_all(text, |captures: &regex::Captures<'_>| {
        compact_absolute_path(&captures[1])
    })
    .into_owned()
}

fn compact_path_label(label: &str) -> String {
    if label.contains('/') {
        compact_relative_path(label)
    } else {
        label.to_string()
    }
}

fn compact_relative_path(label: &str) -> String {
    let (path, suffix) = split_path_suffix(label);
    let segments: Vec<&str> = path
        .split('/')
        .filter(|segment| !segment.is_empty())
        .collect();
    if segments.len() <= 3 {
        return label.to_string();
    }

    format!(
        ".../{}/{}{}",
        segments[segments.len() - 2],
        segments[segments.len() - 1],
        suffix
    )
}

fn compact_absolute_path(path: &str) -> String {
    let (path, suffix) = split_path_suffix(path);
    let segments: Vec<&str> = path
        .split('/')
        .filter(|segment| !segment.is_empty())
        .collect();
    if segments.len() <= 3 {
        return format!("{path}{suffix}");
    }

    format!(
        ".../{}/{}{}",
        segments[segments.len() - 2],
        segments[segments.len() - 1],
        suffix
    )
}

fn split_path_suffix(path: &str) -> (&str, &str) {
    for marker in ["#L", ":", "?"] {
        if let Some(index) = path.find(marker) {
            return (&path[..index], &path[index..]);
        }
    }
    (path, "")
}
