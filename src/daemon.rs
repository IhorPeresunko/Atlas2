use std::{
    fs,
    path::PathBuf,
    process::{Command, Stdio},
};

use crate::{
    config::{self, CliSttProvider, ServeArgs},
    error::{AppError, AppResult},
};

const PID_FILE_NAME: &str = "atlas2.pid";
const LOG_FILE_NAME: &str = "atlas2.log";
const PROVIDER_FILE_NAME: &str = "serve-provider";

fn pid_file() -> AppResult<PathBuf> {
    Ok(config::state_dir()?.join(PID_FILE_NAME))
}

/// Resolves the path to the running executable, tolerating the case where the
/// binary was replaced in place (e.g. by `upgrade`). After such a replacement
/// Linux reports `current_exe()` as the now-unlinked inode with a trailing
/// `" (deleted)"`; the real path then holds the freshly installed binary.
fn resolve_self_exe() -> AppResult<PathBuf> {
    let exe = std::env::current_exe()
        .map_err(|error| AppError::Config(format!("failed to resolve executable path: {error}")))?;
    if exe.exists() {
        return Ok(exe);
    }
    let raw = exe.to_string_lossy();
    if let Some(stripped) = raw.strip_suffix(" (deleted)") {
        let path = PathBuf::from(stripped);
        if path.exists() {
            return Ok(path);
        }
    }
    Err(AppError::Config(format!(
        "could not locate the atlas2 executable (resolved to {})",
        exe.display()
    )))
}

fn provider_file() -> AppResult<PathBuf> {
    Ok(config::state_dir()?.join(PROVIDER_FILE_NAME))
}

/// Records the STT provider the daemon was launched with, so the daemon can be
/// faithfully restarted (e.g. after `upgrade`) without the original flags.
fn persist_serve_provider(provider: CliSttProvider) -> AppResult<()> {
    let value = match provider {
        CliSttProvider::None => "none",
        CliSttProvider::ElevenLabs => "11labs",
    };
    fs::write(provider_file()?, value)
        .map_err(|error| AppError::Config(format!("failed to record serve options: {error}")))
}

fn read_serve_provider() -> CliSttProvider {
    match provider_file().and_then(|path| Ok(fs::read_to_string(path).ok())) {
        Ok(Some(value)) if value.trim() == "11labs" => CliSttProvider::ElevenLabs,
        _ => CliSttProvider::None,
    }
}

pub fn log_file() -> AppResult<PathBuf> {
    Ok(config::state_dir()?.join(LOG_FILE_NAME))
}

/// Returns the PID of the running Atlas2 server, if one is alive. Cleans up a
/// stale PID file when the recorded process is gone.
fn running_pid() -> AppResult<Option<i32>> {
    let path = pid_file()?;
    let contents = match fs::read_to_string(&path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(AppError::Config(format!(
                "failed to read PID file {}: {error}",
                path.display()
            )));
        }
    };

    let Ok(pid) = contents.trim().parse::<i32>() else {
        let _ = fs::remove_file(&path);
        return Ok(None);
    };

    if process_alive(pid) {
        Ok(Some(pid))
    } else {
        let _ = fs::remove_file(&path);
        Ok(None)
    }
}

/// Probes whether `pid` is alive using `kill(pid, 0)`.
fn process_alive(pid: i32) -> bool {
    // Signal 0 performs error checking without delivering a signal: returns 0 if
    // the process exists, or fails with EPERM (exists, not ours) / ESRCH (gone).
    let result = unsafe { libc::kill(pid, 0) };
    if result == 0 {
        return true;
    }
    let errno = unsafe { *libc::__errno_location() };
    errno == libc::EPERM
}

/// Launches Atlas2 as a detached background process and returns immediately.
pub fn start(args: &ServeArgs) -> AppResult<()> {
    if let Some(pid) = running_pid()? {
        println!("Atlas2 is already running (pid {pid}).");
        return Ok(());
    }

    // A detached process has no terminal to prompt on, so the token must already
    // be configured. Tell the user how to set it instead of silently failing.
    if config::stored_telegram_bot_token()?.is_none() {
        return Err(AppError::Config(
            "no Telegram bot token configured; run `atlas2 set bottoken <value>` first".into(),
        ));
    }
    // Persist a key passed on the command line so the detached process (and any
    // later restart/upgrade) can load it from disk rather than the argv.
    if let Some(key) = &args.stt_api_key {
        config::set_secret("sttkey", key)?;
    }
    if args.stt_provider == CliSttProvider::ElevenLabs && config::stored_stt_api_key()?.is_none() {
        return Err(AppError::Config(
            "--stt-provider 11labs needs an API key; run `atlas2 set sttkey <value>` or pass --stt-api-key".into(),
        ));
    }

    let exe = resolve_self_exe()?;

    let log_path = log_file()?;
    if let Some(parent) = log_path.parent() {
        fs::create_dir_all(parent).map_err(|error| {
            AppError::Config(format!(
                "failed to create state directory {}: {error}",
                parent.display()
            ))
        })?;
    }
    let log = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .map_err(|error| {
            AppError::Config(format!(
                "failed to open log file {}: {error}",
                log_path.display()
            ))
        })?;
    let log_err = log.try_clone().map_err(|error| {
        AppError::Config(format!("failed to duplicate log file handle: {error}"))
    })?;

    let mut command = Command::new(&exe);
    command.arg("run");
    if args.stt_provider == CliSttProvider::ElevenLabs {
        command.args(["--stt-provider", "11labs"]);
    }
    command
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(log_err));

    // Detach from the controlling terminal so the server survives the shell that
    // launched it (e.g. closing the SSH session that ran `atlas2 start`).
    #[cfg(unix)]
    unsafe {
        use std::os::unix::process::CommandExt;
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }

    let child = command
        .spawn()
        .map_err(|error| AppError::Config(format!("failed to spawn background process: {error}")))?;

    fs::write(pid_file()?, child.id().to_string()).map_err(|error| {
        AppError::Config(format!("failed to write PID file: {error}"))
    })?;
    persist_serve_provider(args.stt_provider)?;

    println!("Atlas2 started (pid {}).", child.id());
    println!("Logs: {}", log_path.display());
    Ok(())
}

/// Stops the background Atlas2 process by sending SIGTERM.
pub fn stop() -> AppResult<()> {
    match running_pid()? {
        Some(pid) => {
            let result = unsafe { libc::kill(pid, libc::SIGTERM) };
            if result != 0 {
                let errno = unsafe { *libc::__errno_location() };
                return Err(AppError::Config(format!(
                    "failed to stop Atlas2 (pid {pid}): errno {errno}"
                )));
            }
            let _ = fs::remove_file(pid_file()?);
            println!("Stopped Atlas2 (pid {pid}).");
        }
        None => println!("Atlas2 is not running."),
    }
    Ok(())
}

/// Reports whether the background Atlas2 process is running.
pub fn status() -> AppResult<()> {
    match running_pid()? {
        Some(pid) => println!("Atlas2 is running (pid {pid})."),
        None => println!("Atlas2 is not running."),
    }
    Ok(())
}

/// Downloads and installs the latest release in place using the dist install
/// receipt, then restarts the background daemon if it was running.
pub async fn upgrade() -> AppResult<()> {
    let was_running = running_pid()?.is_some();
    let provider = read_serve_provider();

    let current = env!("CARGO_PKG_VERSION");
    println!("Current version: {current}. Checking for updates...");

    let mut updater = axoupdater::AxoUpdater::new_for("atlas2");
    updater.load_receipt().map_err(|error| {
        AppError::Config(format!(
            "could not read the install receipt (was atlas2 installed via the release installer?): {error}"
        ))
    })?;

    // Install the update before touching the running daemon: if it fails, the
    // old binary and the live daemon are left untouched.
    match updater.run().await {
        Ok(Some(result)) => {
            println!("Upgraded to {}.", result.new_version);
        }
        Ok(None) => {
            println!("Already on the latest version.");
            return Ok(());
        }
        Err(error) => {
            return Err(AppError::Config(format!("upgrade failed: {error}")));
        }
    }

    if was_running {
        println!("Restarting the background daemon...");
        stop()?;
        start(&ServeArgs {
            stt_provider: provider,
            stt_api_key: None,
        })?;
    }
    Ok(())
}
