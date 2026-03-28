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
- Any non-command text in a group with an active session is treated as a prompt for Codex.

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
- Follow-up prompts resume the stored Codex thread using `codex exec resume <thread_id>`.
- Codex runs with the selected workspace as its working directory.
- Session metadata must persist across restarts in SQLite.
- Session isolation must be preserved across groups.
- Prompts for the same group must be serialized so overlapping turns do not corrupt session state.

## Telegram Output and Status

- Codex output should be streamed back into Telegram as the turn progresses.
- In groups, streaming is implemented by editing a live bot message.
- Telegram `sendMessageDraft` may be used only where the Bot API allows it; current group behavior relies on message edits.
- Progress updates, command execution output, and agent text should be reflected in the live message.
- Approval requests should be posted as separate messages with inline buttons.

## Approval Flow

- Atlas2 should surface Codex approval/action requests as Telegram buttons whenever the Codex event stream exposes them.
- Group admins can approve or reject via Telegram buttons.
- Approval decisions must be persisted in SQLite.
- Invalid, stale, or repeated approval clicks must be rejected safely.
- Current limitation: Atlas2 records the approval decision, but fully automatic continuation of the interrupted exec-mode turn is not yet available through the current `codex exec --json` integration. The next prompt continues the workflow.

## Runtime and Distribution

- Atlas2 should run as a normal local binary on a VM or workstation.
- On startup, Atlas2 should prompt interactively for the Telegram bot token if it is not already present in the process environment.
- The prompted bot token should stay in process memory and does not need to be written to `.env`.
- Atlas2 should not depend on Docker or Docker Compose for normal use.
- SQLite is the default persistence backend for a shareable single-instance build.
- The local machine must already have `codex` installed and authenticated.

## Persistence

- SQLite stores:
  - Telegram chat metadata
  - active session bindings
  - session records
  - folder browser state
  - pending approvals
- Data should survive process restarts.
- Database files should be created automatically if the parent directory exists or can be created.

## Non-Goals and Current Limits

- Atlas2 does not yet support rebinding a group to an older existing session.
- Atlas2 does not yet support a separate control chat outside the group workflow.
- Atlas2 does not yet implement a fully resumable approval continuation loop inside a single paused exec turn.
- Atlas2 does not currently restrict folder browsing to an allowlist of roots.
