use crate::{
    codex::CodexClient,
    config::{Config, ServeArgs},
    error::AppResult,
    filesystem::FilesystemService,
    services::AppServices,
    storage::Storage,
    stt::SttClient,
    telegram::TelegramClient,
    telegram_ingress,
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
        telegram_ingress::handle_update(&self.services, update).await
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
