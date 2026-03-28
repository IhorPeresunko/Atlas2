use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{Value, json};

use crate::{
    domain::{TelegramChatId, TelegramUserId},
    error::{AppError, AppResult},
};

#[derive(Clone)]
pub struct TelegramClient {
    http: reqwest::Client,
    base_url: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum ParseMode {
    #[serde(rename = "HTML")]
    Html,
}

impl TelegramClient {
    pub fn new(api_base: &str, bot_token: &str) -> Self {
        let base_url = format!("{}/bot{}", api_base.trim_end_matches('/'), bot_token);
        Self {
            http: reqwest::Client::new(),
            base_url,
        }
    }

    pub async fn get_updates(
        &self,
        offset: Option<i64>,
        timeout_seconds: u64,
    ) -> AppResult<Vec<Update>> {
        let mut payload = json!({
            "timeout": timeout_seconds,
            "allowed_updates": ["message", "callback_query"]
        });
        if let Some(offset) = offset {
            payload["offset"] = json!(offset);
        }

        self.call("getUpdates", &payload).await
    }

    pub async fn send_message(
        &self,
        chat_id: TelegramChatId,
        text: &str,
        parse_mode: Option<ParseMode>,
        reply_markup: Option<InlineKeyboardMarkup>,
    ) -> AppResult<Message> {
        let mut payload = json!({
            "chat_id": chat_id.0,
            "text": text,
        });
        if let Some(parse_mode) = parse_mode {
            payload["parse_mode"] = serde_json::to_value(parse_mode)?;
        }
        if let Some(markup) = reply_markup {
            payload["reply_markup"] = serde_json::to_value(markup)?;
        }
        self.call("sendMessage", &payload).await
    }

    pub async fn edit_message_text(
        &self,
        chat_id: TelegramChatId,
        message_id: i64,
        text: &str,
        parse_mode: Option<ParseMode>,
        reply_markup: Option<InlineKeyboardMarkup>,
    ) -> AppResult<Message> {
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

    pub async fn get_chat_member(
        &self,
        chat_id: TelegramChatId,
        user_id: TelegramUserId,
    ) -> AppResult<ChatMember> {
        self.call(
            "getChatMember",
            &json!({
                "chat_id": chat_id.0,
                "user_id": user_id.0,
            }),
        )
        .await
    }

    async fn call<T: DeserializeOwned>(&self, method: &str, payload: &Value) -> AppResult<T> {
        let response = self
            .http
            .post(format!("{}/{}", self.base_url, method))
            .json(payload)
            .send()
            .await?;

        let envelope: TelegramEnvelope<T> = response.json().await?;
        if !envelope.ok {
            return Err(AppError::Telegram(
                envelope
                    .description
                    .unwrap_or_else(|| format!("telegram method {method} failed")),
            ));
        }

        envelope.result.ok_or_else(|| {
            AppError::Telegram(format!("telegram method {method} returned no result"))
        })
    }
}

#[derive(Debug, Deserialize)]
struct TelegramEnvelope<T> {
    ok: bool,
    result: Option<T>,
    description: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Update {
    pub update_id: i64,
    pub message: Option<Message>,
    pub callback_query: Option<CallbackQuery>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Message {
    pub message_id: i64,
    pub chat: Chat,
    pub from: Option<User>,
    pub text: Option<String>,
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
pub struct ChatMember {
    pub status: String,
}

impl ChatMember {
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
