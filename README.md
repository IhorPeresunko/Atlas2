# Atlas2

Atlas2 connects Telegram groups to local Codex CLI sessions.

Each Telegram group has one active Codex session at a time. A group admin runs `/new`, selects a working directory on the host, and then sends prompts in the group. Atlas2 streams Codex output back into Telegram and stores session state in SQLite.

## Current Features

- `/new` folder selection inside Telegram
- one active session per Telegram group
- prompts sent from Telegram to Codex
- streamed progress/output back into Telegram
- approval buttons when exposed by the Codex event stream
- SQLite-backed session and approval state

## Run

Requirements:

- Rust
- local `codex` binary installed and logged in

Start Atlas2:

```bash
cargo run
```

Atlas2 will prompt for the Telegram bot token at startup.

## Telegram Flow

1. Add the bot to a Telegram group.
2. Make the bot an admin.
3. Send `/new`.
4. Select a folder.
5. Send prompts in the group.
6. Use `/sessions` to list known sessions.

## Notes

- Atlas2 is designed as a local binary, not a Docker-first app.
- SQLite is the default persistence backend.
- Approval decisions are recorded, but fully automatic continuation of an interrupted approval-bound exec turn is not complete yet.
# atlas2
