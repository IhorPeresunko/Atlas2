//! Resuming an existing provider thread for a chat's active session.
//!
//! `/new` always starts a fresh thread; this service instead attaches the chat's
//! active session to a thread that already exists on disk (started here or via
//! the laptop CLI, which share the provider's session directory). The thread
//! history is read from the reader matching the session's provider.

use crate::{
    domain::{TelegramChatId, TelegramUserId, ThreadId},
    error::{AppError, AppResult},
    presentation::{render_resume_prompt, render_resume_transcript, resume_threads_markup},
    provider::ThreadReaderRegistry,
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
    readers: ThreadReaderRegistry,
}

impl<Tg: TelegramApi> ResumeService<Tg> {
    pub fn new(storage: Storage, telegram: Tg, readers: ThreadReaderRegistry) -> Self {
        Self {
            storage,
            telegram,
            readers,
        }
    }

    /// Lists the recent threads sharing the active session's workspace, read from
    /// the active session's provider. Returns instructive text (and no markup)
    /// when there is no active session or no matching threads.
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
            .readers
            .get(session.provider)?
            .list_threads_for_cwd(&workspace, RESUME_THREAD_LIMIT)
            .await?;
        if threads.is_empty() {
            return Ok((format!("No threads found for `{workspace}`."), None));
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
        let thread_id = ThreadId(thread_id_raw.to_string());
        let reader = self.readers.get(session.provider)?;

        // Re-validate the picked thread still belongs to this workspace; guards
        // against a stale picker after the active session changed.
        let thread_cwd = reader
            .thread_cwd(&thread_id)
            .await?
            .ok_or_else(|| AppError::Validation("that thread no longer exists".into()))?;
        if thread_cwd != session.workspace_path.0 {
            return Err(AppError::Validation(
                "that thread belongs to a different workspace".into(),
            ));
        }

        // Overwrite the active session's binding; the resume cursor is cleared
        // and rebuilt by `open_thread` on the next prompt's `thread/resume`.
        self.storage
            .update_session_provider_state(&session.session_id, Some(&thread_id), None)
            .await?;

        let messages = reader
            .read_recent_messages(&thread_id, RESUME_TRANSCRIPT_MESSAGES)
            .await?;

        Ok(ResumeCallbackResult {
            confirmation: format!(
                "Resumed thread in `{}`.\nSend a prompt to continue.",
                session.workspace_path.0
            ),
            transcript: render_resume_transcript(&messages),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{ProviderKind, SessionId, SessionRecord, SessionStatus, WorkspacePath};
    use crate::provider::CodexThreadReader;
    use crate::telegram::TelegramClient;
    use chrono::Utc;
    use std::collections::HashMap;
    use std::fs as std_fs;
    use std::path::Path;
    use std::sync::Arc;
    use tempfile::tempdir;

    /// A reader registry serving the given Codex reader for the Codex kind (the
    /// kind the resume tests seed).
    fn codex_readers(reader: CodexThreadReader) -> ThreadReaderRegistry {
        let mut readers = HashMap::new();
        readers.insert(
            ProviderKind::Codex,
            Arc::new(reader) as Arc<dyn crate::provider::ThreadHistoryReader>,
        );
        ThreadReaderRegistry::new(readers)
    }

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
                provider: ProviderKind::Codex,
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
            codex_readers(CodexThreadReader::new(temp.path().to_path_buf())),
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
            codex_readers(CodexThreadReader::new(temp.path().to_path_buf())),
        );

        let (text, markup) = service.begin_resume(chat_id).await.unwrap();
        assert!(text.contains("Pick a thread"));
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
            codex_readers(CodexThreadReader::new(temp.path().to_path_buf())),
        );

        let (text, markup) = service.begin_resume(chat_id).await.unwrap();
        assert!(text.contains("No threads found"));
        assert!(markup.is_none());
    }
}
