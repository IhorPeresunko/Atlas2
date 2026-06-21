//! Reader for Claude CLI transcript files (`~/.claude/projects/**/*.jsonl`).
//!
//! Infrastructure adapter mirroring [`super::super::codex::CodexThreadReader`]: it
//! understands the Claude Code transcript JSONL format and nothing about Telegram
//! or session business rules. Atlas2's local Claude and the laptop Claude CLI
//! share these files, so this reader lets a chat discover and resume an existing
//! session regardless of where it was started.
//!
//! Each transcript file is named `<session-id>.jsonl` and holds one JSON object
//! per line. Lines carry a `cwd`, a `timestamp`, a `sessionId`, and a `message`
//! whose shape mirrors the Anthropic Messages API (string or content-block
//! array). The file stem is the session id Atlas2 stores as the provider thread.

use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde_json::Value;
use tokio::{
    fs,
    io::{AsyncBufReadExt, BufReader},
};

use crate::{
    domain::ThreadId,
    error::AppResult,
    provider::{ConversationMessage, MessageRole, ThreadHistoryReader, ThreadSummary},
};

#[derive(Clone)]
pub struct ClaudeThreadReader {
    projects_dir: PathBuf,
}

impl ClaudeThreadReader {
    pub fn new(projects_dir: PathBuf) -> Self {
        Self { projects_dir }
    }

    /// Lists the most-recent Claude sessions whose recorded `cwd` equals `cwd`,
    /// newest first, capped at `limit`. Files that cannot be read or parsed are
    /// skipped rather than failing the whole listing.
    pub async fn list_threads_for_cwd(
        &self,
        cwd: &str,
        limit: usize,
    ) -> AppResult<Vec<ThreadSummary>> {
        let mut summaries = Vec::new();
        for file in self.transcript_files().await? {
            if let Some(summary) = read_thread_summary(&file, cwd).await {
                summaries.push(summary);
            }
        }
        summaries.sort_by(|a, b| b.started_at.cmp(&a.started_at));
        summaries.truncate(limit);
        Ok(summaries)
    }

    /// Reads the last `limit` user/assistant messages of a session in
    /// chronological order. Tool results and meta lines are filtered out. A
    /// missing transcript file yields an empty list.
    pub async fn read_recent_messages(
        &self,
        thread_id: &ThreadId,
        limit: usize,
    ) -> AppResult<Vec<ConversationMessage>> {
        let Some(file) = self.locate_thread_file(thread_id).await? else {
            return Ok(Vec::new());
        };
        let content = fs::read_to_string(&file).await?;
        let mut messages: Vec<ConversationMessage> = content
            .lines()
            .filter_map(parse_conversation_message)
            .collect();
        if messages.len() > limit {
            messages = messages.split_off(messages.len() - limit);
        }
        Ok(messages)
    }

    /// Returns the recorded `cwd` for a session, if its transcript exists.
    pub async fn thread_cwd(&self, thread_id: &ThreadId) -> AppResult<Option<String>> {
        let Some(file) = self.locate_thread_file(thread_id).await? else {
            return Ok(None);
        };
        Ok(read_first_cwd(&file).await)
    }

    /// The transcript filename is `<session-id>.jsonl`, so we match on the stem.
    async fn locate_thread_file(&self, thread_id: &ThreadId) -> AppResult<Option<PathBuf>> {
        let target = format!("{}.jsonl", thread_id.0);
        for file in self.transcript_files().await? {
            if file
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name == target)
            {
                return Ok(Some(file));
            }
        }
        Ok(None)
    }

    /// Recursively collects `*.jsonl` files under the projects directory. A
    /// missing directory yields an empty list.
    async fn transcript_files(&self) -> AppResult<Vec<PathBuf>> {
        let mut files = Vec::new();
        let mut stack = vec![self.projects_dir.clone()];
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

#[async_trait::async_trait]
impl ThreadHistoryReader for ClaudeThreadReader {
    async fn list_threads_for_cwd(
        &self,
        cwd: &str,
        limit: usize,
    ) -> AppResult<Vec<ThreadSummary>> {
        ClaudeThreadReader::list_threads_for_cwd(self, cwd, limit).await
    }

    async fn read_recent_messages(
        &self,
        thread_id: &ThreadId,
        limit: usize,
    ) -> AppResult<Vec<ConversationMessage>> {
        ClaudeThreadReader::read_recent_messages(self, thread_id, limit).await
    }

    async fn thread_cwd(&self, thread_id: &ThreadId) -> AppResult<Option<String>> {
        ClaudeThreadReader::thread_cwd(self, thread_id).await
    }
}

/// Reads the first `cwd` field found in a transcript file.
async fn read_first_cwd(file: &Path) -> Option<String> {
    let handle = fs::File::open(file).await.ok()?;
    let mut lines = BufReader::new(handle).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        if let Some(cwd) = serde_json::from_str::<Value>(&line)
            .ok()
            .and_then(|value| value.get("cwd").and_then(Value::as_str).map(str::to_string))
        {
            return Some(cwd);
        }
    }
    None
}

/// Reads a thread summary (session id from the filename, first timestamp, first
/// real user prompt preview), returning `None` when the file does not match
/// `cwd` or cannot be parsed.
async fn read_thread_summary(file: &Path, cwd: &str) -> Option<ThreadSummary> {
    let stem = file.file_stem().and_then(|stem| stem.to_str())?.to_string();
    let handle = fs::File::open(file).await.ok()?;
    let mut lines = BufReader::new(handle).lines();

    let mut matched_cwd = false;
    let mut started_at: Option<DateTime<Utc>> = None;
    let mut preview = String::new();

    while let Ok(Some(line)) = lines.next_line().await {
        let Ok(value) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if let Some(line_cwd) = value.get("cwd").and_then(Value::as_str) {
            if line_cwd != cwd {
                return None;
            }
            matched_cwd = true;
        }
        if started_at.is_none()
            && let Some(ts) = value.get("timestamp").and_then(Value::as_str)
            && let Ok(parsed) = DateTime::parse_from_rfc3339(ts)
        {
            started_at = Some(parsed.with_timezone(&Utc));
        }
        if preview.is_empty()
            && let Some(message) = parse_conversation_message(&line)
            && message.role == MessageRole::User
        {
            preview = first_nonempty_line(&message.text);
        }
    }

    if !matched_cwd {
        return None;
    }

    Some(ThreadSummary {
        thread_id: ThreadId(stem),
        started_at: started_at.unwrap_or_else(Utc::now),
        preview,
    })
}

fn parse_conversation_message(line: &str) -> Option<ConversationMessage> {
    let value: Value = serde_json::from_str(line).ok()?;
    let role = match value.get("type")?.as_str()? {
        "user" => MessageRole::User,
        "assistant" => MessageRole::Assistant,
        _ => return None,
    };
    let content = value.get("message")?.get("content")?;
    let text = extract_text(content)?;
    if text.trim().is_empty() {
        return None;
    }
    if role == MessageRole::User && is_synthetic_user_text(&text) {
        return None;
    }
    Some(ConversationMessage { role, text })
}

/// Extracts the visible text from a Claude message `content`, which is either a
/// plain string or an array of content blocks. Only `text` blocks contribute;
/// `tool_use` / `tool_result` blocks are ignored for transcript display.
fn extract_text(content: &Value) -> Option<String> {
    if let Some(text) = content.as_str() {
        return Some(text.to_string());
    }
    let parts: Vec<&str> = content
        .as_array()?
        .iter()
        .filter(|item| item.get("type").and_then(Value::as_str) == Some("text"))
        .filter_map(|item| item.get("text").and_then(Value::as_str))
        .collect();
    if parts.is_empty() {
        return None;
    }
    Some(parts.join("\n"))
}

/// Detects the synthetic/meta user messages Claude records (slash-command
/// envelopes, command output, and the local-command caveat), which are not part
/// of the real conversation.
fn is_synthetic_user_text(text: &str) -> bool {
    let trimmed = text.trim_start();
    trimmed.starts_with("<command-name>")
        || trimmed.starts_with("<local-command-stdout>")
        || text.contains("Caveat: The messages below were generated by the user")
        || text.contains("<command-message>")
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

    fn user_line(cwd: &str, ts: &str, text: &str) -> String {
        format!(
            r#"{{"type":"user","cwd":"{cwd}","timestamp":"{ts}","message":{{"role":"user","content":{}}}}}"#,
            serde_json::to_string(text).unwrap()
        )
    }

    fn assistant_line(cwd: &str, ts: &str, text: &str) -> String {
        format!(
            r#"{{"type":"assistant","cwd":"{cwd}","timestamp":"{ts}","message":{{"role":"assistant","content":[{{"type":"text","text":{}}}]}}}}"#,
            serde_json::to_string(text).unwrap()
        )
    }

    /// Writes a transcript mirroring the real layout
    /// (`<projects>/<encoded-cwd>/<session-id>.jsonl`).
    fn write_transcript(dir: &Path, encoded: &str, session_id: &str, lines: &[String]) {
        let project = dir.join(encoded);
        std_fs::create_dir_all(&project).unwrap();
        std_fs::write(
            project.join(format!("{session_id}.jsonl")),
            format!("{}\n", lines.join("\n")),
        )
        .unwrap();
    }

    #[tokio::test]
    async fn lists_only_matching_cwd_newest_first_capped() {
        let temp = tempdir().unwrap();
        let reader = ClaudeThreadReader::new(temp.path().to_path_buf());

        write_transcript(
            temp.path(),
            "-work-match",
            "11111111-0000-0000-0000-000000000001",
            &[user_line("/work/match", "2026-01-02T10:00:00Z", "first prompt")],
        );
        write_transcript(
            temp.path(),
            "-work-match",
            "11111111-0000-0000-0000-000000000002",
            &[user_line("/work/match", "2026-01-02T12:00:00Z", "newer prompt")],
        );
        write_transcript(
            temp.path(),
            "-work-other",
            "22222222-0000-0000-0000-000000000003",
            &[user_line("/work/other", "2026-01-02T13:00:00Z", "other workspace")],
        );

        let threads = reader.list_threads_for_cwd("/work/match", 10).await.unwrap();
        assert_eq!(threads.len(), 2);
        assert_eq!(threads[0].thread_id.0, "11111111-0000-0000-0000-000000000002");
        assert_eq!(threads[0].preview, "newer prompt");

        let limited = reader.list_threads_for_cwd("/work/match", 1).await.unwrap();
        assert_eq!(limited.len(), 1);
        assert_eq!(limited[0].thread_id.0, "11111111-0000-0000-0000-000000000002");
    }

    #[tokio::test]
    async fn preview_skips_synthetic_command_envelopes() {
        let temp = tempdir().unwrap();
        let reader = ClaudeThreadReader::new(temp.path().to_path_buf());
        write_transcript(
            temp.path(),
            "-work-match",
            "33333333-0000-0000-0000-000000000001",
            &[
                user_line(
                    "/work/match",
                    "2026-01-02T10:00:00Z",
                    "<command-name>/clear</command-name>",
                ),
                user_line("/work/match", "2026-01-02T10:00:01Z", "the real first prompt"),
            ],
        );

        let threads = reader.list_threads_for_cwd("/work/match", 10).await.unwrap();
        assert_eq!(threads.len(), 1);
        assert_eq!(threads[0].preview, "the real first prompt");
    }

    #[tokio::test]
    async fn reads_last_messages_in_order() {
        let temp = tempdir().unwrap();
        let reader = ClaudeThreadReader::new(temp.path().to_path_buf());
        let session_id = "44444444-0000-0000-0000-000000000001";
        let mut lines = Vec::new();
        for index in 0..8 {
            lines.push(user_line("/work/match", "2026-01-02T10:00:00Z", &format!("user {index}")));
            lines.push(assistant_line(
                "/work/match",
                "2026-01-02T10:00:00Z",
                &format!("assistant {index}"),
            ));
        }
        write_transcript(temp.path(), "-work-match", session_id, &lines);

        let messages = reader
            .read_recent_messages(&ThreadId(session_id.into()), 10)
            .await
            .unwrap();

        assert_eq!(messages.len(), 10);
        assert_eq!(messages.first().unwrap().role, MessageRole::User);
        assert_eq!(messages.first().unwrap().text, "user 3");
        assert_eq!(messages.last().unwrap().role, MessageRole::Assistant);
        assert_eq!(messages.last().unwrap().text, "assistant 7");
    }

    #[tokio::test]
    async fn thread_cwd_reads_first_cwd_and_tolerates_missing() {
        let temp = tempdir().unwrap();
        let reader = ClaudeThreadReader::new(temp.path().to_path_buf());
        let session_id = "55555555-0000-0000-0000-000000000001";
        write_transcript(
            temp.path(),
            "-work-match",
            session_id,
            &[user_line("/work/match", "2026-01-02T10:00:00Z", "hello")],
        );

        let cwd = reader
            .thread_cwd(&ThreadId(session_id.into()))
            .await
            .unwrap();
        assert_eq!(cwd.as_deref(), Some("/work/match"));

        let missing = reader
            .thread_cwd(&ThreadId("00000000-0000-0000-0000-000000000000".into()))
            .await
            .unwrap();
        assert_eq!(missing, None);
    }
}
