//! Claude CLI provider.
//!
//! Drives the `claude` binary in headless streaming mode
//! (`--print --output-format stream-json --input-format stream-json --verbose`)
//! and translates its event stream into the provider-agnostic [`ProviderEvent`]
//! vocabulary, mirroring [`super::codex::CodexProvider`].
//!
//! Protocol summary (one JSON object per stdout line):
//! - `{"type":"system","subtype":"init","session_id":...}` — emitted once at
//!   start; carries the session id Atlas2 stores as the provider thread.
//! - `{"type":"assistant","message":{content:[...]}}` — assistant turns; text
//!   blocks become [`ProviderEvent::Output`], `Bash` tool-use blocks become
//!   command events, and an `ExitPlanMode` tool-use carries the proposed plan.
//! - `{"type":"user","message":{content:[{tool_result}]}}` — tool results.
//! - `{"type":"result",...}` — terminal turn outcome.
//! - `{"type":"control_request","request":{subtype:"can_use_tool",...}}` — a
//!   permission prompt, answered with a `control_response` on stdin.

use std::{
    collections::HashMap,
    path::PathBuf,
    process::Stdio,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use serde_json::{Value, json};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::{Child, Command},
    sync::{Mutex, mpsc},
};
use uuid::Uuid;

use crate::{
    domain::{
        ApprovalId, PromptMode, SessionId, SessionRecord, ThreadId, UserInputAnswer,
        UserInputRequestId,
    },
    error::{AppError, AppResult},
    provider::{
        ModelOption, Provider, ProviderApprovalRequest, ProviderEvent, ReasoningEffortOption,
        TurnResult,
    },
};

mod sessions;

pub use sessions::ClaudeThreadReader;

#[derive(Debug, Clone)]
pub struct ClaudeProvider {
    claude_bin: String,
    additional_dirs: Vec<PathBuf>,
    runtimes: Arc<Mutex<HashMap<SessionId, Arc<ClaudeLiveHandle>>>>,
}

impl ClaudeProvider {
    pub fn new(claude_bin: String, additional_dirs: Vec<PathBuf>) -> Self {
        Self {
            claude_bin,
            additional_dirs,
            runtimes: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub async fn run_turn<F>(
        &self,
        session: &SessionRecord,
        prompt: &str,
        mode: PromptMode,
        model: Option<&str>,
        reasoning_effort: Option<&str>,
        dangerously_skip_permissions: bool,
        mut on_event: F,
    ) -> AppResult<TurnResult>
    where
        F: FnMut(ProviderEvent) -> AppResult<()>,
    {
        // Atlas2 owns the session id: resume the stored one, or mint a fresh one
        // so the new thread is addressable for later turns.
        let (session_arg, thread_id, resuming) = match session.provider_thread_id.as_ref() {
            Some(thread_id) => (
                SessionArg::Resume(thread_id.0.clone()),
                thread_id.0.clone(),
                true,
            ),
            None => {
                let fresh = Uuid::new_v4().to_string();
                (SessionArg::Fresh(fresh.clone()), fresh, false)
            }
        };

        let mut runtime = ClaudeRuntime::start(
            &self.claude_bin,
            &self.additional_dirs,
            &session.workspace_path.0,
            mode,
            model,
            reasoning_effort,
            dangerously_skip_permissions,
            &session_arg,
        )
        .await?;
        self.runtimes
            .lock()
            .await
            .insert(session.session_id.clone(), runtime.handle());

        // Announce the thread up front so it is persisted even if the turn fails
        // before the `init` event arrives.
        let mut result = TurnResult {
            thread_id: Some(ThreadId(thread_id.clone())),
            resume_cursor_json: Some(resume_cursor_json(&thread_id)),
            ..TurnResult::default()
        };
        if !resuming {
            on_event(ProviderEvent::ThreadStarted {
                thread_id: ThreadId(thread_id.clone()),
                resume_cursor_json: Some(resume_cursor_json(&thread_id)),
            })?;
        }

        let run_result = async {
            runtime.send_user_prompt(prompt).await?;
            loop {
                let Some(event) = runtime.next_event().await? else {
                    return Err(AppError::Provider(
                        "claude exited before the turn completed".into(),
                    ));
                };
                // In plan mode a proposed plan ends the turn: present it and wait
                // for the user's Implement/Add-details choice. Otherwise Claude —
                // whose `ExitPlanMode` is auto-denied in headless mode — keeps
                // looping, repeatedly re-proposing the plan and failing tool calls
                // against the read-only sandbox. Stopping here makes plan turns
                // one-shot, matching Codex. The session resumes cleanly for the
                // follow-up implementation turn (verified: a killed plan turn does
                // not corrupt `--resume`).
                let plan_finished =
                    mode == PromptMode::Plan && matches!(event, ProviderEvent::PlanCompleted { .. });
                match &event {
                    ProviderEvent::ThreadStarted {
                        thread_id,
                        resume_cursor_json,
                    } => {
                        result.thread_id = Some(thread_id.clone());
                        result.resume_cursor_json = resume_cursor_json.clone();
                    }
                    ProviderEvent::TurnCompleted => result.completed = true,
                    ProviderEvent::TurnInterrupted { .. } => result.interrupted = true,
                    ProviderEvent::TurnFailed { message } => {
                        result.failure = Some(message.clone())
                    }
                    _ => {}
                }
                on_event(event.clone())?;
                if plan_finished {
                    result.completed = true;
                    break;
                }
                if result.completed || result.interrupted || result.failure.is_some() {
                    break;
                }
            }
            Ok::<(), AppError>(())
        }
        .await;

        self.runtimes.lock().await.remove(&session.session_id);
        let shutdown_result = runtime.shutdown().await;
        match (run_result, shutdown_result) {
            (Ok(()), _) => Ok(result),
            (Err(error), _) => Err(error),
        }
    }

    /// Claude exposes no model-catalog endpoint, so Atlas2 advertises the known
    /// aliases the CLI accepts. Each carries the effort levels the CLI's
    /// `--effort` flag supports, surfaced as the reasoning-level picker.
    pub fn model_catalog(&self) -> Vec<ModelOption> {
        vec![
            model_option("sonnet", "Claude Sonnet", true),
            model_option("opus", "Claude Opus", false),
            model_option("haiku", "Claude Haiku", false),
        ]
    }

    pub async fn resolve_approval(
        &self,
        session_id: &SessionId,
        approval_id: &ApprovalId,
        approved: bool,
    ) -> AppResult<()> {
        let runtime = self
            .runtimes
            .lock()
            .await
            .get(session_id)
            .cloned()
            .ok_or_else(|| {
                AppError::Validation(
                    "approval is stale because the live Claude runtime is no longer active".into(),
                )
            })?;
        runtime.resolve_approval(approval_id, approved).await
    }

    pub async fn stop_turn(&self, session_id: &SessionId) -> AppResult<()> {
        let runtime = self
            .runtimes
            .lock()
            .await
            .get(session_id)
            .cloned()
            .ok_or_else(|| AppError::Validation("Claude turn is no longer running".into()))?;
        runtime.interrupt().await
    }
}

#[async_trait::async_trait]
impl Provider for ClaudeProvider {
    async fn list_models(&self, _workspace_path: &str) -> AppResult<Vec<ModelOption>> {
        Ok(ClaudeProvider::model_catalog(self))
    }

    async fn run_turn(
        &self,
        session: &SessionRecord,
        prompt: &str,
        mode: PromptMode,
        model: Option<&str>,
        reasoning_effort: Option<&str>,
        dangerously_skip_permissions: bool,
        on_event: Box<dyn FnMut(ProviderEvent) -> AppResult<()> + Send>,
    ) -> AppResult<TurnResult> {
        ClaudeProvider::run_turn(
            self,
            session,
            prompt,
            mode,
            model,
            reasoning_effort,
            dangerously_skip_permissions,
            on_event,
        )
        .await
    }

    async fn resolve_approval(
        &self,
        session_id: &SessionId,
        approval_id: &ApprovalId,
        approved: bool,
    ) -> AppResult<()> {
        ClaudeProvider::resolve_approval(self, session_id, approval_id, approved).await
    }

    async fn resolve_user_input(
        &self,
        _session_id: &SessionId,
        _request_id: &UserInputRequestId,
        _answers: HashMap<String, UserInputAnswer>,
    ) -> AppResult<()> {
        // Claude has no interactive request_user_input equivalent in headless
        // mode, so it never raises one for Atlas2 to answer.
        Err(AppError::Validation(
            "Claude does not support interactive user input requests".into(),
        ))
    }

    async fn stop_turn(&self, session_id: &SessionId) -> AppResult<()> {
        ClaudeProvider::stop_turn(self, session_id).await
    }
}

fn model_option(slug: &str, display_name: &str, is_default: bool) -> ModelOption {
    ModelOption {
        model: slug.to_string(),
        display_name: display_name.to_string(),
        is_default,
        // No default marked: leave effort unset so the CLI applies its own
        // default until the user picks a level in the reasoning-level step.
        default_reasoning_effort: None,
        supported_reasoning_efforts: claude_effort_levels(),
    }
}

/// The effort levels Claude's `--effort` flag accepts, surfaced as the
/// reasoning-level picker. Order matches lowest-to-highest thinking budget.
fn claude_effort_levels() -> Vec<ReasoningEffortOption> {
    [
        ("low", "minimal thinking, fastest"),
        ("medium", "balanced"),
        ("high", "more thorough"),
        ("xhigh", "extended thinking"),
        ("max", "maximum thinking budget"),
    ]
    .into_iter()
    .map(|(effort, description)| ReasoningEffortOption {
        effort: effort.to_string(),
        description: description.to_string(),
    })
    .collect()
}

fn resume_cursor_json(thread_id: &str) -> String {
    json!({ "threadId": thread_id }).to_string()
}

enum SessionArg {
    /// Start a brand-new session with a chosen id (`--session-id`).
    Fresh(String),
    /// Resume an existing session (`--resume`).
    Resume(String),
}

struct ClaudeRuntime {
    child: Child,
    receiver: mpsc::UnboundedReceiver<ProviderEvent>,
    handle: Arc<ClaudeLiveHandle>,
    stdout_task: tokio::task::JoinHandle<()>,
    stderr_task: tokio::task::JoinHandle<()>,
    writer_task: tokio::task::JoinHandle<()>,
}

impl ClaudeRuntime {
    async fn start(
        claude_bin: &str,
        additional_dirs: &[PathBuf],
        workspace_path: &str,
        mode: PromptMode,
        model: Option<&str>,
        reasoning_effort: Option<&str>,
        dangerously_skip_permissions: bool,
        session_arg: &SessionArg,
    ) -> AppResult<Self> {
        let mut command = Command::new(claude_bin);
        command
            .arg("--print")
            .args(["--output-format", "stream-json"])
            .args(["--input-format", "stream-json"])
            .arg("--verbose")
            .args([
                "--permission-mode",
                permission_mode(mode, dangerously_skip_permissions),
            ]);
        match session_arg {
            SessionArg::Fresh(id) => {
                command.args(["--session-id", id]);
            }
            SessionArg::Resume(id) => {
                command.args(["--resume", id]);
            }
        }
        if let Some(model) = model {
            command.args(["--model", model]);
        }
        // Thinking budget; omitted lets the CLI apply its own default.
        if let Some(effort) = reasoning_effort {
            command.args(["--effort", effort]);
        }
        for dir in additional_dirs {
            command.arg("--add-dir").arg(dir);
        }
        command
            .current_dir(workspace_path)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let mut child = command.spawn()?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| AppError::Provider("missing stdin from claude".into()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| AppError::Provider("missing stdout from claude".into()))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| AppError::Provider("missing stderr from claude".into()))?;

        let (sender, mut write_rx) = mpsc::unbounded_channel::<String>();
        let writer_task = tokio::spawn(async move {
            let mut stdin = stdin;
            while let Some(message) = write_rx.recv().await {
                if stdin.write_all(message.as_bytes()).await.is_err()
                    || stdin.write_all(b"\n").await.is_err()
                    || stdin.flush().await.is_err()
                {
                    break;
                }
            }
        });

        let (event_tx, receiver) = mpsc::unbounded_channel::<ProviderEvent>();
        let handle = Arc::new(ClaudeLiveHandle {
            sender,
            pending_approvals: Mutex::new(HashMap::new()),
            next_request_id: AtomicU64::new(1),
        });

        let stdout_task = tokio::spawn(read_stdout_loop(
            BufReader::new(stdout),
            event_tx,
            handle.clone(),
        ));
        let stderr_task = tokio::spawn(read_stderr_loop(BufReader::new(stderr)));

        Ok(Self {
            child,
            receiver,
            handle,
            stdout_task,
            stderr_task,
            writer_task,
        })
    }

    fn handle(&self) -> Arc<ClaudeLiveHandle> {
        self.handle.clone()
    }

    async fn send_user_prompt(&mut self, prompt: &str) -> AppResult<()> {
        // Plan vs normal behavior is selected via `--permission-mode`, so the
        // prompt is forwarded verbatim (no provider-specific contract wrapping).
        let message = json!({
            "type": "user",
            "message": {
                "role": "user",
                "content": [{ "type": "text", "text": prompt }],
            }
        });
        self.handle
            .sender
            .send(message.to_string())
            .map_err(|_| AppError::Provider("failed to send prompt to claude".into()))
    }

    async fn next_event(&mut self) -> AppResult<Option<ProviderEvent>> {
        tokio::select! {
            event = self.receiver.recv() => Ok(event),
            status = self.child.wait() => {
                let status = status?;
                if status.success() {
                    Ok(None)
                } else {
                    Err(AppError::Provider(format!("claude exited with status {status}")))
                }
            }
        }
    }

    async fn shutdown(&mut self) -> AppResult<()> {
        let _ = self.child.start_kill();
        let _ = self.child.wait().await;
        self.writer_task.abort();
        self.stdout_task.abort();
        self.stderr_task.abort();
        Ok(())
    }
}

/// Shared handle for interacting with a live Claude turn (approvals, interrupt).
#[derive(Debug)]
struct ClaudeLiveHandle {
    sender: mpsc::UnboundedSender<String>,
    pending_approvals: Mutex<HashMap<ApprovalId, PendingApproval>>,
    next_request_id: AtomicU64,
}

#[derive(Debug, Clone)]
struct PendingApproval {
    /// The `control_request` id Claude is waiting on.
    request_id: String,
    /// The tool input to echo back when allowing.
    input: Value,
}

impl ClaudeLiveHandle {
    async fn resolve_approval(&self, approval_id: &ApprovalId, approved: bool) -> AppResult<()> {
        let pending = self
            .pending_approvals
            .lock()
            .await
            .remove(approval_id)
            .ok_or_else(|| AppError::Validation("approval request is no longer active".into()))?;
        let response = if approved {
            json!({ "behavior": "allow", "updatedInput": pending.input })
        } else {
            json!({ "behavior": "deny", "message": "Rejected from Telegram" })
        };
        let envelope = json!({
            "type": "control_response",
            "response": {
                "subtype": "success",
                "request_id": pending.request_id,
                "response": response,
            }
        });
        self.sender
            .send(envelope.to_string())
            .map_err(|_| AppError::Provider("failed to send approval decision to claude".into()))
    }

    async fn interrupt(&self) -> AppResult<()> {
        let request_id = format!("atlas-int-{}", self.next_request_id.fetch_add(1, Ordering::Relaxed));
        let envelope = json!({
            "type": "control_request",
            "request_id": request_id,
            "request": { "subtype": "interrupt" }
        });
        self.sender
            .send(envelope.to_string())
            .map_err(|_| AppError::Provider("failed to interrupt claude turn".into()))
    }
}

async fn read_stdout_loop(
    reader: BufReader<tokio::process::ChildStdout>,
    event_tx: mpsc::UnboundedSender<ProviderEvent>,
    handle: Arc<ClaudeLiveHandle>,
) {
    let mut commands = HashMap::<String, String>::new();
    let mut lines = reader.lines();
    while let Ok(Some(line)) = lines.next_line().await {
        if line.trim().is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if value.get("type").and_then(Value::as_str) == Some("control_request") {
            handle_control_request(&value, &event_tx, &handle).await;
            continue;
        }
        for event in map_stream_event(&value, &mut commands) {
            let _ = event_tx.send(event);
        }
    }
}

async fn read_stderr_loop(reader: BufReader<tokio::process::ChildStderr>) {
    let mut lines = reader.lines();
    while let Ok(Some(line)) = lines.next_line().await {
        tracing::warn!(stderr = line, "claude stderr");
    }
}

async fn handle_control_request(
    value: &Value,
    event_tx: &mpsc::UnboundedSender<ProviderEvent>,
    handle: &Arc<ClaudeLiveHandle>,
) {
    let request = value.get("request");
    let subtype = request
        .and_then(|request| request.get("subtype"))
        .and_then(Value::as_str);
    let Some("can_use_tool") = subtype else {
        return;
    };
    let Some(request_id) = value.get("request_id").and_then(Value::as_str) else {
        return;
    };
    let request = request.expect("checked above");
    let tool_name = request
        .get("tool_name")
        .and_then(Value::as_str)
        .unwrap_or("tool");
    let input = request.get("input").cloned().unwrap_or(Value::Null);

    let approval_id = ApprovalId::new();
    handle.pending_approvals.lock().await.insert(
        approval_id.clone(),
        PendingApproval {
            request_id: request_id.to_string(),
            input: input.clone(),
        },
    );
    let _ = event_tx.send(ProviderEvent::ApprovalRequested {
        approval: ProviderApprovalRequest {
            approval_id,
            summary: summarize_permission(tool_name, &input),
            payload: input.to_string(),
        },
    });
}

/// Translates one Claude stream event into zero or more [`ProviderEvent`]s.
/// `commands` tracks `tool_use` id -> command so a later `tool_result` can be
/// rendered as a finished command.
fn map_stream_event(value: &Value, commands: &mut HashMap<String, String>) -> Vec<ProviderEvent> {
    match value.get("type").and_then(Value::as_str) {
        Some("system") => {
            if value.get("subtype").and_then(Value::as_str) == Some("init")
                && let Some(session_id) = value.get("session_id").and_then(Value::as_str)
            {
                return vec![ProviderEvent::ThreadStarted {
                    thread_id: ThreadId(session_id.to_string()),
                    resume_cursor_json: Some(resume_cursor_json(session_id)),
                }];
            }
            Vec::new()
        }
        Some("assistant") => map_assistant_message(value, commands),
        Some("user") => map_tool_results(value, commands),
        Some("result") => {
            let mut events = Vec::new();
            // When permissions are not skipped, headless Claude silently declines
            // tools it would otherwise prompt for. Surface that so the user knows
            // why nothing happened and how to allow it.
            if let Some(note) = permission_denial_note(value) {
                events.push(ProviderEvent::Output { text: note });
            }
            events.push(map_result(value));
            events
        }
        _ => Vec::new(),
    }
}

/// Builds a user-facing note when a turn's `result` reports tools that were
/// blocked for lack of permission. Returns `None` when nothing was denied.
fn permission_denial_note(value: &Value) -> Option<String> {
    let denials = value.get("permission_denials")?.as_array()?;
    if denials.is_empty() {
        return None;
    }
    let mut tools: Vec<&str> = denials
        .iter()
        .filter_map(|denial| denial.get("tool_name").and_then(Value::as_str))
        .collect();
    tools.sort_unstable();
    tools.dedup();
    let tool_list = if tools.is_empty() {
        String::new()
    } else {
        format!(" ({})", tools.join(", "))
    };
    Some(format!(
        "Claude was blocked from {} action(s){tool_list} that need permission. \
         Run /yolo to let Claude run tools without asking in this chat.",
        denials.len()
    ))
}

fn map_assistant_message(value: &Value, commands: &mut HashMap<String, String>) -> Vec<ProviderEvent> {
    let Some(content) = value
        .get("message")
        .and_then(|message| message.get("content"))
        .and_then(Value::as_array)
    else {
        return Vec::new();
    };

    let mut events = Vec::new();
    for block in content {
        match block.get("type").and_then(Value::as_str) {
            Some("text") => {
                if let Some(text) = block.get("text").and_then(Value::as_str)
                    && !text.trim().is_empty()
                {
                    events.push(ProviderEvent::Output {
                        text: text.to_string(),
                    });
                }
            }
            Some("tool_use") => {
                let name = block.get("name").and_then(Value::as_str).unwrap_or("");
                let id = block.get("id").and_then(Value::as_str).unwrap_or_default();
                let input = block.get("input").cloned().unwrap_or(Value::Null);
                if name == "ExitPlanMode" {
                    if let Some(plan) = input.get("plan").and_then(Value::as_str) {
                        events.push(ProviderEvent::PlanCompleted {
                            markdown: plan.to_string(),
                        });
                    }
                } else if name == "Bash" {
                    let command = input
                        .get("command")
                        .and_then(Value::as_str)
                        .unwrap_or("command")
                        .to_string();
                    commands.insert(id.to_string(), command.clone());
                    events.push(ProviderEvent::CommandStarted { command });
                }
            }
            _ => {}
        }
    }
    events
}

fn map_tool_results(value: &Value, commands: &mut HashMap<String, String>) -> Vec<ProviderEvent> {
    let Some(content) = value
        .get("message")
        .and_then(|message| message.get("content"))
        .and_then(Value::as_array)
    else {
        return Vec::new();
    };

    let mut events = Vec::new();
    for block in content {
        if block.get("type").and_then(Value::as_str) != Some("tool_result") {
            continue;
        }
        let tool_use_id = block
            .get("tool_use_id")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let Some(command) = commands.remove(tool_use_id) else {
            // Not a tracked Bash command (e.g. a file read); skip.
            continue;
        };
        let is_error = block
            .get("is_error")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        events.push(ProviderEvent::CommandFinished {
            command,
            exit_code: if is_error { 1 } else { 0 },
            output: tool_result_text(block.get("content")),
        });
    }
    events
}

fn map_result(value: &Value) -> ProviderEvent {
    let is_error = value
        .get("is_error")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let subtype = value.get("subtype").and_then(Value::as_str).unwrap_or("");
    if is_error || subtype.starts_with("error") {
        let message = value
            .get("result")
            .and_then(Value::as_str)
            .filter(|text| !text.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| format!("Claude turn failed ({subtype})"));
        return ProviderEvent::TurnFailed { message };
    }
    ProviderEvent::TurnCompleted
}

/// Extracts a human-readable string from a `tool_result` `content`, which is
/// either a string or an array of `{type:"text",text}` blocks.
fn tool_result_text(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(blocks)) => blocks
            .iter()
            .filter_map(|block| block.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

fn summarize_permission(tool_name: &str, input: &Value) -> String {
    if tool_name == "Bash"
        && let Some(command) = input.get("command").and_then(Value::as_str)
    {
        return format!("Claude wants to run a command.\n`{command}`");
    }
    if let Some(path) = input.get("file_path").and_then(Value::as_str) {
        return format!("Claude wants to use {tool_name} on `{path}`.");
    }
    format!("Claude wants to use the {tool_name} tool.")
}

fn permission_mode(mode: PromptMode, dangerously_skip_permissions: bool) -> &'static str {
    match mode {
        // Plan mode is read-only regardless of the skip toggle.
        PromptMode::Plan => "plan",
        // `bypassPermissions` runs tools without asking; `default` makes headless
        // Claude auto-decline tools that would need approval.
        PromptMode::Normal if dangerously_skip_permissions => "bypassPermissions",
        PromptMode::Normal => "default",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// End-to-end check that a skip-permissions turn actually runs a tool through
    /// Atlas2's own ClaudeProvider path. Ignored by default because it spawns the
    /// real `claude` binary (needs auth, costs tokens). Run with:
    /// `cargo test --bin atlas2 -- --ignored claude_skip_permissions_writes_file`
    #[tokio::test]
    #[ignore]
    async fn claude_skip_permissions_writes_file() {
        use crate::domain::{ProviderKind, SessionId, SessionStatus, WorkspacePath};
        use chrono::Utc;

        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().to_string_lossy().into_owned();
        let provider = ClaudeProvider::new("claude".into(), Vec::new());
        let session = SessionRecord {
            session_id: SessionId::new(),
            chat_id: crate::domain::TelegramChatId(1),
            workspace_path: WorkspacePath(workspace.clone()),
            provider: ProviderKind::Claude,
            provider_thread_id: None,
            resume_cursor_json: None,
            status: SessionStatus::Ready,
            last_error: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };

        let result = provider
            .run_turn(
                &session,
                "Create a file called hello.txt containing the word hi. Just do it.",
                PromptMode::Normal,
                Some("sonnet"),
                None,
                true, // dangerously_skip_permissions
                |_event| Ok(()),
            )
            .await
            .expect("turn runs");

        assert!(result.completed, "turn should complete: {result:?}");
        let written = std::fs::read_to_string(temp.path().join("hello.txt"))
            .expect("hello.txt should have been written under bypassPermissions");
        assert!(written.contains("hi"));
    }

    /// End-to-end check that a plan turn is one-shot: it stops at the first plan
    /// (one `PlanCompleted`), completes, and leaves the workspace unmodified —
    /// rather than looping and trying to implement against the read-only sandbox.
    /// Ignored by default (spawns the real `claude` binary). Run with:
    /// `cargo test --bin atlas2 -- --ignored claude_plan_turn_is_one_shot`
    #[tokio::test]
    #[ignore]
    async fn claude_plan_turn_is_one_shot() {
        use crate::domain::{ProviderKind, SessionId, SessionStatus, WorkspacePath};
        use chrono::Utc;
        use std::sync::{Arc, Mutex};

        let temp = tempfile::tempdir().unwrap();
        let file = temp.path().join("main.rs");
        std::fs::write(&file, "fn main() { println!(\"hi\"); }\n").unwrap();
        let original = std::fs::read_to_string(&file).unwrap();

        let provider = ClaudeProvider::new("claude".into(), Vec::new());
        let session = SessionRecord {
            session_id: SessionId::new(),
            chat_id: crate::domain::TelegramChatId(1),
            workspace_path: WorkspacePath(temp.path().to_string_lossy().into_owned()),
            provider: ProviderKind::Claude,
            provider_thread_id: None,
            resume_cursor_json: None,
            status: SessionStatus::Ready,
            last_error: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };

        let plan_count = Arc::new(Mutex::new(0usize));
        let plan_count_cb = plan_count.clone();
        let result = provider
            .run_turn(
                &session,
                "Make a short plan to add a goodbye() function to main.rs, then implement it.",
                PromptMode::Plan,
                Some("sonnet"),
                None,
                false,
                move |event| {
                    if matches!(event, ProviderEvent::PlanCompleted { .. }) {
                        *plan_count_cb.lock().unwrap() += 1;
                    }
                    Ok(())
                },
            )
            .await
            .expect("plan turn runs");

        assert!(result.completed, "plan turn should complete: {result:?}");
        // One-shot: exactly one plan, and the turn stopped there.
        assert_eq!(*plan_count.lock().unwrap(), 1, "expected exactly one plan");
        // Plan mode is read-only: the file must be untouched.
        assert_eq!(std::fs::read_to_string(&file).unwrap(), original);
    }

    #[test]
    fn maps_init_to_thread_started() {
        let mut commands = HashMap::new();
        let events = map_stream_event(
            &json!({"type":"system","subtype":"init","session_id":"sess-1"}),
            &mut commands,
        );
        match events.as_slice() {
            [ProviderEvent::ThreadStarted { thread_id, .. }] => assert_eq!(thread_id.0, "sess-1"),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn maps_assistant_text_and_bash_command() {
        let mut commands = HashMap::new();
        let events = map_stream_event(
            &json!({
                "type":"assistant",
                "message":{"content":[
                    {"type":"text","text":"running tests"},
                    {"type":"tool_use","id":"t1","name":"Bash","input":{"command":"cargo test"}}
                ]}
            }),
            &mut commands,
        );
        assert_eq!(events.len(), 2);
        assert!(matches!(&events[0], ProviderEvent::Output { text } if text == "running tests"));
        assert!(matches!(&events[1], ProviderEvent::CommandStarted { command } if command == "cargo test"));
        assert_eq!(commands.get("t1").map(String::as_str), Some("cargo test"));
    }

    #[test]
    fn maps_tool_result_to_command_finished() {
        let mut commands = HashMap::from([("t1".to_string(), "cargo test".to_string())]);
        let events = map_stream_event(
            &json!({
                "type":"user",
                "message":{"content":[
                    {"type":"tool_result","tool_use_id":"t1","content":"ok","is_error":false}
                ]}
            }),
            &mut commands,
        );
        match events.as_slice() {
            [ProviderEvent::CommandFinished { command, exit_code, output }] => {
                assert_eq!(command, "cargo test");
                assert_eq!(*exit_code, 0);
                assert_eq!(output, "ok");
            }
            other => panic!("unexpected: {other:?}"),
        }
        assert!(commands.is_empty());
    }

    #[test]
    fn maps_exit_plan_mode_to_plan_completed() {
        let mut commands = HashMap::new();
        let events = map_stream_event(
            &json!({
                "type":"assistant",
                "message":{"content":[
                    {"type":"tool_use","id":"p1","name":"ExitPlanMode","input":{"plan":"# Plan\n- step"}}
                ]}
            }),
            &mut commands,
        );
        match events.as_slice() {
            [ProviderEvent::PlanCompleted { markdown }] => {
                assert_eq!(markdown, "# Plan\n- step")
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn maps_success_and_error_results() {
        let mut commands = HashMap::new();
        let ok = map_stream_event(
            &json!({"type":"result","subtype":"success","is_error":false,"result":"done"}),
            &mut commands,
        );
        assert!(matches!(ok.as_slice(), [ProviderEvent::TurnCompleted]));

        let failed = map_stream_event(
            &json!({"type":"result","subtype":"error_max_turns","is_error":true,"result":"too many turns"}),
            &mut commands,
        );
        match failed.as_slice() {
            [ProviderEvent::TurnFailed { message }] => assert_eq!(message, "too many turns"),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn summarizes_bash_permission_with_command() {
        let summary = summarize_permission("Bash", &json!({"command":"rm -rf build"}));
        assert!(summary.contains("rm -rf build"));
    }

    #[test]
    fn permission_mode_reflects_plan_and_skip_toggle() {
        // Plan is read-only regardless of the skip toggle.
        assert_eq!(permission_mode(PromptMode::Plan, false), "plan");
        assert_eq!(permission_mode(PromptMode::Plan, true), "plan");
        assert_eq!(permission_mode(PromptMode::Normal, false), "default");
        assert_eq!(
            permission_mode(PromptMode::Normal, true),
            "bypassPermissions"
        );
    }

    #[test]
    fn permission_denial_note_lists_blocked_tools() {
        let note = permission_denial_note(&json!({
            "permission_denials": [
                {"tool_name": "Write"},
                {"tool_name": "Bash"},
                {"tool_name": "Write"}
            ]
        }))
        .expect("a note for denied tools");
        assert!(note.contains("Bash"));
        assert!(note.contains("Write"));
        assert!(note.contains("/yolo"));

        // No denials -> no note.
        assert!(permission_denial_note(&json!({"permission_denials": []})).is_none());
        assert!(permission_denial_note(&json!({})).is_none());
    }

    #[test]
    fn catalog_advertises_cli_effort_levels() {
        let catalog = ClaudeProvider::new("claude".into(), Vec::new()).model_catalog();
        assert!(catalog.iter().any(|m| m.model == "sonnet" && m.is_default));

        let efforts: Vec<&str> = catalog[0]
            .supported_reasoning_efforts
            .iter()
            .map(|e| e.effort.as_str())
            .collect();
        // Exactly the values `claude --effort` accepts, in budget order.
        assert_eq!(efforts, ["low", "medium", "high", "xhigh", "max"]);
        // No default marked, so turns omit --effort until the user picks one.
        assert!(catalog.iter().all(|m| m.default_reasoning_effort.is_none()));
    }
}
