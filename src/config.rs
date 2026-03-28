use std::{
    env, fs,
    io::{self, Write},
    path::{Path, PathBuf},
    str::FromStr,
};

use clap::{Parser, ValueEnum};

use crate::error::{AppError, AppResult};

#[derive(Debug, Clone, Parser)]
#[command(name = "atlas2")]
pub struct CliArgs {
    #[arg(long, value_enum, default_value_t = CliSttProvider::None)]
    pub stt_provider: CliSttProvider,
    #[arg(long)]
    pub stt_api_key: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum CliSttProvider {
    #[value(name = "none")]
    None,
    #[value(name = "11labs")]
    ElevenLabs,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SttProvider {
    None,
    ElevenLabs,
}

impl From<CliSttProvider> for SttProvider {
    fn from(value: CliSttProvider) -> Self {
        match value {
            CliSttProvider::None => Self::None,
            CliSttProvider::ElevenLabs => Self::ElevenLabs,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Config {
    pub telegram_bot_token: String,
    pub telegram_api_base: String,
    pub database_url: String,
    pub codex_bin: String,
    pub poll_timeout_seconds: u64,
    pub max_directory_entries: usize,
    pub workspace_additional_writable_dirs: Vec<PathBuf>,
    pub stt_provider: SttProvider,
    pub stt_api_key: Option<String>,
}

impl Config {
    pub fn load(cli: CliArgs) -> AppResult<Self> {
        let telegram_bot_token = load_telegram_bot_token()?;
        let telegram_api_base = env::var("ATLAS2_TELEGRAM_API_BASE")
            .unwrap_or_else(|_| "https://api.telegram.org".to_string());
        let database_path =
            env::var("ATLAS2_DATABASE_PATH").unwrap_or_else(|_| "./data/atlas2.sqlite".to_string());
        let codex_bin = env::var("ATLAS2_CODEX_BIN").unwrap_or_else(|_| "codex".to_string());
        let poll_timeout_seconds = env_u64("ATLAS2_POLL_TIMEOUT_SECONDS", 30)?;
        let max_directory_entries = env_usize("ATLAS2_MAX_DIRECTORY_ENTRIES", 20)?;
        let workspace_additional_writable_dirs =
            env::var("ATLAS2_CODEX_ADD_DIRS").unwrap_or_default();
        let stt_provider = SttProvider::from(cli.stt_provider);
        let stt_api_key = match stt_provider {
            SttProvider::None => None,
            SttProvider::ElevenLabs => Some(load_stt_api_key(cli.stt_api_key)?),
        };

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
            stt_provider,
            stt_api_key,
        })
    }
}

fn load_telegram_bot_token() -> AppResult<String> {
    if let Ok(value) = env::var("ATLAS2_TELEGRAM_BOT_TOKEN") {
        let token = value.trim().to_string();
        if !token.is_empty() {
            return Ok(token);
        }
    }

    let token_path = credential_path("ATLAS2_TELEGRAM_BOT_TOKEN_FILE", "telegram_bot_token")?;
    if let Some(token) = read_secret_from_file(&token_path, "Telegram bot token")? {
        return Ok(token);
    }

    let token = prompt_for_secret("Telegram bot token")?;
    persist_secret(&token_path, &token, "Telegram bot token")?;
    Ok(token)
}

fn load_stt_api_key(cli_value: Option<String>) -> AppResult<String> {
    if let Some(value) = cli_value {
        return normalize_secret(value, "ElevenLabs API key");
    }

    let key_path = credential_path("ATLAS2_STT_API_KEY_FILE", "stt_api_key")?;
    if let Some(key) = read_secret_from_file(&key_path, "ElevenLabs API key")? {
        return Ok(key);
    }

    let key = prompt_for_secret("ElevenLabs API key")?;
    persist_secret(&key_path, &key, "ElevenLabs API key")?;
    Ok(key)
}

fn prompt_for_secret(label: &str) -> AppResult<String> {
    print!("{label}: ");
    io::stdout()
        .flush()
        .map_err(|error| AppError::Config(format!("failed to flush stdout: {error}")))?;

    let mut buffer = String::new();
    io::stdin()
        .read_line(&mut buffer)
        .map_err(|error| AppError::Config(format!("failed to read {label} from stdin: {error}")))?;

    normalize_secret(buffer, label)
}

fn normalize_secret(value: String, label: &str) -> AppResult<String> {
    let secret = value.trim().to_string();
    if secret.is_empty() {
        return Err(AppError::Config(format!(
            "{} cannot be empty",
            label.to_lowercase()
        )));
    }
    Ok(secret)
}

fn read_secret_from_file(path: &Path, label: &str) -> AppResult<Option<String>> {
    match fs::read_to_string(path) {
        Ok(contents) => {
            let secret = contents.trim().to_string();
            if secret.is_empty() {
                Ok(None)
            } else {
                Ok(Some(secret))
            }
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(AppError::Config(format!(
            "failed to read {label} from {}: {error}",
            path.display()
        ))),
    }
}

fn persist_secret(path: &Path, secret: &str, label: &str) -> AppResult<()> {
    let parent = path.parent().ok_or_else(|| {
        AppError::Config(format!(
            "invalid {} storage path: {}",
            label.to_lowercase(),
            path.display()
        ))
    })?;
    fs::create_dir_all(parent).map_err(|error| {
        AppError::Config(format!(
            "failed to create {} directory {}: {error}",
            label.to_lowercase(),
            parent.display()
        ))
    })?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;

        let mut file = fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .mode(0o600)
            .open(path)
            .map_err(|error| {
                AppError::Config(format!(
                    "failed to persist {label} to {}: {error}",
                    path.display()
                ))
            })?;
        file.write_all(secret.as_bytes()).map_err(|error| {
            AppError::Config(format!(
                "failed to write {label} to {}: {error}",
                path.display()
            ))
        })?;
        file.write_all(b"\n").map_err(|error| {
            AppError::Config(format!(
                "failed to finalize {} file {}: {error}",
                label.to_lowercase(),
                path.display()
            ))
        })?;
    }

    #[cfg(not(unix))]
    {
        fs::write(path, format!("{secret}\n")).map_err(|error| {
            AppError::Config(format!(
                "failed to persist {label} to {}: {error}",
                path.display()
            ))
        })?;
    }

    Ok(())
}

fn credential_path(env_key: &str, default_name: &str) -> AppResult<PathBuf> {
    if let Ok(value) = env::var(env_key) {
        let path = PathBuf::from(value);
        if !path.as_os_str().is_empty() {
            return Ok(path);
        }
    }

    state_dir().map(|path| path.join(default_name))
}

fn state_dir() -> AppResult<PathBuf> {
    if let Ok(state_home) = env::var("XDG_STATE_HOME") {
        let path = PathBuf::from(state_home);
        if !path.as_os_str().is_empty() {
            return Ok(path.join("atlas2"));
        }
    }

    let home = env::var("HOME").map_err(|_| {
        AppError::Config("HOME is not set; cannot determine token storage path".into())
    })?;
    Ok(PathBuf::from(home)
        .join(".local")
        .join("state")
        .join("atlas2"))
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

impl FromStr for SttProvider {
    type Err = AppError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "none" => Ok(Self::None),
            "11labs" => Ok(Self::ElevenLabs),
            _ => Err(AppError::Config(format!(
                "unsupported STT provider: {value}"
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{env, fs};

    use tempfile::tempdir;

    use super::{
        CliArgs, CliSttProvider, Config, SttProvider, normalize_secret, read_secret_from_file,
    };

    #[test]
    fn reads_secret_from_file() {
        let temp = tempdir().unwrap();
        let path = temp.path().join("token");
        fs::write(&path, "secret-token\n").unwrap();

        let token = read_secret_from_file(&path, "token").unwrap();
        assert_eq!(token.as_deref(), Some("secret-token"));
    }

    #[test]
    fn empty_secret_is_rejected() {
        let error = normalize_secret("   ".into(), "ElevenLabs API key").unwrap_err();
        assert!(
            error
                .to_string()
                .contains("elevenlabs api key cannot be empty")
        );
    }

    #[test]
    fn config_uses_none_provider_without_stt_key() {
        let temp = tempdir().unwrap();
        let token_path = temp.path().join("telegram-token");
        fs::write(&token_path, "telegram-secret\n").unwrap();

        unsafe {
            env::set_var("ATLAS2_TELEGRAM_BOT_TOKEN_FILE", &token_path);
            env::set_var("HOME", temp.path());
        }

        let config = Config::load(CliArgs {
            stt_provider: CliSttProvider::None,
            stt_api_key: None,
        })
        .unwrap();

        assert_eq!(config.stt_provider, SttProvider::None);
        assert_eq!(config.stt_api_key, None);
    }

    #[test]
    fn config_accepts_stt_key_from_cli() {
        let temp = tempdir().unwrap();
        let token_path = temp.path().join("telegram-token");
        fs::write(&token_path, "telegram-secret\n").unwrap();

        unsafe {
            env::set_var("ATLAS2_TELEGRAM_BOT_TOKEN_FILE", &token_path);
            env::set_var("HOME", temp.path());
        }

        let config = Config::load(CliArgs {
            stt_provider: CliSttProvider::ElevenLabs,
            stt_api_key: Some("sk_test".into()),
        })
        .unwrap();

        assert_eq!(config.stt_provider, SttProvider::ElevenLabs);
        assert_eq!(config.stt_api_key.as_deref(), Some("sk_test"));
    }
}
