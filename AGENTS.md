# AGENTS.md

## Purpose

Atlas2 is a Rust service that connects Telegram groups to Codex sessions running remotely.

Core product behavior:
- Each Codex session is represented by a separate Telegram group.
- A session must run from a selected project folder on the VPS.
- Users send messages in Telegram and receive Codex output back in Telegram.
- When Codex requires approval or action, Atlas2 must present that as Telegram buttons.
- Atlas2 is a proxy/orchestrator, not a replacement for Codex itself.

---

## General Rules

- Always study the codebase before making changes.
- Never guess about existing behavior, architecture, or intent. Read the relevant code first.
- If the codebase and docs are insufficient to make a safe decision, ask the user for clarification.
- Avoid emojis.
- Avoid adding documentation files unless truly necessary. Prefer updating existing docs first.
- If project structure or ownership changes, update `/docs/ARCHITECTURE.md` and any relevant existing docs.

---

## Product Rules

- The primary UX is Telegram groups.
- One Telegram group maps to one Codex session.
- One Codex session must have one explicit working directory.
- Never run a session without a known, validated entry-point folder.
- Folder selection is a first-class requirement, not an optional enhancement.
- Approval requests from Codex must be surfaced as Telegram buttons whenever possible.
- Telegram should receive Codex output as streamed progress updates, not only final results.
- Preserve session isolation. Messages, state, approvals, and working directory must not leak across groups.

---

## Architecture Rules

- Apply strict separation of concerns.
- Keep Telegram transport concerns separate from Codex process management.
- Keep session state management separate from Telegram handlers.
- Keep filesystem/project-folder validation separate from business orchestration.
- Keep infrastructure adapters focused on external systems only:
  - Telegram Bot API
  - Codex CLI process execution
  - database/storage
  - filesystem access
- Keep business logic in services, not in Telegram handlers or process wrappers.
- Do not hide business rules inside:
  - Telegram update handlers
  - inline process-spawning code
  - database query helpers
  - ad-hoc utility functions
- Prefer explicit domain concepts over loose maps and stringly-typed state.

---

## Rust Rules

- The project should be written in Rust.
- Prefer boring, explicit Rust over clever Rust.
- Keep modules small and responsibility-focused.
- Prefer composition over large, stateful god-objects.
- Avoid massive files. Split modules when responsibilities start mixing.
- Use strong types for IDs and important state where practical:
  - session IDs
  - Telegram chat/group IDs
  - project/workspace IDs
  - approval/action IDs
- Avoid passing raw strings everywhere for domain concepts that deserve types.
- Keep handlers thin:
  - parse input
  - authenticate/authorize
  - call service layer
  - map result to response/output
- Keep services responsible for:
  - business rules
  - validation
  - orchestration
  - state transitions
- Keep repositories/adapters responsible for:
  - persistence
  - external API calls
  - process I/O
  - filesystem I/O
- Do not place business policy in repositories or Telegram callback handlers.
- Prefer `Result`-based error handling with explicit error types.
- Return actionable errors. Avoid vague failures.
- Use async only where it helps I/O and streaming. Do not introduce unnecessary async complexity.

---

## Testing Rules

- Add tests for every new behavior boundary you introduce.
- Prefer focused tests over broad, fragile end-to-end tests.
- Test at the layer where the rule lives:
  - service tests for business rules and state transitions
  - adapter tests for Codex/Telegram integration behavior
  - path validation tests for project folder safety
  - handler tests for callback routing and request parsing
- Cover failure paths, not only happy paths.
- For session logic, test:
  - session creation
  - folder validation
  - group-to-session mapping
  - streaming state updates
  - approval flow
  - invalid or repeated approval clicks
- Do not rely on compilation as proof of correctness.

---

## Documentation Rules

- Keep documentation concise and accurate.
- Update existing docs when architecture or behavior changes.
- Do not create speculative docs for features that do not exist yet.

---

## Change Checklist

Before handing off a change, verify:
1. The codebase was studied first.
2. The change respects the Telegram-group-per-session model.
3. The session has an explicit validated working directory.
4. Business logic remains outside handlers/adapters.
5. The change does not break session isolation.
6. Relevant tests were added or updated.
7. Relevant docs were updated if needed.
8. No unnecessary files or abstractions were introduced.