use std::{collections::HashMap, path::PathBuf, process::Stdio, sync::Arc};

use serde_json::{Value, json};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::{Child, Command},
    sync::{Mutex, mpsc, oneshot},
};

use crate::{
    domain::{ApprovalId, CodexThreadId, PromptMode, SessionId, SessionRecord},
    error::{AppError, AppResult},
};

#[derive(Debug, Clone)]
pub struct CodexClient {
    codex_bin: String,
    additional_dirs: Vec<PathBuf>,
    runtimes: Arc<Mutex<HashMap<SessionId, Arc<LiveRuntimeHandle>>>>,
}

impl CodexClient {
    pub fn new(codex_bin: String, additional_dirs: Vec<PathBuf>) -> Self {
        Self {
            codex_bin,
            additional_dirs,
            runtimes: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub async fn run_turn<F>(
        &self,
        session: &SessionRecord,
        prompt: &str,
        mode: PromptMode,
        mut on_event: F,
    ) -> AppResult<CodexTurnResult>
    where
        F: FnMut(CodexEvent) -> AppResult<()>,
    {
        let mut runtime = AppServerRuntime::start(
            &self.codex_bin,
            &self.additional_dirs,
            session.session_id.clone(),
            &session.workspace_path.0,
        )
        .await?;
        self.runtimes
            .lock()
            .await
            .insert(session.session_id.clone(), runtime.handle());

        let run_result = async {
            runtime.initialize().await?;
            let opened_thread = runtime
                .open_thread(
                    session.provider_thread_id.as_ref(),
                    session.resume_cursor_json.as_deref(),
                    mode,
                )
                .await?;

            let mut result = CodexTurnResult {
                thread_id: opened_thread.thread_id.clone(),
                resume_cursor_json: opened_thread.resume_cursor_json.clone(),
                ..CodexTurnResult::default()
            };
            if let Some(thread_id) = opened_thread.thread_id {
                on_event(CodexEvent::ThreadStarted {
                    thread_id,
                    resume_cursor_json: opened_thread.resume_cursor_json,
                })?;
            }

            runtime.start_turn(prompt, mode).await?;

            loop {
                let Some(event) = runtime.next_event().await? else {
                    return Err(AppError::Codex(
                        "codex app-server exited before the turn completed".into(),
                    ));
                };

                match &event {
                    CodexEvent::ThreadStarted {
                        thread_id,
                        resume_cursor_json,
                    } => {
                        result.thread_id = Some(thread_id.clone());
                        result.resume_cursor_json = resume_cursor_json.clone();
                    }
                    CodexEvent::TurnCompleted => {
                        result.completed = true;
                    }
                    CodexEvent::TurnFailed { message } => {
                        result.failure = Some(message.clone());
                    }
                    _ => {}
                }

                on_event(event.clone())?;

                if result.completed || result.failure.is_some() {
                    break;
                }
            }

            Ok(result)
        }
        .await;

        self.runtimes.lock().await.remove(&session.session_id);
        let shutdown_result = runtime.shutdown().await;
        match (run_result, shutdown_result) {
            (Ok(result), Ok(())) => Ok(result),
            (Err(error), Ok(())) => Err(error),
            (Ok(_), Err(error)) => Err(error),
            (Err(run_error), Err(_shutdown_error)) => Err(run_error),
        }
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
                    "approval is stale because the live Codex runtime is no longer active".into(),
                )
            })?;
        runtime.resolve_approval(approval_id, approved).await
    }
}

#[derive(Debug, Clone, Default)]
pub struct CodexTurnResult {
    pub thread_id: Option<CodexThreadId>,
    pub resume_cursor_json: Option<String>,
    pub completed: bool,
    pub failure: Option<String>,
}

#[derive(Debug, Clone)]
pub enum CodexEvent {
    ThreadStarted {
        thread_id: CodexThreadId,
        resume_cursor_json: Option<String>,
    },
    Status {
        text: String,
    },
    Output {
        text: String,
    },
    CommandStarted {
        command: String,
    },
    CommandFinished {
        command: String,
        exit_code: i64,
        output: String,
    },
    ApprovalRequested {
        approval: CodexPendingApproval,
    },
    TurnCompleted,
    TurnFailed {
        message: String,
    },
}

#[derive(Debug, Clone)]
pub struct CodexPendingApproval {
    pub approval_id: ApprovalId,
    pub summary: String,
    pub payload: String,
}

#[derive(Debug, Clone)]
struct ThreadOpenState {
    thread_id: Option<CodexThreadId>,
    resume_cursor_json: Option<String>,
}

struct AppServerRuntime {
    child: Child,
    sender: mpsc::UnboundedSender<String>,
    receiver: mpsc::UnboundedReceiver<CodexEvent>,
    response_waiters: Arc<Mutex<HashMap<u64, oneshot::Sender<AppResult<Value>>>>>,
    handle: Arc<LiveRuntimeHandle>,
    next_request_id: u64,
    writer_task: tokio::task::JoinHandle<()>,
    stdout_task: tokio::task::JoinHandle<()>,
    stderr_task: tokio::task::JoinHandle<()>,
    workspace_path: String,
}

impl AppServerRuntime {
    async fn start(
        codex_bin: &str,
        _additional_dirs: &[PathBuf],
        session_id: SessionId,
        workspace_path: &str,
    ) -> AppResult<Self> {
        let mut command = Command::new(codex_bin);
        command
            .arg("app-server")
            .arg("--session-source")
            .arg("cli")
            .current_dir(workspace_path)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .stdin(Stdio::piped());

        let mut child = command.spawn()?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| AppError::Codex("missing stdin from codex app-server".into()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| AppError::Codex("missing stdout from codex app-server".into()))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| AppError::Codex("missing stderr from codex app-server".into()))?;

        let (sender, mut write_rx) = mpsc::unbounded_channel::<String>();
        let writer_task = tokio::spawn(async move {
            let mut stdin = stdin;
            while let Some(message) = write_rx.recv().await {
                if stdin.write_all(message.as_bytes()).await.is_err() {
                    break;
                }
                if stdin.write_all(b"\n").await.is_err() {
                    break;
                }
                if stdin.flush().await.is_err() {
                    break;
                }
            }
        });

        let (event_tx, receiver) = mpsc::unbounded_channel::<CodexEvent>();
        let response_waiters = Arc::new(Mutex::new(HashMap::<
            u64,
            oneshot::Sender<AppResult<Value>>,
        >::new()));
        let command_outputs = Arc::new(Mutex::new(HashMap::<String, String>::new()));
        let approvals = Arc::new(Mutex::new(HashMap::<ApprovalId, PendingApprovalRequest>::new()));
        let handle = Arc::new(LiveRuntimeHandle {
            approvals,
            sender: sender.clone(),
            session_id,
            current_thread_id: Mutex::new(None),
        });

        let stdout_task = tokio::spawn(read_stdout_loop(
            BufReader::new(stdout),
            event_tx,
            response_waiters.clone(),
            command_outputs,
            handle.clone(),
        ));
        let stderr_task = tokio::spawn(read_stderr_loop(BufReader::new(stderr)));

        Ok(Self {
            child,
            sender,
            receiver,
            response_waiters,
            handle,
            next_request_id: 1,
            writer_task,
            stdout_task,
            stderr_task,
            workspace_path: workspace_path.to_string(),
        })
    }

    fn handle(&self) -> Arc<LiveRuntimeHandle> {
        self.handle.clone()
    }

    async fn initialize(&mut self) -> AppResult<()> {
        self.send_request(
            "initialize",
            json!({
                "clientInfo": {
                    "name": "atlas2",
                    "title": "Atlas2",
                    "version": "0.1.0",
                },
                "capabilities": {
                    "experimentalApi": true
                }
            }),
        )
        .await?;
        self.send_notification("initialized", json!({}))?;
        Ok(())
    }

    async fn open_thread(
        &mut self,
        provider_thread_id: Option<&CodexThreadId>,
        _resume_cursor_json: Option<&str>,
        mode: PromptMode,
    ) -> AppResult<ThreadOpenState> {
        let mut params = json!({
            "cwd": self.workspace_path,
        });
        if mode == PromptMode::Plan {
            params["approvalPolicy"] = json!("on-request");
            params["sandbox"] = json!("read-only");
        }

        let result = if let Some(thread_id) = provider_thread_id {
            self.send_request(
                "thread/resume",
                merge_objects(params, json!({
                    "threadId": thread_id.0,
                })),
            )
            .await?
        } else {
            self.send_request("thread/start", params).await?
        };

        let state = ThreadOpenState {
            thread_id: extract_thread_id(&result),
            resume_cursor_json: build_resume_cursor_json(&result),
        };
        self.handle.set_thread_id(state.thread_id.clone()).await;
        Ok(state)
    }

    async fn start_turn(&mut self, prompt: &str, mode: PromptMode) -> AppResult<()> {
        let thread_id = self
            .handle
            .latest_thread_id()
            .await
            .ok_or_else(|| AppError::Codex("missing provider thread id for turn start".into()))?;

        let turn_prompt = build_codex_prompt(prompt, mode);
        let mut params = json!({
            "threadId": thread_id.0,
            "cwd": self.workspace_path,
            "input": [{
                "type": "text",
                "text": turn_prompt,
                "text_elements": [],
            }],
        });

        if mode == PromptMode::Plan {
            let sandbox_policy = json!({
                "type": "readOnly",
                "networkAccess": false,
            });
            params["approvalPolicy"] = json!("on-request");
            params["sandboxPolicy"] = sandbox_policy;
        }

        self.send_request("turn/start", params).await?;
        Ok(())
    }

    async fn next_event(&mut self) -> AppResult<Option<CodexEvent>> {
        tokio::select! {
            event = self.receiver.recv() => Ok(event),
            status = self.child.wait() => {
                let status = status?;
                if status.success() {
                    Ok(None)
                } else {
                    Err(AppError::Codex(format!("codex app-server exited with status {status}")))
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
        self.response_waiters.lock().await.clear();
        Ok(())
    }

    async fn send_request(&mut self, method: &str, params: Value) -> AppResult<Value> {
        let request_id = self.next_request_id;
        self.next_request_id += 1;

        let (tx, rx) = oneshot::channel();
        self.response_waiters.lock().await.insert(request_id, tx);
        self.sender
            .send(
                json!({
                    "id": request_id,
                    "method": method,
                    "params": params,
                })
                .to_string(),
            )
            .map_err(|_| AppError::Codex(format!("failed to send app-server request {method}")))?;

        rx.await.map_err(|_| {
            AppError::Codex(format!(
                "app-server response channel closed while waiting for {method}"
            ))
        })?
    }

    fn send_notification(&self, method: &str, params: Value) -> AppResult<()> {
        self.sender
            .send(
                json!({
                    "method": method,
                    "params": params,
                })
                .to_string(),
            )
            .map_err(|_| AppError::Codex(format!("failed to send app-server notification {method}")))
    }

}

#[derive(Debug)]
struct LiveRuntimeHandle {
    approvals: Arc<Mutex<HashMap<ApprovalId, PendingApprovalRequest>>>,
    sender: mpsc::UnboundedSender<String>,
    session_id: SessionId,
    current_thread_id: Mutex<Option<CodexThreadId>>,
}

impl LiveRuntimeHandle {
    async fn resolve_approval(&self, approval_id: &ApprovalId, approved: bool) -> AppResult<()> {
        let pending = self
            .approvals
            .lock()
            .await
            .remove(approval_id)
            .ok_or_else(|| AppError::Validation("approval request is no longer active".into()))?;
        let decision = if approved { "accept" } else { "decline" };
        self.sender
            .send(
                json!({
                    "id": pending.request_id,
                    "result": {
                        "decision": decision
                    }
                })
                .to_string(),
            )
            .map_err(|_| {
                AppError::Codex(format!(
                    "failed to send approval decision for session {}",
                    self.session_id.0
                ))
            })
    }

    async fn latest_thread_id(&self) -> Option<CodexThreadId> {
        self.current_thread_id.lock().await.clone()
    }

    async fn set_thread_id(&self, thread_id: Option<CodexThreadId>) {
        *self.current_thread_id.lock().await = thread_id;
    }
}

#[derive(Debug, Clone)]
struct PendingApprovalRequest {
    request_id: Value,
}

async fn read_stdout_loop(
    reader: BufReader<tokio::process::ChildStdout>,
    event_tx: mpsc::UnboundedSender<CodexEvent>,
    response_waiters: Arc<Mutex<HashMap<u64, oneshot::Sender<AppResult<Value>>>>>,
    command_outputs: Arc<Mutex<HashMap<String, String>>>,
    handle: Arc<LiveRuntimeHandle>,
) {
    let text_outputs = Arc::new(Mutex::new(HashMap::<String, String>::new()));
    let mut lines = reader.lines();
    while let Ok(Some(line)) = lines.next_line().await {
        if line.trim().is_empty() {
            continue;
        }

        let Ok(json) = serde_json::from_str::<Value>(&line) else {
            let _ = event_tx.send(CodexEvent::Status {
                text: format!("invalid app-server JSON: {line}"),
            });
            continue;
        };

        if handle_response(&json, &response_waiters).await {
            continue;
        }

        if let Some((request_id, method, params)) = parse_server_request(&json) {
            if handle_server_request(request_id, &method, params, &event_tx, &handle).await {
                continue;
            }
        }

        if let Some((method, params)) = parse_notification(&json) {
            if let Some(event) =
                map_notification(&method, &params, &command_outputs, &text_outputs).await
            {
                if let CodexEvent::ThreadStarted { thread_id, .. } = &event {
                    handle.set_thread_id(Some(thread_id.clone())).await;
                }
                let _ = event_tx.send(event);
            }
        }
    }
}

async fn read_stderr_loop(reader: BufReader<tokio::process::ChildStderr>) {
    let mut lines = reader.lines();
    while let Ok(Some(_line)) = lines.next_line().await {}
}

async fn handle_response(
    json: &Value,
    response_waiters: &Arc<Mutex<HashMap<u64, oneshot::Sender<AppResult<Value>>>>>,
) -> bool {
    let Some(id) = json.get("id").and_then(Value::as_u64) else {
        return false;
    };
    if json.get("method").is_some() {
        return false;
    }

    let sender = response_waiters.lock().await.remove(&id);
    if let Some(sender) = sender {
        if let Some(error) = json.get("error") {
            let message = error
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("unknown app-server error")
                .to_string();
            let _ = sender.send(Err(AppError::Codex(message)));
        } else {
            let _ = sender.send(Ok(json.get("result").cloned().unwrap_or(Value::Null)));
        }
    }
    true
}

fn parse_server_request(json: &Value) -> Option<(Value, String, Value)> {
    Some((
        json.get("id")?.clone(),
        json.get("method")?.as_str()?.to_string(),
        json.get("params").cloned().unwrap_or(Value::Null),
    ))
}

fn parse_notification(json: &Value) -> Option<(String, Value)> {
    if json.get("id").is_some() {
        return None;
    }
    Some((
        json.get("method")?.as_str()?.to_string(),
        json.get("params").cloned().unwrap_or(Value::Null),
    ))
}

async fn handle_server_request(
    request_id: Value,
    method: &str,
    params: Value,
    event_tx: &mpsc::UnboundedSender<CodexEvent>,
    handle: &Arc<LiveRuntimeHandle>,
) -> bool {
    match method {
        "item/commandExecution/requestApproval"
        | "item/fileRead/requestApproval"
        | "item/fileChange/requestApproval" => {
            let approval_id = ApprovalId::new();
            handle.approvals.lock().await.insert(
                approval_id.clone(),
                PendingApprovalRequest {
                    request_id: request_id.clone(),
                },
            );
            let _ = event_tx.send(CodexEvent::ApprovalRequested {
                approval: CodexPendingApproval {
                    approval_id,
                    summary: summarize_approval_request(method, &params),
                    payload: params.to_string(),
                },
            });
            true
        }
        "item/tool/requestUserInput" => {
            let _ = event_tx.send(CodexEvent::Status {
                text: "Codex requested interactive user input that Atlas2 does not support yet."
                    .into(),
            });
            let _ = handle.sender.send(
                json!({
                    "id": request_id,
                    "error": {
                        "code": -32601,
                        "message": "Atlas2 does not support tool user input requests yet."
                    }
                })
                .to_string(),
            );
            true
        }
        _ => {
            let _ = handle.sender.send(
                json!({
                    "id": request_id,
                    "error": {
                        "code": -32601,
                        "message": format!("Unsupported server request: {method}")
                    }
                })
                .to_string(),
            );
            true
        }
    }
}

async fn map_notification(
    method: &str,
    params: &Value,
    command_outputs: &Arc<Mutex<HashMap<String, String>>>,
    text_outputs: &Arc<Mutex<HashMap<String, String>>>,
) -> Option<CodexEvent> {
    match method {
        "thread/started" => Some(CodexEvent::ThreadStarted {
            thread_id: extract_thread_id(params)?,
            resume_cursor_json: build_resume_cursor_json(params),
        }),
        "turn/started" => Some(CodexEvent::Status {
            text: "Codex turn started".into(),
        }),
        "turn/completed" => {
            let status = params
                .get("turn")
                .and_then(|turn| turn.get("status"))
                .and_then(Value::as_str)
                .unwrap_or("completed");
            if status == "failed" || status == "cancelled" || status == "interrupted" {
                let message = params
                    .get("turn")
                    .and_then(|turn| turn.get("error"))
                    .and_then(|error| error.get("message"))
                    .and_then(Value::as_str)
                    .unwrap_or("Codex turn failed")
                    .to_string();
                Some(CodexEvent::TurnFailed { message })
            } else {
                Some(CodexEvent::TurnCompleted)
            }
        }
        "error" => Some(CodexEvent::TurnFailed {
            message: params
                .get("error")
                .and_then(|error| error.get("message"))
                .and_then(Value::as_str)
                .unwrap_or("Codex runtime error")
                .to_string(),
        }),
        "item/agentMessage/delta" => {
            let item_id = params.get("itemId")?.as_str()?.to_string();
            let delta = params.get("delta")?.as_str()?.to_string();
            let mut outputs = text_outputs.lock().await;
            outputs.entry(item_id).or_default().push_str(&delta);
            None
        }
        "item/commandExecution/outputDelta" => {
            let item_id = params.get("itemId")?.as_str()?.to_string();
            let delta = params.get("delta")?.as_str()?.to_string();
            let mut outputs = command_outputs.lock().await;
            outputs.entry(item_id).or_default().push_str(&delta);
            None
        }
        "item/started" => map_item_started(params),
        "item/completed" => map_item_completed(params, command_outputs, text_outputs).await,
        _ => None,
    }
}

fn map_item_started(params: &Value) -> Option<CodexEvent> {
    let item = params.get("item")?;
    let item_type = item.get("type")?.as_str()?;
    if item_type != "commandExecution" {
        return None;
    }
    Some(CodexEvent::CommandStarted {
        command: item
            .get("command")
            .and_then(Value::as_str)
            .unwrap_or("command")
            .to_string(),
    })
}

async fn map_item_completed(
    params: &Value,
    command_outputs: &Arc<Mutex<HashMap<String, String>>>,
    text_outputs: &Arc<Mutex<HashMap<String, String>>>,
) -> Option<CodexEvent> {
    let item = params.get("item")?;
    let item_type = item.get("type")?.as_str()?;
    match item_type {
        "agentMessage" => {
            let item_id = item.get("id").and_then(Value::as_str).unwrap_or_default();
            let buffered = text_outputs.lock().await.remove(item_id).unwrap_or_default();
            let text = if buffered.is_empty() {
                item.get("text")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string()
            } else {
                buffered
            };
            if text.is_empty() {
                None
            } else {
                Some(CodexEvent::Output { text })
            }
        }
        "commandExecution" => {
            let item_id = item.get("id").and_then(Value::as_str).unwrap_or_default();
            let buffered_output = command_outputs
                .lock()
                .await
                .remove(item_id)
                .unwrap_or_default();
            let output = item
                .get("aggregatedOutput")
                .and_then(Value::as_str)
                .map(str::to_string)
                .filter(|value| !value.is_empty())
                .unwrap_or(buffered_output);
            let exit_code = item
                .get("exitCode")
                .and_then(Value::as_i64)
                .or_else(|| item.get("exit_code").and_then(Value::as_i64))
                .unwrap_or_else(|| match item.get("status").and_then(Value::as_str) {
                    Some("completed") => 0,
                    Some("failed") | Some("declined") => 1,
                    _ => -1,
                });
            Some(CodexEvent::CommandFinished {
                command: item
                    .get("command")
                    .and_then(Value::as_str)
                    .unwrap_or("command")
                    .to_string(),
                exit_code,
                output,
            })
        }
        _ => None,
    }
}

fn summarize_approval_request(method: &str, params: &Value) -> String {
    match method {
        "item/commandExecution/requestApproval" => {
            let command = params
                .get("command")
                .and_then(Value::as_str)
                .unwrap_or("command");
            let reason = params
                .get("reason")
                .and_then(Value::as_str)
                .unwrap_or("Codex requested approval to run a command.");
            format!("{reason}\n`{command}`")
        }
        "item/fileRead/requestApproval" => params
            .get("reason")
            .and_then(Value::as_str)
            .unwrap_or("Codex requested approval for additional file reads.")
            .to_string(),
        "item/fileChange/requestApproval" => params
            .get("reason")
            .and_then(Value::as_str)
            .unwrap_or("Codex requested approval to change files.")
            .to_string(),
        _ => "Codex requested approval.".into(),
    }
}

fn extract_thread_id(value: &Value) -> Option<CodexThreadId> {
    value.get("threadId")
        .and_then(Value::as_str)
        .or_else(|| {
            value.get("thread")
                .and_then(|thread| thread.get("id"))
                .and_then(Value::as_str)
        })
        .map(|id| CodexThreadId(id.to_string()))
}

fn build_resume_cursor_json(value: &Value) -> Option<String> {
    extract_thread_id(value).map(|thread_id| {
        json!({
            "threadId": thread_id.0
        })
        .to_string()
    })
}

fn merge_objects(base: Value, overlay: Value) -> Value {
    let mut merged = base.as_object().cloned().unwrap_or_default();
    for (key, value) in overlay.as_object().cloned().unwrap_or_default() {
        merged.insert(key, value);
    }
    Value::Object(merged)
}

fn build_codex_prompt(prompt: &str, mode: PromptMode) -> String {
    match mode {
        PromptMode::Normal => prompt.to_string(),
        PromptMode::Plan => format!(
            concat!(
                "You are in Atlas2 plan mode.\n",
                "Analyze the request and return a concrete implementation plan only.\n",
                "Do not modify files, do not apply patches, and do not run write operations.\n",
                "You may inspect the codebase as needed.\n\n",
                "User request:\n{}"
            ),
            prompt
        ),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use tokio::sync::Mutex;

    use std::{collections::HashMap, sync::Arc};

    use super::{
        CodexEvent, build_resume_cursor_json, extract_thread_id, map_item_completed,
        summarize_approval_request,
    };

    #[test]
    fn extracts_thread_id_from_thread_notifications() {
        let thread_id = extract_thread_id(&json!({
            "thread": {"id": "thread_123"}
        }))
        .unwrap();
        assert_eq!(thread_id.0, "thread_123");
    }

    #[test]
    fn builds_resume_cursor_json_from_thread_state() {
        let cursor = build_resume_cursor_json(&json!({
            "thread": {"id": "thread_123"}
        }))
        .unwrap();
        assert_eq!(cursor, r#"{"threadId":"thread_123"}"#);
    }

    #[test]
    fn summarizes_command_approval_requests() {
        let summary = summarize_approval_request(
            "item/commandExecution/requestApproval",
            &json!({
                "command": "cargo test",
                "reason": "Need to run tests"
            }),
        );
        assert!(summary.contains("Need to run tests"));
        assert!(summary.contains("cargo test"));
    }

    #[tokio::test]
    async fn emits_agent_message_output_on_camel_case_item_completion() {
        let command_outputs = Arc::new(Mutex::new(HashMap::new()));
        let text_outputs = Arc::new(Mutex::new(HashMap::from([(
            "item_1".to_string(),
            "hello from codex".to_string(),
        )])));

        let event = map_item_completed(
            &json!({
                "item": {
                    "id": "item_1",
                    "type": "agentMessage"
                }
            }),
            &command_outputs,
            &text_outputs,
        )
        .await
        .expect("agent message output event");

        match event {
            CodexEvent::Output { text } => assert_eq!(text, "hello from codex"),
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[tokio::test]
    async fn uses_aggregated_command_output_from_completed_item() {
        let command_outputs = Arc::new(Mutex::new(HashMap::new()));
        let text_outputs = Arc::new(Mutex::new(HashMap::new()));

        let event = map_item_completed(
            &json!({
                "item": {
                    "id": "cmd_1",
                    "type": "commandExecution",
                    "command": "pwd",
                    "status": "completed",
                    "aggregatedOutput": "/tmp/project\n"
                }
            }),
            &command_outputs,
            &text_outputs,
        )
        .await
        .expect("command completion event");

        match event {
            CodexEvent::CommandFinished {
                command,
                exit_code,
                output,
            } => {
                assert_eq!(command, "pwd");
                assert_eq!(exit_code, 0);
                assert_eq!(output, "/tmp/project\n");
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }
}
