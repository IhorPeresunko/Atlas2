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

fn pid_file() -> AppResult<PathBuf> {
    Ok(config::state_dir()?.join(PID_FILE_NAME))
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
    if args.stt_provider == CliSttProvider::ElevenLabs
        && args.stt_api_key.is_none()
        && config::stored_stt_api_key()?.is_none()
    {
        return Err(AppError::Config(
            "--stt-provider 11labs needs an API key; run `atlas2 set sttkey <value>` or pass --stt-api-key".into(),
        ));
    }

    let exe = std::env::current_exe()
        .map_err(|error| AppError::Config(format!("failed to resolve executable path: {error}")))?;

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
    if let Some(key) = &args.stt_api_key {
        command.args(["--stt-api-key", key]);
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
