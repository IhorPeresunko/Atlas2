use std::{
    env,
    io::{self, Write},
    path::PathBuf,
};

use crate::error::{AppError, AppResult};

#[derive(Debug, Clone)]
pub struct Config {
    pub telegram_bot_token: String,
    pub telegram_api_base: String,
    pub database_url: String,
    pub codex_bin: String,
    pub poll_timeout_seconds: u64,
    pub max_directory_entries: usize,
    pub workspace_additional_writable_dirs: Vec<PathBuf>,
}

impl Config {
    pub fn load() -> AppResult<Self> {
        let telegram_bot_token = match env::var("ATLAS2_TELEGRAM_BOT_TOKEN") {
            Ok(value) if !value.trim().is_empty() => value,
            _ => prompt_for_token()?,
        };
        let telegram_api_base = env::var("ATLAS2_TELEGRAM_API_BASE")
            .unwrap_or_else(|_| "https://api.telegram.org".to_string());
        let database_path =
            env::var("ATLAS2_DATABASE_PATH").unwrap_or_else(|_| "./data/atlas2.sqlite".to_string());
        let codex_bin = env::var("ATLAS2_CODEX_BIN").unwrap_or_else(|_| "codex".to_string());
        let poll_timeout_seconds = env_u64("ATLAS2_POLL_TIMEOUT_SECONDS", 30)?;
        let max_directory_entries = env_usize("ATLAS2_MAX_DIRECTORY_ENTRIES", 20)?;
        let workspace_additional_writable_dirs =
            env::var("ATLAS2_CODEX_ADD_DIRS").unwrap_or_default();

        let database_url = if database_path.starts_with("sqlite:") {
            database_path
        } else {
            format!("sqlite://{database_path}")
        };

        let additional_dirs = workspace_additional_writable_dirs
            .split(':')
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .collect();

        Ok(Self {
            telegram_bot_token,
            telegram_api_base,
            database_url,
            codex_bin,
            poll_timeout_seconds,
            max_directory_entries,
            workspace_additional_writable_dirs: additional_dirs,
        })
    }
}

fn prompt_for_token() -> AppResult<String> {
    print!("Telegram bot token: ");
    io::stdout()
        .flush()
        .map_err(|error| AppError::Config(format!("failed to flush stdout: {error}")))?;

    let mut buffer = String::new();
    io::stdin()
        .read_line(&mut buffer)
        .map_err(|error| AppError::Config(format!("failed to read token from stdin: {error}")))?;

    let token = buffer.trim().to_string();
    if token.is_empty() {
        return Err(AppError::Config(
            "telegram bot token cannot be empty".into(),
        ));
    }
    Ok(token)
}

fn env_u64(key: &str, default: u64) -> AppResult<u64> {
    match env::var(key) {
        Ok(value) => value
            .parse::<u64>()
            .map_err(|_| AppError::Config(format!("{key} must be an integer"))),
        Err(_) => Ok(default),
    }
}

fn env_usize(key: &str, default: usize) -> AppResult<usize> {
    match env::var(key) {
        Ok(value) => value
            .parse::<usize>()
            .map_err(|_| AppError::Config(format!("{key} must be an integer"))),
        Err(_) => Ok(default),
    }
}
