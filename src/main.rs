mod app;
mod provider;
mod config;
mod daemon;
mod domain;
mod error;
mod filesystem;
mod presentation;
mod services;
mod storage;
mod stt;
mod telegram;
mod telegram_ingress;

use app::App;
use clap::Parser;
use config::{CliArgs, Command, ServeArgs};
use error::AppResult;

#[tokio::main]
async fn main() -> AppResult<()> {
    let cli = CliArgs::parse();

    match cli.command.unwrap_or(Command::Run(ServeArgs::default())) {
        Command::Start(args) => daemon::start(&args),
        Command::Stop => daemon::stop(),
        Command::Status => daemon::status(),
        Command::Set { key, value } => {
            config::set_secret(&key, &value)?;
            println!("Saved {key}.");
            Ok(())
        }
        Command::Upgrade => daemon::upgrade().await,
        Command::Run(args) => run_server(&args).await,
    }
}

async fn run_server(args: &ServeArgs) -> AppResult<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "atlas2=info,sqlx=warn,reqwest=warn".into()),
        )
        .init();

    let app = App::bootstrap(args).await?;
    app.run().await
}
