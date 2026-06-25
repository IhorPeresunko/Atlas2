use std::time::Duration;

use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{Value, json};

use crate::{
    domain::TelegramChatId,
    error::{AppError, AppResult},
};

const TELEGRAM_TEXT_LIMIT: usize = 4096;
const TELEGRAM_MAX_RETRIES: usize = 5;
const TELEGRAM_RETRY_PADDING_SECS: u64 = 1;

#[derive(Clone)]
pub struct TelegramClient {
    http: reqwest::Client,
    base_url: String,
    file_base_url: String,
    bot_token: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum ParseMode {
    #[serde(rename = "HTML")]
    Html,
}

impl TelegramClient {
    pub fn new(api_base: &str, bot_token: &str) -> Self {
        let api_base = api_base.trim_end_matches('/');
        let base_url = format!("{api_base}/bot{bot_token}");
        let file_base_url = format!("{api_base}/file/bot{bot_token}");
        Self {
            http: reqwest::Client::new(),
            base_url,
            file_base_url,
            bot_token: bot_token.to_string(),
        }
    }

    /// Converts an HTTP error into an AppError with the bot token scrubbed.
    /// reqwest errors include the request URL, which embeds the token in its
    /// `/bot<token>/` path segment, so the raw error must never reach the logs.
    fn redact(&self, error: reqwest::Error) -> AppError {
        let message = error.to_string().replace(&self.bot_token, "<redacted>");
        AppError::Telegram(format!("telegram request failed: {message}"))
    }

    pub async fn get_updates(
        &self,
        offset: Option<i64>,
        timeout_seconds: u64,
    ) -> AppResult<Vec<Update>> {
        let mut payload = json!({
            "timeout": timeout_seconds,
            "allowed_updates": ["message", "callback_query", "my_chat_member"]
        });
        if let Some(offset) = offset {
            payload["offset"] = json!(offset);
        }

        self.call("getUpdates", &payload).await
    }

    pub async fn set_my_commands(&self, commands: &[BotCommand]) -> AppResult<bool> {
        self.call("setMyCommands", &json!({ "commands": commands }))
            .await
    }

    pub async fn send_message(
        &self,
        chat_id: TelegramChatId,
        text: &str,
        parse_mode: Option<ParseMode>,
        reply_markup: Option<InlineKeyboardMarkup>,
    ) -> AppResult<Message> {
        let chunks = split_message_text(text, parse_mode);
        let mut sent_messages = Vec::with_capacity(chunks.len());

        for (index, chunk) in chunks.into_iter().enumerate() {
            let mut payload = json!({
                "chat_id": chat_id.0,
                "text": chunk,
            });
            if let Some(parse_mode) = parse_mode {
                payload["parse_mode"] = serde_json::to_value(parse_mode)?;
            }
            if index == 0 {
                if let Some(markup) = reply_markup.as_ref() {
                    payload["reply_markup"] = serde_json::to_value(markup)?;
                }
            }
            sent_messages.push(self.call("sendMessage", &payload).await?);
        }

        sent_messages
            .into_iter()
            .next()
            .ok_or_else(|| AppError::Telegram("telegram sendMessage produced no chunks".into()))
    }

    pub async fn edit_message_text(
        &self,
        chat_id: TelegramChatId,
        message_id: i64,
        text: &str,
        parse_mode: Option<ParseMode>,
        reply_markup: Option<InlineKeyboardMarkup>,
    ) -> AppResult<Message> {
        let text = trim_message_text(text, parse_mode);
        let mut payload = json!({
            "chat_id": chat_id.0,
            "message_id": message_id,
            "text": text,
        });
        if let Some(parse_mode) = parse_mode {
            payload["parse_mode"] = serde_json::to_value(parse_mode)?;
        }
        if let Some(markup) = reply_markup {
            payload["reply_markup"] = serde_json::to_value(markup)?;
        }
        self.call("editMessageText", &payload).await
    }

    pub async fn delete_message(
        &self,
        chat_id: TelegramChatId,
        message_id: i64,
    ) -> AppResult<bool> {
        self.call(
            "deleteMessage",
            &json!({
                "chat_id": chat_id.0,
                "message_id": message_id,
            }),
        )
        .await
    }

    pub async fn answer_callback_query(
        &self,
        callback_query_id: &str,
        text: &str,
        show_alert: bool,
    ) -> AppResult<bool> {
        self.call(
            "answerCallbackQuery",
            &json!({
                "callback_query_id": callback_query_id,
                "text": text,
                "show_alert": show_alert
            }),
        )
        .await
    }

    pub async fn get_file(&self, file_id: &str) -> AppResult<TelegramFile> {
        self.call("getFile", &json!({ "file_id": file_id })).await
    }

    /// Removes the bot from a chat. Used to refuse groups added by anyone other
    /// than the owner, so the bot never lingers in unauthorized groups.
    pub async fn leave_chat(&self, chat_id: TelegramChatId) -> AppResult<bool> {
        self.call("leaveChat", &json!({ "chat_id": chat_id.0 }))
            .await
    }

    pub async fn download_file_bytes(&self, file_path: &str) -> AppResult<Vec<u8>> {
        let response = self
            .http
            .get(format!("{}/{}", self.file_base_url, file_path))
            .send()
            .await
            .map_err(|error| self.redact(error))?;

        if !response.status().is_success() {
            return Err(AppError::Telegram(format!(
                "telegram file download failed with status {}",
                response.status()
            )));
        }

        Ok(response
            .bytes()
            .await
            .map_err(|error| self.redact(error))?
            .to_vec())
    }

    async fn call<T: DeserializeOwned>(&self, method: &str, payload: &Value) -> AppResult<T> {
        let mut retries = 0;
        loop {
            let response = self
                .http
                .post(format!("{}/{}", self.base_url, method))
                .json(payload)
                .send()
                .await
                .map_err(|error| self.redact(error))?;

            let envelope: TelegramEnvelope<T> =
                response.json().await.map_err(|error| self.redact(error))?;
            if envelope.ok {
                return envelope.result.ok_or_else(|| {
                    AppError::Telegram(format!("telegram method {method} returned no result"))
                });
            }

            if let Some(retry_after_secs) = telegram_retry_after_seconds(&envelope) {
                if retries < TELEGRAM_MAX_RETRIES {
                    retries += 1;
                    tracing::warn!(
                        method,
                        retry_after_secs,
                        retries,
                        "Telegram rate limited request; retrying"
                    );
                    tokio::time::sleep(Duration::from_secs(
                        retry_after_secs.saturating_add(TELEGRAM_RETRY_PADDING_SECS),
                    ))
                    .await;
                    continue;
                }
            }

            return Err(AppError::Telegram(
                envelope
                    .description
                    .unwrap_or_else(|| format!("telegram method {method} failed")),
            ));
        }
    }
}

/// Seam over the Telegram Bot API adapter so business logic in `services` (and
/// the presentation delivery loop) can run against a fake instead of real HTTP.
#[async_trait::async_trait]
pub(crate) trait TelegramApi: Clone + Send + Sync {
    async fn send_message(
        &self,
        chat_id: TelegramChatId,
        text: &str,
        parse_mode: Option<ParseMode>,
        reply_markup: Option<InlineKeyboardMarkup>,
    ) -> AppResult<Message>;

    async fn edit_message_text(
        &self,
        chat_id: TelegramChatId,
        message_id: i64,
        text: &str,
        parse_mode: Option<ParseMode>,
        reply_markup: Option<InlineKeyboardMarkup>,
    ) -> AppResult<Message>;

    async fn delete_message(&self, chat_id: TelegramChatId, message_id: i64) -> AppResult<bool>;

    async fn answer_callback_query(
        &self,
        callback_query_id: &str,
        text: &str,
        show_alert: bool,
    ) -> AppResult<bool>;

    async fn get_file(&self, file_id: &str) -> AppResult<TelegramFile>;

    async fn download_file_bytes(&self, file_path: &str) -> AppResult<Vec<u8>>;

    async fn leave_chat(&self, chat_id: TelegramChatId) -> AppResult<bool>;
}

#[async_trait::async_trait]
impl TelegramApi for TelegramClient {
    async fn send_message(
        &self,
        chat_id: TelegramChatId,
        text: &str,
        parse_mode: Option<ParseMode>,
        reply_markup: Option<InlineKeyboardMarkup>,
    ) -> AppResult<Message> {
        TelegramClient::send_message(self, chat_id, text, parse_mode, reply_markup).await
    }

    async fn edit_message_text(
        &self,
        chat_id: TelegramChatId,
        message_id: i64,
        text: &str,
        parse_mode: Option<ParseMode>,
        reply_markup: Option<InlineKeyboardMarkup>,
    ) -> AppResult<Message> {
        TelegramClient::edit_message_text(self, chat_id, message_id, text, parse_mode, reply_markup)
            .await
    }

    async fn delete_message(&self, chat_id: TelegramChatId, message_id: i64) -> AppResult<bool> {
        TelegramClient::delete_message(self, chat_id, message_id).await
    }

    async fn answer_callback_query(
        &self,
        callback_query_id: &str,
        text: &str,
        show_alert: bool,
    ) -> AppResult<bool> {
        TelegramClient::answer_callback_query(self, callback_query_id, text, show_alert).await
    }

    async fn get_file(&self, file_id: &str) -> AppResult<TelegramFile> {
        TelegramClient::get_file(self, file_id).await
    }

    async fn download_file_bytes(&self, file_path: &str) -> AppResult<Vec<u8>> {
        TelegramClient::download_file_bytes(self, file_path).await
    }

    async fn leave_chat(&self, chat_id: TelegramChatId) -> AppResult<bool> {
        TelegramClient::leave_chat(self, chat_id).await
    }
}

fn telegram_retry_after_seconds<T>(envelope: &TelegramEnvelope<T>) -> Option<u64> {
    envelope.parameters.as_ref()?.retry_after
}

fn split_message_text(text: &str, parse_mode: Option<ParseMode>) -> Vec<String> {
    if text.is_empty() {
        return vec![String::new()];
    }

    if parse_mode.is_some() {
        return vec![trim_message_text(text, parse_mode)];
    }

    let mut chunks = Vec::new();
    let mut remaining = text;
    while !remaining.is_empty() {
        if remaining.chars().count() <= TELEGRAM_TEXT_LIMIT {
            chunks.push(remaining.to_string());
            break;
        }

        let split_at = find_split_index(remaining, TELEGRAM_TEXT_LIMIT);
        let (chunk, rest) = remaining.split_at(split_at);
        chunks.push(chunk.to_string());
        remaining = rest;
    }

    chunks
}

fn trim_message_text(text: &str, parse_mode: Option<ParseMode>) -> String {
    let char_count = text.chars().count();
    if char_count <= TELEGRAM_TEXT_LIMIT {
        return text.to_string();
    }

    let mut trimmed = String::new();
    let target = TELEGRAM_TEXT_LIMIT.saturating_sub(3);
    let mut trimmed_chars = 0;
    for ch in text.chars() {
        if trimmed_chars >= target {
            break;
        }
        trimmed.push(ch);
        trimmed_chars += 1;
    }

    if parse_mode.is_some() {
        // HTML messages are expected to be pre-rendered safely upstream.
        return trimmed;
    }

    trimmed.push_str("...");
    trimmed
}

fn find_split_index(text: &str, max_chars: usize) -> usize {
    let mut candidate = None;
    let mut char_count = 0;

    for (byte_index, ch) in text.char_indices() {
        if char_count == max_chars {
            break;
        }
        char_count += 1;
        if ch == '\n' || ch.is_whitespace() {
            candidate = Some(byte_index + ch.len_utf8());
        }
    }

    candidate.unwrap_or_else(|| {
        text.char_indices()
            .nth(max_chars)
            .map(|(index, _)| index)
            .unwrap_or(text.len())
    })
}

#[derive(Debug, Deserialize)]
struct TelegramEnvelope<T> {
    ok: bool,
    result: Option<T>,
    description: Option<String>,
    parameters: Option<TelegramResponseParameters>,
}

#[derive(Debug, Deserialize)]
struct TelegramResponseParameters {
    retry_after: Option<u64>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Update {
    pub update_id: i64,
    pub message: Option<Message>,
    pub callback_query: Option<CallbackQuery>,
    /// Delivered when the bot's own membership in a chat changes (added, removed,
    /// promoted). Carries the actor in `from`, which lets the bot enforce that
    /// only the owner may add it to groups.
    pub my_chat_member: Option<ChatMemberUpdated>,
}

/// A change to a chat member's status. Atlas2 only consumes the variant for the
/// bot itself (`my_chat_member`), to detect who added or removed it.
#[derive(Debug, Clone, Deserialize)]
pub struct ChatMemberUpdated {
    pub chat: Chat,
    pub from: User,
    pub new_chat_member: ChatMember,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Message {
    pub message_id: i64,
    pub chat: Chat,
    pub from: Option<User>,
    pub text: Option<String>,
    pub voice: Option<Voice>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Chat {
    pub id: i64,
    #[serde(rename = "type")]
    pub kind: String,
    pub title: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct User {
    pub id: i64,
    #[serde(rename = "username")]
    pub _username: Option<String>,
    #[serde(rename = "first_name")]
    pub _first_name: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CallbackQuery {
    pub id: String,
    pub from: User,
    pub message: Option<Message>,
    pub data: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Voice {
    pub file_id: String,
    pub file_unique_id: String,
    pub mime_type: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TelegramFile {
    pub file_path: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ChatMember {
    pub status: String,
}

impl ChatMember {
    /// True when this status means the bot is a member of the chat (as opposed to
    /// having left or been removed).
    pub fn is_present(&self) -> bool {
        matches!(
            self.status.as_str(),
            "member" | "administrator" | "creator" | "restricted"
        )
    }

    /// True when this status grants admin rights. A non-admin bot is subject to
    /// Telegram group privacy mode, which withholds ordinary text messages (only
    /// commands, @mentions, and replies reach it), so the bot must warn the owner
    /// to promote it before it can act on normal messages.
    pub fn is_admin(&self) -> bool {
        matches!(self.status.as_str(), "administrator" | "creator")
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct InlineKeyboardMarkup {
    pub inline_keyboard: Vec<Vec<InlineKeyboardButton>>,
}

#[derive(Debug, Clone, Serialize)]
pub struct InlineKeyboardButton {
    pub text: String,
    pub callback_data: String,
}

impl InlineKeyboardMarkup {
    pub fn single_column(buttons: Vec<InlineKeyboardButton>) -> Self {
        Self {
            inline_keyboard: buttons.into_iter().map(|button| vec![button]).collect(),
        }
    }
}

pub fn button(text: impl Into<String>, callback_data: impl Into<String>) -> InlineKeyboardButton {
    InlineKeyboardButton {
        text: text.into(),
        callback_data: callback_data.into(),
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BotCommand {
    pub command: String,
    pub description: String,
}

impl BotCommand {
    pub fn new(command: impl Into<String>, description: impl Into<String>) -> Self {
        Self {
            command: command.into(),
            description: description.into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::Value;

    use super::{
        BotCommand, ChatMember, ParseMode, TELEGRAM_TEXT_LIMIT, TelegramEnvelope, TelegramFile,
        Update, split_message_text, telegram_retry_after_seconds, trim_message_text,
    };

    #[test]
    fn deserializes_voice_message_update() {
        let update: Update = serde_json::from_str(
            r#"{
                "update_id": 1,
                "message": {
                    "message_id": 10,
                    "chat": {"id": -1001, "type": "supergroup", "title": "Atlas"},
                    "from": {"id": 42, "username": "atlas", "first_name": "Atlas"},
                    "voice": {
                        "file_id": "voice-file",
                        "file_unique_id": "voice-unique",
                        "mime_type": "audio/ogg"
                    }
                }
            }"#,
        )
        .unwrap();

        let voice = update.message.unwrap().voice.unwrap();
        assert_eq!(voice.file_id, "voice-file");
        assert_eq!(voice.file_unique_id, "voice-unique");
        assert_eq!(voice.mime_type.as_deref(), Some("audio/ogg"));
    }

    #[test]
    fn deserializes_my_chat_member_update() {
        // Mirrors what Telegram sends when the bot is added to a group: the
        // actor in `from` and the bot's new membership status. This locks in that
        // the optional `my_chat_member` field parses and that an absent
        // `message`/`callback_query` is tolerated.
        let update: Update = serde_json::from_str(
            r#"{
                "update_id": 5,
                "my_chat_member": {
                    "chat": {"id": -1002, "type": "supergroup", "title": "Team"},
                    "from": {"id": 42, "username": "owner", "first_name": "Owner"},
                    "date": 1700000000,
                    "old_chat_member": {"status": "left", "user": {"id": 7, "first_name": "Bot", "is_bot": true}},
                    "new_chat_member": {"status": "member", "user": {"id": 7, "first_name": "Bot", "is_bot": true}}
                }
            }"#,
        )
        .unwrap();

        let membership = update.my_chat_member.expect("my_chat_member present");
        assert_eq!(membership.chat.id, -1002);
        assert_eq!(membership.from.id, 42);
        assert!(membership.new_chat_member.is_present());
        // Added as a plain member, so not an admin: privacy mode applies.
        assert!(!membership.new_chat_member.is_admin());
        assert!(update.message.is_none());
    }

    #[test]
    fn chat_member_admin_status_is_recognized() {
        let present_non_admin = ["member", "restricted"];
        let admin = ["administrator", "creator"];
        let absent = ["left", "kicked"];

        for status in present_non_admin {
            let member = ChatMember {
                status: status.to_string(),
            };
            assert!(member.is_present(), "{status} should be present");
            assert!(!member.is_admin(), "{status} should not be admin");
        }
        for status in admin {
            let member = ChatMember {
                status: status.to_string(),
            };
            assert!(member.is_present(), "{status} should be present");
            assert!(member.is_admin(), "{status} should be admin");
        }
        for status in absent {
            let member = ChatMember {
                status: status.to_string(),
            };
            assert!(!member.is_present(), "{status} should be absent");
            assert!(!member.is_admin(), "{status} should not be admin");
        }
    }

    #[test]
    fn deserializes_get_file_response() {
        let file: TelegramFile = serde_json::from_str(
            r#"{
                "file_path": "voice/file_123.oga"
            }"#,
        )
        .unwrap();

        assert_eq!(file.file_path.as_deref(), Some("voice/file_123.oga"));
    }

    #[test]
    fn splits_plain_text_messages_across_chunks() {
        let text = format!("{}\n{}", "a".repeat(TELEGRAM_TEXT_LIMIT), "b".repeat(64));

        let chunks = split_message_text(&text, None);

        assert_eq!(chunks.len(), 2);
        assert!(chunks[0].chars().count() <= TELEGRAM_TEXT_LIMIT);
        assert!(chunks[1].chars().count() <= TELEGRAM_TEXT_LIMIT);
        assert_eq!(chunks.concat(), text);
    }

    #[test]
    fn trims_formatted_messages_to_single_chunk() {
        let text = "x".repeat(TELEGRAM_TEXT_LIMIT + 20);

        let chunks = split_message_text(&text, Some(ParseMode::Html));

        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0], trim_message_text(&text, Some(ParseMode::Html)));
        assert!(chunks[0].chars().count() <= TELEGRAM_TEXT_LIMIT);
    }

    #[test]
    fn reads_retry_after_from_error_envelope() {
        let envelope: TelegramEnvelope<Value> = serde_json::from_str(
            r#"{
                "ok": false,
                "description": "Too Many Requests: retry after 12",
                "parameters": {
                    "retry_after": 12
                }
            }"#,
        )
        .unwrap();

        assert_eq!(telegram_retry_after_seconds(&envelope), Some(12));
    }

    #[test]
    fn serializes_bot_command_for_set_my_commands() {
        let command = BotCommand::new("resume", "Resume an existing thread");

        let value = serde_json::to_value(command).unwrap();

        assert_eq!(
            value,
            serde_json::json!({
                "command": "resume",
                "description": "Resume an existing thread"
            })
        );
    }
}
