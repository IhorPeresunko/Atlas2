use std::{
    collections::HashMap,
    path::PathBuf,
    process::Stdio,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use regex::Regex;
use serde_json::{Value, json};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::{Child, Command},
    sync::{Mutex, mpsc, oneshot},
};

use crate::{
    domain::{
        ApprovalId, CodexThreadId, PromptMode, SessionId, SessionRecord, UserInputAnswer,
        UserInputQuestion, UserInputRequestId,
    },
    error::{AppError, AppResult},
};

const STALE_THREAD_RECOVERY_MESSAGE: &str = "Stored Codex conversation could not be resumed; Atlas2 started a fresh thread without prior conversation context.";
const CODEX_PLAN_MODE_DEVELOPER_INSTRUCTIONS: &str = concat!(
    "<collaboration_mode># Plan Mode\n\n",
    "You are in Plan Mode until a developer message explicitly ends it.\n",
    "If the user asks for implementation while still in Plan Mode, treat it as a request to plan the implementation, not execute it.\n\n",
    "Rules:\n",
    "- Explore first. Prefer reading the codebase and running non-mutating checks before asking questions.\n",
    "- Do not edit files or perform mutating actions.\n",
    "- Ask follow-up questions when they materially change the plan or confirm an important assumption.\n",
    "- Strongly prefer the `request_user_input` tool for meaningful multiple-choice decisions.\n",
    "- Present the final plan only when it is decision-complete.\n",
    "- Wrap the final plan in a `<proposed_plan>` block on its own lines.\n",
    "- Do not ask whether to proceed after the final plan.\n",
    "</collaboration_mode>",
);
const CODEX_DEFAULT_MODE_DEVELOPER_INSTRUCTIONS: &str = concat!(
    "<collaboration_mode># Collaboration Mode: Default\n\n",
    "You are in Default mode.\n",
    "Make reasonable assumptions and execute the task instead of stopping to ask questions unless a risky ambiguity remains.\n",
    "The `request_user_input` tool is unavailable in Default mode.\n",
    "</collaboration_mode>",
);

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
        model: Option<&str>,
        reasoning_effort: Option<&str>,
        mut on_event: F,
    ) -> AppResult<CodexTurnResult>
    where
        F: FnMut(CodexEvent) -> AppResult<()>,
    {
        let mut resume_thread_id = session.provider_thread_id.clone();
        let mut retry_with_fresh_thread_available = session.provider_thread_id.is_some();

        loop {
            let mut runtime = AppServerRuntime::start(
                &self.codex_bin,
                &self.additional_dirs,
                session.session_id.clone(),
                &session.workspace_path.0,
                model.map(str::to_string),
                reasoning_effort.map(str::to_string),
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
                        resume_thread_id.as_ref(),
                        session.resume_cursor_json.as_deref(),
                        mode,
                    )
                    .await?;

                if let Some(message) = opened_thread.recovery_message.as_ref() {
                    on_event(CodexEvent::Status {
                        text: message.clone(),
                    })?;
                }

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

                if let Err(error) = runtime.start_turn(prompt, mode).await {
                    if should_retry_with_fresh_thread_after_error(
                        resume_thread_id.as_ref(),
                        retry_with_fresh_thread_available,
                        &error,
                    ) {
                        on_event(CodexEvent::Status {
                            text: STALE_THREAD_RECOVERY_MESSAGE.into(),
                        })?;
                        return Ok(CodexRunOutcome::RetryFreshThread);
                    }
                    return Err(error);
                }

                let mut saw_material_turn_activity = false;

                loop {
                    let Some(event) = runtime.next_event().await? else {
                        return Err(AppError::Codex(
                            "codex app-server exited before the turn completed".into(),
                        ));
                    };

                    if let CodexEvent::TurnFailed { message } = &event {
                        if should_retry_with_fresh_thread_after_failure(
                            resume_thread_id.as_ref(),
                            retry_with_fresh_thread_available,
                            message,
                            saw_material_turn_activity,
                        ) {
                            on_event(CodexEvent::Status {
                                text: STALE_THREAD_RECOVERY_MESSAGE.into(),
                            })?;
                            return Ok(CodexRunOutcome::RetryFreshThread);
                        }
                    }

                    match &event {
                        CodexEvent::ThreadStarted {
                            thread_id,
                            resume_cursor_json,
                        } => {
                            result.thread_id = Some(thread_id.clone());
                            result.resume_cursor_json = resume_cursor_json.clone();
                        }
                        CodexEvent::Output { .. }
                        | CodexEvent::CommandStarted { .. }
                        | CodexEvent::CommandFinished { .. }
                        | CodexEvent::ApprovalRequested { .. }
                        | CodexEvent::UserInputRequested { .. }
                        | CodexEvent::PlanCompleted { .. } => {
                            saw_material_turn_activity = true;
                        }
                        CodexEvent::TurnCompleted => {
                            result.completed = true;
                        }
                        CodexEvent::TurnInterrupted { .. } => {
                            result.interrupted = true;
                        }
                        CodexEvent::TurnFailed { message } => {
                            result.failure = Some(message.clone());
                        }
                        CodexEvent::Status { .. } => {}
                    }

                    on_event(event.clone())?;

                    if result.completed || result.interrupted || result.failure.is_some() {
                        break;
                    }
                }

                Ok(CodexRunOutcome::Finished(result))
            }
            .await;

            self.runtimes.lock().await.remove(&session.session_id);
            let shutdown_result = runtime.shutdown().await;
            let outcome = match (run_result, shutdown_result) {
                (Ok(outcome), Ok(())) => Ok(outcome),
                (Err(error), Ok(())) => Err(error),
                (Ok(_), Err(error)) => Err(error),
                (Err(run_error), Err(_shutdown_error)) => Err(run_error),
            }?;

            match outcome {
                CodexRunOutcome::Finished(result) => return Ok(result),
                CodexRunOutcome::RetryFreshThread => {
                    retry_with_fresh_thread_available = false;
                    resume_thread_id = None;
                }
            }
        }
    }

    /// Ask Codex core which models are available, forwarding its own catalog
    /// rather than maintaining a hardcoded list. Spins up a short-lived
    /// app-server purely to issue the `model/list` request.
    pub async fn list_models(&self, workspace_path: &str) -> AppResult<Vec<ModelOption>> {
        let mut runtime = AppServerRuntime::start(
            &self.codex_bin,
            &self.additional_dirs,
            SessionId::new(),
            workspace_path,
            None,
            None,
        )
        .await?;

        let result = async {
            runtime.initialize().await?;

            let mut models = Vec::new();
            let mut cursor: Option<String> = None;
            loop {
                let mut params = json!({ "includeHidden": false });
                if let Some(cursor) = cursor.as_deref() {
                    params["cursor"] = json!(cursor);
                }
                let response = runtime.send_request("model/list", params).await?;
                if let Some(data) = response.get("data").and_then(Value::as_array) {
                    for entry in data {
                        let Some(model) = entry
                            .get("model")
                            .and_then(Value::as_str)
                            .or_else(|| entry.get("id").and_then(Value::as_str))
                        else {
                            continue;
                        };
                        let display_name = entry
                            .get("displayName")
                            .and_then(Value::as_str)
                            .unwrap_or(model)
                            .to_string();
                        let supported_reasoning_efforts = entry
                            .get("supportedReasoningEfforts")
                            .and_then(Value::as_array)
                            .map(|efforts| {
                                efforts
                                    .iter()
                                    .filter_map(|effort| {
                                        let slug = effort
                                            .get("reasoningEffort")
                                            .and_then(Value::as_str)?;
                                        Some(ReasoningEffortOption {
                                            effort: slug.to_string(),
                                            description: effort
                                                .get("description")
                                                .and_then(Value::as_str)
                                                .unwrap_or_default()
                                                .to_string(),
                                        })
                                    })
                                    .collect()
                            })
                            .unwrap_or_default();
                        models.push(ModelOption {
                            model: model.to_string(),
                            display_name,
                            is_default: entry
                                .get("isDefault")
                                .and_then(Value::as_bool)
                                .unwrap_or(false),
                            default_reasoning_effort: entry
                                .get("defaultReasoningEffort")
                                .and_then(Value::as_str)
                                .map(str::to_string),
                            supported_reasoning_efforts,
                        });
                    }
                }
                match response.get("nextCursor").and_then(Value::as_str) {
                    Some(next) if !next.is_empty() => cursor = Some(next.to_string()),
                    _ => break,
                }
            }
            Ok::<_, AppError>(models)
        }
        .await;

        let _ = runtime.shutdown().await;
        result
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

    pub async fn resolve_user_input(
        &self,
        session_id: &SessionId,
        request_id: &UserInputRequestId,
        answers: HashMap<String, UserInputAnswer>,
    ) -> AppResult<()> {
        let runtime = self
            .runtimes
            .lock()
            .await
            .get(session_id)
            .cloned()
            .ok_or_else(|| {
                AppError::Validation(
                    "user input request is stale because the live Codex runtime is no longer active"
                        .into(),
                )
            })?;
        runtime.resolve_user_input(request_id, answers).await
    }

    pub async fn stop_turn(&self, session_id: &SessionId) -> AppResult<()> {
        let runtime = self
            .runtimes
            .lock()
            .await
            .get(session_id)
            .cloned()
            .ok_or_else(|| AppError::Validation("Codex turn is no longer running".into()))?;
        runtime.interrupt_turn().await
    }
}

/// Seam over the Codex app-server adapter so business logic in `services` can be
/// exercised against a fake without spawning a real `codex` process. The boxed
/// callback (rather than a generic closure) keeps the trait object-friendly and
/// lets the async future stay `Send`.
#[async_trait::async_trait]
pub(crate) trait CodexApi: Clone + Send + Sync {
    async fn list_models(&self, workspace_path: &str) -> AppResult<Vec<ModelOption>>;

    async fn run_turn(
        &self,
        session: &SessionRecord,
        prompt: &str,
        mode: PromptMode,
        model: Option<&str>,
        reasoning_effort: Option<&str>,
        on_event: Box<dyn FnMut(CodexEvent) -> AppResult<()> + Send>,
    ) -> AppResult<CodexTurnResult>;

    async fn resolve_approval(
        &self,
        session_id: &SessionId,
        approval_id: &ApprovalId,
        approved: bool,
    ) -> AppResult<()>;

    async fn resolve_user_input(
        &self,
        session_id: &SessionId,
        request_id: &UserInputRequestId,
        answers: HashMap<String, UserInputAnswer>,
    ) -> AppResult<()>;

    async fn stop_turn(&self, session_id: &SessionId) -> AppResult<()>;
}

#[async_trait::async_trait]
impl CodexApi for CodexClient {
    async fn list_models(&self, workspace_path: &str) -> AppResult<Vec<ModelOption>> {
        CodexClient::list_models(self, workspace_path).await
    }

    async fn run_turn(
        &self,
        session: &SessionRecord,
        prompt: &str,
        mode: PromptMode,
        model: Option<&str>,
        reasoning_effort: Option<&str>,
        on_event: Box<dyn FnMut(CodexEvent) -> AppResult<()> + Send>,
    ) -> AppResult<CodexTurnResult> {
        CodexClient::run_turn(
            self,
            session,
            prompt,
            mode,
            model,
            reasoning_effort,
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
        CodexClient::resolve_approval(self, session_id, approval_id, approved).await
    }

    async fn resolve_user_input(
        &self,
        session_id: &SessionId,
        request_id: &UserInputRequestId,
        answers: HashMap<String, UserInputAnswer>,
    ) -> AppResult<()> {
        CodexClient::resolve_user_input(self, session_id, request_id, answers).await
    }

    async fn stop_turn(&self, session_id: &SessionId) -> AppResult<()> {
        CodexClient::stop_turn(self, session_id).await
    }
}

#[derive(Debug, Clone, Default)]
pub struct CodexTurnResult {
    pub thread_id: Option<CodexThreadId>,
    pub resume_cursor_json: Option<String>,
    pub completed: bool,
    pub interrupted: bool,
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
    UserInputRequested {
        request: CodexPendingUserInput,
    },
    PlanCompleted {
        markdown: String,
    },
    TurnCompleted,
    TurnInterrupted {
        message: String,
    },
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
pub struct CodexPendingUserInput {
    pub request_id: UserInputRequestId,
    pub questions: Vec<UserInputQuestion>,
}

enum CodexRunOutcome {
    Finished(CodexTurnResult),
    RetryFreshThread,
}

#[derive(Debug, Clone)]
struct ThreadOpenState {
    thread_id: Option<CodexThreadId>,
    resume_cursor_json: Option<String>,
    recovery_message: Option<String>,
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
    model: Option<String>,
    reasoning_effort: Option<String>,
}

/// A model entry as reported by Codex core via the `model/list` request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelOption {
    /// Slug forwarded back to Codex (e.g. `gpt-5.5`).
    pub model: String,
    /// Human-friendly label for the Telegram picker.
    pub display_name: String,
    /// Whether Codex considers this its default model.
    pub is_default: bool,
    /// Reasoning effort Codex applies for this model when none is chosen.
    pub default_reasoning_effort: Option<String>,
    /// Reasoning efforts this model accepts, in the order Codex advertises them.
    pub supported_reasoning_efforts: Vec<ReasoningEffortOption>,
}

/// A reasoning effort level advertised by a model (e.g. `low`, `high`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReasoningEffortOption {
    /// Slug forwarded back to Codex (e.g. `high`).
    pub effort: String,
    /// Human-friendly description for the Telegram picker.
    pub description: String,
}

impl AppServerRuntime {
    async fn start(
        codex_bin: &str,
        _additional_dirs: &[PathBuf],
        session_id: SessionId,
        workspace_path: &str,
        model: Option<String>,
        reasoning_effort: Option<String>,
    ) -> AppResult<Self> {
        let mut command = Command::new(codex_bin);
        command
            .arg("app-server")
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
        let approvals = Arc::new(Mutex::new(
            HashMap::<ApprovalId, PendingApprovalRequest>::new(),
        ));
        let user_inputs = Arc::new(Mutex::new(HashMap::<
            UserInputRequestId,
            PendingUserInputRequest,
        >::new()));
        let handle = Arc::new(LiveRuntimeHandle {
            approvals,
            user_inputs,
            sender: sender.clone(),
            session_id,
            current_thread_id: Mutex::new(None),
            current_turn_id: Mutex::new(None),
            next_request_id: AtomicU64::new(1_000_000),
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
            model,
            reasoning_effort,
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

        let start_params = params.clone();
        let (result, recovery_message) = if let Some(thread_id) = provider_thread_id {
            let resume_params = merge_objects(
                params,
                json!({
                    "threadId": thread_id.0,
                }),
            );
            match self.send_request("thread/resume", resume_params).await {
                Ok(result) => (result, None),
                Err(error) if should_restart_thread_from_resume_error(&error) => {
                    tracing::warn!(
                        thread_id = %thread_id.0,
                        error = %error,
                        "stored Codex thread could not be resumed; starting a fresh thread"
                    );
                    (
                        self.send_request("thread/start", start_params).await?,
                        Some(STALE_THREAD_RECOVERY_MESSAGE.into()),
                    )
                }
                Err(error) => return Err(error),
            }
        } else {
            (self.send_request("thread/start", start_params).await?, None)
        };

        let state = ThreadOpenState {
            thread_id: extract_thread_id(&result),
            resume_cursor_json: build_resume_cursor_json(&result),
            recovery_message,
        };
        self.handle.set_thread_id(state.thread_id.clone()).await;
        Ok(state)
    }

    async fn start_turn(&mut self, prompt: &str, mode: PromptMode) -> AppResult<()> {
        let thread_id =
            self.handle.latest_thread_id().await.ok_or_else(|| {
                AppError::Codex("missing provider thread id for turn start".into())
            })?;

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
        params["collaborationMode"] = build_collaboration_mode(
            mode,
            self.model.as_deref(),
            self.reasoning_effort.as_deref(),
        );
        // Only override the model when a selection has been made; otherwise let
        // Codex core fall back to its own configured default (~/.codex/config.toml).
        if let Some(model) = self.model.as_deref() {
            params["model"] = json!(model);
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
            .map_err(|_| {
                AppError::Codex(format!("failed to send app-server notification {method}"))
            })
    }
}

#[derive(Debug)]
struct LiveRuntimeHandle {
    approvals: Arc<Mutex<HashMap<ApprovalId, PendingApprovalRequest>>>,
    user_inputs: Arc<Mutex<HashMap<UserInputRequestId, PendingUserInputRequest>>>,
    sender: mpsc::UnboundedSender<String>,
    session_id: SessionId,
    current_thread_id: Mutex<Option<CodexThreadId>>,
    current_turn_id: Mutex<Option<String>>,
    next_request_id: AtomicU64,
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

    async fn resolve_user_input(
        &self,
        request_id: &UserInputRequestId,
        answers: HashMap<String, UserInputAnswer>,
    ) -> AppResult<()> {
        let pending = self
            .user_inputs
            .lock()
            .await
            .remove(request_id)
            .ok_or_else(|| AppError::Validation("user input request is no longer active".into()))?;
        self.sender
            .send(
                json!({
                    "id": pending.request_id,
                    "result": {
                        "answers": answers
                    }
                })
                .to_string(),
            )
            .map_err(|_| {
                AppError::Codex(format!(
                    "failed to send user input response for session {}",
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

    async fn latest_turn_id(&self) -> Option<String> {
        self.current_turn_id.lock().await.clone()
    }

    async fn set_turn_id(&self, turn_id: Option<String>) {
        *self.current_turn_id.lock().await = turn_id;
    }

    async fn interrupt_turn(&self) -> AppResult<()> {
        let thread_id = self.latest_thread_id().await.ok_or_else(|| {
            AppError::Validation("Codex turn is not ready to be interrupted yet".into())
        })?;
        let turn_id = self.latest_turn_id().await.ok_or_else(|| {
            AppError::Validation("Codex turn is not ready to be interrupted yet".into())
        })?;
        let request_id = self.next_request_id.fetch_add(1, Ordering::Relaxed);
        self.sender
            .send(
                json!({
                    "id": request_id,
                    "method": "turn/interrupt",
                    "params": {
                        "threadId": thread_id.0,
                        "turnId": turn_id,
                    }
                })
                .to_string(),
            )
            .map_err(|_| {
                AppError::Codex(format!(
                    "failed to interrupt Codex turn for session {}",
                    self.session_id.0
                ))
            })
    }
}

#[derive(Debug, Clone)]
struct PendingApprovalRequest {
    request_id: Value,
}

#[derive(Debug, Clone)]
struct PendingUserInputRequest {
    request_id: Value,
}

#[derive(Debug, serde::Deserialize)]
struct ToolRequestUserInputParams {
    questions: Vec<UserInputQuestion>,
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
                match &event {
                    CodexEvent::ThreadStarted { thread_id, .. } => {
                        handle.set_thread_id(Some(thread_id.clone())).await;
                    }
                    CodexEvent::Status { .. } if method == "turn/started" => {
                        handle.set_turn_id(extract_turn_id(&params)).await;
                    }
                    CodexEvent::TurnCompleted
                    | CodexEvent::TurnInterrupted { .. }
                    | CodexEvent::TurnFailed { .. } => {
                        handle.set_turn_id(None).await;
                    }
                    _ => {}
                }
                let _ = event_tx.send(event);
            }
        }
    }

    let mut waiters = response_waiters.lock().await;
    for (_id, sender) in waiters.drain() {
        let _ = sender.send(Err(AppError::Codex(
            "codex app-server closed stdout before replying".into(),
        )));
    }
}

async fn read_stderr_loop(reader: BufReader<tokio::process::ChildStderr>) {
    let mut lines = reader.lines();
    while let Ok(Some(line)) = lines.next_line().await {
        tracing::warn!(stderr = line, "codex app-server stderr");
    }
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
            let parsed = match serde_json::from_value::<ToolRequestUserInputParams>(params) {
                Ok(parsed) => parsed,
                Err(error) => {
                    let _ = event_tx.send(CodexEvent::Status {
                        text: format!(
                            "Codex sent an invalid interactive user input request: {error}"
                        ),
                    });
                    let _ = handle.sender.send(
                        json!({
                            "id": request_id,
                            "error": {
                                "code": -32602,
                                "message": "Atlas2 could not parse the request_user_input payload."
                            }
                        })
                        .to_string(),
                    );
                    return true;
                }
            };

            if !supports_telegram_user_input_questions(&parsed.questions) {
                let _ = event_tx.send(CodexEvent::Status {
                    text: "Codex requested interactive user input that Atlas2 cannot render as Telegram buttons."
                        .into(),
                });
                let _ = handle.sender.send(
                    json!({
                        "id": request_id,
                        "error": {
                            "code": -32601,
                            "message": "Atlas2 only supports option-based request_user_input prompts in Telegram."
                        }
                    })
                    .to_string(),
                );
                return true;
            }

            let user_input_id = UserInputRequestId::new();
            handle.user_inputs.lock().await.insert(
                user_input_id.clone(),
                PendingUserInputRequest {
                    request_id: request_id.clone(),
                },
            );
            let _ = event_tx.send(CodexEvent::UserInputRequested {
                request: CodexPendingUserInput {
                    request_id: user_input_id,
                    questions: parsed.questions,
                },
            });
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
            if status == "interrupted" || status == "cancelled" {
                let message = params
                    .get("turn")
                    .and_then(|turn| turn.get("error"))
                    .and_then(|error| error.get("message"))
                    .and_then(Value::as_str)
                    .unwrap_or("Codex turn interrupted")
                    .to_string();
                Some(CodexEvent::TurnInterrupted { message })
            } else if status == "failed" {
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
        "codex/event/task_complete" => map_task_complete_notification(params),
        "item/plan/delta" | "turn/plan/updated" => None,
        "item/started" => map_item_started(params),
        "item/completed" => map_item_completed(params, command_outputs, text_outputs).await,
        _ => None,
    }
}

fn map_task_complete_notification(params: &Value) -> Option<CodexEvent> {
    let message = params
        .get("msg")
        .and_then(|msg| msg.get("last_agent_message"))
        .and_then(Value::as_str)?;
    extract_proposed_plan_markdown(message).map(|markdown| CodexEvent::PlanCompleted { markdown })
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
            let buffered = text_outputs
                .lock()
                .await
                .remove(item_id)
                .unwrap_or_default();
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
            } else if let Some(markdown) = extract_proposed_plan_markdown(&text) {
                Some(CodexEvent::PlanCompleted { markdown })
            } else {
                Some(CodexEvent::Output { text })
            }
        }
        "plan" | "Plan" => {
            let markdown = item
                .get("text")
                .and_then(Value::as_str)
                .or_else(|| item.get("content").and_then(Value::as_str))
                .unwrap_or_default()
                .trim()
                .to_string();
            if markdown.is_empty() {
                None
            } else {
                Some(CodexEvent::PlanCompleted { markdown })
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

fn extract_proposed_plan_markdown(text: &str) -> Option<String> {
    let re = Regex::new(r"(?s)<proposed_plan>\s*(.*?)\s*</proposed_plan>")
        .expect("valid proposed plan regex");
    let captures = re.captures(text)?;
    let markdown = captures.get(1)?.as_str().trim();
    if markdown.is_empty() {
        None
    } else {
        Some(markdown.to_string())
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

fn supports_telegram_user_input_questions(questions: &[UserInputQuestion]) -> bool {
    !questions.is_empty()
        && questions.iter().all(|question| {
            question
                .options
                .as_ref()
                .map(|options| !options.is_empty())
                .unwrap_or(false)
        })
}

fn extract_thread_id(value: &Value) -> Option<CodexThreadId> {
    value
        .get("threadId")
        .and_then(Value::as_str)
        .or_else(|| {
            value
                .get("thread")
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

fn extract_turn_id(value: &Value) -> Option<String> {
    value
        .get("turnId")
        .and_then(Value::as_str)
        .or_else(|| {
            value
                .get("turn")
                .and_then(|turn| turn.get("id"))
                .and_then(Value::as_str)
        })
        .map(str::to_string)
}

fn is_stale_thread_error_message(message: &str) -> bool {
    let message = message.to_ascii_lowercase();
    message.contains("invalid_encrypted_content")
        // A thread id whose rollout was never persisted (e.g. a prior turn
        // failed before completing). Resuming it is hopeless; start fresh.
        || message.contains("no rollout found")
        || (message.contains("encrypted content")
            && message.contains("could not")
            && (message.contains("decrypted")
                || message.contains("parsed")
                || message.contains("verified")))
}

fn should_restart_thread_from_resume_error(error: &AppError) -> bool {
    let AppError::Codex(message) = error else {
        return false;
    };
    is_stale_thread_error_message(message)
}

fn should_retry_with_fresh_thread_after_error(
    provider_thread_id: Option<&CodexThreadId>,
    retry_with_fresh_thread_available: bool,
    error: &AppError,
) -> bool {
    provider_thread_id.is_some()
        && retry_with_fresh_thread_available
        && should_restart_thread_from_resume_error(error)
}

fn should_retry_with_fresh_thread_after_failure(
    provider_thread_id: Option<&CodexThreadId>,
    retry_with_fresh_thread_available: bool,
    failure_message: &str,
    saw_material_turn_activity: bool,
) -> bool {
    provider_thread_id.is_some()
        && retry_with_fresh_thread_available
        && !saw_material_turn_activity
        && is_stale_thread_error_message(failure_message)
}

fn merge_objects(base: Value, overlay: Value) -> Value {
    let mut merged = base.as_object().cloned().unwrap_or_default();
    for (key, value) in overlay.as_object().cloned().unwrap_or_default() {
        merged.insert(key, value);
    }
    Value::Object(merged)
}

fn build_collaboration_mode(
    mode: PromptMode,
    model: Option<&str>,
    reasoning_effort: Option<&str>,
) -> Value {
    let (mode_name, developer_instructions) = match mode {
        PromptMode::Normal => ("default", CODEX_DEFAULT_MODE_DEVELOPER_INSTRUCTIONS),
        PromptMode::Plan => ("plan", CODEX_PLAN_MODE_DEVELOPER_INSTRUCTIONS),
    };
    let mut settings = json!({
        "developer_instructions": developer_instructions,
    });
    // Forward the chosen model/effort to core; omit when unset so core uses its
    // own defaults.
    if let Some(model) = model {
        settings["model"] = json!(model);
    }
    if let Some(reasoning_effort) = reasoning_effort {
        settings["reasoning_effort"] = json!(reasoning_effort);
    }
    json!({
        "mode": mode_name,
        "settings": settings,
    })
}

pub(crate) fn build_codex_prompt(prompt: &str, mode: PromptMode) -> String {
    match mode {
        PromptMode::Normal => prompt.to_string(),
        PromptMode::Plan => format!(
            concat!(
                "You are in Atlas2 plan mode.\n",
                "Analyze the request and return a concrete implementation plan only.\n",
                "Do not modify files, do not apply patches, and do not run write operations.\n",
                "You may inspect the codebase and run non-mutating commands as needed.\n",
                "\n",
                "Plan mode rules:\n",
                "- Stay in plan mode until a developer message explicitly ends it.\n",
                "- If the user asks you to implement while still in plan mode, treat it as a request to plan the implementation.\n",
                "- Ask follow-up questions when they materially change the plan or confirm an important assumption.\n",
                "- Strongly prefer the request_user_input tool for those follow-up questions whenever the choice can be expressed with meaningful options.\n",
                "- Prefer exploring the repository before asking questions that can be answered from local context.\n",
                "\n",
                "Finalization rules:\n",
                "- Only present the final plan when it is decision-complete and leaves no important decisions to the implementer.\n",
                "- When you present the official plan, wrap it in a <proposed_plan> block exactly like this:\n",
                "<proposed_plan>\n",
                "# Plan title\n",
                "...\n",
                "</proposed_plan>\n",
                "- The opening and closing tags must each be on their own line.\n",
                "- Use Markdown inside the block.\n",
                "- Output at most one <proposed_plan> block per turn.\n",
                "- Do not ask \"should I proceed?\" after the final plan.\n",
                "\n",
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
        CODEX_PLAN_MODE_DEVELOPER_INSTRUCTIONS, CodexEvent, ToolRequestUserInputParams,
        build_collaboration_mode, build_resume_cursor_json, extract_proposed_plan_markdown,
        extract_thread_id, extract_turn_id, is_stale_thread_error_message, map_item_completed,
        map_notification, map_task_complete_notification, should_restart_thread_from_resume_error,
        should_retry_with_fresh_thread_after_failure, summarize_approval_request,
        supports_telegram_user_input_questions,
    };
    use crate::domain::{PromptMode, UserInputOption, UserInputQuestion};
    use crate::error::AppError;

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
    fn retries_with_fresh_thread_after_invalid_encrypted_content_resume_error() {
        let error = AppError::Codex(
            "The encrypted content gAAA... could not be verified. Reason: Encrypted content could not be decrypted or parsed. code=invalid_encrypted_content"
                .into(),
        );

        assert!(should_restart_thread_from_resume_error(&error));
    }

    #[test]
    fn does_not_restart_thread_for_unrelated_resume_errors() {
        let error = AppError::Codex("thread/resume failed with rate limit".into());

        assert!(!should_restart_thread_from_resume_error(&error));
    }

    #[test]
    fn detects_stale_thread_error_from_turn_failure_message() {
        assert!(is_stale_thread_error_message(
            "The encrypted content gAAA... could not be verified. Reason: Encrypted content could not be decrypted or parsed."
        ));
    }

    #[test]
    fn restarts_thread_when_rollout_is_missing() {
        // A thread id was persisted but its rollout never was (e.g. the first
        // turn failed before completing); resuming it must start fresh.
        let error = AppError::Codex(
            "no rollout found for thread id 019ed606-6a50-7f41-974d-54a98d9580aa".into(),
        );
        assert!(should_restart_thread_from_resume_error(&error));
    }

    #[test]
    fn retries_with_fresh_thread_after_early_stale_thread_failure() {
        assert!(should_retry_with_fresh_thread_after_failure(
            Some(&crate::domain::CodexThreadId("thread_123".into())),
            true,
            "invalid_encrypted_content",
            false,
        ));
    }

    #[test]
    fn does_not_retry_with_fresh_thread_after_material_turn_activity() {
        assert!(!should_retry_with_fresh_thread_after_failure(
            Some(&crate::domain::CodexThreadId("thread_123".into())),
            true,
            "invalid_encrypted_content",
            true,
        ));
    }

    #[test]
    fn extracts_turn_id_from_turn_notifications() {
        let turn_id = extract_turn_id(&json!({
            "turn": {"id": "turn_123"}
        }))
        .unwrap();
        assert_eq!(turn_id, "turn_123");
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

    #[test]
    fn accepts_option_based_user_input_questions_for_telegram() {
        assert!(supports_telegram_user_input_questions(&[
            UserInputQuestion {
                id: "next_step".into(),
                header: "Plan".into(),
                question: "What next?".into(),
                is_other: false,
                is_secret: false,
                options: Some(vec![UserInputOption {
                    label: "Implement".into(),
                    description: "Start implementation".into(),
                }]),
            }
        ]));
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

    #[test]
    fn extracts_proposed_plan_markdown_from_tagged_message() {
        let markdown = extract_proposed_plan_markdown(
            "intro\n<proposed_plan>\n# Ship it\n\n- one\n</proposed_plan>\noutro",
        )
        .unwrap();
        assert_eq!(markdown, "# Ship it\n\n- one");
    }

    #[tokio::test]
    async fn maps_plan_items_to_plan_completed_events() {
        let command_outputs = Arc::new(Mutex::new(HashMap::new()));
        let text_outputs = Arc::new(Mutex::new(HashMap::new()));

        let event = map_item_completed(
            &json!({
                "item": {
                    "id": "plan_1",
                    "type": "Plan",
                    "text": "## Final plan\n\n- one\n- two"
                }
            }),
            &command_outputs,
            &text_outputs,
        )
        .await
        .expect("plan completion event");

        match event {
            CodexEvent::PlanCompleted { markdown } => {
                assert_eq!(markdown, "## Final plan\n\n- one\n- two");
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[test]
    fn maps_task_complete_notification_to_plan_completed_event() {
        let event = map_task_complete_notification(&json!({
            "id": "turn-1",
            "msg": {
                "type": "task_complete",
                "turn_id": "turn-1",
                "last_agent_message": "<proposed_plan>\n# Ship it\n\n- one\n</proposed_plan>"
            }
        }))
        .expect("plan completion event");

        match event {
            CodexEvent::PlanCompleted { markdown } => {
                assert_eq!(markdown, "# Ship it\n\n- one");
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[test]
    fn builds_plan_collaboration_mode_with_plan_instructions() {
        let collaboration =
            build_collaboration_mode(PromptMode::Plan, Some("gpt-5.5"), Some("high"));

        assert_eq!(collaboration["mode"], "plan");
        assert_eq!(collaboration["settings"]["model"], "gpt-5.5");
        assert_eq!(collaboration["settings"]["reasoning_effort"], "high");
        assert!(
            collaboration["settings"]["developer_instructions"]
                .as_str()
                .unwrap_or_default()
                .contains("request_user_input")
        );
    }

    #[test]
    fn collaboration_mode_omits_model_when_unset() {
        let collaboration = build_collaboration_mode(PromptMode::Normal, None, None);

        assert!(collaboration["settings"].get("model").is_none());
        assert!(collaboration["settings"].get("reasoning_effort").is_none());
    }

    #[test]
    fn plan_mode_instructions_require_proposed_plan_block() {
        assert!(CODEX_PLAN_MODE_DEVELOPER_INSTRUCTIONS.contains("<proposed_plan>"));
    }

    #[test]
    fn parses_request_user_input_payload_without_optional_flags() {
        let parsed: ToolRequestUserInputParams = serde_json::from_value(json!({
            "questions": [
                {
                    "id": "scope",
                    "header": "Scope",
                    "question": "Pick one scope",
                    "options": [
                        {
                            "label": "Small seam",
                            "description": "Keep changes narrow"
                        }
                    ]
                }
            ]
        }))
        .expect("request_user_input payload should parse");

        assert_eq!(parsed.questions.len(), 1);
        assert!(!parsed.questions[0].is_other);
        assert!(!parsed.questions[0].is_secret);
    }

    #[test]
    fn accepts_option_questions_even_with_optional_flags_enabled() {
        assert!(supports_telegram_user_input_questions(&[
            UserInputQuestion {
                id: "scope".into(),
                header: "Scope".into(),
                question: "Choose the scope".into(),
                is_other: true,
                is_secret: true,
                options: Some(vec![UserInputOption {
                    label: "Small seam".into(),
                    description: "Keep changes narrow".into(),
                }]),
            }
        ]));
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

    #[tokio::test]
    async fn maps_interrupted_turn_completion_to_interrupted_event() {
        let command_outputs = Arc::new(Mutex::new(HashMap::new()));
        let text_outputs = Arc::new(Mutex::new(HashMap::new()));

        let event = map_notification(
            "turn/completed",
            &json!({
                "turn": {
                    "id": "turn_123",
                    "status": "interrupted",
                    "error": { "message": "Stopped by user" }
                }
            }),
            &command_outputs,
            &text_outputs,
        )
        .await
        .expect("interrupted turn event");

        match event {
            CodexEvent::TurnInterrupted { message } => {
                assert_eq!(message, "Stopped by user");
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }
}
