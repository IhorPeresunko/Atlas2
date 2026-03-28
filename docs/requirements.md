# Atlas2 Requirements

## Product Model

- Atlas2 connects Telegram groups to Codex CLI sessions running on the host machine.
- One Telegram group maps to one active Codex session at a time.
- A group can replace its active session by running `/new`, which always creates a fresh session.
- Atlas2 is a proxy/orchestrator around Codex CLI, not a replacement for Codex.

## Telegram UX

- The primary interface is a Telegram group.
- The bot must be added to the group and group admins are the only users allowed to create sessions or resolve approvals.
- `/start` and `/help` show available commands.
- `/new` starts a folder-selection flow inside the current group.
- `/sessions` lists all known sessions globally, including the group and workspace bound to each.
- `/plan <prompt>` runs a plan-only Codex turn for the current session.
- Any non-command text in a group with an active session is treated as a prompt for Codex.
- Telegram `voice` messages in a group with an active session should be transcribed and treated as prompts for Codex when STT is enabled.

## Folder Selection

- A session must never start without an explicit validated working directory.
- Folder selection happens inside Telegram through inline buttons.
- The folder browser starts at `/`.
- Users can navigate down into directories, move up to the parent directory, cancel the flow, or select the current directory.
- Callback payloads must stay within Telegram limits; folder browsing must not rely on raw absolute paths inside callback data.
- After selecting a folder, the original folder-selection message should be replaced with a status message such as `Started new session in X`.
- Any selected path must be normalized, canonicalized, exist on disk, and be a directory.
- v1 allows selecting any absolute directory visible on the host machine.

## Codex Session Behavior

- Atlas2 uses the local `codex` binary on the host machine.
- A fresh session starts on the first prompt after `/new`.
- Follow-up prompts resume the stored Codex provider thread through `codex app-server`.
- New Atlas2 sessions should use `codex app-server --session-source cli` so Telegram-originated work is resumable from normal Codex CLI history.
- Codex runs with the selected workspace as its working directory.
- Plan-mode turns must always be available through Telegram and must be routed as read-only planning requests rather than normal execution turns.
- Session metadata must persist across restarts in SQLite.
- Session isolation must be preserved across groups.
- Prompts for the same group must be serialized so overlapping turns do not corrupt session state.

## Telegram Output and Status

- Codex output should be streamed back into Telegram as the turn progresses.
- Each streamed Codex chunk should be sent as its own Telegram message in arrival order.
- Progress updates, command execution output, and agent text should each be reflected as separate Telegram messages.
- Approval requests should be posted as separate messages with inline buttons.

## Approval Flow

- Atlas2 should surface Codex approval/action requests as Telegram buttons whenever the Codex event stream exposes them.
- Group admins can approve or reject via Telegram buttons.
- Approval decisions must be persisted in SQLite.
- Invalid, stale, or repeated approval clicks must be rejected safely.
- Approval decisions should continue the live app-server turn when the runtime is still active.
- After an Atlas2 restart, previously pending approval buttons may become stale and must be rejected safely.

## Runtime and Distribution

- Atlas2 should run as a normal local binary on a VM or workstation.
- On startup, Atlas2 should load the Telegram bot token from the process environment when available.
- If the token is not already present in the process environment, Atlas2 should load it from a local persisted token file when available.
- If no token is available from either source, Atlas2 should prompt once, keep the token in memory for the running process, and persist it to a local token file for later restarts.
- When started with `--stt-provider 11labs`, Atlas2 should load the ElevenLabs API key from `--stt-api-key` when provided, otherwise from a local persisted key file, otherwise by prompting once and persisting it for later restarts.
- Atlas2 should not depend on Docker or Docker Compose for normal use.
- SQLite is the default persistence backend for a shareable single-instance build.
- The local machine must already have `codex` installed and authenticated.

## Persistence

- SQLite stores:
  - Telegram chat metadata
  - active session bindings
  - session records, including backend marker, provider thread ID, resume cursor, and last error
  - folder browser state
  - pending approvals
- Data should survive process restarts.
- Database files should be created automatically if the parent directory exists or can be created.

## Non-Goals and Current Limits

- Atlas2 does not yet support rebinding a group to an older existing session.
- Atlas2 does not yet support a separate control chat outside the group workflow.
- Atlas2 does not recover an in-flight app-server turn across an Atlas2 process restart; the next prompt resumes from the last saved provider thread state instead.
- Atlas2 does not currently restrict folder browsing to an allowlist of roots.
