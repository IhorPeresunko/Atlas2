//! Resuming an existing Codex thread for a chat's active session.
//!
//! `/new` always starts a fresh Codex thread; this service instead attaches the
//! chat's active session to a thread that already exists on disk (started here
//! or via the laptop Codex CLI, which share `~/.codex/sessions`).

use crate::{
    codex_sessions::CodexSessionsReader,
    domain::{CodexThreadId, TelegramChatId, TelegramUserId},
    error::{AppError, AppResult},
    presentation::{render_resume_prompt, render_resume_transcript, resume_threads_markup},
    storage::Storage,
    telegram::{InlineKeyboardMarkup, TelegramApi},
};

use super::require_group_admin;

const RESUME_THREAD_LIMIT: usize = 10;
const RESUME_TRANSCRIPT_MESSAGES: usize = 10;

pub struct ResumeCallbackResult {
    pub confirmation: String,
    pub transcript: String,
}

#[derive(Clone)]
pub struct ResumeService<Tg: TelegramApi> {
    storage: Storage,
    telegram: Tg,
    reader: CodexSessionsReader,
}

impl<Tg: TelegramApi> ResumeService<Tg> {
    pub fn new(storage: Storage, telegram: Tg, reader: CodexSessionsReader) -> Self {
        Self {
            storage,
            telegram,
            reader,
        }
    }

    /// Lists the recent Codex threads sharing the active session's workspace.
    /// Returns instructive text (and no markup) when there is no active session
    /// or no matching threads.
    pub async fn begin_resume(
        &self,
        chat_id: TelegramChatId,
    ) -> AppResult<(String, Option<InlineKeyboardMarkup>)> {
        let Some(session) = self.storage.get_active_session_for_chat(chat_id).await? else {
            return Ok((
                "No active session. Run /new first to pick a workspace.".into(),
                None,
            ));
        };
        let workspace = session.workspace_path.0;

        let threads = self
            .reader
            .list_threads_for_cwd(&workspace, RESUME_THREAD_LIMIT)
            .await?;
        if threads.is_empty() {
            return Ok((format!("No Codex threads found for `{workspace}`."), None));
        }

        Ok((
            render_resume_prompt(&workspace),
            Some(resume_threads_markup(&threads)),
        ))
    }

    /// Binds the chat's active session to the chosen thread and returns a
    /// confirmation plus the thread's recent transcript for context.
    pub async fn handle_resume_callback(
        &self,
        chat_id: TelegramChatId,
        user_id: TelegramUserId,
        thread_id_raw: &str,
    ) -> AppResult<ResumeCallbackResult> {
        require_group_admin(&self.telegram, chat_id, user_id).await?;

        let session = self
            .storage
            .get_active_session_for_chat(chat_id)
            .await?
            .ok_or_else(|| {
                AppError::Validation("no active session for this chat; run /new first".into())
            })?;
        let thread_id = CodexThreadId(thread_id_raw.to_string());

        // Re-validate the picked thread still belongs to this workspace; guards
        // against a stale picker after the active session changed.
        let thread_cwd = self
            .reader
            .thread_cwd(&thread_id)
            .await?
            .ok_or_else(|| AppError::Validation("that Codex thread no longer exists".into()))?;
        if thread_cwd != session.workspace_path.0 {
            return Err(AppError::Validation(
                "that Codex thread belongs to a different workspace".into(),
            ));
        }

        // Overwrite the active session's binding; the resume cursor is cleared
        // and rebuilt by `open_thread` on the next prompt's `thread/resume`.
        self.storage
            .update_session_provider_state(&session.session_id, Some(&thread_id), None)
            .await?;

        let messages = self
            .reader
            .read_recent_messages(&thread_id, RESUME_TRANSCRIPT_MESSAGES)
            .await?;

        Ok(ResumeCallbackResult {
            confirmation: format!(
                "Resumed Codex thread in `{}`.\nSend a prompt to continue.",
                session.workspace_path.0
            ),
            transcript: render_resume_transcript(&messages),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{SessionBackend, SessionId, SessionRecord, SessionStatus, WorkspacePath};
    use crate::telegram::TelegramClient;
    use chrono::Utc;
    use std::fs as std_fs;
    use std::path::Path;
    use tempfile::tempdir;

    fn write_thread(dir: &Path, thread_id: &str, cwd: &str) {
        let day = dir.join("2026").join("01").join("02");
        std_fs::create_dir_all(&day).unwrap();
        let meta = format!(
            r#"{{"type":"session_meta","payload":{{"id":"{thread_id}","timestamp":"2026-01-02T10:00:00Z","cwd":"{cwd}"}}}}"#
        );
        let msg = r#"{"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"hello"}]}}"#;
        std_fs::write(
            day.join(format!("rollout-2026-01-02T00-00-00-{thread_id}.jsonl")),
            format!("{meta}\n{msg}\n"),
        )
        .unwrap();
    }

    async fn seed_active_session(storage: &Storage, chat_id: TelegramChatId, workspace: &str) {
        storage
            .upsert_chat(chat_id, "supergroup", Some("Atlas"))
            .await
            .unwrap();
        let session_id = SessionId::new();
        storage
            .insert_session(&SessionRecord {
                session_id: session_id.clone(),
                chat_id,
                workspace_path: WorkspacePath(workspace.to_string()),
                backend: SessionBackend::AppServer,
                provider_thread_id: None,
                resume_cursor_json: None,
                status: SessionStatus::Ready,
                last_error: None,
                created_at: Utc::now(),
                updated_at: Utc::now(),
            })
            .await
            .unwrap();
        storage
            .set_active_session(chat_id, Some(&session_id))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn begin_resume_without_active_session_instructs_new() {
        let storage = Storage::connect("sqlite::memory:").await.unwrap();
        let chat_id = TelegramChatId(7);
        storage
            .upsert_chat(chat_id, "supergroup", Some("Atlas"))
            .await
            .unwrap();
        let temp = tempdir().unwrap();
        let service = ResumeService::new(
            storage,
            TelegramClient::new("http://127.0.0.1:9", "token"),
            CodexSessionsReader::new(temp.path().to_path_buf()),
        );

        let (text, markup) = service.begin_resume(chat_id).await.unwrap();
        assert!(text.contains("/new"));
        assert!(markup.is_none());
    }

    #[tokio::test]
    async fn begin_resume_lists_matching_threads() {
        let storage = Storage::connect("sqlite::memory:").await.unwrap();
        let chat_id = TelegramChatId(8);
        let temp = tempdir().unwrap();
        let workspace = temp.path().to_string_lossy().into_owned();
        seed_active_session(&storage, chat_id, &workspace).await;
        write_thread(temp.path(), "aaaaaaaa-0000-0000-0000-000000000001", &workspace);

        let service = ResumeService::new(
            storage,
            TelegramClient::new("http://127.0.0.1:9", "token"),
            CodexSessionsReader::new(temp.path().to_path_buf()),
        );

        let (text, markup) = service.begin_resume(chat_id).await.unwrap();
        assert!(text.contains("Pick a Codex thread"));
        let markup = markup.expect("markup present");
        assert_eq!(markup.inline_keyboard.len(), 1);
        assert!(
            markup.inline_keyboard[0][0]
                .callback_data
                .starts_with("resume-select:")
        );
    }

    #[tokio::test]
    async fn begin_resume_with_no_threads_reports_empty() {
        let storage = Storage::connect("sqlite::memory:").await.unwrap();
        let chat_id = TelegramChatId(9);
        let temp = tempdir().unwrap();
        let workspace = temp.path().to_string_lossy().into_owned();
        seed_active_session(&storage, chat_id, &workspace).await;

        let service = ResumeService::new(
            storage,
            TelegramClient::new("http://127.0.0.1:9", "token"),
            CodexSessionsReader::new(temp.path().to_path_buf()),
        );

        let (text, markup) = service.begin_resume(chat_id).await.unwrap();
        assert!(text.contains("No Codex threads found"));
        assert!(markup.is_none());
    }
}
