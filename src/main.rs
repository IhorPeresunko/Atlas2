mod app;
mod codex;
mod config;
mod domain;
mod error;
mod filesystem;
mod services;
mod storage;
mod telegram;

use app::App;
use error::AppResult;

#[tokio::main]
async fn main() -> AppResult<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "atlas2=info,sqlx=warn,reqwest=warn".into()),
        )
        .init();

    let app = App::bootstrap().await?;
    app.run().await
}
