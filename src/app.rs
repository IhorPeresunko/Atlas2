use std::{collections::HashMap, sync::Arc, time::Duration};

use crate::{
    config::{Config, ServeArgs},
    domain::ProviderKind,
    error::AppResult,
    filesystem::FilesystemService,
    provider::{
        ClaudeProvider, ClaudeThreadReader, CodexProvider, CodexThreadReader, Provider,
        ProviderRegistry, ThreadHistoryReader, ThreadReaderRegistry,
    },
    services::AppServices,
    storage::Storage,
    stt::SttClient,
    telegram::TelegramClient,
    telegram_ingress,
};

/// Upper bound on the exponential backoff between failed `getUpdates` polls, so
/// the daemon keeps retrying briefly through a Telegram outage rather than
/// either hammering the API or giving up.
const MAX_POLL_BACKOFF_SECS: u64 = 30;

#[derive(Clone)]
pub struct App {
    services: AppServices,
}

impl App {
    pub async fn bootstrap(args: &ServeArgs) -> AppResult<Self> {
        let config = Config::load(args)?;
        ensure_database_parent_dir(&config.database_url)?;
        let storage = Storage::connect(&config.database_url).await?;
        if config.owner_id.is_none() && storage.get_owner_id().await?.is_none() {
            tracing::info!(
                "no owner yet: the first person to DM this bot or add it to a group becomes \
                 its owner (or set ATLAS2_OWNER_ID to pin it explicitly)"
            );
        }
        storage.mark_interrupted_sessions_failed().await?;
        let telegram = TelegramClient::new(&config.telegram_api_base, &config.telegram_bot_token);
        telegram
            .set_my_commands(&telegram_ingress::bot_commands())
            .await?;
        let filesystem = FilesystemService::default();
        let (providers, readers) = build_provider_registries(&config);
        let stt = SttClient::from_config(&config)?;

        Ok(Self {
            services: AppServices::new(
                config, storage, telegram, filesystem, providers, readers, stt,
            ),
        })
    }

    pub async fn run(self) -> AppResult<()> {
        tracing::info!("Atlas2 starting with Telegram long polling");
        let mut offset = None;
        let mut backoff_secs = 0u64;

        loop {
            let updates = match self
                .services
                .telegram
                .get_updates(offset, self.services.config.poll_timeout_seconds)
                .await
            {
                Ok(updates) => {
                    backoff_secs = 0;
                    updates
                }
                // A transient Telegram-side failure (e.g. a 502 Bad Gateway, which
                // Telegram returns routinely) must never take the daemon down. Log
                // it, back off, and keep polling. `offset` is left untouched so the
                // same batch is re-fetched once Telegram recovers; persistent
                // misconfiguration (e.g. a bad token) is already caught at startup.
                Err(error) => {
                    backoff_secs = (backoff_secs * 2).clamp(1, MAX_POLL_BACKOFF_SECS);
                    tracing::warn!(%error, backoff_secs, "getUpdates failed; retrying after backoff");
                    tokio::time::sleep(Duration::from_secs(backoff_secs)).await;
                    continue;
                }
            };

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

/// Builds every provider Atlas2 supports and registers it by kind, so a chat can
/// pick one per session in `/new` and turns dispatch to the right one. This is
/// the single composition root where concrete providers are named.
fn build_provider_registries(config: &Config) -> (ProviderRegistry, ThreadReaderRegistry) {
    let additional_dirs = config.workspace_additional_writable_dirs.clone();

    let codex = CodexProvider::new(config.codex_bin.clone(), additional_dirs.clone());
    let claude = ClaudeProvider::new(config.claude_bin.clone(), additional_dirs);

    let mut providers: HashMap<ProviderKind, Arc<dyn Provider>> = HashMap::new();
    providers.insert(ProviderKind::Codex, Arc::new(codex));
    providers.insert(ProviderKind::Claude, Arc::new(claude));

    let mut readers: HashMap<ProviderKind, Arc<dyn ThreadHistoryReader>> = HashMap::new();
    readers.insert(
        ProviderKind::Codex,
        Arc::new(CodexThreadReader::new(config.codex_sessions_dir.clone())),
    );
    readers.insert(
        ProviderKind::Claude,
        Arc::new(ClaudeThreadReader::new(config.claude_sessions_dir.clone())),
    );

    (
        ProviderRegistry::new(providers),
        ThreadReaderRegistry::new(readers),
    )
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
