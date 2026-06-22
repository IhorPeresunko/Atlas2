use std::str::FromStr;

use chrono::{DateTime, Utc};
use sqlx::{
    Row, SqlitePool,
    sqlite::{SqliteConnectOptions, SqlitePoolOptions},
};

use crate::{
    domain::{
        ApprovalId, ApprovalStatus, ChatBinding, ThreadId, FolderBrowseState, HistoricProject,
        PendingApproval, PendingPlanFollowUp, PendingUserInput, PlanFollowUpId, PlanFollowUpStatus,
        ProviderKind, SessionId, SessionRecord, SessionStatus, SessionSummary, TelegramChatId,
        TelegramUserId, UserInputRequestId, UserInputStatus, WorkspacePath,
    },
    error::{AppError, AppResult},
};

#[derive(Clone)]
pub struct Storage {
    pool: SqlitePool,
}

impl Storage {
    pub async fn connect(database_url: &str) -> AppResult<Self> {
        // Create the database file if it does not exist yet, so a fresh install
        // (e.g. a new VM with no `data/` directory) works without a manual step.
        let connect_options = SqliteConnectOptions::from_str(database_url)
            .map_err(|error| {
                AppError::Config(format!("invalid database URL '{database_url}': {error}"))
            })?
            .create_if_missing(true);
        let pool = SqlitePoolOptions::new()
            .max_connections(5)
            .connect_with(connect_options)
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
                active_session_id TEXT,
                model TEXT,
                reasoning_effort TEXT,
                dangerously_skip_permissions INTEGER NOT NULL DEFAULT 0,
                authorized INTEGER NOT NULL DEFAULT 0
            );

            CREATE TABLE IF NOT EXISTS sessions (
                session_id TEXT PRIMARY KEY,
                chat_id INTEGER NOT NULL,
                workspace_path TEXT NOT NULL,
                provider TEXT NOT NULL DEFAULT 'codex',
                provider_thread_id TEXT,
                resume_cursor_json TEXT,
                status TEXT NOT NULL,
                last_error TEXT,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS app_settings (
                key TEXT PRIMARY KEY,
                value TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS folder_browse_state (
                chat_id INTEGER PRIMARY KEY,
                current_path TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS pending_new_session (
                chat_id INTEGER PRIMARY KEY,
                workspace_path TEXT NOT NULL
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

            CREATE TABLE IF NOT EXISTS pending_user_inputs (
                request_id TEXT PRIMARY KEY,
                session_id TEXT NOT NULL,
                chat_id INTEGER NOT NULL,
                questions_json TEXT NOT NULL,
                answers_json TEXT NOT NULL,
                status TEXT NOT NULL,
                created_at TEXT NOT NULL,
                resolved_by INTEGER
            );

            CREATE TABLE IF NOT EXISTS pending_plan_followups (
                follow_up_id TEXT PRIMARY KEY,
                session_id TEXT NOT NULL,
                chat_id INTEGER NOT NULL,
                plan_markdown TEXT NOT NULL,
                status TEXT NOT NULL,
                created_at TEXT NOT NULL,
                resolved_by INTEGER
            );
            "#,
        )
        .execute(&self.pool)
        .await?;

        self.ensure_session_column("provider", "TEXT NOT NULL DEFAULT 'codex'")
            .await?;
        self.ensure_session_column("provider_thread_id", "TEXT")
            .await?;
        self.ensure_session_column("resume_cursor_json", "TEXT")
            .await?;
        self.ensure_session_column("last_error", "TEXT").await?;
        self.ensure_chat_column("model", "TEXT").await?;
        self.ensure_chat_column("reasoning_effort", "TEXT").await?;
        self.ensure_chat_column("dangerously_skip_permissions", "INTEGER NOT NULL DEFAULT 0")
            .await?;
        self.ensure_chat_column("authorized", "INTEGER NOT NULL DEFAULT 0")
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

        sqlx::query(&format!(
            "ALTER TABLE sessions ADD COLUMN {name} {definition}"
        ))
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn ensure_chat_column(&self, name: &str, definition: &str) -> AppResult<()> {
        let rows = sqlx::query("PRAGMA table_info(chats)")
            .fetch_all(&self.pool)
            .await?;
        let exists = rows
            .into_iter()
            .any(|row| row.get::<String, _>("name") == name);
        if exists {
            return Ok(());
        }

        sqlx::query(&format!("ALTER TABLE chats ADD COLUMN {name} {definition}"))
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
            SELECT chat_id, active_session_id, chat_kind, title, model, reasoning_effort,
                   dangerously_skip_permissions, authorized
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

    /// Persist the per-chat Codex model preference. Upserts the chat row so the
    /// preference can be set before any session exists. `None` clears it, so
    /// turns fall back to Codex core's own default. Always resets the reasoning
    /// effort, since it is model-specific and a new model invalidates it.
    pub async fn set_chat_model(
        &self,
        chat_id: TelegramChatId,
        model: Option<&str>,
    ) -> AppResult<()> {
        sqlx::query(
            r#"
            INSERT INTO chats (chat_id, chat_kind, model, reasoning_effort)
            VALUES (?1, 'unknown', ?2, NULL)
            ON CONFLICT(chat_id) DO UPDATE SET
                model = excluded.model,
                reasoning_effort = NULL
            "#,
        )
        .bind(chat_id.0)
        .bind(model)
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    /// Persist the per-chat reasoning effort preference. `None` clears it, so
    /// turns fall back to the model's default effort in Codex core.
    pub async fn set_chat_reasoning_effort(
        &self,
        chat_id: TelegramChatId,
        reasoning_effort: Option<&str>,
    ) -> AppResult<()> {
        sqlx::query(
            r#"
            INSERT INTO chats (chat_id, chat_kind, reasoning_effort)
            VALUES (?1, 'unknown', ?2)
            ON CONFLICT(chat_id) DO UPDATE SET reasoning_effort = excluded.reasoning_effort
            "#,
        )
        .bind(chat_id.0)
        .bind(reasoning_effort)
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    /// Sets whether the chat's agent skips permission prompts (Claude's
    /// `bypassPermissions`). Upserts so it can be toggled before any session.
    pub async fn set_chat_skip_permissions(
        &self,
        chat_id: TelegramChatId,
        skip: bool,
    ) -> AppResult<()> {
        sqlx::query(
            r#"
            INSERT INTO chats (chat_id, chat_kind, dangerously_skip_permissions)
            VALUES (?1, 'unknown', ?2)
            ON CONFLICT(chat_id) DO UPDATE SET
                dangerously_skip_permissions = excluded.dangerously_skip_permissions
            "#,
        )
        .bind(chat_id.0)
        .bind(i64::from(skip))
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    /// Reads the owner's Telegram user ID claimed at runtime (trust-on-first-use),
    /// if one has been claimed. An explicit config/env owner takes precedence over
    /// this and is resolved by the caller.
    pub async fn get_owner_id(&self) -> AppResult<Option<i64>> {
        let row = sqlx::query("SELECT value FROM app_settings WHERE key = 'owner_id'")
            .fetch_optional(&self.pool)
            .await?;
        match row {
            Some(row) => {
                let value: String = row.get("value");
                let parsed = value.parse::<i64>().map_err(|error| {
                    AppError::Validation(format!("stored owner_id '{value}' is not an integer: {error}"))
                })?;
                Ok(Some(parsed))
            }
            None => Ok(None),
        }
    }

    /// Persists the claimed owner. Used once, the first time someone DMs the bot
    /// or adds it to a group while it is unclaimed.
    pub async fn set_owner_id(&self, user_id: i64) -> AppResult<()> {
        sqlx::query(
            r#"
            INSERT INTO app_settings (key, value)
            VALUES ('owner_id', ?1)
            ON CONFLICT(key) DO UPDATE SET value = excluded.value
            "#,
        )
        .bind(user_id.to_string())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Sets whether the bot owner has authorized this chat. Upserts so a chat can
    /// be authorized (e.g. via the owner adding the bot) before any message from
    /// it has been registered.
    pub async fn set_chat_authorized(
        &self,
        chat_id: TelegramChatId,
        authorized: bool,
    ) -> AppResult<()> {
        sqlx::query(
            r#"
            INSERT INTO chats (chat_id, chat_kind, authorized)
            VALUES (?1, 'unknown', ?2)
            ON CONFLICT(chat_id) DO UPDATE SET authorized = excluded.authorized
            "#,
        )
        .bind(chat_id.0)
        .bind(i64::from(authorized))
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    pub async fn insert_session(&self, session: &SessionRecord) -> AppResult<()> {
        sqlx::query(
            r#"
            INSERT INTO sessions (
                session_id, chat_id, workspace_path, provider,
                provider_thread_id, resume_cursor_json, status, last_error, created_at, updated_at
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
            "#,
        )
        .bind(session.session_id.0.to_string())
        .bind(session.chat_id.0)
        .bind(&session.workspace_path.0)
        .bind(session.provider.as_str())
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

    /// Loads a session by id regardless of whether it is the chat's active one.
    /// Used to route live-turn actions (approvals, stop) to the provider that
    /// owns the session.
    pub async fn get_session(&self, session_id: &SessionId) -> AppResult<Option<SessionRecord>> {
        let row = sqlx::query(
            r#"
            SELECT s.session_id, s.chat_id, s.workspace_path, s.provider, s.provider_thread_id,
                   s.resume_cursor_json, s.status, s.last_error, s.created_at, s.updated_at
            FROM sessions s
            WHERE s.session_id = ?1
            "#,
        )
        .bind(session_id.0.to_string())
        .fetch_optional(&self.pool)
        .await?;

        row.map(map_session).transpose()
    }

    pub async fn get_active_session_for_chat(
        &self,
        chat_id: TelegramChatId,
    ) -> AppResult<Option<SessionRecord>> {
        let row = sqlx::query(
            r#"
            SELECT s.session_id, s.chat_id, s.workspace_path, s.provider, s.provider_thread_id,
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
        provider_thread_id: Option<&ThreadId>,
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

    pub async fn mark_interrupted_sessions_failed(&self) -> AppResult<()> {
        sqlx::query(
            r#"
            UPDATE sessions
            SET status = 'failed',
                last_error = 'Atlas2 restarted while a turn was active. Send a new prompt to resume from the last saved thread.',
                updated_at = ?1
            WHERE status IN ('running', 'waiting_for_approval', 'waiting_for_input')
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
            SELECT s.session_id, s.chat_id, c.title, s.workspace_path, s.provider, s.status,
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

    pub async fn list_historic_projects_for_chat(
        &self,
        chat_id: TelegramChatId,
        limit: usize,
    ) -> AppResult<Vec<HistoricProject>> {
        let rows = sqlx::query(
            r#"
            SELECT session_id, workspace_path
            FROM (
                SELECT session_id, workspace_path, created_at,
                       ROW_NUMBER() OVER (
                           PARTITION BY workspace_path
                           ORDER BY created_at DESC
                       ) AS row_num
                FROM sessions
                WHERE chat_id = ?1
            ) ranked
            WHERE row_num = 1
            ORDER BY created_at DESC
            LIMIT ?2
            "#,
        )
        .bind(chat_id.0)
        .bind(limit as i64)
        .fetch_all(&self.pool)
        .await?;

        rows.into_iter()
            .map(|row| {
                Ok(HistoricProject {
                    source_session_id: SessionId(parse_uuid(&row.get::<String, _>("session_id"))?),
                    workspace_path: WorkspacePath(row.get::<String, _>("workspace_path")),
                })
            })
            .collect()
    }

    pub async fn get_session_workspace_for_chat(
        &self,
        chat_id: TelegramChatId,
        session_id: &SessionId,
    ) -> AppResult<Option<WorkspacePath>> {
        let row = sqlx::query(
            r#"
            SELECT workspace_path
            FROM sessions
            WHERE chat_id = ?1
              AND session_id = ?2
            LIMIT 1
            "#,
        )
        .bind(chat_id.0)
        .bind(session_id.0.to_string())
        .fetch_optional(&self.pool)
        .await?;

        Ok(row.map(|row| WorkspacePath(row.get::<String, _>("workspace_path"))))
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

    /// Records the workspace chosen for a not-yet-created session while the user
    /// picks a provider in the `/new` flow.
    pub async fn set_pending_new_session(
        &self,
        chat_id: TelegramChatId,
        workspace: &WorkspacePath,
    ) -> AppResult<()> {
        sqlx::query(
            r#"
            INSERT INTO pending_new_session (chat_id, workspace_path)
            VALUES (?1, ?2)
            ON CONFLICT(chat_id) DO UPDATE SET workspace_path = excluded.workspace_path
            "#,
        )
        .bind(chat_id.0)
        .bind(&workspace.0)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Reads and clears the pending new-session workspace for a chat.
    pub async fn take_pending_new_session(
        &self,
        chat_id: TelegramChatId,
    ) -> AppResult<Option<WorkspacePath>> {
        let row = sqlx::query("SELECT workspace_path FROM pending_new_session WHERE chat_id = ?1")
            .bind(chat_id.0)
            .fetch_optional(&self.pool)
            .await?;
        sqlx::query("DELETE FROM pending_new_session WHERE chat_id = ?1")
            .bind(chat_id.0)
            .execute(&self.pool)
            .await?;
        Ok(row.map(|row| WorkspacePath(row.get::<String, _>("workspace_path"))))
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

    pub async fn expire_pending_approvals_for_session(
        &self,
        session_id: &SessionId,
    ) -> AppResult<()> {
        sqlx::query(
            r#"
            UPDATE pending_approvals
            SET status = 'expired'
            WHERE session_id = ?1
              AND status = 'pending'
            "#,
        )
        .bind(session_id.0.to_string())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn insert_pending_user_input(&self, request: &PendingUserInput) -> AppResult<()> {
        sqlx::query(
            r#"
            INSERT INTO pending_user_inputs (
                request_id, session_id, chat_id, questions_json, answers_json, status, created_at, resolved_by
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
            "#,
        )
        .bind(request.request_id.0.to_string())
        .bind(request.session_id.0.to_string())
        .bind(request.chat_id.0)
        .bind(serde_json::to_string(&request.questions)?)
        .bind(serde_json::to_string(&request.answers)?)
        .bind(request.status.as_str())
        .bind(request.created_at.to_rfc3339())
        .bind(request.resolved_by.map(|user| user.0))
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn get_pending_user_input(
        &self,
        request_id: &UserInputRequestId,
    ) -> AppResult<Option<PendingUserInput>> {
        let row = sqlx::query(
            r#"
            SELECT request_id, session_id, chat_id, questions_json, answers_json, status, created_at, resolved_by
            FROM pending_user_inputs
            WHERE request_id = ?1
            "#,
        )
        .bind(request_id.0.to_string())
        .fetch_optional(&self.pool)
        .await?;

        row.map(map_pending_user_input).transpose()
    }

    pub async fn get_pending_user_input_for_chat(
        &self,
        chat_id: TelegramChatId,
    ) -> AppResult<Option<PendingUserInput>> {
        let row = sqlx::query(
            r#"
            SELECT request_id, session_id, chat_id, questions_json, answers_json, status, created_at, resolved_by
            FROM pending_user_inputs
            WHERE chat_id = ?1
              AND status = 'pending'
            ORDER BY created_at DESC
            LIMIT 1
            "#,
        )
        .bind(chat_id.0)
        .fetch_optional(&self.pool)
        .await?;

        row.map(map_pending_user_input).transpose()
    }

    pub async fn update_pending_user_input_answers(
        &self,
        request_id: &UserInputRequestId,
        answers_json: &str,
    ) -> AppResult<()> {
        sqlx::query(
            r#"
            UPDATE pending_user_inputs
            SET answers_json = ?2
            WHERE request_id = ?1
            "#,
        )
        .bind(request_id.0.to_string())
        .bind(answers_json)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn resolve_pending_user_input(
        &self,
        request_id: &UserInputRequestId,
        status: UserInputStatus,
        resolved_by: TelegramUserId,
        answers_json: &str,
    ) -> AppResult<()> {
        sqlx::query(
            r#"
            UPDATE pending_user_inputs
            SET status = ?2, resolved_by = ?3, answers_json = ?4
            WHERE request_id = ?1
            "#,
        )
        .bind(request_id.0.to_string())
        .bind(status.as_str())
        .bind(resolved_by.0)
        .bind(answers_json)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn expire_pending_user_inputs_for_session(
        &self,
        session_id: &SessionId,
    ) -> AppResult<()> {
        sqlx::query(
            r#"
            UPDATE pending_user_inputs
            SET status = 'expired'
            WHERE session_id = ?1
              AND status = 'pending'
            "#,
        )
        .bind(session_id.0.to_string())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn insert_pending_plan_follow_up(
        &self,
        follow_up: &PendingPlanFollowUp,
    ) -> AppResult<()> {
        sqlx::query(
            r#"
            INSERT INTO pending_plan_followups (
                follow_up_id, session_id, chat_id, plan_markdown, status, created_at, resolved_by
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
            "#,
        )
        .bind(follow_up.follow_up_id.0.to_string())
        .bind(follow_up.session_id.0.to_string())
        .bind(follow_up.chat_id.0)
        .bind(&follow_up.plan_markdown)
        .bind(follow_up.status.as_str())
        .bind(follow_up.created_at.to_rfc3339())
        .bind(follow_up.resolved_by.map(|user| user.0))
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn get_pending_plan_follow_up(
        &self,
        follow_up_id: &PlanFollowUpId,
    ) -> AppResult<Option<PendingPlanFollowUp>> {
        let row = sqlx::query(
            r#"
            SELECT follow_up_id, session_id, chat_id, plan_markdown, status, created_at, resolved_by
            FROM pending_plan_followups
            WHERE follow_up_id = ?1
            "#,
        )
        .bind(follow_up_id.0.to_string())
        .fetch_optional(&self.pool)
        .await?;

        row.map(map_pending_plan_follow_up).transpose()
    }

    pub async fn get_awaiting_plan_follow_up_for_chat(
        &self,
        chat_id: TelegramChatId,
    ) -> AppResult<Option<PendingPlanFollowUp>> {
        let row = sqlx::query(
            r#"
            SELECT follow_up_id, session_id, chat_id, plan_markdown, status, created_at, resolved_by
            FROM pending_plan_followups
            WHERE chat_id = ?1
              AND status = 'awaiting_refinement'
            ORDER BY created_at DESC
            LIMIT 1
            "#,
        )
        .bind(chat_id.0)
        .fetch_optional(&self.pool)
        .await?;

        row.map(map_pending_plan_follow_up).transpose()
    }

    pub async fn resolve_pending_plan_follow_up(
        &self,
        follow_up_id: &PlanFollowUpId,
        status: PlanFollowUpStatus,
        resolved_by: Option<TelegramUserId>,
    ) -> AppResult<()> {
        sqlx::query(
            r#"
            UPDATE pending_plan_followups
            SET status = ?2, resolved_by = ?3
            WHERE follow_up_id = ?1
            "#,
        )
        .bind(follow_up_id.0.to_string())
        .bind(status.as_str())
        .bind(resolved_by.map(|user| user.0))
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn expire_pending_plan_follow_ups_for_session(
        &self,
        session_id: &SessionId,
    ) -> AppResult<()> {
        sqlx::query(
            r#"
            UPDATE pending_plan_followups
            SET status = 'expired'
            WHERE session_id = ?1
              AND status IN ('pending', 'awaiting_refinement')
            "#,
        )
        .bind(session_id.0.to_string())
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
        model: row.get::<Option<String>, _>("model"),
        reasoning_effort: row.get::<Option<String>, _>("reasoning_effort"),
        dangerously_skip_permissions: row.get::<i64, _>("dangerously_skip_permissions") != 0,
        authorized: row.get::<i64, _>("authorized") != 0,
    })
}

fn map_session(row: sqlx::sqlite::SqliteRow) -> AppResult<SessionRecord> {
    Ok(SessionRecord {
        session_id: SessionId(parse_uuid(&row.get::<String, _>("session_id"))?),
        chat_id: TelegramChatId(row.get::<i64, _>("chat_id")),
        workspace_path: WorkspacePath(row.get::<String, _>("workspace_path")),
        provider: ProviderKind::parse(&row.get::<String, _>("provider")).ok_or_else(|| {
            AppError::Storage(sqlx::Error::Decode("invalid session provider".into()))
        })?,
        provider_thread_id: row
            .get::<Option<String>, _>("provider_thread_id")
            .map(ThreadId),
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
        provider: ProviderKind::parse(&row.get::<String, _>("provider")).ok_or_else(|| {
            AppError::Storage(sqlx::Error::Decode("invalid session provider".into()))
        })?,
        status: SessionStatus::parse(&row.get::<String, _>("status")).ok_or_else(|| {
            AppError::Storage(sqlx::Error::Decode("invalid session status".into()))
        })?,
        provider_thread_id: row
            .get::<Option<String>, _>("provider_thread_id")
            .map(ThreadId),
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

fn map_pending_user_input(row: sqlx::sqlite::SqliteRow) -> AppResult<PendingUserInput> {
    Ok(PendingUserInput {
        request_id: UserInputRequestId(parse_uuid(&row.get::<String, _>("request_id"))?),
        session_id: SessionId(parse_uuid(&row.get::<String, _>("session_id"))?),
        chat_id: TelegramChatId(row.get::<i64, _>("chat_id")),
        questions: serde_json::from_str(&row.get::<String, _>("questions_json"))?,
        answers: serde_json::from_str(&row.get::<String, _>("answers_json"))?,
        status: UserInputStatus::parse(&row.get::<String, _>("status")).ok_or_else(|| {
            AppError::Storage(sqlx::Error::Decode("invalid user input status".into()))
        })?,
        created_at: parse_datetime(&row.get::<String, _>("created_at"))?,
        resolved_by: row.get::<Option<i64>, _>("resolved_by").map(TelegramUserId),
    })
}

fn map_pending_plan_follow_up(row: sqlx::sqlite::SqliteRow) -> AppResult<PendingPlanFollowUp> {
    Ok(PendingPlanFollowUp {
        follow_up_id: PlanFollowUpId(parse_uuid(&row.get::<String, _>("follow_up_id"))?),
        session_id: SessionId(parse_uuid(&row.get::<String, _>("session_id"))?),
        chat_id: TelegramChatId(row.get::<i64, _>("chat_id")),
        plan_markdown: row.get::<String, _>("plan_markdown"),
        status: PlanFollowUpStatus::parse(&row.get::<String, _>("status")).ok_or_else(|| {
            AppError::Storage(sqlx::Error::Decode("invalid plan follow-up status".into()))
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
        ApprovalId, ApprovalStatus, ChatBinding, ThreadId, PendingApproval,
        PendingPlanFollowUp, PendingUserInput, PlanFollowUpId, PlanFollowUpStatus, ProviderKind,
        SessionId, SessionRecord, SessionStatus, TelegramChatId, UserInputAnswer, UserInputOption,
        UserInputQuestion, UserInputRequestId, UserInputStatus, WorkspacePath,
    };
    use std::collections::HashMap;

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
            provider: ProviderKind::Codex,
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
                authorized: false,
                model: None,
                reasoning_effort: None,
                dangerously_skip_permissions: false,
            })
        );
        assert_eq!(active.workspace_path.0, "/tmp/project");
        assert_eq!(active.provider, ProviderKind::Codex);
    }

    #[tokio::test]
    async fn stores_and_toggles_skip_permissions() {
        let storage = Storage::connect("sqlite::memory:").await.unwrap();
        let chat_id = TelegramChatId(42);
        storage.upsert_chat(chat_id, "group", None).await.unwrap();

        // Defaults to off.
        let chat = storage.get_chat(chat_id).await.unwrap().unwrap();
        assert!(!chat.dangerously_skip_permissions);

        storage.set_chat_skip_permissions(chat_id, true).await.unwrap();
        let chat = storage.get_chat(chat_id).await.unwrap().unwrap();
        assert!(chat.dangerously_skip_permissions);

        storage.set_chat_skip_permissions(chat_id, false).await.unwrap();
        let chat = storage.get_chat(chat_id).await.unwrap().unwrap();
        assert!(!chat.dangerously_skip_permissions);
    }

    #[tokio::test]
    async fn stores_and_reads_claimed_owner() {
        let storage = Storage::connect("sqlite::memory:").await.unwrap();

        assert_eq!(storage.get_owner_id().await.unwrap(), None);

        storage.set_owner_id(12345).await.unwrap();
        assert_eq!(storage.get_owner_id().await.unwrap(), Some(12345));

        // Idempotent overwrite (last writer wins).
        storage.set_owner_id(67890).await.unwrap();
        assert_eq!(storage.get_owner_id().await.unwrap(), Some(67890));
    }

    #[tokio::test]
    async fn stores_and_toggles_chat_authorization() {
        let storage = Storage::connect("sqlite::memory:").await.unwrap();
        let chat_id = TelegramChatId(77);

        // A chat the owner authorizes before it has ever sent a message: the
        // setter upserts the row.
        storage.set_chat_authorized(chat_id, true).await.unwrap();
        let chat = storage.get_chat(chat_id).await.unwrap().unwrap();
        assert!(chat.authorized);

        // upsert_chat (on every incoming message) must not clobber the flag.
        storage
            .upsert_chat(chat_id, "supergroup", Some("Team"))
            .await
            .unwrap();
        let chat = storage.get_chat(chat_id).await.unwrap().unwrap();
        assert!(chat.authorized);

        storage.set_chat_authorized(chat_id, false).await.unwrap();
        let chat = storage.get_chat(chat_id).await.unwrap().unwrap();
        assert!(!chat.authorized);
    }

    #[tokio::test]
    async fn marks_interrupted_sessions_failed() {
        let storage = Storage::connect("sqlite::memory:").await.unwrap();
        let session = SessionRecord {
            session_id: SessionId::new(),
            chat_id: TelegramChatId(12),
            workspace_path: WorkspacePath("/tmp/project".into()),
            provider: ProviderKind::Codex,
            provider_thread_id: Some(ThreadId("thread_123".into())),
            resume_cursor_json: Some(r#"{"threadId":"thread_123"}"#.into()),
            status: SessionStatus::Running,
            last_error: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };
        storage.insert_session(&session).await.unwrap();

        storage
            .mark_interrupted_sessions_failed()
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

    #[tokio::test]
    async fn expires_pending_approvals_for_session() {
        let storage = Storage::connect("sqlite::memory:").await.unwrap();
        let session = SessionRecord {
            session_id: SessionId::new(),
            chat_id: TelegramChatId(14),
            workspace_path: WorkspacePath("/tmp/project".into()),
            provider: ProviderKind::Codex,
            provider_thread_id: None,
            resume_cursor_json: None,
            status: SessionStatus::WaitingForApproval,
            last_error: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };
        storage.insert_session(&session).await.unwrap();

        let approval = PendingApproval {
            approval_id: ApprovalId::new(),
            session_id: session.session_id.clone(),
            chat_id: session.chat_id,
            payload: "{}".into(),
            summary: "Need approval".into(),
            status: ApprovalStatus::Pending,
            created_at: Utc::now(),
            resolved_by: None,
        };
        storage.insert_pending_approval(&approval).await.unwrap();

        storage
            .expire_pending_approvals_for_session(&session.session_id)
            .await
            .unwrap();

        let updated = storage
            .get_pending_approval(&approval.approval_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(updated.status, ApprovalStatus::Expired);
    }

    #[tokio::test]
    async fn stores_updates_and_expires_pending_user_inputs() {
        let storage = Storage::connect("sqlite::memory:").await.unwrap();
        let session = SessionRecord {
            session_id: SessionId::new(),
            chat_id: TelegramChatId(15),
            workspace_path: WorkspacePath("/tmp/project".into()),
            provider: ProviderKind::Codex,
            provider_thread_id: None,
            resume_cursor_json: None,
            status: SessionStatus::WaitingForInput,
            last_error: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };
        storage.insert_session(&session).await.unwrap();

        let request = PendingUserInput {
            request_id: UserInputRequestId::new(),
            session_id: session.session_id.clone(),
            chat_id: session.chat_id,
            questions: vec![UserInputQuestion {
                id: "next_step".into(),
                header: "Plan".into(),
                question: "What next?".into(),
                is_other: false,
                is_secret: false,
                options: Some(vec![UserInputOption {
                    label: "Implement".into(),
                    description: "Start implementation".into(),
                }]),
            }],
            answers: HashMap::new(),
            status: UserInputStatus::Pending,
            created_at: Utc::now(),
            resolved_by: None,
        };
        storage.insert_pending_user_input(&request).await.unwrap();

        let answers = HashMap::from([(
            "next_step".to_string(),
            UserInputAnswer {
                answers: vec!["Implement".into()],
            },
        )]);
        let answers_json = serde_json::to_string(&answers).unwrap();
        storage
            .update_pending_user_input_answers(&request.request_id, &answers_json)
            .await
            .unwrap();
        storage
            .resolve_pending_user_input(
                &request.request_id,
                UserInputStatus::Answered,
                crate::domain::TelegramUserId(99),
                &answers_json,
            )
            .await
            .unwrap();

        let updated = storage
            .get_pending_user_input(&request.request_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(updated.status, UserInputStatus::Answered);
        assert_eq!(
            updated.answers.get("next_step").unwrap().answers,
            vec!["Implement".to_string()]
        );

        storage
            .expire_pending_user_inputs_for_session(&session.session_id)
            .await
            .unwrap();
        let unchanged = storage
            .get_pending_user_input(&request.request_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(unchanged.status, UserInputStatus::Answered);
    }

    #[tokio::test]
    async fn finds_pending_user_input_for_chat() {
        let storage = Storage::connect("sqlite::memory:").await.unwrap();
        let session = SessionRecord {
            session_id: SessionId::new(),
            chat_id: TelegramChatId(17),
            workspace_path: WorkspacePath("/tmp/project".into()),
            provider: ProviderKind::Codex,
            provider_thread_id: None,
            resume_cursor_json: None,
            status: SessionStatus::WaitingForInput,
            last_error: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };
        storage.insert_session(&session).await.unwrap();

        let request = PendingUserInput {
            request_id: UserInputRequestId::new(),
            session_id: session.session_id.clone(),
            chat_id: session.chat_id,
            questions: vec![UserInputQuestion {
                id: "scope".into(),
                header: "Scope".into(),
                question: "Choose a scope".into(),
                is_other: false,
                is_secret: false,
                options: Some(vec![UserInputOption {
                    label: "Small".into(),
                    description: "Keep it narrow".into(),
                }]),
            }],
            answers: HashMap::new(),
            status: UserInputStatus::Pending,
            created_at: Utc::now(),
            resolved_by: None,
        };
        storage.insert_pending_user_input(&request).await.unwrap();

        let found = storage
            .get_pending_user_input_for_chat(session.chat_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(found.request_id, request.request_id);
    }

    #[tokio::test]
    async fn stores_and_resolves_pending_plan_follow_ups() {
        let storage = Storage::connect("sqlite::memory:").await.unwrap();
        let session = SessionRecord {
            session_id: SessionId::new(),
            chat_id: TelegramChatId(16),
            workspace_path: WorkspacePath("/tmp/project".into()),
            provider: ProviderKind::Codex,
            provider_thread_id: None,
            resume_cursor_json: None,
            status: SessionStatus::Ready,
            last_error: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };
        storage.insert_session(&session).await.unwrap();

        let follow_up = PendingPlanFollowUp {
            follow_up_id: PlanFollowUpId::new(),
            session_id: session.session_id.clone(),
            chat_id: session.chat_id,
            plan_markdown: "# Plan\n\n- Step".into(),
            status: PlanFollowUpStatus::Pending,
            created_at: Utc::now(),
            resolved_by: None,
        };
        storage
            .insert_pending_plan_follow_up(&follow_up)
            .await
            .unwrap();
        storage
            .resolve_pending_plan_follow_up(
                &follow_up.follow_up_id,
                PlanFollowUpStatus::AwaitingRefinement,
                Some(crate::domain::TelegramUserId(7)),
            )
            .await
            .unwrap();

        let updated = storage
            .get_pending_plan_follow_up(&follow_up.follow_up_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(updated.status, PlanFollowUpStatus::AwaitingRefinement);

        let awaiting = storage
            .get_awaiting_plan_follow_up_for_chat(session.chat_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(awaiting.follow_up_id, follow_up.follow_up_id);

        storage
            .expire_pending_plan_follow_ups_for_session(&session.session_id)
            .await
            .unwrap();
        let expired = storage
            .get_pending_plan_follow_up(&follow_up.follow_up_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(expired.status, PlanFollowUpStatus::Expired);
    }

    #[tokio::test]
    async fn lists_historic_projects_per_chat_by_recency_and_dedupes_paths() {
        let storage = Storage::connect("sqlite::memory:").await.unwrap();
        let chat_id = TelegramChatId(30);
        let now = Utc::now();
        let workspace_a = WorkspacePath("/tmp/project-a".into());
        let workspace_b = WorkspacePath("/tmp/project-b".into());

        storage
            .insert_session(&SessionRecord {
                session_id: SessionId::new(),
                chat_id,
                workspace_path: workspace_a.clone(),
                provider: ProviderKind::Codex,
                provider_thread_id: None,
                resume_cursor_json: None,
                status: SessionStatus::Ready,
                last_error: None,
                created_at: now - chrono::Duration::minutes(5),
                updated_at: now - chrono::Duration::minutes(5),
            })
            .await
            .unwrap();
        storage
            .insert_session(&SessionRecord {
                session_id: SessionId::new(),
                chat_id,
                workspace_path: workspace_b.clone(),
                provider: ProviderKind::Codex,
                provider_thread_id: None,
                resume_cursor_json: None,
                status: SessionStatus::Ready,
                last_error: None,
                created_at: now - chrono::Duration::minutes(3),
                updated_at: now - chrono::Duration::minutes(3),
            })
            .await
            .unwrap();
        storage
            .insert_session(&SessionRecord {
                session_id: SessionId::new(),
                chat_id,
                workspace_path: workspace_a.clone(),
                provider: ProviderKind::Codex,
                provider_thread_id: None,
                resume_cursor_json: None,
                status: SessionStatus::Ready,
                last_error: None,
                created_at: now - chrono::Duration::minutes(1),
                updated_at: now - chrono::Duration::minutes(1),
            })
            .await
            .unwrap();
        storage
            .insert_session(&SessionRecord {
                session_id: SessionId::new(),
                chat_id: TelegramChatId(31),
                workspace_path: WorkspacePath("/tmp/other-chat".into()),
                provider: ProviderKind::Codex,
                provider_thread_id: None,
                resume_cursor_json: None,
                status: SessionStatus::Ready,
                last_error: None,
                created_at: now,
                updated_at: now,
            })
            .await
            .unwrap();

        let historic = storage
            .list_historic_projects_for_chat(chat_id, 10)
            .await
            .unwrap();

        assert_eq!(historic.len(), 2);
        assert_eq!(historic[0].workspace_path, workspace_a);
        assert_eq!(historic[1].workspace_path, workspace_b);
    }

    #[tokio::test]
    async fn gets_session_workspace_only_for_matching_chat() {
        let storage = Storage::connect("sqlite::memory:").await.unwrap();
        let session = SessionRecord {
            session_id: SessionId::new(),
            chat_id: TelegramChatId(40),
            workspace_path: WorkspacePath("/tmp/project".into()),
            provider: ProviderKind::Codex,
            provider_thread_id: None,
            resume_cursor_json: None,
            status: SessionStatus::Ready,
            last_error: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };
        storage.insert_session(&session).await.unwrap();

        let matching = storage
            .get_session_workspace_for_chat(session.chat_id, &session.session_id)
            .await
            .unwrap();
        let mismatched = storage
            .get_session_workspace_for_chat(TelegramChatId(41), &session.session_id)
            .await
            .unwrap();

        assert_eq!(matching, Some(WorkspacePath("/tmp/project".into())));
        assert_eq!(mismatched, None);
    }
}
