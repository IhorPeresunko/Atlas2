use std::{
    env, fs,
    path::{Path, PathBuf},
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
    clean_exe_path(&exe, |path| path.exists()).ok_or_else(|| {
        AppError::Config(format!(
            "could not locate the atlas2 executable (resolved to {})",
            exe.display()
        ))
    })
}

/// Returns the first of `exe` / `exe` with a trailing `" (deleted)"` stripped
/// that satisfies `exists`. Pure helper so the deleted-inode handling can be
/// unit tested without replacing the running binary.
fn clean_exe_path(
    exe: &std::path::Path,
    exists: impl Fn(&std::path::Path) -> bool,
) -> Option<PathBuf> {
    if exists(exe) {
        return Some(exe.to_path_buf());
    }
    let raw = exe.to_string_lossy();
    let stripped = PathBuf::from(raw.strip_suffix(" (deleted)")?);
    exists(&stripped).then_some(stripped)
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
    // Read errno portably (the glibc `__errno_location` symbol does not exist on
    // macOS). `last_os_error` reads errno right after the failed `kill`.
    std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
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

    let child = command.spawn().map_err(|error| {
        AppError::Config(format!("failed to spawn background process: {error}"))
    })?;

    fs::write(pid_file()?, child.id().to_string())
        .map_err(|error| AppError::Config(format!("failed to write PID file: {error}")))?;
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
                let error = std::io::Error::last_os_error();
                return Err(AppError::Config(format!(
                    "failed to stop Atlas2 (pid {pid}): {error}"
                )));
            }
            let _ = fs::remove_file(pid_file()?);
            println!("Stopped Atlas2 (pid {pid}).");
        }
        None => println!("Atlas2 is not running."),
    }
    Ok(())
}

/// A single line in the `atlas2 status` report: a presence flag, an aligned
/// label, and a human-readable detail.
struct StatusLine {
    present: bool,
    label: &'static str,
    detail: String,
}

impl StatusLine {
    fn on(label: &'static str, detail: impl Into<String>) -> Self {
        Self {
            present: true,
            label,
            detail: detail.into(),
        }
    }

    fn off(label: &'static str, detail: impl Into<String>) -> Self {
        Self {
            present: false,
            label,
            detail: detail.into(),
        }
    }
}

/// Reports whether the background Atlas2 process is running, the state of the
/// surrounding configuration (Telegram token, coding agents, speech-to-text),
/// and the build this binary was cut from.
pub fn status() -> AppResult<()> {
    let daemon = match running_pid()? {
        Some(pid) => StatusLine::on("Daemon", format!("running (pid {pid})")),
        None => StatusLine::off("Daemon", "not running (start with `atlas2 start`)"),
    };

    let telegram = match config::stored_telegram_bot_token()? {
        Some(_) => StatusLine::on("Telegram", "bot token configured"),
        None => StatusLine::off("Telegram", "no bot token (set with `atlas2 set bottoken <value>`)"),
    };

    let codex = match locate_executable(&config::codex_bin()) {
        Some(path) => StatusLine::on("Codex", format!("connected ({})", path.display())),
        None => StatusLine::off("Codex", "not installed"),
    };

    let claude = match locate_executable(&config::claude_bin()) {
        Some(path) => StatusLine::on("Claude", format!("connected ({})", path.display())),
        None => StatusLine::off("Claude", "not installed"),
    };

    let speech = match config::stored_stt_api_key()? {
        Some(_) => StatusLine::on("Speech-to-text", "ElevenLabs key configured"),
        None => StatusLine::off("Speech-to-text", "disabled (no API key)"),
    };

    print!(
        "{}",
        render_status(
            &[daemon, telegram, codex, claude, speech],
            env!("CARGO_PKG_VERSION"),
        )
    );
    Ok(())
}

/// Renders the status lines into the aligned block printed by `atlas2 status`,
/// footed with the version and build origin. Pure so the layout can be unit
/// tested without touching the filesystem.
fn render_status(lines: &[StatusLine], version: &str) -> String {
    let width = lines.iter().map(|line| line.label.len()).max().unwrap_or(0);
    let mut out = String::from("\n");
    for line in lines {
        let mark = if line.present { '✓' } else { '✗' };
        out.push_str(&format!(
            "  {mark}  {:<width$}  {}\n",
            line.label, line.detail
        ));
    }
    out.push_str(&format!("\n  atlas2 {version} · built in prague\n\n"));
    out
}

/// Finds `program` the way a shell would: an explicit path (absolute, or
/// relative with a directory component) is checked directly, while a bare name
/// is searched across `PATH`. Returns the resolved path when it points at a
/// runnable file.
fn locate_executable(program: &str) -> Option<PathBuf> {
    let path_dirs: Vec<PathBuf> = env::var_os("PATH")
        .map(|paths| env::split_paths(&paths).collect())
        .unwrap_or_default();
    find_executable(program, &path_dirs, is_executable_file)
}

/// Pure core of [`locate_executable`]: `is_exec` reports whether a candidate is
/// a runnable file, injected so the search can be unit tested without real
/// files on disk.
fn find_executable(
    program: &str,
    path_dirs: &[PathBuf],
    is_exec: impl Fn(&Path) -> bool,
) -> Option<PathBuf> {
    let candidate = Path::new(program);
    let has_dir_component = candidate
        .parent()
        .is_some_and(|parent| !parent.as_os_str().is_empty());
    if candidate.is_absolute() || has_dir_component {
        return is_exec(candidate).then(|| candidate.to_path_buf());
    }
    path_dirs
        .iter()
        .map(|dir| dir.join(program))
        .find(|path| is_exec(path))
}

/// Whether `path` is an existing file with at least one executable bit set.
fn is_executable_file(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    match fs::metadata(path) {
        Ok(meta) => meta.is_file() && meta.permissions().mode() & 0o111 != 0,
        Err(_) => false,
    }
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
        if let Err(error) = start(&ServeArgs {
            stt_provider: provider,
            stt_api_key: None,
        }) {
            return Err(AppError::Config(format!(
                "upgrade installed the new version but restarting the daemon failed ({error}); run `atlas2 start` to bring it back up"
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::{StatusLine, clean_exe_path, find_executable, render_status};

    #[test]
    fn find_executable_searches_path_for_bare_name() {
        let dirs = [PathBuf::from("/opt/bin"), PathBuf::from("/usr/local/bin")];
        let found = find_executable("codex", &dirs, |path| {
            path == Path::new("/usr/local/bin/codex")
        });
        assert_eq!(found, Some(PathBuf::from("/usr/local/bin/codex")));
    }

    #[test]
    fn find_executable_returns_none_when_absent_from_path() {
        let dirs = [PathBuf::from("/opt/bin")];
        let found = find_executable("claude", &dirs, |_| false);
        assert_eq!(found, None);
    }

    #[test]
    fn find_executable_checks_explicit_path_directly() {
        // A path with a directory component is not searched across PATH.
        let found = find_executable("/custom/codex", &[], |path| {
            path == Path::new("/custom/codex")
        });
        assert_eq!(found, Some(PathBuf::from("/custom/codex")));
    }

    #[test]
    fn render_status_aligns_labels_and_foots_with_build() {
        let report = render_status(
            &[
                StatusLine::on("Daemon", "running (pid 7)"),
                StatusLine::off("Speech-to-text", "disabled"),
            ],
            "1.2.3",
        );
        // The shorter label is padded to the width of the longest one, so the
        // details line up in a column.
        assert!(report.contains("  ✓  Daemon          running (pid 7)\n"));
        assert!(report.contains("  ✗  Speech-to-text  disabled\n"));
        assert!(report.contains("atlas2 1.2.3 · built in prague"));
    }

    #[test]
    fn clean_exe_path_returns_existing_path_unchanged() {
        let result = clean_exe_path(Path::new("/usr/bin/atlas2"), |p| {
            p == Path::new("/usr/bin/atlas2")
        });
        assert_eq!(result, Some(PathBuf::from("/usr/bin/atlas2")));
    }

    #[test]
    fn clean_exe_path_strips_deleted_suffix_after_in_place_upgrade() {
        // current_exe() points at the unlinked old inode; the real path holds
        // the freshly installed binary.
        let result = clean_exe_path(Path::new("/usr/bin/atlas2 (deleted)"), |p| {
            p == Path::new("/usr/bin/atlas2")
        });
        assert_eq!(result, Some(PathBuf::from("/usr/bin/atlas2")));
    }

    #[test]
    fn clean_exe_path_returns_none_when_nothing_exists() {
        let result = clean_exe_path(Path::new("/usr/bin/atlas2 (deleted)"), |_| false);
        assert_eq!(result, None);
    }
}
