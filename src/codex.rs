use std::{path::PathBuf, process::Stdio};

use serde_json::Value;
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    process::Command,
};

use crate::{
    domain::{ApprovalId, CodexThreadId},
    error::{AppError, AppResult},
};

#[derive(Debug, Clone)]
pub struct CodexClient {
    codex_bin: String,
    additional_dirs: Vec<PathBuf>,
}

impl CodexClient {
    pub fn new(codex_bin: String, additional_dirs: Vec<PathBuf>) -> Self {
        Self {
            codex_bin,
            additional_dirs,
        }
    }

    pub async fn run_turn<F>(
        &self,
        workspace_path: &str,
        thread_id: Option<&CodexThreadId>,
        prompt: &str,
        mut on_event: F,
    ) -> AppResult<CodexTurnResult>
    where
        F: FnMut(CodexEvent) -> AppResult<()>,
    {
        let mut command = Command::new(&self.codex_bin);
        command
            .arg("exec")
            .arg("--json")
            .arg("--skip-git-repo-check")
            .arg("--cd")
            .arg(workspace_path);

        for dir in &self.additional_dirs {
            command.arg("--add-dir").arg(dir);
        }

        if let Some(thread_id) = thread_id {
            command.arg("resume").arg(&thread_id.0).arg(prompt);
        } else {
            command.arg(prompt);
        }

        command.stdout(Stdio::piped()).stderr(Stdio::piped());
        let mut child = command.spawn()?;

        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| AppError::Codex("missing stdout from codex process".into()))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| AppError::Codex("missing stderr from codex process".into()))?;

        let mut stdout_lines = BufReader::new(stdout).lines();
        let mut stderr_lines = BufReader::new(stderr).lines();

        let mut result = CodexTurnResult::default();

        loop {
            tokio::select! {
                line = stdout_lines.next_line() => {
                    let Some(line) = line? else { break; };
                    if line.trim().is_empty() {
                        continue;
                    }

                    let json: Value = serde_json::from_str(&line)?;
                    if let Some(event) = parse_event(&json)? {
                        match &event {
                            CodexEvent::ThreadStarted { thread_id } => {
                                result.thread_id = Some(thread_id.clone());
                            }
                            CodexEvent::ApprovalRequested { approval } => {
                                result.pending_approval = Some(approval.clone());
                            }
                            CodexEvent::TurnCompleted => {
                                result.completed = true;
                            }
                            CodexEvent::TurnFailed { message } => {
                                result.failure = Some(message.clone());
                            }
                            _ => {}
                        }
                        on_event(event)?;
                    }
                }
                line = stderr_lines.next_line() => {
                    let Some(line) = line? else { continue; };
                    if !line.trim().is_empty() {
                        on_event(CodexEvent::Status {
                            text: format!("codex stderr: {line}"),
                        })?;
                    }
                }
            }
        }

        let status = child.wait().await?;
        if !status.success() && result.failure.is_none() {
            return Err(AppError::Codex(format!(
                "codex process exited with status {status}"
            )));
        }

        Ok(result)
    }
}

#[derive(Debug, Clone, Default)]
pub struct CodexTurnResult {
    pub thread_id: Option<CodexThreadId>,
    pub pending_approval: Option<CodexPendingApproval>,
    pub completed: bool,
    pub failure: Option<String>,
}

#[derive(Debug, Clone)]
pub enum CodexEvent {
    ThreadStarted {
        thread_id: CodexThreadId,
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

fn parse_event(json: &Value) -> AppResult<Option<CodexEvent>> {
    let kind = json
        .get("type")
        .and_then(Value::as_str)
        .ok_or_else(|| AppError::Codex("missing event type".into()))?;

    let event = match kind {
        "thread.started" => {
            let thread_id = json
                .get("thread_id")
                .and_then(Value::as_str)
                .ok_or_else(|| AppError::Codex("thread.started missing thread_id".into()))?;
            Some(CodexEvent::ThreadStarted {
                thread_id: CodexThreadId(thread_id.to_string()),
            })
        }
        "turn.started" => Some(CodexEvent::Status {
            text: "Codex turn started".into(),
        }),
        "turn.completed" => Some(CodexEvent::TurnCompleted),
        "turn.failed" => Some(CodexEvent::TurnFailed {
            message: json
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("Codex turn failed")
                .to_string(),
        }),
        "item.completed" => parse_item_event(json, true)?,
        "item.started" => parse_item_event(json, false)?,
        "approval.requested" => {
            let summary = json
                .get("summary")
                .and_then(Value::as_str)
                .unwrap_or("Codex requested approval")
                .to_string();
            Some(CodexEvent::ApprovalRequested {
                approval: CodexPendingApproval {
                    approval_id: ApprovalId::new(),
                    summary,
                    payload: json.to_string(),
                },
            })
        }
        _ => None,
    };

    Ok(event)
}

fn parse_item_event(json: &Value, completed: bool) -> AppResult<Option<CodexEvent>> {
    let item = match json.get("item") {
        Some(item) => item,
        None => return Ok(None),
    };
    let item_type = item.get("type").and_then(Value::as_str).unwrap_or_default();

    let event = match item_type {
        "agent_message" if completed => {
            item.get("text")
                .and_then(Value::as_str)
                .map(|text| CodexEvent::Output {
                    text: text.to_string(),
                })
        }
        "command_execution" => {
            let command = item
                .get("command")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            if completed {
                let exit_code = item.get("exit_code").and_then(Value::as_i64).unwrap_or(-1);
                let output = item
                    .get("aggregated_output")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                Some(CodexEvent::CommandFinished {
                    command,
                    exit_code,
                    output,
                })
            } else {
                Some(CodexEvent::CommandStarted { command })
            }
        }
        _ => None,
    };

    Ok(event)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{CodexEvent, parse_event};

    #[test]
    fn parses_command_completion_event() {
        let event = parse_event(&json!({
            "type": "item.completed",
            "item": {
                "type": "command_execution",
                "command": "/bin/bash -lc pwd",
                "aggregated_output": "/tmp/project\n",
                "exit_code": 0
            }
        }))
        .unwrap()
        .unwrap();

        match event {
            CodexEvent::CommandFinished {
                command,
                exit_code,
                output,
            } => {
                assert_eq!(command, "/bin/bash -lc pwd");
                assert_eq!(exit_code, 0);
                assert_eq!(output, "/tmp/project\n");
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }
}
