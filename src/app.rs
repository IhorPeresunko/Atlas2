use crate::{
    codex::CodexClient,
    config::Config,
    domain::{ApprovalId, TelegramChatId, TelegramUserId},
    error::{AppError, AppResult},
    filesystem::FilesystemService,
    services::{AppServices, FolderCallbackResult},
    storage::Storage,
    telegram::TelegramClient,
};

#[derive(Clone)]
pub struct App {
    services: AppServices,
}

impl App {
    pub async fn bootstrap() -> AppResult<Self> {
        let config = Config::load()?;
        ensure_database_parent_dir(&config.database_url)?;
        let storage = Storage::connect(&config.database_url).await?;
        let telegram = TelegramClient::new(&config.telegram_api_base, &config.telegram_bot_token);
        let filesystem = FilesystemService::default();
        let codex = CodexClient::new(
            config.codex_bin.clone(),
            config.workspace_additional_writable_dirs.clone(),
        );

        Ok(Self {
            services: AppServices::new(config, storage, telegram, filesystem, codex),
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
        if let Some(message) = update.message {
            let chat_id = TelegramChatId(message.chat.id);
            self.services
                .register_chat(chat_id, &message.chat.kind, message.chat.title.as_deref())
                .await?;

            let Some(text) = message.text.clone() else {
                return Ok(());
            };
            let user_id = message
                .from
                .as_ref()
                .map(|user| TelegramUserId(user.id))
                .ok_or_else(|| AppError::Validation("message missing sender".into()))?;

            match text.as_str() {
                "/start" | "/help" => {
                    self.services
                        .telegram
                        .send_message(
                            chat_id,
                            "Atlas2 commands:\n/new - select a folder and create a new session\n/sessions - list known sessions\nAny other text - send a prompt to the active Codex session",
                            None,
                        )
                        .await?;
                }
                "/new" => {
                    self.services.require_group_admin(chat_id, user_id).await?;
                    let text = self.services.begin_folder_selection(chat_id).await?;
                    let markup = self.services.folder_markup("/").await?;
                    self.services
                        .telegram
                        .send_message(chat_id, &text, Some(markup))
                        .await?;
                }
                "/sessions" => {
                    let summary = self.services.render_sessions().await?;
                    self.services
                        .telegram
                        .send_message(chat_id, &summary, None)
                        .await?;
                }
                other if other.starts_with('/') => {
                    self.services
                        .telegram
                        .send_message(chat_id, "Unknown command.", None)
                        .await?;
                }
                prompt => {
                    let prompt = prompt.to_string();
                    let services = self.services.clone();
                    tokio::spawn(async move {
                        if let Err(error) = services.run_prompt(chat_id, &prompt).await {
                            let _ = services
                                .telegram
                                .send_message(chat_id, &format!("Prompt failed: {error}"), None)
                                .await;
                        }
                    });
                }
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

            let response = if let Some(id) = data.strip_prefix("approval-approve:") {
                let approval_id = ApprovalId(uuid::Uuid::parse_str(id).map_err(|error| {
                    AppError::Validation(format!("invalid approval ID in callback: {error}"))
                })?);
                self.services
                    .resolve_approval(approval_id, chat_id, user_id, true)
                    .await
            } else if let Some(id) = data.strip_prefix("approval-reject:") {
                let approval_id = ApprovalId(uuid::Uuid::parse_str(id).map_err(|error| {
                    AppError::Validation(format!("invalid approval ID in callback: {error}"))
                })?);
                self.services
                    .resolve_approval(approval_id, chat_id, user_id, false)
                    .await
            } else {
                match self
                    .services
                    .handle_folder_callback(chat_id, user_id, data)
                    .await?
                {
                    FolderCallbackResult::Render(text, markup) => {
                        self.services
                            .telegram
                            .edit_message_text(chat_id, message.message_id, &text, Some(markup))
                            .await?;
                        Ok("Updated folder browser.".into())
                    }
                    FolderCallbackResult::Replace(text) => {
                        self.services
                            .telegram
                            .edit_message_text(chat_id, message.message_id, &text, None)
                            .await?;
                        Ok(text)
                    }
                }
            };

            let callback_text = match response {
                Ok(text) => text,
                Err(error) => error.to_string(),
            };
            self.services
                .telegram
                .answer_callback_query(&callback.id, &callback_text, false)
                .await?;
        }

        Ok(())
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
