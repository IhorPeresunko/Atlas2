//! Reader for Codex rollout session files (`~/.codex/sessions/**/*.jsonl`).
//!
//! Infrastructure adapter: it understands the Codex rollout JSONL format and
//! nothing about Telegram or session business rules. Atlas2's local Codex and
//! the laptop Codex CLI share these files, so this reader lets a chat discover
//! and resume an existing thread regardless of where it was started.

use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde_json::Value;
use tokio::{
    fs,
    io::{AsyncBufReadExt, BufReader},
};

use crate::{domain::CodexThreadId, error::AppResult};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodexMessageRole {
    User,
    Assistant,
}

/// A Codex thread discovered on disk, summarised for the resume picker.
#[derive(Debug, Clone)]
pub struct CodexThreadSummary {
    pub thread_id: CodexThreadId,
    pub started_at: DateTime<Utc>,
    /// First real user prompt, single line, for a button label.
    pub preview: String,
}

/// A single user/assistant message extracted from a rollout file.
#[derive(Debug, Clone)]
pub struct CodexConversationMessage {
    pub role: CodexMessageRole,
    pub text: String,
}

#[derive(Clone)]
pub struct CodexSessionsReader {
    sessions_dir: PathBuf,
}

impl CodexSessionsReader {
    pub fn new(sessions_dir: PathBuf) -> Self {
        Self { sessions_dir }
    }

    /// Lists the most-recent Codex threads whose starting `cwd` equals `cwd`,
    /// newest first, capped at `limit`. Files that cannot be read or parsed are
    /// skipped rather than failing the whole listing.
    pub async fn list_threads_for_cwd(
        &self,
        cwd: &str,
        limit: usize,
    ) -> AppResult<Vec<CodexThreadSummary>> {
        let mut summaries = Vec::new();
        for file in self.rollout_files().await? {
            if let Some(summary) = read_thread_summary(&file, cwd).await {
                summaries.push(summary);
            }
        }
        summaries.sort_by(|a, b| b.started_at.cmp(&a.started_at));
        summaries.truncate(limit);
        Ok(summaries)
    }

    /// Reads the last `limit` user/assistant messages of a thread in
    /// chronological order. The synthetic AGENTS.md/environment preamble is
    /// filtered out. A missing rollout file yields an empty list.
    pub async fn read_recent_messages(
        &self,
        thread_id: &CodexThreadId,
        limit: usize,
    ) -> AppResult<Vec<CodexConversationMessage>> {
        let Some(file) = self.locate_thread_file(thread_id).await? else {
            return Ok(Vec::new());
        };
        let content = fs::read_to_string(&file).await?;
        let mut messages: Vec<CodexConversationMessage> = content
            .lines()
            .filter_map(parse_conversation_message)
            .collect();
        if messages.len() > limit {
            messages = messages.split_off(messages.len() - limit);
        }
        Ok(messages)
    }

    /// Returns the `session_meta` cwd for a thread, if its rollout file exists.
    /// Used to re-validate a thread still belongs to the chat's workspace.
    pub async fn thread_cwd(&self, thread_id: &CodexThreadId) -> AppResult<Option<String>> {
        let Some(file) = self.locate_thread_file(thread_id).await? else {
            return Ok(None);
        };
        Ok(read_session_meta(&file).await.map(|meta| meta.cwd))
    }

    async fn locate_thread_file(&self, thread_id: &CodexThreadId) -> AppResult<Option<PathBuf>> {
        // The thread id is also encoded in the rollout filename
        // (`rollout-<ts>-<threadId>.jsonl`), so we can match on the suffix.
        let suffix = format!("-{}.jsonl", thread_id.0);
        for file in self.rollout_files().await? {
            if file
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.ends_with(&suffix))
            {
                return Ok(Some(file));
            }
        }
        Ok(None)
    }

    /// Recursively collects `*.jsonl` files under the sessions directory. A
    /// missing directory yields an empty list.
    async fn rollout_files(&self) -> AppResult<Vec<PathBuf>> {
        let mut files = Vec::new();
        let mut stack = vec![self.sessions_dir.clone()];
        while let Some(dir) = stack.pop() {
            let Ok(mut entries) = fs::read_dir(&dir).await else {
                continue;
            };
            while let Ok(Some(entry)) = entries.next_entry().await {
                let path = entry.path();
                let Ok(file_type) = entry.file_type().await else {
                    continue;
                };
                if file_type.is_dir() {
                    stack.push(path);
                } else if path.extension().and_then(|ext| ext.to_str()) == Some("jsonl") {
                    files.push(path);
                }
            }
        }
        Ok(files)
    }
}

struct SessionMeta {
    thread_id: CodexThreadId,
    cwd: String,
    started_at: DateTime<Utc>,
}

/// Reads and parses the leading `session_meta` line of a rollout file.
async fn read_session_meta(file: &Path) -> Option<SessionMeta> {
    let handle = fs::File::open(file).await.ok()?;
    let mut lines = BufReader::new(handle).lines();
    let first = lines.next_line().await.ok()??;
    parse_session_meta(&first)
}

/// Reads a thread summary (meta + first real user prompt preview), returning
/// `None` when the file does not match `cwd` or cannot be parsed.
async fn read_thread_summary(file: &Path, cwd: &str) -> Option<CodexThreadSummary> {
    let handle = fs::File::open(file).await.ok()?;
    let mut lines = BufReader::new(handle).lines();
    let first = lines.next_line().await.ok()??;
    let meta = parse_session_meta(&first)?;
    if meta.cwd != cwd {
        return None;
    }

    let mut preview = String::new();
    while let Ok(Some(line)) = lines.next_line().await {
        if let Some(message) = parse_conversation_message(&line)
            && message.role == CodexMessageRole::User
        {
            preview = first_nonempty_line(&message.text);
            break;
        }
    }

    Some(CodexThreadSummary {
        thread_id: meta.thread_id,
        started_at: meta.started_at,
        preview,
    })
}

fn parse_session_meta(line: &str) -> Option<SessionMeta> {
    let value: Value = serde_json::from_str(line).ok()?;
    if value.get("type")?.as_str()? != "session_meta" {
        return None;
    }
    let payload = value.get("payload")?;
    let thread_id = payload.get("id")?.as_str()?.to_string();
    let cwd = payload.get("cwd")?.as_str()?.to_string();
    let started_at = DateTime::parse_from_rfc3339(payload.get("timestamp")?.as_str()?)
        .ok()?
        .with_timezone(&Utc);
    Some(SessionMeta {
        thread_id: CodexThreadId(thread_id),
        cwd,
        started_at,
    })
}

fn parse_conversation_message(line: &str) -> Option<CodexConversationMessage> {
    let value: Value = serde_json::from_str(line).ok()?;
    if value.get("type")?.as_str()? != "response_item" {
        return None;
    }
    let payload = value.get("payload")?;
    if payload.get("type")?.as_str()? != "message" {
        return None;
    }
    let role = match payload.get("role")?.as_str()? {
        "user" => CodexMessageRole::User,
        "assistant" => CodexMessageRole::Assistant,
        _ => return None,
    };
    let text = extract_text(payload.get("content")?)?;
    if text.trim().is_empty() {
        return None;
    }
    if role == CodexMessageRole::User && is_synthetic_user_text(&text) {
        return None;
    }
    Some(CodexConversationMessage { role, text })
}

fn extract_text(content: &Value) -> Option<String> {
    let parts: Vec<&str> = content
        .as_array()?
        .iter()
        .filter_map(|item| item.get("text").and_then(Value::as_str))
        .collect();
    if parts.is_empty() {
        return None;
    }
    Some(parts.join("\n"))
}

/// Detects the synthetic user message Codex injects at thread start (AGENTS.md
/// instructions and the `<environment_context>` block), which is not part of the
/// real conversation.
fn is_synthetic_user_text(text: &str) -> bool {
    let trimmed = text.trim_start();
    trimmed.starts_with("# AGENTS.md") || text.contains("<environment_context>")
}

fn first_nonempty_line(text: &str) -> String {
    text.lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("")
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs as std_fs;
    use tempfile::tempdir;

    fn meta_line(thread_id: &str, cwd: &str, timestamp: &str) -> String {
        format!(
            r#"{{"type":"session_meta","payload":{{"id":"{thread_id}","timestamp":"{timestamp}","cwd":"{cwd}"}}}}"#
        )
    }

    fn user_line(text: &str) -> String {
        format!(
            r#"{{"type":"response_item","payload":{{"type":"message","role":"user","content":[{{"type":"input_text","text":{}}}]}}}}"#,
            serde_json::to_string(text).unwrap()
        )
    }

    fn assistant_line(text: &str) -> String {
        format!(
            r#"{{"type":"response_item","payload":{{"type":"message","role":"assistant","content":[{{"type":"output_text","text":{}}}]}}}}"#,
            serde_json::to_string(text).unwrap()
        )
    }

    /// Writes a rollout file mirroring the real layout
    /// (`<dir>/2026/01/02/rollout-<ts>-<threadId>.jsonl`).
    fn write_rollout(dir: &Path, thread_id: &str, lines: &[String]) {
        let day = dir.join("2026").join("01").join("02");
        std_fs::create_dir_all(&day).unwrap();
        let path = day.join(format!("rollout-2026-01-02T00-00-00-{thread_id}.jsonl"));
        std_fs::write(path, format!("{}\n", lines.join("\n"))).unwrap();
    }

    #[tokio::test]
    async fn lists_only_matching_cwd_newest_first_capped() {
        let temp = tempdir().unwrap();
        let reader = CodexSessionsReader::new(temp.path().to_path_buf());

        write_rollout(
            temp.path(),
            "aaaaaaaa-0000-0000-0000-000000000001",
            &[
                meta_line(
                    "aaaaaaaa-0000-0000-0000-000000000001",
                    "/work/match",
                    "2026-01-02T10:00:00Z",
                ),
                user_line("first prompt"),
            ],
        );
        write_rollout(
            temp.path(),
            "aaaaaaaa-0000-0000-0000-000000000002",
            &[
                meta_line(
                    "aaaaaaaa-0000-0000-0000-000000000002",
                    "/work/match",
                    "2026-01-02T12:00:00Z",
                ),
                user_line("newer prompt"),
            ],
        );
        write_rollout(
            temp.path(),
            "bbbbbbbb-0000-0000-0000-000000000003",
            &[
                meta_line(
                    "bbbbbbbb-0000-0000-0000-000000000003",
                    "/work/other",
                    "2026-01-02T13:00:00Z",
                ),
                user_line("other workspace"),
            ],
        );

        let threads = reader.list_threads_for_cwd("/work/match", 10).await.unwrap();
        assert_eq!(threads.len(), 2);
        assert_eq!(threads[0].thread_id.0, "aaaaaaaa-0000-0000-0000-000000000002");
        assert_eq!(threads[0].preview, "newer prompt");
        assert_eq!(threads[1].thread_id.0, "aaaaaaaa-0000-0000-0000-000000000001");

        let limited = reader.list_threads_for_cwd("/work/match", 1).await.unwrap();
        assert_eq!(limited.len(), 1);
        assert_eq!(limited[0].thread_id.0, "aaaaaaaa-0000-0000-0000-000000000002");
    }

    #[tokio::test]
    async fn preview_skips_synthetic_preamble() {
        let temp = tempdir().unwrap();
        let reader = CodexSessionsReader::new(temp.path().to_path_buf());
        write_rollout(
            temp.path(),
            "cccccccc-0000-0000-0000-000000000001",
            &[
                meta_line(
                    "cccccccc-0000-0000-0000-000000000001",
                    "/work/match",
                    "2026-01-02T10:00:00Z",
                ),
                user_line("# AGENTS.md instructions for /work/match\n<environment_context>\n  <cwd>/work/match</cwd>\n</environment_context>"),
                user_line("the real first prompt"),
            ],
        );

        let threads = reader.list_threads_for_cwd("/work/match", 10).await.unwrap();
        assert_eq!(threads.len(), 1);
        assert_eq!(threads[0].preview, "the real first prompt");
    }

    #[tokio::test]
    async fn reads_last_messages_filtering_preamble() {
        let temp = tempdir().unwrap();
        let reader = CodexSessionsReader::new(temp.path().to_path_buf());
        let thread_id = "dddddddd-0000-0000-0000-000000000001";
        let mut lines = vec![meta_line(thread_id, "/work/match", "2026-01-02T10:00:00Z")];
        lines.push(user_line(
            "# AGENTS.md instructions for /work/match\nsome rules",
        ));
        for index in 0..8 {
            lines.push(user_line(&format!("user {index}")));
            lines.push(assistant_line(&format!("assistant {index}")));
        }
        write_rollout(temp.path(), thread_id, &lines);

        let messages = reader
            .read_recent_messages(&CodexThreadId(thread_id.into()), 10)
            .await
            .unwrap();

        // 16 real messages exist; last 10 returned in order, no preamble.
        assert_eq!(messages.len(), 10);
        assert_eq!(messages.first().unwrap().role, CodexMessageRole::User);
        assert_eq!(messages.first().unwrap().text, "user 3");
        assert_eq!(messages.last().unwrap().role, CodexMessageRole::Assistant);
        assert_eq!(messages.last().unwrap().text, "assistant 7");
        assert!(messages.iter().all(|m| !m.text.contains("AGENTS.md")));
    }

    #[tokio::test]
    async fn tolerates_malformed_lines_and_missing_dir() {
        let temp = tempdir().unwrap();
        let reader = CodexSessionsReader::new(temp.path().join("does-not-exist"));
        assert!(reader.list_threads_for_cwd("/x", 10).await.unwrap().is_empty());

        let reader = CodexSessionsReader::new(temp.path().to_path_buf());
        let thread_id = "eeeeeeee-0000-0000-0000-000000000001";
        write_rollout(
            temp.path(),
            thread_id,
            &[
                meta_line(thread_id, "/work/match", "2026-01-02T10:00:00Z"),
                "not json at all".into(),
                user_line("survives the garbage line"),
            ],
        );

        let messages = reader
            .read_recent_messages(&CodexThreadId(thread_id.into()), 10)
            .await
            .unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].text, "survives the garbage line");
    }

    #[tokio::test]
    async fn thread_cwd_reads_meta() {
        let temp = tempdir().unwrap();
        let reader = CodexSessionsReader::new(temp.path().to_path_buf());
        let thread_id = "ffffffff-0000-0000-0000-000000000001";
        write_rollout(
            temp.path(),
            thread_id,
            &[meta_line(thread_id, "/work/match", "2026-01-02T10:00:00Z")],
        );

        let cwd = reader
            .thread_cwd(&CodexThreadId(thread_id.into()))
            .await
            .unwrap();
        assert_eq!(cwd.as_deref(), Some("/work/match"));

        let missing = reader
            .thread_cwd(&CodexThreadId("00000000-0000-0000-0000-000000000000".into()))
            .await
            .unwrap();
        assert_eq!(missing, None);
    }
}
