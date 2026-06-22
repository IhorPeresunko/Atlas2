# Atlas2

Atlas2 connects Telegram groups to a local coding agent — Codex or Claude.

Each Telegram group has one active agent session at a time. A group admin runs `/new`, selects a historic project or taps `Add new project` to browse for a working directory, then picks which agent (Codex or Claude) should run the session, and sends prompts in the group. Atlas2 drives the chosen provider (Codex via `codex app-server`, Claude via the `claude` CLI in streaming mode), streams its output back into Telegram as separate progress messages, and stores session state in SQLite. Both providers are available from a single `atlas2 start`; the choice is made per session in `/new`.

## Current Features

- `/new` historic project picker plus folder selection inside Telegram
- `/resume` to attach the active session to an existing Codex thread in the same workspace (including threads started from the laptop Codex CLI), showing its last 10 messages
- one active session per Telegram group
- prompts sent from Telegram to Codex
- Telegram `voice` messages transcribed through ElevenLabs STT when enabled
- `/plan <prompt>` for a read-only planning turn
- streamed progress/output back into Telegram as separate messages
- long plain-text outputs split across multiple Telegram messages when needed
- plan-mode multiple-choice follow-up questions rendered as Telegram buttons
- completed plans get Telegram follow-up buttons for `Implement` and `Add details`
- Stop button for live Codex turns
- approval buttons when exposed by the Codex event stream
- SQLite-backed session and approval state
- Telegram-created sessions persist a provider thread ID so they can be resumed through Codex CLI

## Run

Requirements:

- Linux x86_64
- a local agent binary installed and logged in for each provider you want to use: `codex` and/or `claude` (a session can only use a provider whose binary is present)

### Install

Install the prebuilt binary with the one-line installer:

```bash
curl -fsSL https://github.com/IhorPeresunko/Atlas2/releases/latest/download/atlas2-installer.sh | sh
```

This downloads the right binary for your platform (Linux x86_64, glibc or
static musl) and installs `atlas2` into `~/.cargo/bin`, adding it to your
`PATH`.

### Usage

Atlas2 is controlled through subcommands:

```bash
atlas2 set bottoken <token>   # store the Telegram bot token
atlas2 set ownerid <id>       # optional: pin the owner (otherwise claimed on first contact)
atlas2 start                  # launch in the background and return immediately
atlas2 status                 # report whether the background process is running
atlas2 stop                   # stop the background process
atlas2 upgrade                # update to the latest release, restarting if running
atlas2 run                    # run in the foreground (blocks the terminal)
```

`atlas2 start` spawns a detached background process, writes its PID to `~/.local/state/atlas2/atlas2.pid`, and streams logs to `~/.local/state/atlas2/atlas2.log`. It returns control to your shell right away and the process survives the terminal that launched it. Configure the bot token with `atlas2 set bottoken <token>` first, since a background process cannot prompt for it.

## Access control (important)

Atlas2 drives a coding agent that can read and write files and run commands on the
machine it runs on, with the privileges of the user running `atlas2`. Because of
that, **the bot only responds to you (the owner) and to groups you have explicitly
authorized.** Everyone else is ignored.

**Becoming the owner is automatic.** The first person to message the bot or add it
to a group becomes its owner (trust-on-first-use), and that's locked in from then
on. So right after creating the bot with BotFather, just send it a direct message —
you're now the owner. No need to look up your numeric ID. (Telegram's Bot API does
not expose a bot's creator, which is why ownership is established this way.)

- **Only the owner can enable a group.** When *you* add the bot to a group, that
  group is authorized automatically and every member of it can then use the bot.
  When anyone else adds the bot to a group, the bot **leaves immediately**.
- **Direct messages work only for the owner.** Nobody else can DM the bot.
- For groups the bot is already in (e.g. after upgrading to this version), run
  `/activate` in the group as the owner to authorize it; `/deactivate` revokes it.
- Until someone claims it, the bot stays inert and authorizes nothing
  (fail-closed). Claim it yourself before sharing the bot's username.

To pin the owner explicitly instead of relying on first-contact (e.g. for a
scripted deploy, or to override a mistaken claim), set your Telegram numeric user
ID via `atlas2 set ownerid <id>` or the `ATLAS2_OWNER_ID` environment variable; an
explicit value always wins and disables auto-claim. Find your ID with
[@userinfobot](https://t.me/userinfobot).

As defense-in-depth, you can also disable "Allow Groups?" in BotFather, or leave it
on — the owner check above is what actually enforces access. Telegram itself has no
setting to restrict *who* may add a bot to a group, so this is enforced in Atlas2.

`atlas2 upgrade` downloads the latest release in place and restarts the background daemon if it was running.

### Files

Atlas2 follows the XDG base-directory layout (overridable per item via env vars):

| Purpose | Default location | Override |
| --- | --- | --- |
| Credentials (bot token, STT key, owner ID) | `~/.config/atlas2/` | `ATLAS2_TELEGRAM_BOT_TOKEN_FILE`, `ATLAS2_STT_API_KEY_FILE`, `ATLAS2_OWNER_ID_FILE` |
| SQLite database | `~/.local/share/atlas2/atlas2.sqlite` | `ATLAS2_DATABASE_PATH` |
| Log + PID files | `~/.local/state/atlas2/` | `XDG_STATE_HOME` |

Atlas2 loads the Telegram bot token from `ATLAS2_TELEGRAM_BOT_TOKEN` when set. Otherwise it reuses the persisted token from `~/.config/atlas2/telegram_bot_token` (the file written by `atlas2 set bottoken`), or prompts once when run in the foreground and saves it for later restarts.

Enable voice-message transcription with ElevenLabs by passing the flag to `start` or `run`:

```bash
atlas2 set sttkey sk_...      # store the ElevenLabs API key
atlas2 start --stt-provider 11labs
```

When `--stt-provider 11labs` is enabled, Atlas2 loads the ElevenLabs API key from `--stt-api-key` when provided. Otherwise it reuses the persisted key from `~/.config/atlas2/stt_api_key` (the file written by `atlas2 set sttkey`), or prompts once when run in the foreground and saves it for later restarts. A key passed to `atlas2 start --stt-api-key` is persisted so the background process can reload it on restart.

You can also provide both flags directly:

```bash
atlas2 run --stt-provider 11labs --stt-api-key sk_...
```

## Telegram Flow

1. As the owner, add the bot to a Telegram group (this authorizes the group automatically; for a group the bot is already in, send `/activate`). Add your teammates to the same group so they can use it too.
2. Send `/new`.
3. Reuse a historic project or tap `Add new project` and select a folder, then pick the agent (Codex or Claude) for the session.
4. Send prompts in the group.
5. Send a Telegram voice message to have Atlas2 transcribe it and forward the transcript to Codex.
6. Use `/plan <prompt>` when you want a plan-only turn without file changes.
7. Use `/sessions` to list known sessions.
8. Use the `Stop` button on a running turn to interrupt the live Codex execution.

## Notes

- Atlas2 is designed as a local binary, not a Docker-first app.
- SQLite is the default persistence backend.
- Approval decisions continue the live app-server turn while Atlas2 is running.
