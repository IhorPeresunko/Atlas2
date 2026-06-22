use std::{
    env, fs,
    io::{self, Write},
    path::{Path, PathBuf},
    str::FromStr,
};

use clap::{Args, Parser, Subcommand, ValueEnum};
use directories::{BaseDirs, ProjectDirs};

use crate::error::{AppError, AppResult};

#[derive(Debug, Clone, Parser)]
#[command(
    name = "atlas2",
    version,
    about = "Telegram bridge for local coding-agent sessions"
)]
pub struct CliArgs {
    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Debug, Clone, Subcommand)]
pub enum Command {
    /// Start Atlas2 in the background and return immediately
    Start(ServeArgs),
    /// Run Atlas2 in the foreground (blocks the terminal)
    Run(ServeArgs),
    /// Stop the background Atlas2 process
    Stop,
    /// Show whether Atlas2 is running
    Status,
    /// Store a configuration value (keys: bottoken, sttkey)
    Set {
        /// Configuration key to set (bottoken, sttkey)
        key: String,
        /// Value to store
        value: String,
    },
    /// Download and install the latest release, restarting the daemon if running
    Upgrade,
}

#[derive(Debug, Clone, Default, Args)]
pub struct ServeArgs {
    #[arg(long, value_enum, default_value_t = CliSttProvider::None)]
    pub stt_provider: CliSttProvider,
    #[arg(long)]
    pub stt_api_key: Option<String>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, ValueEnum)]
pub enum CliSttProvider {
    #[default]
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
    /// Telegram user ID of the bot owner. Only this user may DM the bot or
    /// authorize a group (by adding the bot or running `/activate`). `None` means
    /// no owner is configured, in which case the bot is inert (fails closed).
    pub owner_id: Option<i64>,
    pub telegram_api_base: String,
    pub database_url: String,
    pub codex_bin: String,
    pub codex_sessions_dir: PathBuf,
    pub claude_bin: String,
    pub claude_sessions_dir: PathBuf,
    pub poll_timeout_seconds: u64,
    pub max_directory_entries: usize,
    pub workspace_additional_writable_dirs: Vec<PathBuf>,
    pub stt_provider: SttProvider,
    pub stt_api_key: Option<String>,
}

impl Config {
    pub fn load(args: &ServeArgs) -> AppResult<Self> {
        let telegram_bot_token = load_telegram_bot_token()?;
        let owner_id = load_owner_id()?;
        let telegram_api_base = env::var("ATLAS2_TELEGRAM_API_BASE")
            .unwrap_or_else(|_| "https://api.telegram.org".to_string());
        let database_path = match env::var("ATLAS2_DATABASE_PATH") {
            Ok(path) => path,
            Err(_) => data_dir()?
                .join("atlas2.sqlite")
                .to_string_lossy()
                .into_owned(),
        };
        let codex_bin = codex_bin();
        let codex_sessions_dir = codex_sessions_dir()?;
        let claude_bin = claude_bin();
        let claude_sessions_dir = claude_sessions_dir()?;
        let poll_timeout_seconds = env_u64("ATLAS2_POLL_TIMEOUT_SECONDS", 30)?;
        let max_directory_entries = env_usize("ATLAS2_MAX_DIRECTORY_ENTRIES", 20)?;
        let workspace_additional_writable_dirs =
            env::var("ATLAS2_CODEX_ADD_DIRS").unwrap_or_default();
        let stt_provider = SttProvider::from(args.stt_provider);
        let stt_api_key = match stt_provider {
            SttProvider::None => None,
            SttProvider::ElevenLabs => Some(load_stt_api_key(args.stt_api_key.clone())?),
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
            owner_id,
            telegram_api_base,
            database_url,
            codex_bin,
            codex_sessions_dir,
            claude_bin,
            claude_sessions_dir,
            poll_timeout_seconds,
            max_directory_entries,
            workspace_additional_writable_dirs: additional_dirs,
            stt_provider,
            stt_api_key,
        })
    }
}

/// The `codex` executable Atlas2 drives, overridable via `ATLAS2_CODEX_BIN`.
/// Shared by the running server and `atlas2 status` so both agree on what they
/// look for.
pub fn codex_bin() -> String {
    env::var("ATLAS2_CODEX_BIN").unwrap_or_else(|_| "codex".to_string())
}

/// The `claude` executable Atlas2 drives, overridable via `ATLAS2_CLAUDE_BIN`.
pub fn claude_bin() -> String {
    env::var("ATLAS2_CLAUDE_BIN").unwrap_or_else(|_| "claude".to_string())
}

fn load_telegram_bot_token() -> AppResult<String> {
    if let Some(token) = stored_telegram_bot_token()? {
        return Ok(token);
    }

    let token_path = credential_path("ATLAS2_TELEGRAM_BOT_TOKEN_FILE", "telegram_bot_token")?;
    let token = prompt_for_secret("Telegram bot token")?;
    persist_secret(&token_path, &token, "Telegram bot token")?;
    Ok(token)
}

/// Reads the Telegram bot token from the environment or the persisted credential
/// file without prompting. Used by the daemon launcher to fail fast before
/// detaching, since a detached process cannot prompt on stdin.
pub fn stored_telegram_bot_token() -> AppResult<Option<String>> {
    if let Ok(value) = env::var("ATLAS2_TELEGRAM_BOT_TOKEN") {
        let token = value.trim().to_string();
        if !token.is_empty() {
            return Ok(Some(token));
        }
    }

    read_credential(
        "ATLAS2_TELEGRAM_BOT_TOKEN_FILE",
        "telegram_bot_token",
        "Telegram bot token",
    )
}

/// Reads the bot owner's Telegram user ID from the environment or the persisted
/// config file. Returns `None` when unset; the caller treats that as "no owner
/// configured" and refuses to act on any chat.
fn load_owner_id() -> AppResult<Option<i64>> {
    if let Ok(value) = env::var("ATLAS2_OWNER_ID") {
        let trimmed = value.trim();
        if !trimmed.is_empty() {
            return Ok(Some(parse_owner_id(trimmed)?));
        }
    }

    match read_credential("ATLAS2_OWNER_ID_FILE", "owner_id", "owner Telegram user ID")? {
        Some(value) => Ok(Some(parse_owner_id(&value)?)),
        None => Ok(None),
    }
}

fn parse_owner_id(value: &str) -> AppResult<i64> {
    value.trim().parse::<i64>().map_err(|_| {
        AppError::Config(format!(
            "owner Telegram user ID must be an integer, got '{value}'"
        ))
    })
}

/// Reads the persisted ElevenLabs API key without prompting, mirroring
/// [`stored_telegram_bot_token`].
pub fn stored_stt_api_key() -> AppResult<Option<String>> {
    read_credential(
        "ATLAS2_STT_API_KEY_FILE",
        "stt_api_key",
        "ElevenLabs API key",
    )
}

/// Persists a configuration secret addressed by a short key, mirroring the
/// credential files the foreground server reads on startup.
pub fn set_secret(key: &str, value: &str) -> AppResult<()> {
    let (path, label) = match key {
        "bottoken" | "token" | "telegram" => (
            credential_path("ATLAS2_TELEGRAM_BOT_TOKEN_FILE", "telegram_bot_token")?,
            "Telegram bot token",
        ),
        "sttkey" | "stt" => (
            credential_path("ATLAS2_STT_API_KEY_FILE", "stt_api_key")?,
            "ElevenLabs API key",
        ),
        "ownerid" | "owner" => (
            credential_path("ATLAS2_OWNER_ID_FILE", "owner_id")?,
            "owner Telegram user ID",
        ),
        other => {
            return Err(AppError::Config(format!(
                "unknown config key '{other}'; known keys: bottoken, sttkey, ownerid"
            )));
        }
    };

    let secret = normalize_secret(value.to_string(), label)?;
    // The owner ID is not a secret, but it shares the credential-file plumbing.
    // Validate it parses as an integer so a typo fails at `set` time, not later.
    if matches!(key, "ownerid" | "owner") {
        parse_owner_id(&secret)?;
    }
    persist_secret(&path, &secret, label)?;
    Ok(())
}

fn load_stt_api_key(cli_value: Option<String>) -> AppResult<String> {
    if let Some(value) = cli_value {
        return normalize_secret(value, "ElevenLabs API key");
    }

    if let Some(key) = read_credential(
        "ATLAS2_STT_API_KEY_FILE",
        "stt_api_key",
        "ElevenLabs API key",
    )? {
        return Ok(key);
    }

    let key_path = credential_path("ATLAS2_STT_API_KEY_FILE", "stt_api_key")?;
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

fn project_dirs() -> AppResult<ProjectDirs> {
    ProjectDirs::from("", "", "atlas2").ok_or_else(|| {
        AppError::Config("could not determine a home directory for atlas2 paths".into())
    })
}

/// Resolves the Codex rollout sessions directory. Honors an explicit override,
/// then `$CODEX_HOME/sessions`, and finally `~/.codex/sessions` (the Codex CLI
/// default that Atlas2 and the laptop CLI share).
fn codex_sessions_dir() -> AppResult<PathBuf> {
    if let Ok(value) = env::var("ATLAS2_CODEX_SESSIONS_DIR")
        && !value.is_empty()
    {
        return Ok(PathBuf::from(value));
    }
    let codex_home = match env::var("CODEX_HOME") {
        Ok(value) if !value.is_empty() => PathBuf::from(value),
        _ => BaseDirs::new()
            .ok_or_else(|| {
                AppError::Config("could not determine a home directory for the Codex sessions path".into())
            })?
            .home_dir()
            .join(".codex"),
    };
    Ok(codex_home.join("sessions"))
}

/// Resolves the Claude CLI projects directory, where it records session
/// transcripts (`~/.claude/projects/**/*.jsonl`). Honors an explicit override,
/// then `$CLAUDE_CONFIG_DIR/projects`, and finally `~/.claude/projects`.
fn claude_sessions_dir() -> AppResult<PathBuf> {
    if let Ok(value) = env::var("ATLAS2_CLAUDE_SESSIONS_DIR")
        && !value.is_empty()
    {
        return Ok(PathBuf::from(value));
    }
    let claude_home = match env::var("CLAUDE_CONFIG_DIR") {
        Ok(value) if !value.is_empty() => PathBuf::from(value),
        _ => BaseDirs::new()
            .ok_or_else(|| {
                AppError::Config(
                    "could not determine a home directory for the Claude sessions path".into(),
                )
            })?
            .home_dir()
            .join(".claude"),
    };
    Ok(claude_home.join("projects"))
}

/// Per-user config directory (`~/.config/atlas2` on Linux). Holds credentials
/// and the dist install receipt.
pub fn config_dir() -> AppResult<PathBuf> {
    Ok(project_dirs()?.config_dir().to_path_buf())
}

/// Per-user data directory (`~/.local/share/atlas2` on Linux). Holds the
/// SQLite database.
pub fn data_dir() -> AppResult<PathBuf> {
    Ok(project_dirs()?.data_dir().to_path_buf())
}

/// Per-user state directory (`~/.local/state/atlas2` on Linux). Holds the log
/// and PID files. Falls back to the data directory on platforms without a
/// distinct state dir.
pub fn state_dir() -> AppResult<PathBuf> {
    let dirs = project_dirs()?;
    Ok(dirs
        .state_dir()
        .unwrap_or_else(|| dirs.data_dir())
        .to_path_buf())
}

/// Canonical path for a credential, honoring an explicit env override and
/// otherwise living in the config directory.
fn credential_path(env_key: &str, default_name: &str) -> AppResult<PathBuf> {
    if let Ok(value) = env::var(env_key) {
        let path = PathBuf::from(value);
        if !path.as_os_str().is_empty() {
            return Ok(path);
        }
    }

    config_dir().map(|path| path.join(default_name))
}

/// Reads a credential from its canonical location.
fn read_credential(env_key: &str, default_name: &str, label: &str) -> AppResult<Option<String>> {
    let path = credential_path(env_key, default_name)?;
    read_secret_from_file(&path, label)
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
        CliSttProvider, Config, ServeArgs, SttProvider, normalize_secret,
        read_secret_from_file,
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

        let config = Config::load(&ServeArgs {
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

        let config = Config::load(&ServeArgs {
            stt_provider: CliSttProvider::ElevenLabs,
            stt_api_key: Some("sk_test".into()),
        })
        .unwrap();

        assert_eq!(config.stt_provider, SttProvider::ElevenLabs);
        assert_eq!(config.stt_api_key.as_deref(), Some("sk_test"));
    }
}
