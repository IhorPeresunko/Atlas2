use chrono::{DateTime, Utc};
use sqlx::{Row, SqlitePool, sqlite::SqlitePoolOptions};

use crate::{
    domain::{
        ApprovalId, ApprovalStatus, ChatBinding, CodexThreadId, FolderBrowseState,
        PendingApproval, SessionBackend, SessionId, SessionRecord, SessionStatus, SessionSummary,
        TelegramChatId, TelegramUserId, WorkspacePath,
    },
    error::{AppError, AppResult},
};

#[derive(Clone)]
pub struct Storage {
    pool: SqlitePool,
}

impl Storage {
    pub async fn connect(database_url: &str) -> AppResult<Self> {
        let pool = SqlitePoolOptions::new()
            .max_connections(5)
            .connect(database_url)
            .await?;
        let storage = Self { pool };
        storage.migrate().await?;
        Ok(storage)
    }

    async fn migrate(&self) -> AppResult<()> {
        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS chats (
                chat_id INTEGER PRIMARY KEY,
                chat_kind TEXT NOT NULL,
                title TEXT,
                active_session_id TEXT
            );

            CREATE TABLE IF NOT EXISTS sessions (
                session_id TEXT PRIMARY KEY,
                chat_id INTEGER NOT NULL,
                workspace_path TEXT NOT NULL,
                codex_thread_id TEXT,
                backend TEXT NOT NULL DEFAULT 'exec_legacy',
                provider_thread_id TEXT,
                resume_cursor_json TEXT,
                status TEXT NOT NULL,
                last_error TEXT,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS folder_browse_state (
                chat_id INTEGER PRIMARY KEY,
                current_path TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS pending_approvals (
                approval_id TEXT PRIMARY KEY,
                session_id TEXT NOT NULL,
                chat_id INTEGER NOT NULL,
                payload TEXT NOT NULL,
                summary TEXT NOT NULL,
                status TEXT NOT NULL,
                created_at TEXT NOT NULL,
                resolved_by INTEGER
            );
            "#,
        )
        .execute(&self.pool)
        .await?;

        self.ensure_session_column("backend", "TEXT NOT NULL DEFAULT 'exec_legacy'")
            .await?;
        self.ensure_session_column("provider_thread_id", "TEXT")
            .await?;
        self.ensure_session_column("resume_cursor_json", "TEXT")
            .await?;
        self.ensure_session_column("last_error", "TEXT").await?;
        sqlx::query(
            r#"
            UPDATE sessions
            SET provider_thread_id = COALESCE(provider_thread_id, codex_thread_id)
            WHERE provider_thread_id IS NULL AND codex_thread_id IS NOT NULL
            "#,
        )
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    async fn ensure_session_column(&self, name: &str, definition: &str) -> AppResult<()> {
        let rows = sqlx::query("PRAGMA table_info(sessions)")
            .fetch_all(&self.pool)
            .await?;
        let exists = rows
            .into_iter()
            .any(|row| row.get::<String, _>("name") == name);
        if exists {
            return Ok(());
        }

        sqlx::query(&format!("ALTER TABLE sessions ADD COLUMN {name} {definition}"))
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn upsert_chat(
        &self,
        chat_id: TelegramChatId,
        chat_kind: &str,
        title: Option<&str>,
    ) -> AppResult<()> {
        sqlx::query(
            r#"
            INSERT INTO chats (chat_id, chat_kind, title)
            VALUES (?1, ?2, ?3)
            ON CONFLICT(chat_id) DO UPDATE SET
                chat_kind = excluded.chat_kind,
                title = excluded.title
            "#,
        )
        .bind(chat_id.0)
        .bind(chat_kind)
        .bind(title)
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    pub async fn get_chat(&self, chat_id: TelegramChatId) -> AppResult<Option<ChatBinding>> {
        let row = sqlx::query(
            r#"
            SELECT chat_id, active_session_id, chat_kind, title
            FROM chats
            WHERE chat_id = ?1
            "#,
        )
        .bind(chat_id.0)
        .fetch_optional(&self.pool)
        .await?;

        row.map(map_chat_binding).transpose()
    }

    pub async fn set_active_session(
        &self,
        chat_id: TelegramChatId,
        session_id: Option<&SessionId>,
    ) -> AppResult<()> {
        sqlx::query(
            r#"
            UPDATE chats
            SET active_session_id = ?2
            WHERE chat_id = ?1
            "#,
        )
        .bind(chat_id.0)
        .bind(session_id.map(|id| id.0.to_string()))
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    pub async fn insert_session(&self, session: &SessionRecord) -> AppResult<()> {
        sqlx::query(
            r#"
            INSERT INTO sessions (
                session_id, chat_id, workspace_path, codex_thread_id, backend,
                provider_thread_id, resume_cursor_json, status, last_error, created_at, updated_at
            ) VALUES (?1, ?2, ?3, NULL, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
            "#,
        )
        .bind(session.session_id.0.to_string())
        .bind(session.chat_id.0)
        .bind(&session.workspace_path.0)
        .bind(session.backend.as_str())
        .bind(session.provider_thread_id.as_ref().map(|id| id.0.as_str()))
        .bind(session.resume_cursor_json.as_deref())
        .bind(session.status.as_str())
        .bind(session.last_error.as_deref())
        .bind(session.created_at.to_rfc3339())
        .bind(session.updated_at.to_rfc3339())
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    pub async fn get_active_session_for_chat(
        &self,
        chat_id: TelegramChatId,
    ) -> AppResult<Option<SessionRecord>> {
        let row = sqlx::query(
            r#"
            SELECT s.session_id, s.chat_id, s.workspace_path, s.backend, s.provider_thread_id,
                   s.resume_cursor_json, s.status, s.last_error, s.created_at, s.updated_at
            FROM chats c
            JOIN sessions s ON s.session_id = c.active_session_id
            WHERE c.chat_id = ?1
            "#,
        )
        .bind(chat_id.0)
        .fetch_optional(&self.pool)
        .await?;

        row.map(map_session).transpose()
    }

    pub async fn update_session_status(
        &self,
        session_id: &SessionId,
        status: SessionStatus,
        last_error: Option<&str>,
    ) -> AppResult<()> {
        sqlx::query(
            r#"
            UPDATE sessions
            SET status = ?2,
                last_error = ?3,
                updated_at = ?4
            WHERE session_id = ?1
            "#,
        )
        .bind(session_id.0.to_string())
        .bind(status.as_str())
        .bind(last_error)
        .bind(Utc::now().to_rfc3339())
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    pub async fn update_session_provider_state(
        &self,
        session_id: &SessionId,
        provider_thread_id: Option<&CodexThreadId>,
        resume_cursor_json: Option<&str>,
    ) -> AppResult<()> {
        sqlx::query(
            r#"
            UPDATE sessions
            SET provider_thread_id = ?2,
                resume_cursor_json = ?3,
                updated_at = ?4
            WHERE session_id = ?1
            "#,
        )
        .bind(session_id.0.to_string())
        .bind(provider_thread_id.map(|id| id.0.as_str()))
        .bind(resume_cursor_json)
        .bind(Utc::now().to_rfc3339())
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    pub async fn mark_interrupted_app_server_sessions_failed(&self) -> AppResult<()> {
        sqlx::query(
            r#"
            UPDATE sessions
            SET status = 'failed',
                last_error = 'Atlas2 restarted while the app-server runtime was active. Send a new prompt to resume from the last saved thread.',
                updated_at = ?1
            WHERE backend = 'app_server'
              AND status IN ('running', 'waiting_for_approval')
            "#,
        )
        .bind(Utc::now().to_rfc3339())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn list_sessions(&self) -> AppResult<Vec<SessionSummary>> {
        let rows = sqlx::query(
            r#"
            SELECT s.session_id, s.chat_id, c.title, s.workspace_path, s.backend, s.status,
                   s.provider_thread_id, s.last_error, s.created_at
            FROM sessions s
            LEFT JOIN chats c ON c.chat_id = s.chat_id
            ORDER BY s.created_at DESC
            "#,
        )
        .fetch_all(&self.pool)
        .await?;

        rows.into_iter().map(map_session_summary).collect()
    }

    pub async fn set_folder_browse_state(&self, state: &FolderBrowseState) -> AppResult<()> {
        sqlx::query(
            r#"
            INSERT INTO folder_browse_state (chat_id, current_path)
            VALUES (?1, ?2)
            ON CONFLICT(chat_id) DO UPDATE SET current_path = excluded.current_path
            "#,
        )
        .bind(state.chat_id.0)
        .bind(&state.current_path.0)
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    pub async fn get_folder_browse_state(
        &self,
        chat_id: TelegramChatId,
    ) -> AppResult<Option<FolderBrowseState>> {
        let row = sqlx::query(
            r#"
            SELECT chat_id, current_path
            FROM folder_browse_state
            WHERE chat_id = ?1
            "#,
        )
        .bind(chat_id.0)
        .fetch_optional(&self.pool)
        .await?;

        Ok(row.map(|row| FolderBrowseState {
            chat_id: TelegramChatId(row.get::<i64, _>("chat_id")),
            current_path: WorkspacePath(row.get::<String, _>("current_path")),
        }))
    }

    pub async fn clear_folder_browse_state(&self, chat_id: TelegramChatId) -> AppResult<()> {
        sqlx::query("DELETE FROM folder_browse_state WHERE chat_id = ?1")
            .bind(chat_id.0)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn insert_pending_approval(&self, approval: &PendingApproval) -> AppResult<()> {
        sqlx::query(
            r#"
            INSERT INTO pending_approvals (
                approval_id, session_id, chat_id, payload, summary, status, created_at, resolved_by
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
            "#,
        )
        .bind(approval.approval_id.0.to_string())
        .bind(approval.session_id.0.to_string())
        .bind(approval.chat_id.0)
        .bind(&approval.payload)
        .bind(&approval.summary)
        .bind(approval.status.as_str())
        .bind(approval.created_at.to_rfc3339())
        .bind(approval.resolved_by.map(|user| user.0))
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn get_pending_approval(
        &self,
        approval_id: &ApprovalId,
    ) -> AppResult<Option<PendingApproval>> {
        let row = sqlx::query(
            r#"
            SELECT approval_id, session_id, chat_id, payload, summary, status, created_at, resolved_by
            FROM pending_approvals
            WHERE approval_id = ?1
            "#,
        )
        .bind(approval_id.0.to_string())
        .fetch_optional(&self.pool)
        .await?;

        row.map(map_pending_approval).transpose()
    }

    pub async fn resolve_approval(
        &self,
        approval_id: &ApprovalId,
        status: ApprovalStatus,
        resolved_by: TelegramUserId,
    ) -> AppResult<()> {
        sqlx::query(
            r#"
            UPDATE pending_approvals
            SET status = ?2, resolved_by = ?3
            WHERE approval_id = ?1
            "#,
        )
        .bind(approval_id.0.to_string())
        .bind(status.as_str())
        .bind(resolved_by.0)
        .execute(&self.pool)
        .await?;
        Ok(())
    }
}

fn map_chat_binding(row: sqlx::sqlite::SqliteRow) -> AppResult<ChatBinding> {
    let active_session_id = row
        .get::<Option<String>, _>("active_session_id")
        .map(|value| parse_uuid(&value).map(SessionId))
        .transpose()?;

    Ok(ChatBinding {
        chat_id: TelegramChatId(row.get::<i64, _>("chat_id")),
        active_session_id,
        chat_kind: row.get::<String, _>("chat_kind"),
        title: row.get::<Option<String>, _>("title"),
    })
}

fn map_session(row: sqlx::sqlite::SqliteRow) -> AppResult<SessionRecord> {
    Ok(SessionRecord {
        session_id: SessionId(parse_uuid(&row.get::<String, _>("session_id"))?),
        chat_id: TelegramChatId(row.get::<i64, _>("chat_id")),
        workspace_path: WorkspacePath(row.get::<String, _>("workspace_path")),
        backend: SessionBackend::parse(&row.get::<String, _>("backend")).ok_or_else(|| {
            AppError::Storage(sqlx::Error::Decode("invalid session backend".into()))
        })?,
        provider_thread_id: row
            .get::<Option<String>, _>("provider_thread_id")
            .map(CodexThreadId),
        resume_cursor_json: row.get::<Option<String>, _>("resume_cursor_json"),
        status: SessionStatus::parse(&row.get::<String, _>("status")).ok_or_else(|| {
            AppError::Storage(sqlx::Error::Decode("invalid session status".into()))
        })?,
        last_error: row.get::<Option<String>, _>("last_error"),
        created_at: parse_datetime(&row.get::<String, _>("created_at"))?,
        updated_at: parse_datetime(&row.get::<String, _>("updated_at"))?,
    })
}

fn map_session_summary(row: sqlx::sqlite::SqliteRow) -> AppResult<SessionSummary> {
    Ok(SessionSummary {
        session_id: SessionId(parse_uuid(&row.get::<String, _>("session_id"))?),
        chat_id: TelegramChatId(row.get::<i64, _>("chat_id")),
        chat_title: row.get::<Option<String>, _>("title"),
        workspace_path: WorkspacePath(row.get::<String, _>("workspace_path")),
        backend: SessionBackend::parse(&row.get::<String, _>("backend")).ok_or_else(|| {
            AppError::Storage(sqlx::Error::Decode("invalid session backend".into()))
        })?,
        status: SessionStatus::parse(&row.get::<String, _>("status")).ok_or_else(|| {
            AppError::Storage(sqlx::Error::Decode("invalid session status".into()))
        })?,
        provider_thread_id: row
            .get::<Option<String>, _>("provider_thread_id")
            .map(CodexThreadId),
        last_error: row.get::<Option<String>, _>("last_error"),
        created_at: parse_datetime(&row.get::<String, _>("created_at"))?,
    })
}

fn map_pending_approval(row: sqlx::sqlite::SqliteRow) -> AppResult<PendingApproval> {
    Ok(PendingApproval {
        approval_id: ApprovalId(parse_uuid(&row.get::<String, _>("approval_id"))?),
        session_id: SessionId(parse_uuid(&row.get::<String, _>("session_id"))?),
        chat_id: TelegramChatId(row.get::<i64, _>("chat_id")),
        payload: row.get::<String, _>("payload"),
        summary: row.get::<String, _>("summary"),
        status: ApprovalStatus::parse(&row.get::<String, _>("status")).ok_or_else(|| {
            AppError::Storage(sqlx::Error::Decode("invalid approval status".into()))
        })?,
        created_at: parse_datetime(&row.get::<String, _>("created_at"))?,
        resolved_by: row.get::<Option<i64>, _>("resolved_by").map(TelegramUserId),
    })
}

fn parse_uuid(value: &str) -> AppResult<uuid::Uuid> {
    uuid::Uuid::parse_str(value)
        .map_err(|error| AppError::Validation(format!("invalid UUID {value}: {error}")))
}

fn parse_datetime(value: &str) -> AppResult<DateTime<Utc>> {
    Ok(DateTime::parse_from_rfc3339(value)
        .map_err(|error| AppError::Validation(format!("invalid timestamp {value}: {error}")))?
        .with_timezone(&Utc))
}

#[cfg(test)]
mod tests {
    use chrono::Utc;

    use super::Storage;
    use crate::domain::{
        ChatBinding, CodexThreadId, SessionBackend, SessionId, SessionRecord, SessionStatus,
        TelegramChatId, WorkspacePath,
    };

    #[tokio::test]
    async fn stores_and_reads_active_session_binding() {
        let storage = Storage::connect("sqlite::memory:").await.unwrap();
        storage
            .upsert_chat(TelegramChatId(10), "supergroup", Some("Atlas"))
            .await
            .unwrap();

        let session = SessionRecord {
            session_id: SessionId::new(),
            chat_id: TelegramChatId(10),
            workspace_path: WorkspacePath("/tmp/project".into()),
            backend: SessionBackend::AppServer,
            provider_thread_id: None,
            resume_cursor_json: None,
            status: SessionStatus::Ready,
            last_error: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };
        storage.insert_session(&session).await.unwrap();
        storage
            .set_active_session(TelegramChatId(10), Some(&session.session_id))
            .await
            .unwrap();

        let chat = storage.get_chat(TelegramChatId(10)).await.unwrap();
        let active = storage
            .get_active_session_for_chat(TelegramChatId(10))
            .await
            .unwrap()
            .unwrap();

        assert_eq!(
            chat,
            Some(ChatBinding {
                chat_id: TelegramChatId(10),
                active_session_id: Some(session.session_id.clone()),
                chat_kind: "supergroup".into(),
                title: Some("Atlas".into()),
            })
        );
        assert_eq!(active.workspace_path.0, "/tmp/project");
        assert_eq!(active.backend, SessionBackend::AppServer);
    }

    #[tokio::test]
    async fn marks_interrupted_app_server_sessions_failed() {
        let storage = Storage::connect("sqlite::memory:").await.unwrap();
        let session = SessionRecord {
            session_id: SessionId::new(),
            chat_id: TelegramChatId(12),
            workspace_path: WorkspacePath("/tmp/project".into()),
            backend: SessionBackend::AppServer,
            provider_thread_id: Some(CodexThreadId("thread_123".into())),
            resume_cursor_json: Some(r#"{"threadId":"thread_123"}"#.into()),
            status: SessionStatus::Running,
            last_error: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };
        storage.insert_session(&session).await.unwrap();

        storage
            .mark_interrupted_app_server_sessions_failed()
            .await
            .unwrap();

        let updated = storage
            .list_sessions()
            .await
            .unwrap()
            .into_iter()
            .find(|item| item.session_id == session.session_id)
            .unwrap();
        assert_eq!(updated.status, SessionStatus::Failed);
        assert!(updated.last_error.unwrap().contains("Atlas2 restarted"));
    }
}
