# AI Agent Reference

This directory is optimized for AI agents, not human onboarding.

Rules:

- Follow `token-policy.md`.
- Use root `AGENTS.md` and `docs/ai-map.md` as the shortest entrypoints.
- Read only the smallest file needed.
- Prefer these files before scanning the whole repo.
- Treat human docs as explanatory; treat AI docs as compact contracts.
- Do not mirror bilingual content here.
- Verify code truth before editing when a contract references a source path.

Suggested load order:

1. Root `AGENTS.md`.
2. `docs/ai-map.md`.
3. `token-policy.md` for mandatory context rules.
4. `task-router.md` to choose the smallest context set.
5. One focused AI doc selected by the router.
6. `commands.md` only when running checks/builds.
7. `workflows.md` only when implementing a known task type.

Keep items 1-4 stable and first in prompts or explicit context caches.

Primary human docs:

- `docs/index.md`
- `docs/ai-map.md`
- `docs/getting-started.md`
- `docs/execution-flow.md`
- `docs/structure.md`
- `docs/logging.md`

Token policy:

- Canonical policy lives in `token-policy.md`.
- Start with `task-router.md`, then load only the focused docs/source files it names.
- Never inspect `build/`, `target/`, `logs/`, or `vendor/` unless the task exception requires it.
